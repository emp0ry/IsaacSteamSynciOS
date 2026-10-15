use crate::{
    local::unix_ms,
    model::{FileIdentity, RemoteSave, STEAM_APP_ID},
    save_compat::CloudVersion,
    steam::{
        cm::SteamSession,
        proto::{
            CCloudAppFileInfo, CCloudBeginAppUploadBatchRequest, CCloudBeginAppUploadBatchResponse,
            CCloudClientBeginFileUploadRequest, CCloudClientBeginFileUploadResponse,
            CCloudClientCommitFileUploadRequest, CCloudClientCommitFileUploadResponse,
            CCloudClientFileDownloadRequest, CCloudClientFileDownloadResponse,
            CCloudCompleteAppUploadBatchRequest, CCloudCompleteAppUploadBatchResponse,
            CCloudGetAppFileChangelistRequest, CCloudGetAppFileChangelistResponse,
        },
    },
};
use anyhow::{Context, Result, bail};
use prost::Message;
use sha1::{Digest, Sha1};
use std::{
    io::{Cursor, Read},
    time::Duration,
};
use steam_cm_protocol::{emsg::EMsg, protobuf::CMsgProtoBufHeader};
use tokio::time::timeout;

const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_SAVE_TRANSFER_SIZE: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct CloudFile {
    pub save: RemoteSave,
    pub raw: CCloudAppFileInfo,
    pub request_name: String,
}

pub struct SteamCloud {
    http: reqwest::Client,
}

