//! Synchronization identity excludes only the verified load counter and its
//! dependent CRC. Byte-for-byte SHA-256/Steam SHA-1 verification is unchanged.
//! Format reference: TeamREPENTOGON/REPENTOGON, SaveSyncing/SaveFile.h.
use crate::{
    backups::BackupManager, isaac_format::canonicalize_save, local::identity_for_bytes,
    model::FileSyncState, run_compat, run_state, save_compat,
};
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

pub fn comparison_hash(progress: &[u8], run: Option<&[u8]>) -> Result<String> {
    let canonical = canonicalize_save(progress)?.bytes;
    let mut hash = Sha256::new();
    hash.update(b"IsaacSteamSync/progression-content/v1\0");
    // Validation locates all eleven sections and requires exactly eight
    // trailer bytes. Keep every section and the Continue checksum link.
    hash.update(&canonical[..canonical.len() - 8]);
    let progress_hash = hex::encode(hash.finalize());
    if let Some(run) = run {
        run_state::validate_pair(&canonical, run)?;
        let run = run_state::canonicalize(run)?;
        Ok(run_state::bundle_hash(&progress_hash, &run))
    } else {
        Ok(progress_hash)
    }
}

/// Recover an old BASE only from backed-up bytes proven to match its full
/// hash and, when enabled, its full bundle hash. No current version is guessed
/// to be BASE merely because it has a recent timestamp or a larger counter.
pub fn recover_legacy_base(
    support: &Path,
    entry: &FileSyncState,
    include_run: bool,
) -> Result<Option<String>> {
    if include_run && entry.base_bundle_sha256.is_none() {
        return Ok(None);
    }
    let manager = BackupManager::new(support);
    let records = manager.list()?;
    for record in records.iter().filter(|record| {
        record.slot == entry.slot
            && save_compat::CloudVersion::from_filename(&record.filename).is_some()
    }) {
        let candidate = (|| -> Result<Option<String>> {
            let progress = fs::read(manager.verify(record)?)?;
            let progress = save_compat::project_for_ios(&progress)?.bytes;
            let (progress, run) = if include_run {
                let run_record = records.iter().find(|run| {
                    run.slot == record.slot
                        && run.source == record.source
                        && run.operation_id == record.operation_id
                        && run.filename == format!("rep_gamestate{}.dat", entry.slot)
                });
                let Some(run_record) = run_record else {
                    return Ok(None);
                };
                let run = fs::read(manager.verify(run_record)?)?;
                let pair = run_compat::adapt_pair(&progress, &run, false)?;
                (pair.progress, Some(pair.run))
            } else {
                (progress, None)
            };
            let identity = identity_for_bytes(&progress).sha256;
            if identity != entry.base_sha256 {
                return Ok(None);
            }
            if let Some(run) = &run
                && Some(run_state::bundle_hash(&identity, run)) != entry.base_bundle_sha256
            {
                return Ok(None);
            }
            Ok(Some(comparison_hash(&progress, run.as_deref())?))
        })();
        if let Ok(Some(identity)) = candidate {
            return Ok(Some(identity));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        isaac_format::write_valid_checksum_for_tests,
        sync_engine::{SyncDecision, decide},
    };

    fn base() -> Vec<u8> {
        save_compat::tests::fixture(save_compat::CloudVersion::Repentance)
    }
    fn counter_change(bytes: &mut [u8]) {
        let offset = bytes.len() - 8;
        bytes[offset..offset + 4].copy_from_slice(&60_420u32.to_le_bytes());
        write_valid_checksum_for_tests(bytes);
    }

    #[test]
    fn load_counter_is_not_progress_and_never_orders_versions() {
        let base = base();
        let mut local = base.clone();
        counter_change(&mut local);
        let mut remote = base.clone();
        remote[33] = 1;
        write_valid_checksum_for_tests(&mut remote);
        let common = comparison_hash(&base, None).unwrap();
        let local_hash = comparison_hash(&local, None).unwrap();
        let remote_hash = comparison_hash(&remote, None).unwrap();
        assert_ne!(
            identity_for_bytes(&base).sha256,
            identity_for_bytes(&local).sha256
        );
        assert_eq!(common, local_hash);
        assert_eq!(
            decide(Some(&common), &local_hash, &remote_hash),
            SyncDecision::DownloadRemote
        );
        assert_eq!(
            decide(Some(&common), &remote_hash, &local_hash),
            SyncDecision::UploadLocal
        );
        local[34] = 1;
        write_valid_checksum_for_tests(&mut local);
        assert_eq!(
            decide(
                Some(&common),
                &comparison_hash(&local, None).unwrap(),
                &remote_hash
            ),
            SyncDecision::Conflict
        );
        local[40] ^= 1;
        assert!(comparison_hash(&local, None).is_err());
    }

    #[test]
    fn continue_identity_and_link_are_never_ignored() {
        let mut progress = base();
        let mut run = b"ISAACNG_GSR0146\0".to_vec();
        run.resize(100, 0);
        let crc = crate::isaac_format::isaac_crc32(&run[16..], 0xfedc_ba76);
        run.extend_from_slice(&(crc ^ 0x9669_6996).to_le_bytes());
        progress[16..20].copy_from_slice(&crc.to_le_bytes());
        write_valid_checksum_for_tests(&mut progress);
        let identity = comparison_hash(&progress, Some(&run)).unwrap();
        counter_change(&mut progress);
        assert_eq!(identity, comparison_hash(&progress, Some(&run)).unwrap());
        assert_ne!(identity, comparison_hash(&progress, None).unwrap());
        run[28] = 1;
        assert!(comparison_hash(&progress, Some(&run)).is_err());
    }

    #[test]
    fn legacy_base_requires_exact_verified_backup() {
        let support =
            std::env::temp_dir().join(format!("isaac-content-base-{}", uuid::Uuid::new_v4()));
        let manager = BackupManager::new(&support);
        let progress = base();
        let identity = identity_for_bytes(&progress);
        let mut entry = FileSyncState {
            slot: 1,
            local_relative_path: "Documents/Repentance/rep_persistentgamedata1.dat".into(),
            remote_name: "rep_persistentgamedata1.dat".into(),
            base_sha256: identity.sha256.clone(),
            base_steam_sha1: None,
            last_local: identity.clone(),
            last_remote: identity,
            last_successful_sync_unix_ms: 1,
            base_bundle_sha256: None,
            base_sync_sha256: None,
        };
        assert_eq!(recover_legacy_base(&support, &entry, false).unwrap(), None);
        let record = manager
            .backup_bytes(
                &progress,
                "rep_persistentgamedata1.dat",
                1,
                crate::model::BackupSource::LocalBeforeForce,
                "verified-base",
                None,
            )
            .unwrap();
        assert_eq!(
            recover_legacy_base(&support, &entry, false).unwrap(),
            Some(comparison_hash(&progress, None).unwrap())
        );
        assert_eq!(recover_legacy_base(&support, &entry, true).unwrap(), None);
        manager
            .backup_bytes(
                &[],
                "rep_gamestate1.dat",
                1,
                crate::model::BackupSource::LocalBeforeForce,
                "verified-base",
                None,
            )
            .unwrap();
        entry.base_bundle_sha256 = Some(run_state::bundle_hash(&entry.base_sha256, &[]));
        assert_eq!(
            recover_legacy_base(&support, &entry, true).unwrap(),
            Some(comparison_hash(&progress, Some(&[])).unwrap())
        );
        entry.base_bundle_sha256 = Some("different-run".into());
        assert_eq!(recover_legacy_base(&support, &entry, true).unwrap(), None);
        let mut unknown = progress;
        counter_change(&mut unknown);
        fs::write(
            support.join("backups").join(record.data_relative_path),
            unknown,
        )
        .unwrap();
        assert_eq!(recover_legacy_base(&support, &entry, false).unwrap(), None);
        fs::remove_dir_all(support).unwrap();
    }

    #[test]
    #[ignore = "requires private real BASE/local/Steam conflict fixtures"]
    fn private_counter_only_local_change_allows_remote_run() {
        let pair = |prefix: &str| {
            let progress =
                fs::read(std::env::var(format!("ISAAC_{prefix}_PROGRESS_FIXTURE")).unwrap())
                    .unwrap();
            let run =
                fs::read(std::env::var(format!("ISAAC_{prefix}_RUN_FIXTURE")).unwrap()).unwrap();
            run_compat::adapt_pair(&progress, &run, false).unwrap()
        };
        let base = pair("BASE");
        let local = pair("IOS");
        let remote = pair("DESKTOP");
        let base_hash = comparison_hash(&base.progress, Some(&base.run)).unwrap();
        let local_hash = comparison_hash(&local.progress, Some(&local.run)).unwrap();
        let remote_hash = comparison_hash(&remote.progress, Some(&remote.run)).unwrap();
        assert_ne!(
            identity_for_bytes(&base.progress).sha256,
            identity_for_bytes(&local.progress).sha256
        );
        assert_eq!(base.run, local.run);
        assert_eq!(base_hash, local_hash);
        assert_eq!(
            decide(Some(&base_hash), &local_hash, &remote_hash),
            SyncDecision::DownloadRemote
        );
    }
}
