//! Continue-file support for the shared vanilla Repentance GSR0146 stream.
//! Repentance+ runs and rerun snapshots are intentionally not converted.
use crate::{
    isaac_format::{canonicalize_save, decode_raw_lz4, isaac_crc32},
    local::identity_for_bytes,
};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 16] = b"ISAACNG_GSR0146\0";
const MAX_SIZE: usize = 32 * 1024 * 1024;

pub fn slot_for_filename(name: &str) -> Option<u8> {
    let lower = name.to_ascii_lowercase();
    (1..=3).find(|slot| {
        lower == format!("rep_gamestate{slot}.dat") || lower == format!("gamestate{slot}.dat")
    })
}

pub fn canonicalize(input: &[u8]) -> Result<Vec<u8>> {
    if input.is_empty() {
        return Ok(Vec::new());
    } // Native no-Continue representation.
    if input.len() > MAX_SIZE {
        bail!("run save exceeds the safety limit");
    }
    let bytes = if input.starts_with(MAGIC) {
        input.to_vec()
    } else {
        if input.starts_with(b"ISAAC") {
            bail!("unsupported run-save version; Continue was not transferred");
        }
        decode_raw_lz4(input).context("invalid compressed Continue save")?
    };
    if bytes.len() < 64 || !bytes.starts_with(MAGIC) {
        bail!("Continue transfer supports only vanilla Repentance GSR0146");
    }
    let checksum = checksum(&bytes);
    let stored = u32::from_le_bytes(bytes[bytes.len() - 4..].try_into()?);
    if checksum != stored ^ 0x9669_6996 {
        bail!("Continue checksum mismatch; the valid save was not replaced");
    }
    Ok(bytes)
}

fn checksum(canonical: &[u8]) -> u32 {
    isaac_crc32(&canonical[16..canonical.len() - 4], 0xfedc_ba76)
}

/// The native loader also compares the run CRC with the progression stream's
/// first u32. Two individually valid files are not necessarily a valid pair.
pub fn validate_pair(progress: &[u8], run: &[u8]) -> Result<()> {
    let progress = canonicalize_save(progress)?.bytes;
    let run = canonicalize(run)?;
    if !run.is_empty() && u32::from_le_bytes(progress[16..20].try_into()?) != checksum(&run) {
        bail!("progression and Continue belong to different snapshots; transfer refused");
    }
    Ok(())
}

/// Encode the shared stream as a standard raw LZ4 literal-only block for iOS.
pub fn encode_for_ios(input: &[u8]) -> Result<Vec<u8>> {
    let bytes = canonicalize(input)?;
    if bytes.is_empty() {
        return Ok(bytes);
    }
    let mut encoded = vec![0xf0];
    let mut length = bytes.len() - 15;
    while length >= 255 {
        encoded.push(255);
        length -= 255;
    }
    encoded.push(length as u8);
    encoded.extend_from_slice(&bytes);
    if canonicalize(&encoded)? != bytes {
        bail!("Continue LZ4 encoding verification failed");
    }
    Ok(encoded)
}

pub fn local_path(persistent: &Path, slot: u8) -> Result<PathBuf> {
    let parent = persistent
        .parent()
        .context("persistent save has no directory")?;
    let prefixed = persistent
        .file_name()
        .and_then(|value| value.to_str())
        .is_some_and(|name| name.to_ascii_lowercase().starts_with("rep_"));
    Ok(parent.join(format!(
        "{}gamestate{slot}.dat",
        if prefixed { "rep_" } else { "" }
    )))
}

pub fn remote_name(persistent_name: &str, slot: u8) -> Result<String> {
    let (prefix, basename) = persistent_name
        .rsplit_once(['/', '\\'])
        .unwrap_or(("", persistent_name));
    if !basename.eq_ignore_ascii_case(&format!("rep_persistentgamedata{slot}.dat"))
        && !basename.eq_ignore_ascii_case(&format!("persistentgamedata{slot}.dat"))
    {
        bail!("run transfer is unavailable for this Steam generation");
    }
    let name = format!(
        "{}gamestate{slot}.dat",
        if basename.to_ascii_lowercase().starts_with("rep_") {
            "rep_"
        } else {
            ""
        }
    );
    Ok(if prefix.is_empty() {
        name
    } else {
        format!("{prefix}/{name}")
    })
}

pub fn read_optional(path: &Path) -> Result<(Vec<u8>, String)> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((vec![], identity_for_bytes(&[]).sha256));
        }
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_SIZE as u64 {
        bail!("invalid Continue file type or size");
    }
    let raw = std::fs::read(path)?;
    let after = std::fs::symlink_metadata(path)?;
    if raw.len() as u64 != metadata.len()
        || metadata.len() != after.len()
        || metadata.modified()? != after.modified()?
    {
        bail!("Continue file changed while being read");
    }
    Ok((canonicalize(&raw)?, identity_for_bytes(&raw).sha256))
}