impl SteamCloud {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("Valve/Steam HTTP Client 1.0")
            .timeout(TRANSFER_TIMEOUT)
            .build()?;
        Ok(Self { http })
    }

    pub async fn list_isaac_saves(&self, session: &SteamSession) -> Result<(u64, Vec<CloudFile>)> {
        let response: CCloudGetAppFileChangelistResponse = call(
            session,
            "Cloud.GetAppFileChangelist#1",
            &CCloudGetAppFileChangelistRequest {
                appid: Some(STEAM_APP_ID),
                synced_change_number: Some(0),
            },
        )
        .await?;
        if response.is_only_delta.unwrap_or(false) {
            bail!("Steam returned a delta for a full cloud changelist request");
        }

        let mut saves = Vec::new();
        for file in response
            .files
            .iter()
            .filter(|file| file.persist_state.unwrap_or(0) == 0)
        {
            let prefix = file
                .path_prefix_index
                .and_then(|index| response.path_prefixes.get(index as usize))
                .cloned()
                .unwrap_or_default();
            let filename = file.file_name.clone().unwrap_or_default();
            let request_name = join_cloud_name(&prefix, &filename);
            let Some(basename) = request_name.rsplit(['/', '\\']).next() else {
                continue;
            };
            let Some(slot) = CloudVersion::from_filename(basename)
                .map(|(_, slot)| slot)
                .or_else(|| crate::run_state::slot_for_filename(basename))
            else {
                continue;
            };
            let sha = file.sha_file.as_ref().map(hex::encode);
            saves.push(CloudFile {
                save: RemoteSave {
                    slot,
                    filename: basename.to_owned(),
                    path_prefix: prefix,
                    identity: FileIdentity {
                        sha256: String::new(),
                        size: file.raw_file_size.unwrap_or(0) as u64,
                        modified_unix_ms: file
                            .time_stamp
                            .map(|seconds| seconds.saturating_mul(1000)),
                        steam_sha1: sha,
                    },
                },
                raw: file.clone(),
                request_name,
            });
        }
        saves.sort_by_key(|file| file.save.slot);
        Ok((response.current_change_number.unwrap_or(0), saves))
    }

    pub async fn download(&self, session: &SteamSession, file: &CloudFile) -> Result<Vec<u8>> {
        let info: CCloudClientFileDownloadResponse = call(
            session,
            "Cloud.ClientFileDownload#1",
            &CCloudClientFileDownloadRequest {
                appid: Some(STEAM_APP_ID),
                filename: Some(file.request_name.clone()),
                realm: Some(1),
                force_proxy: Some(false),
            },
        )
        .await?;
        if info.is_explicit_delete.unwrap_or(false) {
            bail!("Steam file is explicitly deleted");
        }
        if info.encrypted.unwrap_or(false) {
            bail!("encrypted Steam Cloud files are not supported");
        }
        let host = info
            .url_host
            .as_deref()
            .context("Steam download host missing")?;
        let path = info
            .url_path
            .as_deref()
            .context("Steam download path missing")?;
        let scheme = if info.use_https.unwrap_or(true) {
            "https"
        } else {
            "http"
        };
        let raw_size = info.raw_file_size.context("Steam raw file size missing")? as usize;
        let file_size = info.file_size.unwrap_or(raw_size as u32) as usize;
        if raw_size > MAX_SAVE_TRANSFER_SIZE || file_size > MAX_SAVE_TRANSFER_SIZE {
            bail!("Steam save exceeds the transfer safety limit");
        }
        let mut request = self.http.get(format!("{scheme}://{host}{path}"));
        for header in &info.request_headers {
            if let (Some(name), Some(value)) = (&header.name, &header.value) {
                request = request.header(name, value);
            }
        }
        let compressed = timeout(TRANSFER_TIMEOUT, async {
            let response = request.send().await?.error_for_status()?;
            if response
                .content_length()
                .is_some_and(|length| length > MAX_SAVE_TRANSFER_SIZE as u64)
            {
                bail!("Steam response exceeds the transfer safety limit");
            }
            let bytes = response.bytes().await?.to_vec();
            if bytes.len() > MAX_SAVE_TRANSFER_SIZE {
                bail!("Steam response exceeds the transfer safety limit");
            }
            Ok::<_, anyhow::Error>(bytes)
        })
        .await
        .context("Steam download timed out")??;

        let bytes = if file_size != raw_size {
            unpack_single_zip_entry(&compressed, raw_size)?
        } else {
            compressed
        };
        if bytes.len() != raw_size {
            bail!(
                "downloaded size {} does not match expected {raw_size}",
                bytes.len()
            );
        }
        let expected_sha = info.sha_file.as_ref().or(file.raw.sha_file.as_ref());
        if let Some(expected) = expected_sha {
            let actual = Sha1::digest(&bytes);
            if actual.as_slice() != expected.as_slice() {
                bail!("downloaded Steam SHA-1 mismatch");
            }
        }
        Ok(bytes)
    }

    pub async fn upload_verified(
        &self,
        session: &SteamSession,
        remote_name: &str,
        bytes: &[u8],
    ) -> Result<CloudFile> {
        self.upload_many_verified(session, &[(remote_name.to_owned(), bytes.to_vec())])
            .await?
            .into_iter()
            .next()
            .context("verified Steam upload missing")
    }

    /// One UFS batch for progression + Continue, with read-back verification of
    /// every file. A failed/incomplete batch never advances the caller's BASE.
    pub async fn upload_many_verified(
        &self,
        session: &SteamSession,
        uploads: &[(String, Vec<u8>)],
    ) -> Result<Vec<CloudFile>> {
        validate_upload_manifest(uploads)?;
        // Steam can reject an identical ClientBeginFileUpload with
        // DuplicateRequest. Verify existing bytes and transfer only changed
        // members, while still verifying the complete desired pair below.
        let (_, existing) = self.list_isaac_saves(session).await?;
        let mut changed = Vec::new();
        let mut unchanged = Vec::new();
        for (name, bytes) in uploads {
            let matches: Vec<_> = existing
                .iter()
                .filter(|file| file.request_name == *name)
                .collect();
            if matches.len() > 1 {
                bail!("ambiguous Steam upload destination");
            }
            let expected = hex::encode(Sha1::digest(bytes));
            if let Some(file) = matches.first()
                && file.save.identity.steam_sha1.as_deref() == Some(expected.as_str())
            {
                if self.download(session, file).await? != *bytes {
                    bail!("unchanged Steam file failed byte verification");
                }
                unchanged.push((*file).clone());
            } else {
                changed.push((name, bytes));
            }
        }
        if changed.is_empty() {
            return Ok(unchanged);
        }
        let batch: CCloudBeginAppUploadBatchResponse = call(
            session,
            "Cloud.BeginAppUploadBatch#1",
            &CCloudBeginAppUploadBatchRequest {
                appid: Some(STEAM_APP_ID),
                machine_name: Some("IsaacCloudSync iPhone".to_owned()),
                files_to_upload: changed.iter().map(|(name, _)| (*name).clone()).collect(),
                files_to_delete: vec![],
                client_id: Some(random_client_id()),
                app_build_id: Some(0),
            },
        )
        .await?;
        let batch_id = batch.batch_id.context("Steam upload batch ID missing")?;

        let transfer = async {
            for (name, bytes) in &changed {
                self.upload_in_batch(session, batch_id, name, bytes, &Sha1::digest(bytes))
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let complete_result: Result<CCloudCompleteAppUploadBatchResponse> = call(
            session,
            "Cloud.CompleteAppUploadBatchBlocking#1",
            &CCloudCompleteAppUploadBatchRequest {
                appid: Some(STEAM_APP_ID),
                batch_id: Some(batch_id),
                batch_eresult: Some(if transfer.is_ok() { 1 } else { 2 }),
            },
        )
        .await;
        if let Err(complete_error) = complete_result
            && transfer.is_ok()
        {
            return Err(complete_error.context("complete Steam upload batch"));
        }
        transfer?;

        let (_, files) = self.list_isaac_saves(session).await?;
        let mut verified = Vec::new();
        for (name, bytes) in uploads {
            let expected_hex = hex::encode(Sha1::digest(bytes));
            let file = files
                .iter()
                .find(|file| {
                    file.request_name == *name
                        && file.save.identity.steam_sha1.as_deref() == Some(expected_hex.as_str())
                })
                .context("uploaded Steam batch file was not verifiable in cloud changelist")?;
            if self.download(session, file).await? != *bytes {
                bail!("uploaded Steam bytes failed read-back verification");
            }
            verified.push(file.clone());
        }
        Ok(verified)
    }

    async fn upload_in_batch(
        &self,
        session: &SteamSession,
        batch_id: u64,
        remote_name: &str,
        bytes: &[u8],
        sha: &[u8],
    ) -> Result<()> {
        let begin: CCloudClientBeginFileUploadResponse = call(
            session,
            "Cloud.ClientBeginFileUpload#1",
            &CCloudClientBeginFileUploadRequest {
                appid: Some(STEAM_APP_ID),
                file_size: Some(bytes.len().try_into().context("save too large")?),
                raw_file_size: Some(bytes.len().try_into().context("save too large")?),
                file_sha: Some(sha.to_vec()),
                time_stamp: Some(unix_ms() / 1000),
                filename: Some(remote_name.to_owned()),
                platforms_to_sync: Some(u32::MAX),
                cell_id: Some(0),
                can_encrypt: Some(false),
                is_shared_file: Some(false),
                deprecated_realm: Some(1),
                upload_batch_id: Some(batch_id),
            },
        )
        .await?;
        if begin.encrypt_file.unwrap_or(false) {
            bail!("Steam unexpectedly requested file encryption");
        }

        let mut transferred = true;
        for block in &begin.block_requests {
            let host = block
                .url_host
                .as_deref()
                .context("Steam upload host missing")?;
            let path = block
                .url_path
                .as_deref()
                .context("Steam upload path missing")?;
            let scheme = if block.use_https.unwrap_or(true) {
                "https"
            } else {
                "http"
            };
            let offset = block.block_offset.unwrap_or(0) as usize;
            let length = block.block_length.unwrap_or(0) as usize;
            let body = if let Some(explicit) = block
                .explicit_body_data
                .as_ref()
                .filter(|value| !value.is_empty())
            {
                explicit.clone()
            } else {
                let end = offset
                    .checked_add(length)
                    .context("upload block overflow")?;
                bytes
                    .get(offset..end)
                    .context("upload block outside staged save")?
                    .to_vec()
            };
            let mut request = self.http.put(format!("{scheme}://{host}{path}")).body(body);
            for header in &block.request_headers {
                if let (Some(name), Some(value)) = (&header.name, &header.value) {
                    request = request.header(name, value);
                }
            }
            let response = timeout(TRANSFER_TIMEOUT, request.send()).await;
            match response {
                Ok(Ok(response)) if response.status().is_success() => {}
                _ => {
                    transferred = false;
                    break;
                }
            }
        }

        let commit: CCloudClientCommitFileUploadResponse = call(
            session,
            "Cloud.ClientCommitFileUpload#1",
            &CCloudClientCommitFileUploadRequest {
                transfer_succeeded: Some(transferred),
                appid: Some(STEAM_APP_ID),
                file_sha: Some(sha.to_vec()),
                filename: Some(remote_name.to_owned()),
            },
        )
        .await?;
        if !transferred || !commit.file_committed.unwrap_or(false) {
            bail!("Steam did not commit uploaded file");
        }
        Ok(())
    }
}

fn validate_upload_manifest(uploads: &[(String, Vec<u8>)]) -> Result<()> {
    if uploads.is_empty() || uploads.len() > 2 {
        bail!("invalid Isaac slot upload batch");
    }
    let mut progress = None;
    let mut run = None;
    for (name, bytes) in uploads {
        if name.len() > 2048 || bytes.len() > MAX_SAVE_TRANSFER_SIZE {
            bail!("Isaac upload exceeds safety limits");
        }
        let basename = name.rsplit(['/', '\\']).next().unwrap_or("");
        if let Some((version, slot)) = CloudVersion::from_filename(basename) {
            if progress.is_some() {
                bail!("an upload batch must contain only one progression file");
            }
            let projected = crate::save_compat::project_for_ios(bytes)?;
            if projected.version != version
                || crate::isaac_format::canonicalize_save(bytes)?.bytes != *bytes
            {
                bail!("Steam requires a canonical progression stream of the selected generation");
            }
            progress = Some((name, bytes, version, slot));
        } else if crate::run_state::slot_for_filename(basename).is_some() {
            if run.is_some() || crate::run_state::canonicalize(bytes)? != *bytes {
                bail!("invalid or compressed Steam Continue upload");
            }
            run = Some((name, bytes));
        } else {
            bail!("unsupported Isaac upload file");
        }
    }
    let (name, bytes, version, slot) = progress.context("Continue cannot be uploaded alone")?;
    if let Some((run_name, run_bytes)) = run {
        if version != CloudVersion::Repentance
            || crate::run_state::remote_name(name, slot)? != *run_name
        {
            bail!("progression and Continue must target the same Repentance slot and cloud path");
        }
        crate::run_state::validate_pair(bytes, run_bytes)?;
    }
    Ok(())
}

/// Resolve a generation without timestamp guessing. A previous exact binding
/// stays pinned in Auto; otherwise coexistence requires an explicit choice.
pub fn select_persistent_saves(
    files: &[CloudFile],
    state: &crate::model::SyncState,
) -> Result<Vec<CloudFile>> {
    if state.compatibility.version == CloudVersion::RepentancePlus {
        bail!(
            "Repentance+ synchronization is not enabled in this release; use ordinary Repentance"
        );
    }
    let mut selected = Vec::new();
    for slot in 1..=3 {
        let mut candidates: Vec<_> = files
            .iter()
            .filter(|file| {
                file.save.slot == slot && CloudVersion::from_filename(&file.save.filename).is_some()
            })
            .collect();
        if state.compatibility.version == CloudVersion::Auto {
            if let Some(bound) = state.files.values().find(|entry| entry.slot == slot)
                && candidates
                    .iter()
                    .any(|file| file.request_name == bound.remote_name)
            {
                candidates.retain(|file| file.request_name == bound.remote_name);
            }
        } else {
            candidates.retain(|file| {
                CloudVersion::from_filename(&file.save.filename)
                    .is_some_and(|(version, _)| version == state.compatibility.version)
            });
        }
        if candidates.len() > 1 {
            bail!(
                "Steam slot {slot} has multiple save versions/paths; choose ordinary Repentance in Steam Save Version settings (no file was overwritten)"
            );
        }
        if let Some(file) = candidates.first() {
            if CloudVersion::from_filename(&file.save.filename)
                .is_some_and(|(version, _)| version == CloudVersion::RepentancePlus)
            {
                bail!(
                    "Steam slot {slot} uses Repentance+; synchronization for that version is not enabled in this release"
                );
            }
            selected.push((*file).clone());
        }
    }
    Ok(selected)
}

async fn call<Req, Resp>(session: &SteamSession, target: &str, request: &Req) -> Result<Resp>
where
    Req: Message,
    Resp: Message + Default,
{
    let state = session.connection.state_snapshot().await;
    let packet = timeout(
        RPC_TIMEOUT,
        session.connection.request(
            EMsg::ServiceMethodCallFromClient,
            CMsgProtoBufHeader {
                steamid: state.steamid,
                client_sessionid: state.client_session_id,
                target_job_name: Some(target.to_owned()),
                ..Default::default()
            },
            request,
        ),
    )
    .await
    .with_context(|| format!("Steam RPC timed out: {target}"))??;
    if let Some(result) = packet.header.eresult.filter(|value| *value != 1) {
        bail!("Steam RPC {target} failed with EResult {result}");
    }
    Ok(packet.decode_body::<Resp>()?)
}

fn join_cloud_name(prefix: &str, filename: &str) -> String {
    if prefix.is_empty() {
        return filename.to_owned();
    }
    if prefix.ends_with(['/', '\\']) {
        format!("{prefix}{filename}")
    } else {
        format!("{prefix}/{filename}")
    }
}

fn unpack_single_zip_entry(bytes: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    if archive.len() != 1 {
        bail!("Steam compressed file must contain exactly one ZIP entry");
    }
    let entry = archive.by_index(0)?;
    if entry.is_dir() {
        bail!("Steam compressed file contained a directory");
    }
    if entry.size() > max_output as u64 {
        bail!("Steam compressed save exceeds its declared raw size");
    }
    let mut output = Vec::with_capacity(entry.size() as usize);
    entry
        .take(max_output.saturating_add(1) as u64)
        .read_to_end(&mut output)?;
    if output.len() > max_output {
        bail!("Steam compressed save exceeds its declared raw size");
    }
    Ok(output)
}

fn random_client_id() -> u64 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u64::from_le_bytes(bytes[..8].try_into().expect("fixed slice"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file(name: &str) -> CloudFile {
        let (_, slot) = CloudVersion::from_filename(name).unwrap();
        CloudFile {
            save: RemoteSave {
                slot,
                filename: name.to_owned(),
                path_prefix: String::new(),
                identity: crate::local::identity_for_bytes(b"data"),
            },
            raw: Default::default(),
            request_name: name.to_owned(),
        }
    }
    #[test]
    fn auto_never_picks_a_generation_by_timestamp() {
        let files = vec![
            file("rep_persistentgamedata1.dat"),
            file("rep+persistentgamedata1.dat"),
        ];
        let mut state = crate::model::SyncState::default();
        assert!(select_persistent_saves(&files, &state).is_err());
        state.compatibility.version = CloudVersion::RepentancePlus;
        assert!(select_persistent_saves(&files, &state).is_err());
        state.compatibility.version = CloudVersion::Repentance;
        assert_eq!(
            select_persistent_saves(&files, &state).unwrap()[0].request_name,
            "rep_persistentgamedata1.dat"
        );
        let duplicates = vec![
            file("rep_persistentgamedata1.dat"),
            file("rep_persistentgamedata1.dat"),
        ];
        state.compatibility.version = CloudVersion::Repentance;
        assert!(select_persistent_saves(&duplicates, &state).is_err());
    }

    #[test]
    fn upload_manifest_cannot_cross_slots_generations_or_paths() {
        let progress = crate::save_compat::tests::fixture(CloudVersion::Repentance);
        let item = || ("rep_persistentgamedata3.dat".to_owned(), progress.clone());
        assert!(validate_upload_manifest(&[item()]).is_ok());
        assert!(validate_upload_manifest(&[item(), item()]).is_err());
        assert!(validate_upload_manifest(&[("rep_gamestate3.dat".into(), vec![])]).is_err());
        assert!(validate_upload_manifest(&[item(), ("rep_gamestate3.dat".into(), vec![])]).is_ok());
        assert!(
            validate_upload_manifest(&[item(), ("rep_gamestate1.dat".into(), vec![])]).is_err()
        );
        assert!(
            validate_upload_manifest(&[item(), ("other/rep_gamestate3.dat".into(), vec![])])
                .is_err()
        );
        assert!(
            validate_upload_manifest(&[("rep+persistentgamedata3.dat".into(), progress)]).is_err()
        );
        let plus = crate::save_compat::tests::fixture(CloudVersion::RepentancePlus);
        assert!(
            validate_upload_manifest(&[
                ("rep+persistentgamedata3.dat".into(), plus),
                ("rep_gamestate3.dat".into(), vec![])
            ])
            .is_err()
        );
        assert!(validate_upload_manifest(&[("achievements.dat".into(), vec![])]).is_err());
    }

    #[test]
    fn repentance_plus_is_not_automatically_enabled_in_this_release() {
        let files = vec![file("rep+persistentgamedata1.dat")];
        assert!(select_persistent_saves(&files, &crate::model::SyncState::default()).is_err());
    }

    #[test]
    fn cloud_name_preserves_steam_prefix() {
        assert_eq!(
            join_cloud_name(
                "WinMy Games/Binding of Isaac Repentance",
                "rep_persistentgamedata1.dat"
            ),
            "WinMy Games/Binding of Isaac Repentance/rep_persistentgamedata1.dat"
        );
        assert_eq!(
            join_cloud_name("", "rep_persistentgamedata2.dat"),
            "rep_persistentgamedata2.dat"
        );
    }

    #[test]
    fn compressed_download_accepts_exactly_one_file() {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut archive = zip::ZipWriter::new(&mut cursor);
            archive
                .start_file(
                    "rep_persistentgamedata1.dat",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            archive.write_all(b"verified-save").unwrap();
            archive.finish().unwrap();
        }
        assert_eq!(
            unpack_single_zip_entry(cursor.get_ref(), b"verified-save".len()).unwrap(),
            b"verified-save"
        );
        assert!(unpack_single_zip_entry(cursor.get_ref(), 4).is_err());
    }
}
