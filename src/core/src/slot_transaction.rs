//! Durable recovery for the two-file progression/Continue replacement.
//! The caller holds the startup replacement guard. A crash during apply is
//! rolled back before network sync and before the startup barrier releases.
use crate::{atomic, local::identity_for_bytes};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

const MAX_FILE_SIZE: usize = 32 * 1024 * 1024;
const MAX_JOURNAL_SIZE: u64 = 512 * 1024 * 1024 + 65536;

#[derive(Serialize, Deserialize)]
struct Entry {
    relative: PathBuf,
    old: Vec<u8>,
    old_sha256: String,
    new: Vec<u8>,
    new_sha256: String,
}
#[derive(Serialize, Deserialize)]
struct Journal {
    committed: bool,
    entries: Vec<Entry>,
}

fn destination(home: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        bail!("invalid slot recovery path");
    }
    let path = home.join(relative);
    let parent = path
        .parent()
        .context("recovery path has no parent")?
        .canonicalize()?;
    if !parent.starts_with(home.canonicalize()?) {
        bail!("recovery path escapes the app sandbox");
    }
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && !metadata.file_type().is_file()
    {
        bail!("recovery target is not a regular file");
    }
    Ok(parent.join(
        path.file_name()
            .context("recovery target has no filename")?,
    ))
}

fn journal_path(support: &Path) -> PathBuf {
    support.join("state/pending-slot-transaction.json")
}

pub fn recover(home: &Path, support: &Path) -> Result<()> {
    let path = journal_path(support);
    if !path.exists() {
        return Ok(());
    }
    if fs::symlink_metadata(&path)?.len() > MAX_JOURNAL_SIZE
        || !fs::symlink_metadata(&path)?.file_type().is_file()
    {
        bail!("invalid slot recovery journal type or size");
    }
    let data = fs::read(&path)?;
    if data.len() as u64 > MAX_JOURNAL_SIZE {
        bail!("slot recovery journal exceeds safety limits");
    }
    let journal: Journal = serde_json::from_slice(&data)?;
    if journal.entries.len() != 2 {
        bail!("invalid slot recovery manifest");
    }
    // Validate the entire journal before restoring either file. A corrupt
    // second entry must not leave the first entry partially rolled back.
    let mut destinations = BTreeSet::new();
    for entry in &journal.entries {
        if entry.old.len() > MAX_FILE_SIZE
            || entry.new.len() > MAX_FILE_SIZE
            || identity_for_bytes(&entry.old).sha256 != entry.old_sha256
            || identity_for_bytes(&entry.new).sha256 != entry.new_sha256
            || !destinations.insert(destination(home, &entry.relative)?)
        {
            bail!("slot recovery manifest failed verification");
        }
    }
    if !journal.committed {
        for entry in &journal.entries {
            let target = destination(home, &entry.relative)?;
            atomic::write_bytes(&target, &entry.old)?;
            if identity_for_bytes(&fs::read(&target)?).sha256 != entry.old_sha256 {
                bail!("slot recovery replacement failed verification");
            }
        }
    }
    fs::remove_file(path)?;
    Ok(())
}

