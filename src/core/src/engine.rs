use crate::{
    achievements, atomic,
    backups::BackupManager,
    isaac_format::{SaveEncoding, canonical_identity_for_path, canonicalize_save},
    keychain,
    local::{
        discover_saves, discover_saves_for_slots, discover_saves_immediate, identity_for_bytes,
        identity_for_path, locate_save_path_for_restore, unique_temp_path, unix_ms,
        wait_for_stable_snapshot,
    },
    logging::Logger,
    model::{
        BackupSource, FileIdentity, FileSyncState, LocalSave, PendingLocalRestore,
        PendingRemotePull, SyncState,
    },
    save_compat::{self, CloudVersion, RepPlusExtra},
    state::StateStore,
    steam::{
        cloud::{CloudFile, SteamCloud, select_persistent_saves},
        cm::{self, SteamSession},
    },
    sync_engine::{SyncDecision, decide},
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    sync::{Mutex as AsyncMutex, mpsc},
};
use uuid::Uuid;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Serialize)]
pub struct SaveSummary {
    pub slot: u8,
    pub filename: String,
    pub size: u64,
    pub modified_unix_ms: Option<u64>,
    pub sha256: Option<String>,
    pub steam_sha1: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingChoice {
    pub slot: u8,
    pub kind: String,
    pub local: SaveSummary,
    pub remote: SaveSummary,
    pub remote_name: String,
    pub local_run_sha256: Option<String>,
    pub remote_run_sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSnapshot {
    pub phase: String,
    pub detail: String,
    pub authenticated: bool,
    pub account_connected: bool,
    pub steam_id: Option<u64>,
    pub steam_playing: bool,
    pub qr_url: Option<String>,
    pub guard_kind: Option<String>,
    pub last_sync_unix_ms: Option<u64>,
    pub local_saves: Vec<SaveSummary>,
    pub remote_saves: Vec<SaveSummary>,
    pub pending_choices: Vec<PendingChoice>,
    pub backup_count: usize,
    pub offline_play_allowed: bool,
    pub cloud_version: CloudVersion,
    pub auto_upload: bool,
    pub transfer_runs: bool,
    pub pc_mods: bool,
    pub cloud_synchronized: bool,
}

impl Default for StatusSnapshot {
    fn default() -> Self {
        Self {
            phase: "idle".to_owned(),
            detail: "Not connected".to_owned(),
            authenticated: false,
            account_connected: false,
            steam_id: None,
            steam_playing: false,
            qr_url: None,
            guard_kind: None,
            last_sync_unix_ms: None,
            local_saves: vec![],
            remote_saves: vec![],
            pending_choices: vec![],
            backup_count: 0,
            offline_play_allowed: true,
            cloud_version: CloudVersion::Auto,
            auto_upload: true,
            transfer_runs: true,
            pc_mods: false,
            cloud_synchronized: false,
        }
    }
}

pub struct Engine {
    home: PathBuf,
    support: PathBuf,
    runtime: Runtime,
    session: AsyncMutex<Option<SteamSession>>,
    guard_sender: Mutex<Option<mpsc::UnboundedSender<String>>>,
    auth_cancel: Mutex<Option<Arc<AtomicBool>>>,
    status: Mutex<StatusSnapshot>,
    preflight_saves: Mutex<Option<Vec<LocalSave>>>,
    operation_active: AtomicBool,
    foreground: AtomicBool,
    replacement_allowed: AtomicBool,
    replacement_guard: Mutex<()>,
    auto_upload: AtomicBool,
    menu_ready: AtomicBool,
    logger: Logger,
}

struct RunContext {
    local_path: PathBuf,
    local_bytes: Vec<u8>,
    remote_name: String,
    remote_file: Option<CloudFile>,
    remote_raw: Vec<u8>,
    remote_bytes: Vec<u8>,
    remote_progress: Option<Vec<u8>>,
}

impl Engine {
    pub fn new(home: PathBuf) -> Result<Arc<Self>> {
        let support = home.join("Library/Application Support/IsaacCloudSync");
        for directory in ["state", "backups", "logs", "tmp"] {
            fs::create_dir_all(support.join(directory))?;
        }
        crate::slot_transaction::recover(&home, &support)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("IsaacCloudCore")
            .build()?;
        let logger = Logger::new(&support);
        logger.log("info", "bootstrap", "portable core initialized");
        let mut initial_status = StatusSnapshot::default();
        if let Ok(state) = StateStore::new(&support).load() {
            initial_status.cloud_version = state.compatibility.version;
            initial_status.auto_upload = state.compatibility.auto_upload;
            initial_status.transfer_runs = state.compatibility.transfer_runs;
            initial_status.pc_mods = state.compatibility.pc_mods;
        }
        let auto_upload = initial_status.auto_upload;
        if matches!(keychain::load_refresh_token(), Ok(Some(_))) {
            initial_status.account_connected = true;
            initial_status.detail = "Steam account connected".to_owned();
        }
        let engine = Arc::new(Self {
            home,
            support,
            runtime,
            session: AsyncMutex::new(None),
            guard_sender: Mutex::new(None),
            auth_cancel: Mutex::new(None),
            status: Mutex::new(initial_status),
            preflight_saves: Mutex::new(None),
            operation_active: AtomicBool::new(false),
            foreground: AtomicBool::new(true),
            replacement_allowed: AtomicBool::new(false),
            replacement_guard: Mutex::new(()),
            auto_upload: AtomicBool::new(auto_upload),
            menu_ready: AtomicBool::new(false),
            logger,
        });
        match discover_saves_immediate(&engine.home).and_then(Self::ensure_unique_slots) {
            Ok(saves) => {
                engine.logger.log(
                    "info",
                    "preflight",
                    &format!(
                        "captured {} stable local save identities during dylib construction",
                        saves.len()
                    ),
                );
                *engine
                    .preflight_saves
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(saves);
            }
            Err(error) => engine.logger.log(
                "error",
                "preflight",
                &format!("constructor save capture failed: {}", safe_error(&error)),
            ),
        }
        engine.start_save_monitor();
        Ok(engine)
    }

    fn start_save_monitor(self: &Arc<Self>) {
        let mut paths: Vec<PathBuf> = self
            .preflight_saves
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|saves| saves.iter().map(|save| save.path.clone()).collect())
            .unwrap_or_default();
        for slot in 1..=3 {
            paths.push(self.home.join(format!(
                "Documents/Repentance/rep_persistentgamedata{slot}.dat"
            )));
            paths.push(
                self.home
                    .join(format!("Documents/rep_persistentgamedata{slot}.dat")),
            );
            paths.push(
                self.home
                    .join(format!("Documents/persistentgamedata{slot}.dat")),
            );
            paths.push(
                self.home
                    .join(format!("Documents/Repentance/rep_gamestate{slot}.dat")),
            );
            paths.push(self.home.join(format!("Documents/rep_gamestate{slot}.dat")));
        }
        paths.sort();
        paths.dedup();
        let weak = Arc::downgrade(self);
        self.runtime.spawn(async move {
            let start = Instant::now();
            let mut watcher =
                crate::save_monitor::Debouncer::new(crate::save_monitor::fingerprint(&paths));
            let mut retry_after = 0;
            let mut last_attempt = None;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(engine) = weak.upgrade() else {
                    break;
                };
                let seconds = start.elapsed().as_secs();
                watcher.observe(crate::save_monitor::fingerprint(&paths), seconds);
                if !engine.foreground.load(Ordering::Acquire)
                    || !engine.auto_upload.load(Ordering::Acquire)
                    || engine.replacement_allowed.load(Ordering::Acquire)
                    || engine.operation_active.load(Ordering::Acquire)
                {
                    continue;
                }
                // Continue is a disk snapshot, not the live in-memory run.
                // Wait for native menu detection before publishing a pair.
                if !engine.menu_ready.load(Ordering::Acquire) {
                    continue;
                }
                let status = engine.snapshot();
                if !status.account_connected {
                    continue;
                }
                if matches!(status.phase.as_str(), "offline" | "error")
                    && last_attempt.is_some_and(|last| seconds < last + 60)
                {
                    continue;
                }
                if seconds >= retry_after
                    && matches!(
                        status.phase.as_str(),
                        "offline" | "error" | "upload_required"
                    )
                {
                    watcher.retry();
                }
                if seconds < retry_after
                    || !watcher.ready(seconds)
                    || matches!(
                        status.phase.as_str(),
                        "conflict" | "first_sync" | "restart_required" | "awaiting_steam_guard"
                    )
                {
                    continue;
                }
                if engine.automatic_sync("auto_save") {
                    watcher.submitted();
                    retry_after = seconds + 6;
                    last_attempt = Some(seconds);
                    engine.logger.log(
                        "info",
                        "save-monitor",
                        "stable file change queued for background three-way sync",
                    );
                }
            }
        });
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        self.status
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default()
    }

    pub fn set_cloud_version(self: &Arc<Self>, version: CloudVersion) -> bool {
        self.spawn_operation("configuring", move |engine| async move {
            let store = StateStore::new(&engine.support);
            let mut state = store.load()?;
            if state.compatibility.version != version {
                state.compatibility.version = version;
                // A target switch must not reuse the previous cloud object's BASE.
                state.files.clear();
                state.pending_remote_pulls.clear();
                state.cloud_change_number = None;
                store.save(&state)?;
            }
            engine.update_status(|status| {
                status.cloud_version = version;
                status.cloud_synchronized = false;
                status.pending_choices.clear();
                status.detail =
                    "Steam save version updated; the next check will compare safely".to_owned();
            });
            Ok(())
        })
    }

