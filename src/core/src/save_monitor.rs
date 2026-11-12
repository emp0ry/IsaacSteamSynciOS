//! Metadata-only polling. Hashing and networking happen only after debouncing,
//! in the engine's existing serialized background operation lane.
use std::{path::PathBuf, time::SystemTime};

pub type Fingerprint = Vec<Option<(u64, SystemTime)>>;

// Save-upload preferences and Cloud errors/conflicts are deliberately not
// eligibility inputs for this independent, additive achievement operation.
pub fn achievement_poll_allowed(
    foreground: bool,
    menu_ready: bool,
    startup_active: bool,
    connected: bool,
    cloud_phase: &str,
) -> bool {
    foreground
        && menu_ready
        && !startup_active
        && connected
        && !matches!(
            cloud_phase,
            "connecting" | "awaiting_steam_guard" | "restart_required"
        )
}

pub fn fingerprint(paths: &[PathBuf]) -> Fingerprint {
    paths
        .iter()
        .map(|path| {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            if !metadata.file_type().is_file() {
                return None;
            }
            Some((metadata.len(), metadata.modified().ok()?))
        })
        .collect()
}

pub struct Debouncer {
    last: Fingerprint,
    changed_at: u64,
    pending: bool,
}

impl Debouncer {
    pub fn new(last: Fingerprint) -> Self {
        Self {
            last,
            changed_at: 0,
            pending: true,
        }
    }
    pub fn observe(&mut self, value: Fingerprint, seconds: u64) {
        if self.last != value {
            self.last = value;
            self.changed_at = seconds;
            self.pending = true;
        }
    }
    pub fn ready(&self, seconds: u64) -> bool {
        self.pending && seconds.saturating_sub(self.changed_at) >= 4
    }
    pub fn submitted(&mut self) {
        self.pending = false;
    }
    pub fn retry(&mut self) {
        self.pending = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coalesces_writes_and_retries_without_rehashing_unchanged_files() {
        let stamp = SystemTime::UNIX_EPOCH;
        let mut watcher = Debouncer::new(vec![Some((10, stamp))]);
        assert!(!watcher.ready(3));
        assert!(watcher.ready(4));
        watcher.submitted();
        assert!(!watcher.ready(10));
        watcher.observe(vec![Some((20, stamp))], 11);
        watcher.observe(vec![Some((30, stamp))], 13);
        assert!(!watcher.ready(16));
        assert!(watcher.ready(17));
        watcher.submitted();
        watcher.retry();
        assert!(watcher.ready(80));
    }

    #[test]
    fn achievements_do_not_depend_on_cloud_upload_success_or_conflict_resolution() {
        for phase in [
            "idle",
            "offline",
            "error",
            "conflict",
            "first_sync",
            "upload_required",
        ] {
            assert!(achievement_poll_allowed(true, true, false, true, phase));
        }
        for phase in ["connecting", "awaiting_steam_guard", "restart_required"] {
            assert!(!achievement_poll_allowed(true, true, false, true, phase));
        }
        assert!(!achievement_poll_allowed(false, true, false, true, "idle"));
        assert!(!achievement_poll_allowed(true, false, false, true, "idle"));
        assert!(!achievement_poll_allowed(true, true, true, true, "idle"));
        assert!(!achievement_poll_allowed(true, true, false, false, "idle"));
    }
}