pub fn replace_pair(home: &Path, support: &Path, targets: [(&Path, &[u8]); 2]) -> Result<()> {
    // Never overwrite an unresolved recovery record.
    recover(home, support)?;
    let mut entries = Vec::new();
    let mut destinations = BTreeSet::new();
    for (path, new) in targets {
        let relative = path
            .strip_prefix(home)
            .context("slot target outside home")?
            .to_path_buf();
        let target = destination(home, &relative)?;
        if new.len() > MAX_FILE_SIZE || !destinations.insert(target.clone()) {
            bail!("invalid or duplicate slot transaction target");
        }
        if fs::metadata(&target).is_ok_and(|metadata| metadata.len() > MAX_FILE_SIZE as u64) {
            bail!("old slot file exceeds safety limits");
        }
        let old = match fs::read(target) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(error) => return Err(error.into()),
        };
        if old.len() > MAX_FILE_SIZE {
            bail!("old slot file changed beyond safety limits");
        }
        entries.push(Entry {
            relative,
            old_sha256: identity_for_bytes(&old).sha256,
            old,
            new_sha256: identity_for_bytes(new).sha256,
            new: new.to_vec(),
        });
    }
    let mut journal = Journal {
        committed: false,
        entries,
    };
    let path = journal_path(support);
    atomic::write_bytes(&path, &serde_json::to_vec(&journal)?)?;
    let apply = (|| -> Result<()> {
        for entry in &journal.entries {
            let target = destination(home, &entry.relative)?;
            atomic::write_bytes(&target, &entry.new)?;
            if identity_for_bytes(&fs::read(&target)?).sha256 != entry.new_sha256 {
                bail!("slot replacement verification failed");
            }
        }
        journal.committed = true;
        atomic::write_bytes(&path, &serde_json::to_vec(&journal)?)?;
        // A committed journal is harmless if cleanup is interrupted. Recovery
        // only removes it and must never roll a verified pair back afterwards.
        let _ = fs::remove_file(&path);
        Ok(())
    })();
    if let Err(error) = apply {
        recover(home, support).context("slot apply failed and recovery could not finish")?;
        return Err(error.context("slot apply failed; previous progression and Continue restored"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (PathBuf, PathBuf) {
        let home = std::env::temp_dir().join(format!("isaac-slot-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(home.join("Documents")).unwrap();
        let support = home.join("Library/Application Support/IsaacCloudSync");
        fs::create_dir_all(support.join("state")).unwrap();
        (home, support)
    }
    #[test]
    fn replaces_and_verifies_both_files() {
        let (home, support) = setup();
        let progress = home.join("Documents/progress");
        let run = home.join("Documents/run");
        fs::write(&progress, b"old-progress").unwrap();
        fs::write(&run, b"old-run").unwrap();
        replace_pair(
            &home,
            &support,
            [(&progress, b"new-progress"), (&run, b"new-run")],
        )
        .unwrap();
        assert_eq!(fs::read(progress).unwrap(), b"new-progress");
        assert_eq!(fs::read(run).unwrap(), b"new-run");
        assert!(!journal_path(&support).exists());
        let _ = fs::remove_dir_all(home);
    }
    #[test]
    fn interruption_recovers_both_old_files() {
        let (home, support) = setup();
        let entries = ["progress", "run"]
            .iter()
            .map(|name| Entry {
                relative: PathBuf::from(format!("Documents/{name}")),
                old: b"old".to_vec(),
                old_sha256: identity_for_bytes(b"old").sha256,
                new: b"new".to_vec(),
                new_sha256: identity_for_bytes(b"new").sha256,
            })
            .collect();
        let journal = Journal {
            committed: false,
            entries,
        };
        atomic::write_bytes(
            &journal_path(&support),
            &serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
        fs::write(home.join("Documents/progress"), b"new").unwrap();
        fs::write(home.join("Documents/run"), b"old").unwrap();
        recover(&home, &support).unwrap();
        assert_eq!(fs::read(home.join("Documents/progress")).unwrap(), b"old");
        assert_eq!(fs::read(home.join("Documents/run")).unwrap(), b"old");
        let _ = fs::remove_dir_all(home);
    }
    #[test]
    fn traversal_and_corrupted_recovery_are_rejected() {
        let (home, support) = setup();
        assert!(destination(&home, Path::new("../other")).is_err());
        let entry = || Entry {
            relative: PathBuf::from("Documents/run"),
            old: vec![1],
            old_sha256: "wrong".into(),
            new: vec![],
            new_sha256: "wrong".into(),
        };
        atomic::write_bytes(
            &journal_path(&support),
            &serde_json::to_vec(&Journal {
                committed: false,
                entries: vec![entry(), entry()],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(recover(&home, &support).is_err());
        assert!(journal_path(&support).exists());
        let _ = fs::remove_dir_all(home);
    }
}