    pub fn set_auto_upload(self: &Arc<Self>, enabled: bool) -> bool {
        self.spawn_operation("configuring", move |engine| async move {
            let store = StateStore::new(&engine.support);
            let mut state = store.load()?;
            state.compatibility.auto_upload = enabled;
            store.save(&state)?;
            engine.auto_upload.store(enabled, Ordering::Release);
            engine.update_status(|status| {
                status.auto_upload = enabled;
                status.detail = if enabled {
                    "Automatic stable-save uploads enabled"
                } else {
                    "Uploads wait for Sync Now"
                }
                .to_owned();
            });
            Ok(())
        })
    }

    pub fn set_menu_ready(&self, ready: bool) {
        self.menu_ready.store(ready, Ordering::Release);
    }

    pub fn set_run_transfer(self: &Arc<Self>, enabled: bool) -> bool {
        self.spawn_operation("configuring", move |engine| async move {
            let store = StateStore::new(&engine.support);
            let mut state = store.load()?;
            state.compatibility.transfer_runs = enabled;
            state.pending_remote_pulls.clear();
            for entry in state.files.values_mut() {
                entry.base_sync_sha256 = None;
            }
            store.save(&state)?;
            engine.update_status(|status| {
                status.transfer_runs = enabled;
                status.cloud_synchronized = false;
                status.detail = "Continue transfer setting updated".to_owned();
            });
            Ok(())
        })
    }

    pub fn set_pc_mods(self: &Arc<Self>, enabled: bool) -> bool {
        self.spawn_operation("configuring", move |engine| async move {
            let store = StateStore::new(&engine.support);
            let mut state = store.load()?;
            state.compatibility.pc_mods = enabled;
            state.pending_remote_pulls.clear();
            for entry in state.files.values_mut() {
                entry.base_bundle_sha256 = None;
                entry.base_sync_sha256 = None;
            }
            store.save(&state)?;
            engine.update_status(|status| {
                status.pc_mods = enabled;
                status.cloud_synchronized = false;
                status.detail =
                    "PC mod-state adaptation updated; compare and choose the first pair again"
                        .to_owned();
            });
            Ok(())
        })
    }

    pub fn backups_json(&self) -> Result<String> {
        Ok(serde_json::to_string(
            &BackupManager::new(&self.support).list()?,
        )?)
    }

    pub fn record_host_event(&self, category: &str, message: &str) {
        self.logger.log("info", category, message);
    }