pub fn bundle_hash(progression_sha256: &str, run: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"IsaacSteamSync/Repentance-slot-pair/v1\0");
    digest.update(progression_sha256.as_bytes());
    digest.update((run.len() as u64).to_le_bytes());
    digest.update(run);
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_state_and_supported_names_are_exact() {
        assert!(canonicalize(&[]).unwrap().is_empty());
        assert_eq!(slot_for_filename("rep_gamestate1.dat"), Some(1));
        assert_eq!(slot_for_filename("rep+gamestate1.dat"), None);
        assert_eq!(slot_for_filename("rep_rerunstate1.dat"), None);
        assert!(remote_name("rep+persistentgamedata1.dat", 1).is_err());
        assert_eq!(
            remote_name("path/rep_persistentgamedata2.dat", 2).unwrap(),
            "path/rep_gamestate2.dat"
        );
        assert!(canonicalize(b"ISAACNG_GSR0147\0unsupported").is_err());
        assert!(canonicalize(MAGIC).is_err());
    }
    #[test]
    fn run_changes_and_run_completion_change_bundle_identity() {
        assert_ne!(
            bundle_hash("progress", b"run-a"),
            bundle_hash("progress", b"run-b")
        );
        assert_ne!(
            bundle_hash("progress", b"run-a"),
            bundle_hash("progress", b"")
        );
        assert_ne!(
            bundle_hash("progress-a", b"run"),
            bundle_hash("progress-b", b"run")
        );
        use crate::sync_engine::{SyncDecision, decide};
        let base = bundle_hash("progress", b"run-a");
        let local = bundle_hash("progress", b"run-b");
        let remote = bundle_hash("progress", b"run-c");
        assert_eq!(
            decide(Some(&base), &local, &base),
            SyncDecision::UploadLocal
        );
        assert_eq!(
            decide(Some(&base), &base, &remote),
            SyncDecision::DownloadRemote
        );
        assert_eq!(decide(Some(&base), &local, &remote), SyncDecision::Conflict);
        assert_eq!(
            decide(None, &local, &remote),
            SyncDecision::FirstSyncChoiceRequired
        );
    }
    #[test]
    fn checksum_and_progression_link_are_both_required() {
        let mut run = MAGIC.to_vec();
        run.resize(100, 0);
        let crc = isaac_crc32(&run[16..], 0xfedc_ba76);
        run.extend_from_slice(&(crc ^ 0x9669_6996).to_le_bytes());
        assert_eq!(canonicalize(&run).unwrap(), run);
        assert_eq!(canonicalize(&encode_for_ios(&run).unwrap()).unwrap(), run);
        let mut progress =
            crate::save_compat::tests::fixture(crate::save_compat::CloudVersion::Repentance);
        assert!(validate_pair(&progress, &run).is_err());
        progress[16..20].copy_from_slice(&crc.to_le_bytes());
        let length = progress.len();
        let progress_crc = isaac_crc32(&progress[16..length - 4], 0xfedc_ba76);
        progress[length - 4..].copy_from_slice(&progress_crc.to_le_bytes());
        validate_pair(&progress, &run).unwrap();
        assert!(validate_pair(&progress, &[]).is_ok());
        run[24] ^= 1;
        assert!(canonicalize(&run).is_err());
        assert!(encode_for_ios(&run).is_err());
    }
    #[test]
    #[ignore = "requires private desktop and iOS Continue fixtures"]
    fn private_desktop_and_ios_streams_have_the_supported_version() {
        for key in ["ISAAC_IOS_RUN_FIXTURE", "ISAAC_DESKTOP_RUN_FIXTURE"] {
            let path =
                std::env::var(key).expect("set private fixture paths outside the repository");
            let raw = std::fs::read(path).unwrap();
            let normalized = canonicalize(&raw).unwrap();
            assert_eq!(&normalized[..16], MAGIC);
            eprintln!(
                "{key}: {} raw bytes, {} canonical bytes, header GSR0146",
                raw.len(),
                normalized.len()
            );
            let computed = crate::isaac_format::isaac_crc32(
                &normalized[16..normalized.len() - 4],
                0xfedc_ba76,
            );
            let footer = u32::from_le_bytes(normalized[normalized.len() - 4..].try_into().unwrap())
                ^ 0x9669_6996;
            eprintln!("{key}: CRC valid: {}", computed == footer);
            let progress_key = key.replace("RUN_FIXTURE", "PROGRESS_FIXTURE");
            if let Ok(path) = std::env::var(progress_key) {
                let progress =
                    crate::isaac_format::canonicalize_save(&std::fs::read(path).unwrap())
                        .unwrap()
                        .bytes;
                let positions: Vec<_> = progress
                    .windows(4)
                    .enumerate()
                    .filter_map(|(index, value)| (value == computed.to_le_bytes()).then_some(index))
                    .collect();
                eprintln!(
                    "{key}: matching progression offsets: {positions:?}, header link matches: {}, trailer link matches: {}",
                    progress[16..20] == computed.to_le_bytes(),
                    progress[progress.len() - 8..progress.len() - 4] == computed.to_le_bytes()
                );
            }
            if key == "ISAAC_DESKTOP_RUN_FIXTURE"
                && let Ok(output) = std::env::var("ISAAC_TEST_RUN_OUTPUT")
            {
                std::fs::write(output, encode_for_ios(&normalized).unwrap()).unwrap();
            }
        }
    }
}