    pub fn connect_qr(self: &Arc<Self>) -> bool {
        let cancelled = Arc::new(AtomicBool::new(false));
        let previous_cancel = self
            .auth_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Arc::clone(&cancelled));
        let started = self.spawn_operation("connecting", |engine| async move {
            engine.update_status(|status| {
                status.detail = "Connecting to Steam…".to_owned();
                status.qr_url = None;
                status.guard_kind = None;
            });
            let callback_engine = Arc::clone(&engine);
            let result = cm::connect_with_qr(
                move |url| {
                    callback_engine.update_status(|status| {
                        status.phase = "awaiting_steam_guard".to_owned();
                        status.detail = "Approve this sign-in with Steam Mobile".to_owned();
                        status.qr_url = Some(url.to_owned());
                        status.guard_kind = Some("qr".to_owned());
                    });
                },
                Arc::clone(&cancelled),
            )
            .await;
            *engine
                .auth_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            if cancelled.load(Ordering::Acquire) {
                engine.update_status(|status| {
                    status.phase = "idle".to_owned();
                    status.detail = "Steam sign-in cancelled".to_owned();
                    status.qr_url = None;
                    status.guard_kind = None;
                });
                return Ok(());
            }
            let (session, refresh_token) = result?;
            keychain::store_refresh_token(&refresh_token)?;
            engine.finish_login(session).await?;
            engine.run_sync("connect").await
        });
        if !started {
            *self
                .auth_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous_cancel;
        }
        started
    }

    pub fn connect_credentials(self: &Arc<Self>, account: String, password: String) -> bool {
        let cancelled = Arc::new(AtomicBool::new(false));
        let previous_cancel = self
            .auth_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(Arc::clone(&cancelled));
        let (sender, receiver) = mpsc::unbounded_channel();
        *self
            .guard_sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sender);
        let started = self.spawn_operation("connecting", move |engine| async move {
            engine.update_status(|status| {
                status.detail = "Signing in to Steam…".to_owned();
                status.qr_url = None;
                status.guard_kind = None;
            });
            let password = Zeroizing::new(password);
            let callback_engine = Arc::clone(&engine);
            let result = cm::connect_with_credentials(
                &account,
                &password,
                move |prompt| {
                    callback_engine.update_status(|status| {
                        status.phase = "awaiting_steam_guard".to_owned();
                        status.qr_url = None;
                        match prompt {
                            cm::CredentialPrompt::EmailCode => {
                                status.guard_kind = Some("email_code".to_owned());
                                status.detail = "Enter the Steam Guard email code".to_owned();
                            }
                            cm::CredentialPrompt::DeviceCode => {
                                status.guard_kind = Some("device_code".to_owned());
                                status.detail = "Enter the Steam Guard code".to_owned();
                            }
                            cm::CredentialPrompt::DeviceConfirmation => {
                                status.guard_kind = Some("device_confirmation".to_owned());
                                status.detail = "Approve this sign-in in Steam Mobile".to_owned();
                            }
                        }
                    });
                },
                receiver,
                Arc::clone(&cancelled),
            )
            .await;
            *engine
                .guard_sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            *engine
                .auth_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            if cancelled.load(Ordering::Acquire) {
                engine.update_status(|status| {
                    status.phase = "idle".to_owned();
                    status.detail = "Steam sign-in cancelled".to_owned();
                    status.qr_url = None;
                    status.guard_kind = None;
                });
                return Ok(());
            }
            let (session, refresh_token) = result?;
            keychain::store_refresh_token(&refresh_token)?;
            engine.finish_login(session).await?;
            engine.run_sync("connect").await
        });
        if !started {
            *self
                .guard_sender
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            *self
                .auth_cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = previous_cancel;
        }
        started
    }

    pub fn cancel_login(&self) -> bool {
        let cancelled = self
            .auth_cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        let Some(cancelled) = cancelled else {
            return false;
        };
        cancelled.store(true, Ordering::Release);
        self.guard_sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.update_status(|status| {
            status.phase = "cancelling".to_owned();
            status.detail = "Cancelling Steam sign-in…".to_owned();
            status.qr_url = None;
            status.guard_kind = None;
        });
        true
    }

    pub fn submit_guard_code(&self, code: String) -> bool {
        let sender = self
            .guard_sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        sender.is_some_and(|sender| sender.send(code).is_ok())
    }

    pub fn set_foreground(self: &Arc<Self>, foreground: bool) -> bool {
        self.foreground.store(foreground, Ordering::Release);
        let engine = Arc::clone(self);
        self.runtime.spawn(async move {
            let result = if foreground {
                engine.ensure_session().await
            } else {
                let session = engine.session.lock().await;
                match session.as_ref() {
                    Some(session) => cm::set_playing(session, false).await,
                    None => Ok(()),
                }
            };
            match result {
                Ok(()) => {
                    engine.update_status(|status| status.steam_playing = foreground);
                    engine.logger.log(
                        "info",
                        "presence",
                        if foreground {
                            "AppID 250900 presence active"
                        } else {
                            "AppID 250900 presence cleared"
                        },
                    );
                }
                Err(error) => engine.logger.log(
                    "error",
                    "presence",
                    &format!("presence update failed: {}", safe_error(&error)),
                ),
            }
        });
        true
    }

    pub fn automatic_sync(self: &Arc<Self>, trigger: &str) -> bool {
        if trigger == "background"
            && (!self.auto_upload.load(Ordering::Acquire)
                || !self.menu_ready.load(Ordering::Acquire)
                || !self.snapshot().account_connected)
        {
            return false;
        }
        let trigger = trigger.to_owned();
        self.spawn_operation("syncing", move |engine| async move {
            if let Err(error) = engine.ensure_session().await {
                engine.logger.log(
                    "info",
                    "offline",
                    "Steam unavailable; continuing with local save",
                );
                engine.update_status(|status| {
                    status.phase = "offline".to_owned();
                    status.detail = safe_error(&error);
                    status.offline_play_allowed = true;
                });
                return Ok(());
            }
            engine.run_sync(&trigger).await?;
            if trigger == "manual" {
                match engine.run_achievement_sync().await {
                    Ok(added) => engine.report_achievement_result(added),
                    Err(error) => {
                        let message = safe_error(&error);
                        engine.logger.log(
                            "error",
                            "achievements",
                            &format!("save-derived achievement sync failed: {message}"),
                        );
                        engine.update_status(|status| {
                            if status.phase == "idle" || status.phase == "syncing" {
                                status.phase = "idle".to_owned();
                                status.detail = format!(
                                    "Steam Cloud synchronized; achievement sync failed: {message}"
                                );
                            }
                        });
                    }
                }
            }
            Ok(())
        })
    }

    /// Synchronize achievements through the same serialized Steam operation
    /// lane as UFS. This prevents a second CM login from racing the Cloud
    /// session and potentially causing Steam to close either connection.
    pub fn sync_achievements(self: &Arc<Self>) -> bool {
        self.spawn_operation("syncing", |engine| async move {
            engine.ensure_session().await?;
            let added = engine.run_achievement_sync().await?;
            engine.report_achievement_result(added);
            Ok(())
        })
    }

    pub fn disconnect(self: &Arc<Self>) -> bool {
        self.spawn_operation("disconnecting", |engine| async move {
            keychain::delete_refresh_token()?;
            let mut session = engine.session.lock().await;
            if let Some(existing) = session.as_ref() {
                let _ = cm::set_playing(existing, false).await;
            }
            *session = None;
            drop(session);
            engine.update_status(|status| {
                *status = StatusSnapshot::default();
                status.detail = "Disconnected from Steam".to_owned();
            });
            engine
                .logger
                .log("info", "authentication", "Steam session disconnected");
            Ok(())
        })
    }

    pub fn resolve_choice(self: &Arc<Self>, slot: u8, use_local: bool) -> bool {
        self.spawn_operation("resolving", move |engine| async move {
            engine.ensure_session().await?;
            engine.force_slot(slot, use_local, true).await?;
            if use_local {
                engine.run_sync("resolution").await?;
            }
            Ok(())
        })
    }

    pub fn force(self: &Arc<Self>, slot: u8, use_local: bool) -> bool {
        self.spawn_operation("forcing", move |engine| async move {
            engine.ensure_session().await?;
            engine.force_slot(slot, use_local, false).await
        })
    }

    pub fn restore_backup(self: &Arc<Self>, backup_id: String) -> bool {
        self.spawn_operation("restoring", move |engine| async move {
            let manager = BackupManager::new(&engine.support);
            let records = manager.list()?;
            let selected = records
                .iter()
                .find(|record| record.backup_id == backup_id)
                .context("backup not found")?;
            let record = records
                .iter()
                .filter(|record| CloudVersion::from_filename(&record.filename).is_some())
                .find(|record| {
                    record.backup_id == backup_id
                        || (record.slot == selected.slot
                            && record.operation_id == selected.operation_id
                            && record.source == selected.source)
                })
                .cloned()
                .context("Continue backup has no matching progression snapshot; restore refused")?;
            let run_record = records.iter().find(|candidate| {
                candidate.slot == record.slot
                    && candidate.operation_id == record.operation_id
                    && candidate.source == record.source
                    && crate::run_state::slot_for_filename(&candidate.filename) == Some(record.slot)
            });
            if let Some(run) = run_record {
                let bytes = fs::read(manager.verify(run)?)?;
                crate::run_state::canonicalize(&bytes)?;
            }
            let backup_path = manager.verify(&record)?;
            save_compat::project_for_ios(&fs::read(backup_path)?)
                .context("backup is not a supported valid progression save")?;
            let store = StateStore::new(&engine.support);
            let mut state = store.load()?;
            state.pending_local_restore = Some(PendingLocalRestore {
                backup_id: record.backup_id,
                slot: record.slot,
                expected_sha256: record.sha256,
                requested_unix_ms: unix_ms(),
                run_backup_id: run_record.map(|record| record.backup_id.clone()),
                expected_run_sha256: run_record.map(|record| record.sha256.clone()),
            });
            store.save(&state)?;
            engine.update_status(|status| {
                status.phase = "restart_required".to_owned();
                status.detail =
                    "Backup verified and queued; relaunch Isaac to restore it safely".to_owned();
            });
            engine.logger.log(
                "info",
                "backup",
                "verified backup queued for prelaunch restore",
            );
            Ok(())
        })
    }

    pub fn preflight(self: &Arc<Self>, timeout_ms: u64) -> bool {
        self.replacement_allowed.store(true, Ordering::Release);
        if let Err(error) = self.apply_pending_restore() {
            self.logger.log(
                "error",
                "backup",
                &format!("queued restore was not applied: {}", safe_error(&error)),
            );
        }
        let started = self.automatic_sync("prelaunch");
        if !started {
            self.replacement_allowed.store(false, Ordering::Release);
            return true;
        }
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while self.operation_active.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        let completed = !self.operation_active.load(Ordering::Acquire);
        // A pull holds this guard from its final permission check until the
        // atomic replacement is durable. Once this lock is acquired no save
        // can be replaced after the prelaunch barrier returns.
        let _guard = self
            .replacement_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.replacement_allowed.store(false, Ordering::Release);
        completed
    }

    fn apply_pending_restore(&self) -> Result<()> {
        let store = StateStore::new(&self.support);
        let mut state = store.load()?;
        let Some(pending) = state.pending_local_restore.clone() else {
            return Ok(());
        };
        let manager = BackupManager::new(&self.support);
        let record = manager
            .list()?
            .into_iter()
            .find(|record| record.backup_id == pending.backup_id)
            .context("queued backup no longer exists")?;
        if record.slot != pending.slot || record.sha256 != pending.expected_sha256 {
            bail!("queued backup manifest no longer matches the approved version");
        }
        let backup_path = manager.verify(&record)?;
        let mut projected = save_compat::project_for_ios(&fs::read(backup_path)?)
            .context("queued backup is not a valid Repentance save")?;
        let mut run_bytes = if let Some(id) = &pending.run_backup_id {
            let run = manager
                .list()?
                .into_iter()
                .find(|record| &record.backup_id == id)
                .context("queued Continue backup missing")?;
            if run.slot != pending.slot
                || pending.expected_run_sha256.as_deref() != Some(run.sha256.as_str())
            {
                bail!("queued Continue backup changed");
            }
            crate::run_state::canonicalize(&fs::read(manager.verify(&run)?)?)?
        } else {
            vec![]
        };
        if state.compatibility.pc_mods {
            let pair = crate::run_compat::adapt_pair(&projected.bytes, &run_bytes, false)?;
            projected.bytes = pair.progress;
            run_bytes = pair.run;
        } else {
            crate::run_state::validate_pair(&projected.bytes, &run_bytes)?;
        }
        let preferred = if CloudVersion::from_filename(&record.filename).map(|(version, _)| version)
            == Some(CloudVersion::RepentancePlus)
        {
            CloudVersion::Repentance.filename(pending.slot)?
        } else {
            record.filename.clone()
        };
        let destination = locate_save_path_for_restore(&self.home, pending.slot, &preferred)?;
        let _replacement_guard = self
            .replacement_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.replacement_allowed.load(Ordering::Acquire) {
            bail!("queued restore is only allowed during prelaunch");
        }
        let run_destination = crate::run_state::local_path(&destination, pending.slot)?;
        let operation = Uuid::new_v4().to_string();
        manager.backup_file(
            &destination,
            pending.slot,
            BackupSource::LocalBeforeRestore,
            &operation,
        )?;
        if run_destination.exists() {
            manager.backup_file(
                &run_destination,
                pending.slot,
                BackupSource::LocalBeforeRestore,
                &operation,
            )?;
        }
        crate::slot_transaction::replace_pair(
            &self.home,
            &self.support,
            [
                (&destination, &projected.bytes),
                (
                    &run_destination,
                    &crate::run_state::encode_for_ios(&run_bytes)?,
                ),
            ],
        )?;
        let restored_identity = canonical_identity_for_path(&destination)?;
        let restored = LocalSave {
            slot: pending.slot,
            relative_path: destination
                .strip_prefix(&self.home)
                .unwrap_or(&destination)
                .to_string_lossy()
                .into_owned(),
            path: destination,
            identity: restored_identity,
        };
        let mut preflight = self
            .preflight_saves
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let saves = preflight.get_or_insert_with(Vec::new);
        saves.retain(|save| save.slot != pending.slot);
        saves.push(restored);
        saves.sort_by_key(|save| save.slot);
        state.pending_local_restore = None;
        state.files.retain(|_, entry| entry.slot != pending.slot);
        state.pending_remote_pulls.remove(&pending.slot);
        remember_extra(&mut state, pending.slot, projected.extra.as_ref());
        store.save(&state)?;
        self.logger.log(
            "info",
            "backup",
            "queued backup restored atomically and verified before launch",
        );
        Ok(())
    }

    fn spawn_operation<F, Fut>(self: &Arc<Self>, phase: &str, build: F) -> bool
    where
        F: FnOnce(Arc<Engine>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        if self
            .operation_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.update_status(|status| {
            status.phase = phase.to_owned();
            status.detail = phase.to_owned();
        });
        let engine = Arc::clone(self);
        self.runtime.spawn(async move {
            let result = build(Arc::clone(&engine)).await;
            if let Err(error) = result {
                // A failed CM/UFS operation may leave the transport alive but
                // unusable. Drop it so the next attempt performs a clean
                // refresh-token reconnect. The Keychain credential is kept.
                *engine.session.lock().await = None;
                let message = safe_error(&error);
                engine.logger.log("error", "operation", &message);
                engine.update_status(|status| {
                    status.phase = "error".to_owned();
                    status.detail = message;
                    status.offline_play_allowed = true;
                    status.authenticated = false;
                    status.steam_playing = false;
                });
            } else {
                engine.update_status(|status| {
                    if status.phase != "conflict"
                        && status.phase != "first_sync"
                        && status.phase != "offline"
                        && status.phase != "restart_required"
                        && status.phase != "upload_required"
                    {
                        status.phase = "idle".to_owned();
                    }
                });
            }
            engine.operation_active.store(false, Ordering::Release);
        });
        true
    }

    async fn ensure_session(&self) -> Result<()> {
        let mut session = self.session.lock().await;
        if let Some(existing) = session.as_ref()
            && !existing.connection.is_closed().await
        {
            self.apply_presence(existing).await?;
            return Ok(());
        }
        self.logger.log(
            "info",
            "authentication",
            "loading refresh credential from Keychain",
        );
        let token = keychain::load_refresh_token()?
            .context("Connect a Steam account to enable cloud sync")?;
        self.logger.log(
            "info",
            "authentication",
            "refresh credential loaded; connecting to Steam CM",
        );
        let connected = cm::connect_with_refresh_token(&token).await?;
        self.logger.log(
            "info",
            "authentication",
            "refresh-token Steam CM login succeeded",
        );
        let steam_id = connected.steam_id;
        self.bind_state_to_account(steam_id, None)?;
        self.apply_presence(&connected).await?;
        *session = Some(connected);
        self.update_status(|status| {
            status.authenticated = true;
            status.account_connected = true;
            status.steam_id = Some(steam_id);
            status.qr_url = None;
            status.guard_kind = None;
            status.detail = "Steam connected".to_owned();
        });
        Ok(())
    }

    async fn finish_login(&self, session: SteamSession) -> Result<()> {
        let steam_id = session.steam_id;
        let account_name = session.account_name.clone();
        self.bind_state_to_account(steam_id, (!account_name.is_empty()).then_some(account_name))?;
        self.apply_presence(&session).await?;
        *self.session.lock().await = Some(session);
        self.update_status(|status| {
            status.authenticated = true;
            status.account_connected = true;
            status.steam_id = Some(steam_id);
            status.qr_url = None;
            status.guard_kind = None;
            status.detail = "Steam connected".to_owned();
        });
        self.logger
            .log("info", "authentication", "Steam authentication succeeded");
        Ok(())
    }

    async fn apply_presence(&self, session: &SteamSession) -> Result<()> {
        let playing = self.foreground.load(Ordering::Acquire);
        cm::set_playing(session, playing).await?;
        self.update_status(|status| status.steam_playing = playing);
        Ok(())
    }

    async fn run_achievement_sync(&self) -> Result<usize> {
        self.logger.log(
            "info",
            "achievements",
            "reading unlocks from native Isaac persistent saves",
        );
        self.update_status(|status| {
            if status.phase == "idle" {
                status.phase = "syncing".to_owned();
                status.detail = "Comparing native save achievements with Steam…".to_owned();
            }
        });
        let session_guard = self.session.lock().await;
        let session = session_guard
            .as_ref()
            .context("Steam session unavailable for achievement sync")?;
        achievements::sync_from_local_saves(&self.home, session).await
    }

    fn report_achievement_result(&self, added: usize) {
        self.logger.log(
            "info",
            "achievements",
            &format!("save-derived achievement sync verified; added={added}; cleared=0"),
        );
        self.update_status(|status| {
            if status.phase == "idle" || status.phase == "syncing" {
                status.detail = if added == 0 {
                    "Steam Cloud and save-derived achievements are synchronized".to_owned()
                } else if added == 1 {
                    "Steam Cloud synchronized; 1 missing save achievement added to Steam".to_owned()
                } else {
                    format!(
                        "Steam Cloud synchronized; {added} missing save achievements added to Steam"
                    )
                };
            }
        });
    }

    fn bind_state_to_account(&self, steam_id: u64, account_name: Option<String>) -> Result<()> {
        let store = StateStore::new(&self.support);
        let mut state = store.load()?;
        if state.steam_id.is_some_and(|prior| prior != steam_id) {
            state.files.clear();
            state.cloud_change_number = None;
            state.pending_remote_pulls.clear();
            state.compatibility = Default::default();
            self.logger.log(
                "info",
                "authentication",
                "Steam account changed; synchronization BASE was cleared",
            );
        }
        state.steam_id = Some(steam_id);
        if account_name.is_some() {
            state.account_name = account_name;
        }
        store.save(&state)?;
        self.update_status(|status| status.cloud_version = state.compatibility.version);
        self.auto_upload
            .store(state.compatibility.auto_upload, Ordering::Release);
        self.update_status(|status| status.auto_upload = state.compatibility.auto_upload);
        self.update_status(|status| status.transfer_runs = state.compatibility.transfer_runs);
        self.update_status(|status| status.pc_mods = state.compatibility.pc_mods);
        Ok(())
    }

    fn local_saves(&self) -> Result<Vec<LocalSave>> {
        let saves = discover_saves(&self.home)?;
        Self::ensure_unique_slots(saves)
    }

    fn local_saves_for_slots(&self, slots: &BTreeSet<u8>) -> Result<Vec<LocalSave>> {
        let saves = discover_saves_for_slots(&self.home, Some(slots))?;
        Self::ensure_unique_slots(saves)
    }

    fn ensure_unique_slots(saves: Vec<LocalSave>) -> Result<Vec<LocalSave>> {
        for pair in saves.windows(2) {
            if pair[0].slot == pair[1].slot {
                bail!(
                    "multiple local save candidates found for slot {}",
                    pair[0].slot
                );
            }
        }
        Ok(saves)
    }

    async fn run_sync(&self, trigger: &str) -> Result<()> {
        let allow_upload = trigger == "manual"
            || (matches!(trigger, "auto_save" | "background")
                && self.auto_upload.load(Ordering::Acquire));
        self.logger.log("info", "lifecycle", trigger);
        self.update_status(|status| {
            status.phase = "syncing".to_owned();
            status.cloud_synchronized = false;
            status.detail = "Discovering Isaac saves…".to_owned();
            status.pending_choices.clear();
        });
        let cloud = SteamCloud::new()?;
        let session_guard = self.session.lock().await;
        let session = session_guard
            .as_ref()
            .context("Steam session unavailable")?;
        let store = StateStore::new(&self.support);
        let mut state = store.load()?;
        let (change_number, all_remote) = cloud.list_isaac_saves(session).await?;
        let remote = select_persistent_saves(&all_remote, &state)?;
        let local = if trigger == "prelaunch" {
            self.preflight_saves
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
                .context("early preflight save snapshot unavailable")?
        } else if allow_upload {
            self.local_saves()?
        } else {
            let remote_slots = remote.iter().map(|file| file.save.slot).collect();
            self.local_saves_for_slots(&remote_slots)?
        };
        let local = if trigger == "prelaunch" {
            local
                .into_iter()
                .filter(|save| remote.iter().any(|file| file.save.slot == save.slot))
                .collect()
        } else {
            local
        };
        if local.is_empty() {
            bail!("no matching Isaac persistent save files were found inside the app sandbox");
        }
        for file in &remote {
            self.logger.log(
                "info",
                "cloud",
                &format!(
                    "slot={} remote_name={} size={} steam_sha1={}",
                    file.save.slot,
                    file.request_name,
                    file.save.identity.size,
                    file.save
                        .identity
                        .steam_sha1
                        .as_deref()
                        .map(short_hash)
                        .unwrap_or("unavailable")
                ),
            );
        }
        self.update_status(|status| {
            status.local_saves = local.iter().map(local_summary).collect();
            status.remote_saves = remote.iter().map(remote_summary).collect();
            status.detail = "Comparing local, Steam, and base hashes…".to_owned();
        });

        let mut pending = Vec::new();
        let mut uploads_waiting = 0usize;
        let mut run_identities = std::collections::BTreeMap::new();
        let operation_id = Uuid::new_v4().to_string();

        for local_save in &local {
            let Some(remote_file) = remote.iter().find(|file| file.save.slot == local_save.slot)
            else {
                BackupManager::new(&self.support).backup_file(
                    &local_save.path,
                    local_save.slot,
                    BackupSource::LocalFirstSync,
                    &operation_id,
                )?;
                let mut choice = missing_remote_choice(local_save);
                choice.remote_name = new_remote_name(&state, local_save.slot)?;
                pending.push(choice);
                continue;
            };
            let prior = state
                .files
                .values()
                .find(|entry| {
                    entry.slot == local_save.slot && entry.remote_name == remote_file.request_name
                })
                .cloned();
            // Saves are tiny, so download every candidate. Besides providing a
            // SHA-256 identity, this detects the raw-LZ4 representation written
            // by native iOS and prevents it from ever being sent to Windows.
            let remote_raw_bytes = cloud.download(session, remote_file).await?;
            let remote_canonical = match canonicalize_save(&remote_raw_bytes) {
                Ok(save) => save,
                Err(error) => {
                    BackupManager::new(&self.support).backup_bytes(
                        &remote_raw_bytes,
                        &remote_file.save.filename,
                        local_save.slot,
                        BackupSource::SteamInvalid,
                        &operation_id,
                        remote_file.save.identity.steam_sha1.clone(),
                    )?;
                    return Err(error.context(format!(
                        "Steam save slot {} is not a valid canonical or iOS LZ4 Repentance save; exact cloud bytes were backed up",
                        local_save.slot
                    )));
                }
            };
            let mut remote_needs_compatibility_repair =
                remote_canonical.encoding == SaveEncoding::IosRawLz4;
            let mut projected = save_compat::project_for_ios(&remote_canonical.bytes)?;
            validate_remote_version(remote_file, projected.version)?;
            let mut remote_identity = identity_for_bytes(&projected.bytes);
            remote_identity.modified_unix_ms = remote_file.save.identity.modified_unix_ms;
            remote_identity.steam_sha1 = remote_file.save.identity.steam_sha1.clone();
            let run = self
                .run_context(
                    &cloud,
                    session,
                    local_save,
                    &remote_file.request_name,
                    Some(&remote_canonical.bytes),
                    projected.version,
                    &all_remote,
                    state.compatibility.transfer_runs,
                    true,
                )
                .await?;
            if let Some(run) = &run {
                if crate::run_compat::modding_state(&run.remote_raw)?
                    .is_some_and(|(_, modded)| modded != state.compatibility.pc_mods)
                {
                    remote_needs_compatibility_repair = true;
                }
                if let Some(progress) = &run.remote_progress {
                    projected.bytes = progress.clone();
                    remote_identity.sha256 = identity_for_bytes(progress).sha256;
                    remote_identity.size = progress.len() as u64;
                }
                run_identities.insert(
                    local_save.slot,
                    (
                        identity_for_bytes(&run.local_bytes).sha256,
                        identity_for_bytes(&run.remote_bytes).sha256,
                    ),
                );
            }
            if remote_needs_compatibility_repair {
                self.logger.log(
                    "warn",
                    "compatibility",
                    &format!(
                        "slot={} Steam copy contains iOS LZ4 framing; canonical repair is required",
                        local_save.slot
                    ),
                );
            }

            if let Some(requested) = state.pending_remote_pulls.get(&local_save.slot).cloned() {
                let still_same_remote = requested.remote_name == remote_file.request_name
                    && requested.expected_steam_sha1 == remote_identity.steam_sha1
                    && requested.expected_run_sha256
                        == run
                            .as_ref()
                            .map(|run| identity_for_bytes(&run.remote_bytes).sha256);
                if still_same_remote && self.replacement_allowed.load(Ordering::Acquire) {
                    self.pull_one(
                        local_save,
                        remote_file,
                        &remote_canonical.bytes,
                        run.as_ref(),
                        &operation_id,
                        &mut state,
                    )?;
                    state.pending_remote_pulls.remove(&local_save.slot);
                    continue;
                }
                if !still_same_remote {
                    state.pending_remote_pulls.remove(&local_save.slot);
                    self.logger.log(
                        "info",
                        "download",
                        "queued Steam version changed; requesting a new choice",
                    );
                } else {
                    pending.push(choice_summary(
                        local_save,
                        remote_file,
                        &remote_identity,
                        "remote_update_restart_required",
                    ));
                    continue;
                }
            }
            let local_raw_compare = run
                .as_ref()
                .map(|run| {
                    crate::run_state::bundle_hash(&local_save.identity.sha256, &run.local_bytes)
                })
                .unwrap_or_else(|| local_save.identity.sha256.clone());
            let remote_raw_compare = run
                .as_ref()
                .map(|run| {
                    crate::run_state::bundle_hash(&remote_identity.sha256, &run.remote_bytes)
                })
                .unwrap_or_else(|| remote_identity.sha256.clone());
            let local_bytes = canonicalize_save(&fs::read(&local_save.path)?)?.bytes;
            if identity_for_bytes(&local_bytes).sha256 != local_save.identity.sha256 {
                bail!("local save changed during comparison; synchronize again");
            }
            let local_compare = crate::sync_identity::comparison_hash(
                &local_bytes,
                run.as_ref().map(|run| run.local_bytes.as_slice()),
            )?;
            let remote_compare = crate::sync_identity::comparison_hash(
                &projected.bytes,
                run.as_ref().map(|run| run.remote_bytes.as_slice()),
            )?;
            let mut base = prior
                .as_ref()
                .and_then(|entry| entry.base_sync_sha256.clone());
            if base.is_none()
                && let Some(entry) = &prior
            {
                let old_base = if run.is_some() {
                    entry.base_bundle_sha256.as_deref()
                } else {
                    Some(entry.base_sha256.as_str())
                };
                base = if old_base == Some(local_raw_compare.as_str()) {
                    Some(local_compare.clone())
                } else if old_base == Some(remote_raw_compare.as_str()) {
                    Some(remote_compare.clone())
                } else {
                    crate::sync_identity::recover_legacy_base(&self.support, entry, run.is_some())?
                };
                if let Some(identity) = &base
                    && let Some(entry) = state.files.get_mut(&remote_file.request_name)
                {
                    entry.base_sync_sha256 = Some(identity.clone());
                }
            }
            let decision = decide(base.as_deref(), &local_compare, &remote_compare);
            if matches!(decision, SyncDecision::Unchanged | SyncDecision::Converged)
                && local_save.identity.sha256 != remote_identity.sha256
            {
                self.logger.log(
                    "info",
                    "comparison",
                    &format!(
                        "slot={} verified content matches; load-counter/CRC differences ignored",
                        local_save.slot
                    ),
                );
            }
            self.logger.log(
                "info",
                "comparison",
                &format!(
                    "slot={} local_sha256={} remote_sha256={} base_sha256={} result={}",
                    local_save.slot,
                    short_hash(&local_save.identity.sha256),
                    short_hash(&remote_identity.sha256),
                    prior
                        .as_ref()
                        .map(|entry| short_hash(&entry.base_sha256))
                        .unwrap_or("none"),
                    decision_label(decision)
                ),
            );

            match decision {
                SyncDecision::Unchanged => {
                    remember_extra(&mut state, local_save.slot, projected.extra.as_ref());
                    if let Some(entry) = state.files.get_mut(&remote_file.request_name) {
                        entry.last_local = local_save.identity.clone();
                        entry.last_remote = remote_identity.clone();
                        entry.last_successful_sync_unix_ms = unix_ms();
                    }
                    if remote_needs_compatibility_repair {
                        if allow_upload {
                            self.push_one(
                                &cloud,
                                session,
                                local_save,
                                remote_file,
                                &remote_raw_bytes,
                                run.as_ref(),
                                &operation_id,
                                &mut state,
                            )
                            .await?;
                        } else {
                            uploads_waiting += 1;
                        }
                    }
                }
                SyncDecision::Converged => {
                    remember_extra(&mut state, local_save.slot, projected.extra.as_ref());
                    if remote_needs_compatibility_repair && allow_upload {
                        self.push_one(
                            &cloud,
                            session,
                            local_save,
                            remote_file,
                            &remote_raw_bytes,
                            run.as_ref(),
                            &operation_id,
                            &mut state,
                        )
                        .await?;
                    } else {
                        upsert_state(
                            &mut state,
                            make_file_state(
                                local_save,
                                remote_file,
                                &remote_identity,
                                run.as_ref().map(|_| local_raw_compare.clone()),
                                local_compare.clone(),
                            ),
                        );
                        if remote_needs_compatibility_repair {
                            uploads_waiting += 1;
                        }
                    }
                }
                SyncDecision::UploadLocal => {
                    if allow_upload {
                        self.push_one(
                            &cloud,
                            session,
                            local_save,
                            remote_file,
                            &remote_raw_bytes,
                            run.as_ref(),
                            &operation_id,
                            &mut state,
                        )
                        .await?;
                    } else {
                        uploads_waiting += 1;
                        self.logger.log(
                            "info",
                            "upload",
                            &format!(
                                "slot={} iPhone change is waiting for explicit Sync Now",
                                local_save.slot
                            ),
                        );
                    }
                }
                SyncDecision::DownloadRemote => {
                    if self.replacement_allowed.load(Ordering::Acquire) {
                        self.pull_one(
                            local_save,
                            remote_file,
                            &remote_canonical.bytes,
                            run.as_ref(),
                            &operation_id,
                            &mut state,
                        )?;
                    } else {
                        pending.push(choice_summary(
                            local_save,
                            remote_file,
                            &remote_identity,
                            "remote_update_restart_required",
                        ));
                    }
                }
                SyncDecision::FirstSyncChoiceRequired | SyncDecision::Conflict => {
                    self.backup_both(
                        local_save,
                        remote_file,
                        &remote_raw_bytes,
                        &operation_id,
                        if decision == SyncDecision::FirstSyncChoiceRequired {
                            (BackupSource::LocalFirstSync, BackupSource::SteamFirstSync)
                        } else {
                            (BackupSource::LocalConflict, BackupSource::SteamConflict)
                        },
                    )?;
                    if let Some(run) = &run {
                        let (local_source, remote_source) =
                            if decision == SyncDecision::FirstSyncChoiceRequired {
                                (BackupSource::LocalFirstSync, BackupSource::SteamFirstSync)
                            } else {
                                (BackupSource::LocalConflict, BackupSource::SteamConflict)
                            };
                        self.backup_run(
                            run,
                            local_save.slot,
                            &operation_id,
                            local_source,
                            remote_source,
                        )?;
                    }
                    pending.push(choice_summary(
                        local_save,
                        remote_file,
                        &remote_identity,
                        if decision == SyncDecision::FirstSyncChoiceRequired {
                            "first_sync"
                        } else {
                            "conflict"
                        },
                    ));
                }
            }
        }

        for choice in &mut pending {
            if let Some((local_run, remote_run)) = run_identities.get(&choice.slot) {
                choice.local_run_sha256 = Some(local_run.clone());
                choice.remote_run_sha256 = Some(remote_run.clone());
            }
        }
        state.cloud_change_number = Some(change_number);
        store.save(&state)?;
        let backup_count = BackupManager::new(&self.support).list()?.len();
        self.update_status(|status| {
            status.pending_choices = pending.clone();
            status.backup_count = backup_count;
            status.last_sync_unix_ms = Some(unix_ms());
            status.cloud_synchronized = pending.is_empty() && uploads_waiting == 0;
            if pending.is_empty() && uploads_waiting == 0 {
                status.phase = "idle".to_owned();
                status.detail = "Steam Cloud is synchronized".to_owned();
            } else if pending.iter().any(|choice| choice.kind == "conflict") {
                status.phase = "conflict".to_owned();
                status.detail = "A true save conflict requires your choice".to_owned();
            } else if pending
                .iter()
                .any(|choice| choice.kind == "remote_update_restart_required")
            {
                status.phase = "restart_required".to_owned();
                status.detail = "Relaunch Isaac to apply the selected Steam save safely".to_owned();
            } else if uploads_waiting > 0 {
                status.phase = "upload_required".to_owned();
                status.detail = if uploads_waiting == 1 {
                    "iPhone progress is waiting; tap Sync Now to upload a Windows-compatible save"
                        .to_owned()
                } else {
                    format!(
                        "{uploads_waiting} iPhone saves are waiting; tap Sync Now to upload them"
                    )
                };
            } else {
                status.phase = "first_sync".to_owned();
                status.detail = "First sync requires your choice".to_owned();
            }
        });
        Ok(())
    }

    async fn force_slot(&self, slot: u8, use_local: bool, must_be_pending: bool) -> Result<()> {
        if must_be_pending
            && !self
                .snapshot()
                .pending_choices
                .iter()
                .any(|choice| choice.slot == slot)
        {
            bail!("save slot does not have a pending conflict or first-sync choice");
        }
        let allowed_slots = BTreeSet::from([slot]);
        let local = self
            .local_saves_for_slots(&allowed_slots)?
            .into_iter()
            .find(|save| save.slot == slot)
            .context("local save slot not found")?;
        let cloud = SteamCloud::new()?;
        let session_guard = self.session.lock().await;
        let session = session_guard
            .as_ref()
            .context("Steam session unavailable")?;
        let store = StateStore::new(&self.support);
        let mut state = store.load()?;
        let (_, all_remote) = cloud.list_isaac_saves(session).await?;
        let remote = select_persistent_saves(&all_remote, &state)?;
        let remote = remote.iter().find(|file| file.save.slot == slot);
        let operation_id = Uuid::new_v4().to_string();
        if let Some(remote) = remote {
            let remote_bytes = cloud.download(session, remote).await?;
            let projected = save_compat::project_for_ios(&remote_bytes)?;
            validate_remote_version(remote, projected.version)?;
            let run = self
                .run_context(
                    &cloud,
                    session,
                    &local,
                    &remote.request_name,
                    Some(&remote_bytes),
                    projected.version,
                    &all_remote,
                    state.compatibility.transfer_runs,
                    !use_local,
                )
                .await?;
            self.backup_both(
                &local,
                remote,
                &remote_bytes,
                &operation_id,
                (
                    BackupSource::LocalBeforeForce,
                    BackupSource::SteamBeforeForce,
                ),
            )?;
            if let Some(run) = &run {
                self.backup_run(
                    run,
                    slot,
                    &operation_id,
                    BackupSource::LocalBeforeForce,
                    BackupSource::SteamBeforeForce,
                )?;
            }
            if use_local {
                self.push_one(
                    &cloud,
                    session,
                    &local,
                    remote,
                    &remote_bytes,
                    run.as_ref(),
                    &operation_id,
                    &mut state,
                )
                .await?;
            } else {
                state.pending_remote_pulls.insert(
                    slot,
                    PendingRemotePull {
                        slot,
                        remote_name: remote.request_name.clone(),
                        expected_steam_sha1: remote.save.identity.steam_sha1.clone(),
                        requested_unix_ms: unix_ms(),
                        expected_run_sha256: run
                            .as_ref()
                            .map(|run| identity_for_bytes(&run.remote_bytes).sha256),
                    },
                );
            }
        } else if use_local {
            let remote_name = new_remote_name(&state, slot)?;
            self.push_new(
                &cloud,
                session,
                &local,
                &remote_name,
                &operation_id,
                &mut state,
            )
            .await?;
        } else {
            bail!("Steam save slot not found; there is no remote version to pull");
        }
        store.save(&state)?;
        self.update_status(|status| {
            status.pending_choices.retain(|choice| choice.slot != slot);
            if use_local {
                status.detail = format!("Slot {slot} resolved and verified");
            } else {
                status.phase = "restart_required".to_owned();
                status.detail = format!(
                    "Slot {slot} Steam save is verified and queued; relaunch Isaac to apply it"
                );
            }
        });
        Ok(())
    }

    async fn push_new(
        &self,
        cloud: &SteamCloud,
        session: &SteamSession,
        local: &LocalSave,
        remote_name: &str,
        operation_id: &str,
        state: &mut SyncState,
    ) -> Result<()> {
        let (stable_raw, canonical_bytes, canonical_identity) = self.prepare_upload(local).await?;
        let target =
            CloudVersion::from_filename(remote_name.rsplit(['/', '\\']).next().unwrap_or(""))
                .context("invalid new Steam save filename")?
                .0;
        let bytes = save_compat::prepare_for_steam(
            &canonical_bytes,
            target,
            state.compatibility.rep_plus_extras.get(&local.slot),
        )?;
        let (_, latest) = cloud.list_isaac_saves(session).await?;
        let run = self
            .run_context(
                cloud,
                session,
                local,
                remote_name,
                None,
                target,
                &latest,
                state.compatibility.transfer_runs,
                false,
            )
            .await?;
        if latest.iter().any(|file| {
            file.request_name == remote_name
                || (file.save.slot == local.slot
                    && CloudVersion::from_filename(&file.save.filename)
                        .is_some_and(|(version, _)| version == target))
        }) {
            bail!("Steam created this save while the upload was being prepared; synchronize again");
        }
        BackupManager::new(&self.support).backup_file(
            &local.path,
            local.slot,
            BackupSource::LocalFirstSync,
            operation_id,
        )?;
        let run_bytes = self
            .prepare_run_upload(cloud, session, local, &stable_raw, run.as_ref())
            .await?;
        if let Some(run) = &run {
            self.backup_run(
                run,
                local.slot,
                operation_id,
                BackupSource::LocalFirstSync,
                BackupSource::SteamBeforePush,
            )?;
        }
        let verified = upload_slot(
            cloud,
            session,
            remote_name,
            bytes,
            run.as_ref(),
            run_bytes.as_deref(),
            state.compatibility.pc_mods,
        )
        .await?;
        if target == CloudVersion::RepentancePlus {
            let extra = state
                .compatibility
                .rep_plus_extras
                .get(&local.slot)
                .cloned()
                .unwrap_or_default();
            remember_extra(state, local.slot, Some(&extra));
        }
        let observed_local = canonical_identity.clone();
        let mut remote_identity = verified.save.identity;
        remote_identity.sha256 = canonical_identity.sha256.clone();
        remote_identity.size = canonical_identity.size;
        let base_bundle_sha256 = run_bytes
            .as_ref()
            .map(|bytes| crate::run_state::bundle_hash(&observed_local.sha256, bytes));
        upsert_state(
            state,
            FileSyncState {
                slot: local.slot,
                local_relative_path: local.relative_path.clone(),
                remote_name: verified.request_name,
                base_sha256: canonical_identity.sha256,
                base_steam_sha1: remote_identity.steam_sha1.clone(),
                last_local: observed_local,
                last_remote: remote_identity,
                last_successful_sync_unix_ms: unix_ms(),
                base_bundle_sha256,
                base_sync_sha256: Some(crate::sync_identity::comparison_hash(
                    &canonical_bytes,
                    run_bytes.as_deref(),
                )?),
            },
        );
        self.logger
            .log("info", "upload", "new Steam file committed and verified");
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn push_one(
        &self,
        cloud: &SteamCloud,
        session: &SteamSession,
        local: &LocalSave,
        remote: &CloudFile,
        remote_bytes: &[u8],
        run: Option<&RunContext>,
        operation_id: &str,
        state: &mut SyncState,
    ) -> Result<()> {
        let (stable_raw, canonical_bytes, canonical_identity) = self.prepare_upload(local).await?;
        let projected_remote = save_compat::project_for_ios(remote_bytes)?;
        validate_remote_version(remote, projected_remote.version)?;
        // Preserve the latest verified remote extension exactly, even after an
        // explicit force-push. Do not OR counters or reconstruct from Steam stats.
        let bytes = save_compat::prepare_for_steam(
            &canonical_bytes,
            projected_remote.version,
            projected_remote.extra.as_ref(),
        )?;
        self.assert_remote_unchanged(cloud, session, remote, remote_bytes)
            .await?;
        let run_bytes = self
            .prepare_run_upload(cloud, session, local, &stable_raw, run)
            .await?;
        let manager = BackupManager::new(&self.support);
        if run.is_some() {
            manager.backup_bytes(
                &canonical_bytes,
                local
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .context("invalid local progression name")?,
                local.slot,
                BackupSource::LocalBeforeForce,
                operation_id,
                None,
            )?;
        }
        manager.backup_bytes(
            remote_bytes,
            &remote.save.filename,
            local.slot,
            BackupSource::SteamBeforePush,
            operation_id,
            remote.save.identity.steam_sha1.clone(),
        )?;
        if let Some(run) = run {
            self.backup_run(
                run,
                local.slot,
                operation_id,
                BackupSource::LocalBeforeForce,
                BackupSource::SteamBeforePush,
            )?;
        }
        let verified = upload_slot(
            cloud,
            session,
            &remote.request_name,
            bytes,
            run,
            run_bytes.as_deref(),
            state.compatibility.pc_mods,
        )
        .await?;
        remember_extra(state, local.slot, projected_remote.extra.as_ref());
        let mut remote_identity = verified.save.identity;
        remote_identity.sha256 = canonical_identity.sha256.clone();
        remote_identity.size = canonical_identity.size;
        // BASE is the staged version that existed locally and was verified on
        // Steam. Later local writes remain local-only changes against that BASE.
        let observed_local = canonical_identity.clone();
        upsert_state(
            state,
            FileSyncState {
                slot: local.slot,
                local_relative_path: local.relative_path.clone(),
                remote_name: remote.request_name.clone(),
                base_sha256: observed_local.sha256.clone(),
                base_steam_sha1: remote_identity.steam_sha1.clone(),
                last_local: observed_local,
                last_remote: remote_identity,
                last_successful_sync_unix_ms: unix_ms(),
                base_bundle_sha256: run_bytes
                    .as_ref()
                    .map(|bytes| crate::run_state::bundle_hash(&canonical_identity.sha256, bytes)),
                base_sync_sha256: Some(crate::sync_identity::comparison_hash(
                    &canonical_bytes,
                    run_bytes.as_deref(),
                )?),
            },
        );
        self.logger
            .log("info", "upload", "Steam upload committed and verified");
        Ok(())
    }

    async fn prepare_upload(
        &self,
        local: &LocalSave,
    ) -> Result<(FileIdentity, Vec<u8>, FileIdentity)> {
        let staging = unique_temp_path(&self.support.join("tmp"), "upload");
        let stable_raw = wait_for_stable_snapshot(&local.path, &staging).await?;
        let staged_bytes = fs::read(&staging);
        let _ = fs::remove_file(&staging);
        let canonical = canonicalize_save(&staged_bytes?)?;
        if save_compat::project_for_ios(&canonical.bytes)?.version != CloudVersion::Repentance {
            bail!("the native iOS save has an unsupported generation; upload refused");
        }
        if canonical.encoding == SaveEncoding::IosRawLz4 {
            self.logger.log(
                "info",
                "compatibility",
                &format!(
                    "slot={} decoded native iOS LZ4 save for Windows-compatible upload",
                    local.slot
                ),
            );
        }
        let mut canonical_identity = identity_for_bytes(&canonical.bytes);
        canonical_identity.modified_unix_ms = stable_raw.modified_unix_ms;
        Ok((stable_raw, canonical.bytes, canonical_identity))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_context(
        &self,
        cloud: &SteamCloud,
        session: &SteamSession,
        local: &LocalSave,
        persistent_name: &str,
        remote_progress: Option<&[u8]>,
        version: CloudVersion,
        files: &[CloudFile],
        enabled: bool,
        verify_remote_pair: bool,
    ) -> Result<Option<RunContext>> {
        if !enabled || version != CloudVersion::Repentance {
            return Ok(None);
        }
        let local_path = crate::run_state::local_path(&local.path, local.slot)?;
        let (local_bytes, _) = crate::run_state::read_optional(&local_path)?;
        let name = crate::run_state::remote_name(persistent_name, local.slot)?;
        let candidates: Vec<_> = files
            .iter()
            .filter(|file| file.request_name == name)
            .collect();
        if candidates.len() > 1 {
            bail!("ambiguous Steam Continue file");
        }
        let remote_file = candidates.first().map(|file| (*file).clone());
        let remote_raw = match &remote_file {
            Some(file) => cloud.download(session, file).await?,
            None => vec![],
        };
        let mut remote_bytes = if verify_remote_pair {
            crate::run_state::canonicalize(&remote_raw)?
        } else {
            // A confirmed force-push can repair an invalid/orphan remote run.
            // Its exact raw bytes are backed up and guarded against concurrent
            // Steam changes before replacement. Never use this path for pulls.
            crate::run_state::canonicalize(&remote_raw).unwrap_or_default()
        };
        let local_progress = canonicalize_save(&fs::read(&local.path)?)?.bytes;
        if identity_for_bytes(&local_progress).sha256 != local.identity.sha256 {
            bail!("local progression changed while reading Continue; synchronize again");
        }
        crate::run_state::validate_pair(&local_progress, &local_bytes)?;
        let pc_mods = StateStore::new(&self.support).load()?.compatibility.pc_mods;
        let mut adapted_progress = remote_progress.map(|bytes| bytes.to_vec());
        if verify_remote_pair {
            if let Some(progress) = remote_progress {
                crate::run_state::validate_pair(progress, &remote_bytes)?;
                if pc_mods {
                    let pair = crate::run_compat::adapt_pair(progress, &remote_bytes, false)?;
                    adapted_progress = Some(pair.progress);
                    remote_bytes = pair.run;
                } else if crate::run_compat::modding_state(&remote_bytes)?
                    .is_some_and(|(_, modded)| modded)
                {
                    bail!(
                        "Steam Continue uses Lua mods; enable PC Mods Compatibility or disable PC mods"
                    );
                }
            } else if !remote_bytes.is_empty() {
                bail!("Steam has an orphan Continue file; choose an existing progression snapshot");
            }
        }
        Ok(Some(RunContext {
            local_path,
            local_bytes,
            remote_name: name,
            remote_file,
            remote_raw,
            remote_bytes,
            remote_progress: adapted_progress,
        }))
    }

    fn backup_run(
        &self,
        run: &RunContext,
        slot: u8,
        operation: &str,
        local_source: BackupSource,
        remote_source: BackupSource,
    ) -> Result<()> {
        let manager = BackupManager::new(&self.support);
        let name = run
            .local_path
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid local Continue name")?;
        if run.local_path.exists() {
            manager.backup_file(&run.local_path, slot, local_source, operation)?;
        } else {
            manager.backup_bytes(&[], name, slot, local_source, operation, None)?;
        }
        let remote_name = run
            .remote_name
            .rsplit(['/', '\\'])
            .next()
            .context("invalid remote Continue name")?;
        manager.backup_bytes(
            &run.remote_raw,
            remote_name,
            slot,
            remote_source,
            operation,
            run.remote_file
                .as_ref()
                .and_then(|file| file.save.identity.steam_sha1.clone()),
        )?;
        Ok(())
    }

    async fn prepare_run_upload(
        &self,
        cloud: &SteamCloud,
        session: &SteamSession,
        local: &LocalSave,
        progress_raw: &FileIdentity,
        run: Option<&RunContext>,
    ) -> Result<Option<Vec<u8>>> {
        let Some(run) = run else {
            return Ok(None);
        };
        if !self.menu_ready.load(Ordering::Acquire) {
            bail!("save and exit to Isaac's menu before transferring Continue");
        }
        let (bytes, raw_sha) = crate::run_state::read_optional(&run.local_path)?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let (_, after) = crate::run_state::read_optional(&run.local_path)?;
        if raw_sha != after || identity_for_path(&local.path)?.sha256 != progress_raw.sha256 {
            bail!("progression/Continue pair is still changing; upload deferred");
        }
        crate::run_state::validate_pair(&fs::read(&local.path)?, &bytes)?;
        match &run.remote_file {
            Some(file) => {
                self.assert_remote_unchanged(cloud, session, file, &run.remote_raw)
                    .await?
            }
            None => {
                let (_, latest) = cloud.list_isaac_saves(session).await?;
                if latest
                    .iter()
                    .any(|file| file.request_name == run.remote_name)
                {
                    bail!("Steam Continue appeared while staging; synchronize again");
                }
            }
        }
        Ok(Some(bytes))
    }

    async fn assert_remote_unchanged(
        &self,
        cloud: &SteamCloud,
        session: &SteamSession,
        expected: &CloudFile,
        expected_bytes: &[u8],
    ) -> Result<()> {
        let (_, latest_files) = cloud.list_isaac_saves(session).await?;
        let latest = latest_files
            .iter()
            .find(|file| file.request_name == expected.request_name)
            .context("Steam save disappeared while the upload was being prepared")?;
        let unchanged = match (
            expected.save.identity.steam_sha1.as_deref(),
            latest.save.identity.steam_sha1.as_deref(),
        ) {
            (Some(old), Some(new)) => old == new,
            _ => {
                let latest_bytes = cloud.download(session, latest).await?;
                identity_for_bytes(&latest_bytes).sha256
                    == identity_for_bytes(expected_bytes).sha256
            }
        };
        if !unchanged {
            bail!("Steam save changed while the upload was being prepared; synchronize again");
        }
        Ok(())
    }

    fn pull_one(
        &self,
        local: &LocalSave,
        remote: &CloudFile,
        remote_bytes: &[u8],
        run: Option<&RunContext>,
        operation_id: &str,
        state: &mut SyncState,
    ) -> Result<()> {
        let mut projected = save_compat::project_for_ios(remote_bytes)?;
        validate_remote_version(remote, projected.version)?;
        if let Some(progress) = run.and_then(|run| run.remote_progress.as_ref()) {
            projected.bytes = progress.clone();
        }
        let remote_raw = remote_bytes;
        let remote_bytes = projected.bytes.as_slice();
        let _replacement_guard = self
            .replacement_guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !self.replacement_allowed.load(Ordering::Acquire) {
            bail!(
                "local replacement is only allowed at the prelaunch barrier or after an explicit warning"
            );
        }
        let manager = BackupManager::new(&self.support);
        let backup = manager.backup_file(
            &local.path,
            local.slot,
            BackupSource::LocalBeforePull,
            operation_id,
        )?;
        let remote_identity = identity_for_bytes(remote_bytes);
        if let Some(run) = run {
            manager.backup_bytes(
                remote_raw,
                &remote.save.filename,
                local.slot,
                BackupSource::SteamBeforeForce,
                operation_id,
                remote.save.identity.steam_sha1.clone(),
            )?;
            self.backup_run(
                run,
                local.slot,
                operation_id,
                BackupSource::LocalBeforePull,
                BackupSource::SteamBeforeForce,
            )?;
            crate::slot_transaction::replace_pair(
                &self.home,
                &self.support,
                [
                    (&local.path, remote_bytes),
                    (
                        &run.local_path,
                        &crate::run_state::encode_for_ios(&run.remote_bytes)?,
                    ),
                ],
            )?;
        } else if let Err(write_error) = atomic::write_bytes(&local.path, remote_bytes) {
            manager
                .restore_local(&backup.backup_id, &local.path)
                .context("rollback local save after failed atomic replacement")?;
            return Err(write_error.context("atomic save replacement failed; old save restored"));
        }
        // The pair transaction verifies both new files before its commit point.
        // Do not perform a second, progression-only rollback after that point:
        // it could mix the old progression with the newly committed Continue.
        let verified = if run.is_some() {
            remote_identity.clone()
        } else {
            match identity_for_path(&local.path) {
                Ok(identity) => identity,
                Err(verification_error) => {
                    manager
                        .restore_local(&backup.backup_id, &local.path)
                        .context("rollback local save after replacement verification error")?;
                    return Err(verification_error
                        .context("could not verify replaced save; old save restored"));
                }
            }
        };
        if verified.sha256 != remote_identity.sha256 {
            manager
                .restore_local(&backup.backup_id, &local.path)
                .context("rollback local save after replacement verification failure")?;
            bail!("atomic save replacement verification failed; old save restored");
        }
        let mut remote_observed = remote.save.identity.clone();
        remote_observed.sha256 = remote_identity.sha256.clone();
        remote_observed.size = remote_identity.size;
        upsert_state(
            state,
            FileSyncState {
                slot: local.slot,
                local_relative_path: local.relative_path.clone(),
                remote_name: remote.request_name.clone(),
                base_sha256: verified.sha256.clone(),
                base_steam_sha1: remote_observed.steam_sha1.clone(),
                last_local: verified,
                last_remote: remote_observed,
                last_successful_sync_unix_ms: unix_ms(),
                base_bundle_sha256: run.map(|run| {
                    crate::run_state::bundle_hash(&remote_identity.sha256, &run.remote_bytes)
                }),
                base_sync_sha256: Some(crate::sync_identity::comparison_hash(
                    remote_bytes,
                    run.map(|run| run.remote_bytes.as_slice()),
                )?),
            },
        );
        remember_extra(state, local.slot, projected.extra.as_ref());
        self.logger.log(
            "info",
            "download",
            "atomic local replacement succeeded and verified",
        );
        Ok(())
    }

    fn backup_both(
        &self,
        local: &LocalSave,
        remote: &CloudFile,
        remote_bytes: &[u8],
        operation_id: &str,
        sources: (BackupSource, BackupSource),
    ) -> Result<()> {
        let manager = BackupManager::new(&self.support);
        manager.backup_file(&local.path, local.slot, sources.0, operation_id)?;
        manager.backup_bytes(
            remote_bytes,
            &remote.save.filename,
            local.slot,
            sources.1,
            operation_id,
            remote.save.identity.steam_sha1.clone(),
        )?;
        Ok(())
    }

    fn update_status(&self, update: impl FnOnce(&mut StatusSnapshot)) {
        if let Ok(mut status) = self.status.lock() {
            update(&mut status);
        }
    }
}

async fn upload_slot(
    cloud: &SteamCloud,
    session: &SteamSession,
    name: &str,
    bytes: Vec<u8>,
    run: Option<&RunContext>,
    run_bytes: Option<&[u8]>,
    pc_mods: bool,
) -> Result<CloudFile> {
    if let (Some(run), Some(run_bytes)) = (run, run_bytes) {
        let pair = crate::run_compat::adapt_pair(&bytes, run_bytes, pc_mods)?;
        cloud
            .upload_many_verified(
                session,
                &[
                    (name.to_owned(), pair.progress),
                    (run.remote_name.clone(), pair.run),
                ],
            )
            .await?
            .into_iter()
            .find(|file| file.request_name == name)
            .context("progression missing from verified upload pair")
    } else if run.is_none() && run_bytes.is_none() {
        cloud.upload_verified(session, name, &bytes).await
    } else {
        bail!("incomplete progression/Continue upload pair");
    }
}

fn validate_remote_version(file: &CloudFile, version: CloudVersion) -> Result<()> {
    if CloudVersion::from_filename(&file.save.filename).map(|(generation, _)| generation)
        != Some(version)
    {
        bail!(
            "Steam save filename and verified binary generation disagree; no conversion or replacement performed"
        );
    }
    Ok(())
}

fn remember_extra(state: &mut SyncState, slot: u8, extra: Option<&RepPlusExtra>) {
    if let Some(extra) = extra {
        state
            .compatibility
            .rep_plus_extras
            .insert(slot, extra.clone());
    }
}

fn new_remote_name(state: &SyncState, slot: u8) -> Result<String> {
    if let Some(bound) = state.files.values().find(|file| file.slot == slot) {
        return Ok(bound.remote_name.clone());
    }
    let version = match state.compatibility.version {
        CloudVersion::Auto => CloudVersion::Repentance,
        selected => selected,
    };
    version.filename(slot)
}

fn local_summary(save: &LocalSave) -> SaveSummary {
    SaveSummary {
        slot: save.slot,
        filename: save.relative_path.clone(),
        size: save.identity.size,
        modified_unix_ms: save.identity.modified_unix_ms,
        sha256: Some(save.identity.sha256.clone()),
        steam_sha1: save.identity.steam_sha1.clone(),
    }
}

fn remote_summary(file: &CloudFile) -> SaveSummary {
    SaveSummary {
        slot: file.save.slot,
        filename: file.request_name.clone(),
        size: file.save.identity.size,
        modified_unix_ms: file.save.identity.modified_unix_ms,
        sha256: (!file.save.identity.sha256.is_empty()).then(|| file.save.identity.sha256.clone()),
        steam_sha1: file.save.identity.steam_sha1.clone(),
    }
}

fn choice_summary(
    local: &LocalSave,
    remote: &CloudFile,
    remote_identity: &FileIdentity,
    kind: &str,
) -> PendingChoice {
    let mut remote_summary = remote_summary(remote);
    remote_summary.sha256 = Some(remote_identity.sha256.clone());
    PendingChoice {
        slot: local.slot,
        kind: kind.to_owned(),
        local: local_summary(local),
        remote: remote_summary,
        remote_name: remote.request_name.clone(),
        local_run_sha256: None,
        remote_run_sha256: None,
    }
}

fn missing_remote_choice(local: &LocalSave) -> PendingChoice {
    PendingChoice {
        slot: local.slot,
        kind: "first_sync_remote_missing".to_owned(),
        local: local_summary(local),
        remote: SaveSummary {
            slot: local.slot,
            filename: "Not present on Steam".to_owned(),
            size: 0,
            modified_unix_ms: None,
            sha256: None,
            steam_sha1: None,
        },
        remote_name: local
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned(),
        local_run_sha256: None,
        remote_run_sha256: None,
    }
}

fn make_file_state(
    local: &LocalSave,
    remote: &CloudFile,
    remote_identity: &FileIdentity,
    base_bundle_sha256: Option<String>,
    base_sync_sha256: String,
) -> FileSyncState {
    FileSyncState {
        slot: local.slot,
        local_relative_path: local.relative_path.clone(),
        remote_name: remote.request_name.clone(),
        base_sha256: local.identity.sha256.clone(),
        base_steam_sha1: remote_identity.steam_sha1.clone(),
        last_local: local.identity.clone(),
        last_remote: remote_identity.clone(),
        last_successful_sync_unix_ms: unix_ms(),
        base_bundle_sha256,
        base_sync_sha256: Some(base_sync_sha256),
    }
}

fn upsert_state(state: &mut SyncState, entry: FileSyncState) {
    let stale: Vec<String> = state
        .files
        .iter()
        .filter(|(_, value)| value.slot == entry.slot)
        .map(|(key, _)| key.clone())
        .collect();
    for key in stale {
        state.files.remove(&key);
    }
    state.files.insert(entry.remote_name.clone(), entry);
}

fn decision_label(decision: SyncDecision) -> &'static str {
    match decision {
        SyncDecision::Unchanged => "unchanged",
        SyncDecision::UploadLocal => "local-only change",
        SyncDecision::DownloadRemote => "remote-only change",
        SyncDecision::Converged => "already converged",
        SyncDecision::Conflict => "true divergence",
        SyncDecision::FirstSyncChoiceRequired => "first sync choice required",
    }
}

fn short_hash(value: &str) -> &str {
    value.get(..16).unwrap_or(value)
}

fn safe_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    if message.contains("http://")
        || message.contains("https://")
        || message.matches('.').count() >= 2
    {
        "Steam network operation failed; local play is still available".to_owned()
    } else {
        message.chars().take(400).collect()
    }
}
