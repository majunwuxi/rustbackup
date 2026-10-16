use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

use backup_storage::DriveHistoryVersion;

use crate::{CredentialStore, OAuthClient, OAuthError};

const DRIVE_API: &str = "https://www.googleapis.com/drive/v3";
const DRIVE_UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";
const FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const CHUNK_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTEMPTS: u32 = 5;
const MAX_LIST_PAGES: usize = 128;

#[derive(Clone, Debug)]
pub struct DriveEndpoints {
    pub api: String,
    pub upload: String,
}

impl Default for DriveEndpoints {
    fn default() -> Self {
        Self {
            api: DRIVE_API.to_owned(),
            upload: DRIVE_UPLOAD.to_owned(),
        }
    }
}

#[derive(Debug, Error)]
pub enum DriveError {
    #[error(transparent)]
    OAuth(#[from] OAuthError),
    #[error("Drive endpoint configuration is invalid")]
    InvalidConfiguration,
    #[error("Drive request failed ({0})")]
    HttpStatus(u16),
    #[error("Drive service could not be reached")]
    Network,
    #[error("Drive returned invalid metadata")]
    InvalidResponse,
    #[error("Drive resumable session is unavailable")]
    SessionExpired,
    #[error("Drive upload integrity check failed")]
    IntegrityMismatch,
    #[error("Drive retry limit was reached")]
    RetryExhausted,
    #[error("restore destination must be a new file in an existing directory")]
    InvalidRestoreDestination,
    #[error("local file metadata changed during the upload")]
    LocalFileChanged,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DriveFile {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub md5_checksum: Option<String>,
    #[serde(default)]
    pub app_properties: BTreeMap<String, String>,
    #[serde(default)]
    pub parents: Vec<String>,
}

impl DriveFile {
    pub fn size_bytes(&self) -> Result<Option<u64>, DriveError> {
        self.size
            .as_deref()
            .map(|value| value.parse().map_err(|_| DriveError::InvalidResponse))
            .transpose()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriveFolders {
    pub root_id: String,
    pub task_id: String,
    pub history_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadResult {
    pub file: DriveFile,
    pub md5_checksum: String,
    pub size_bytes: u64,
    pub reconciled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchivedDriveVersion {
    pub file: DriveFile,
    pub source_revision: i64,
    pub content_hash: String,
    pub size_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct DriveHistoryCopy<'a> {
    pub task_id: &'a str,
    pub source_file_id: i64,
    pub source_revision: i64,
    pub source_hash: &'a str,
    pub source_size: u64,
    pub relative_path: &'a str,
    pub current_drive_file_id: &'a str,
    pub history_folder_id: &'a str,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

pub struct DriveClient {
    oauth: Arc<OAuthClient>,
    credentials: Arc<dyn CredentialStore>,
    account_id: String,
    endpoints: DriveEndpoints,
    agent: ureq::Agent,
    folder_cache: Mutex<HashMap<String, String>>,
}

impl DriveClient {
    pub fn new(
        oauth: Arc<OAuthClient>,
        credentials: Arc<dyn CredentialStore>,
        account_id: impl Into<String>,
        endpoints: DriveEndpoints,
    ) -> Result<Self, DriveError> {
        let account_id = account_id.into();
        if !valid_identifier(&account_id) {
            return Err(DriveError::InvalidConfiguration);
        }
        validate_drive_endpoint(&endpoints.api, "/drive/v3")?;
        validate_drive_endpoint(&endpoints.upload, "/upload/drive/v3")?;
        Ok(Self {
            oauth,
            credentials,
            account_id,
            endpoints,
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .timeout_read(Duration::from_secs(60))
                .timeout_write(Duration::from_secs(60))
                .build(),
            folder_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn check_authorized(&self) -> Result<(), DriveError> {
        self.oauth
            .access_token(&self.account_id)
            .map(|_| ())
            .map_err(Into::into)
    }

    /// Creates or finds the application-owned root, task, and history folders.
    /// Markers are private appProperties, so repeated calls reuse stable IDs.
    pub fn ensure_task_folders(
        &self,
        task_id: &str,
        folder_name: &str,
    ) -> Result<DriveFolders, DriveError> {
        if !valid_identifier(task_id) || !valid_drive_name(folder_name) {
            return Err(DriveError::InvalidConfiguration);
        }
        let root = self.ensure_folder("__rustbackup__", "root", "RustBackup", None)?;
        let task_name = folder_name
            .rsplit(['/', '\\'])
            .find(|part| !part.is_empty())
            .ok_or(DriveError::InvalidConfiguration)?;
        let task = self.ensure_folder(task_id, "task", task_name, Some(&root.id))?;
        let history =
            self.ensure_folder(task_id, "history", ".RustBackupHistory", Some(&task.id))?;
        Ok(DriveFolders {
            root_id: root.id,
            task_id: task.id,
            history_id: history.id,
        })
    }

    /// Returns a managed current file by task and stable source file ID. If
    /// legacy duplicates exist, the lexicographically smallest ID is reused;
    /// this method never deletes remote files.
    pub fn find_current_file(
        &self,
        task_id: &str,
        source_file_id: i64,
    ) -> Result<Option<DriveFile>, DriveError> {
        let matches = self.list_by_properties(&[
            ("rb_task", task_id.to_owned()),
            ("rb_file", source_file_id.to_string()),
            ("rb_role", "current".to_owned()),
        ])?;
        Ok(matches
            .into_iter()
            .min_by(|left, right| left.id.cmp(&right.id)))
    }

    /// Moves an already committed application-owned file to the folder chain
    /// that matches its source-relative path, without re-uploading its bytes.
    pub fn ensure_current_file_path(
        &self,
        task_id: &str,
        source_file_id: i64,
        relative_path: &str,
        task_folder_id: &str,
        drive_file_id: &str,
    ) -> Result<DriveFile, DriveError> {
        if !valid_identifier(task_id)
            || source_file_id <= 0
            || !valid_identifier(task_folder_id)
            || !valid_identifier(drive_file_id)
        {
            return Err(DriveError::InvalidConfiguration);
        }
        let (directory_parts, file_name, normalized_path) =
            split_relative_file_path(relative_path)?;
        let remote = self
            .get_file(drive_file_id)?
            .ok_or(DriveError::InvalidResponse)?;
        validate_current_file_identity(&remote, task_id, source_file_id)?;
        for parent_id in &remote.parents {
            if parent_id != task_folder_id {
                self.validate_managed_source_parent(parent_id, task_id)?;
            }
        }
        let target_parent_id =
            self.ensure_relative_folders(task_id, &directory_parts, task_folder_id)?;
        self.ensure_current_file_location(
            task_id,
            source_file_id,
            &normalized_path,
            &file_name,
            task_folder_id,
            &target_parent_id,
            remote,
        )
    }

    /// Copies the currently committed remote file into the app-managed history
    /// folder before the current file is updated. The appProperties marker
    /// makes a retry idempotent.
    pub fn preserve_history_version(
        &self,
        version: DriveHistoryCopy<'_>,
    ) -> Result<DriveFile, DriveError> {
        let DriveHistoryCopy {
            task_id,
            source_file_id,
            source_revision,
            source_hash,
            source_size,
            relative_path,
            current_drive_file_id,
            history_folder_id,
        } = version;
        if !valid_identifier(task_id)
            || source_file_id <= 0
            || source_revision <= 0
            || source_hash.len() != 64
            || !source_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !valid_drive_name(relative_path)
            || !valid_identifier(current_drive_file_id)
            || !valid_identifier(history_folder_id)
        {
            return Err(DriveError::InvalidConfiguration);
        }
        let marker = format!("{source_file_id}:{source_revision}");
        if let Some(existing) = self
            .list_by_properties(&[
                ("rb_task", task_id.to_owned()),
                ("rb_role", "history".to_owned()),
                ("rb_version", marker.clone()),
            ])?
            .into_iter()
            .min_by(|left, right| left.id.cmp(&right.id))
        {
            return Ok(existing);
        }

        let mut properties = BTreeMap::new();
        properties.insert("rb_task".to_owned(), task_id.to_owned());
        properties.insert("rb_file".to_owned(), source_file_id.to_string());
        properties.insert("rb_revision".to_owned(), source_revision.to_string());
        properties.insert("rb_hash".to_owned(), source_hash.to_ascii_lowercase());
        properties.insert("rb_size".to_owned(), source_size.to_string());
        properties.insert("rb_role".to_owned(), "history".to_owned());
        properties.insert("rb_version".to_owned(), marker);
        properties.insert("rb_path".to_owned(), truncate_property(relative_path));
        let name = format!("{relative_path} (revision {source_revision})");
        let url = format!(
            "{}/files/{}/copy?fields=id,name,mimeType,size,md5Checksum,appProperties,parents",
            self.api_root(),
            encode_path_segment(current_drive_file_id)
        );
        let body = serde_json::json!({
            "name": truncate_drive_name(&name),
            "parents": [history_folder_id],
            "appProperties": properties,
        });
        let file: DriveFile = self.api_json("POST", &url, Some(&body))?;
        validate_remote_file(&file)?;
        Ok(file)
    }

    pub fn preserve_current_revision(
        &self,
        task_id: &str,
        source_file_id: i64,
        expected_revision: i64,
        relative_path: &str,
        current_drive_file_id: &str,
        history_folder_id: &str,
    ) -> Result<ArchivedDriveVersion, DriveError> {
        let current = self
            .get_file(current_drive_file_id)?
            .ok_or(DriveError::InvalidResponse)?;
        let revision = current
            .app_properties
            .get("rb_revision")
            .and_then(|value| value.parse::<i64>().ok())
            .ok_or(DriveError::InvalidResponse)?;
        let content_hash = current
            .app_properties
            .get("rb_hash")
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .cloned()
            .ok_or(DriveError::InvalidResponse)?;
        let size_bytes = current.size_bytes()?.ok_or(DriveError::InvalidResponse)?;
        if revision != expected_revision {
            return Err(DriveError::InvalidResponse);
        }
        let archived = self.preserve_history_version(DriveHistoryCopy {
            task_id,
            source_file_id,
            source_revision: revision,
            source_hash: &content_hash,
            source_size: size_bytes,
            relative_path,
            current_drive_file_id,
            history_folder_id,
        })?;
        Ok(ArchivedDriveVersion {
            file: archived,
            source_revision: revision,
            content_hash,
            size_bytes,
        })
    }

    /// Deletes only a history file whose private markers and parent folder
    /// still match the local manifest. The current file is never eligible here.
    pub fn delete_history_version(
        &self,
        version: &DriveHistoryVersion,
        history_folder_id: &str,
    ) -> Result<bool, DriveError> {
        if version.account_id != self.account_id
            || !valid_identifier(&version.task_id)
            || version.file_id <= 0
            || version.source_revision <= 0
            || !valid_identifier(&version.drive_file_id)
            || !valid_identifier(history_folder_id)
            || version.content_hash.len() != 64
            || !version
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(DriveError::InvalidConfiguration);
        }
        let Some(remote) = self.get_file(&version.drive_file_id)? else {
            return Ok(false);
        };
        let properties = &remote.app_properties;
        let expected_revision_marker = format!("{}:{}", version.file_id, version.source_revision);
        let matches_manifest = remote.id == version.drive_file_id
            && remote
                .parents
                .iter()
                .any(|parent| parent == history_folder_id)
            && properties
                .get("rb_role")
                .is_some_and(|value| value == "history")
            && properties
                .get("rb_task")
                .is_some_and(|value| value == &version.task_id)
            && properties
                .get("rb_file")
                .is_some_and(|value| value == &version.file_id.to_string())
            && properties
                .get("rb_revision")
                .is_some_and(|value| value == &version.source_revision.to_string())
            && properties
                .get("rb_version")
                .is_some_and(|value| value == &expected_revision_marker)
            && properties
                .get("rb_hash")
                .is_some_and(|value| value.eq_ignore_ascii_case(&version.content_hash))
            && properties
                .get("rb_size")
                .and_then(|value| value.parse::<u64>().ok())
                == Some(version.size_bytes);
        if !matches_manifest {
            return Err(DriveError::InvalidResponse);
        }

        let url = format!(
            "{}/files/{}",
            self.api_root(),
            encode_path_segment(&version.drive_file_id)
        );
        let response = self.api_response("DELETE", &url, None)?;
        if response.status == 404 {
            return Ok(false);
        }
        if !(200..300).contains(&response.status) {
            return Err(status_error(&response));
        }
        Ok(true)
    }

    /// Resumable one-way upload. The session URI is kept in the OS credential
    /// vault, not SQLite, because it acts as an upload capability. The caller
    /// supplies the already committed target ID when updating.
    #[allow(clippy::too_many_arguments)]
    pub fn upload_file(
        &self,
        task_id: &str,
        source_file_id: i64,
        source_revision: i64,
        relative_path: &str,
        source_hash: &str,
        source_path: &Path,
        parent_folder_id: &str,
        existing_drive_file_id: Option<&str>,
    ) -> Result<UploadResult, DriveError> {
        let (directory_parts, file_name, normalized_path) =
            split_relative_file_path(relative_path)?;
        if !valid_identifier(task_id)
            || source_file_id <= 0
            || source_revision <= 0
            || source_hash.len() != 64
            || !source_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !valid_identifier(parent_folder_id)
            || existing_drive_file_id.is_some_and(|id| !valid_identifier(id))
        {
            return Err(DriveError::InvalidConfiguration);
        }
        let metadata =
            fs::symlink_metadata(source_path).map_err(|_| DriveError::LocalFileChanged)?;
        if !metadata.is_file() || is_reparse_point(&metadata) {
            return Err(DriveError::LocalFileChanged);
        }
        let size = metadata.len();
        let (blake3_hex, md5_checksum) = hash_local_file(source_path)?;
        if blake3_hex != source_hash.to_ascii_lowercase() {
            return Err(DriveError::LocalFileChanged);
        }
        let mut file_id = existing_drive_file_id.map(str::to_owned);
        if file_id.is_none() {
            file_id = self
                .find_current_file(task_id, source_file_id)?
                .map(|file| file.id);
        }
        let target_parent_id =
            self.ensure_relative_folders(task_id, &directory_parts, parent_folder_id)?;
        let session_key =
            upload_session_key(&self.account_id, task_id, source_file_id, source_revision);

        // An uncertain request can finish after the client loses its response.
        // Reconcile the revision marker before creating another upload session.
        if let Some(remote_id) = file_id.as_deref() {
            if let Some(remote) = self.get_file(remote_id)? {
                let remote = self.ensure_current_file_location(
                    task_id,
                    source_file_id,
                    &normalized_path,
                    &file_name,
                    parent_folder_id,
                    &target_parent_id,
                    remote,
                )?;
                if is_same_revision(&remote, source_revision, source_hash, size) {
                    self.credentials.remove(&session_key)?;
                    return self.finish_upload(
                        task_id,
                        source_file_id,
                        &normalized_path,
                        &file_name,
                        parent_folder_id,
                        &target_parent_id,
                        UploadResult {
                            file: remote,
                            md5_checksum,
                            size_bytes: size,
                            reconciled: true,
                        },
                    );
                }
            }
        } else if let Some(remote) =
            self.find_revision_file(task_id, source_file_id, source_revision)?
        {
            if is_same_revision(&remote, source_revision, source_hash, size) {
                self.credentials.remove(&session_key)?;
                return self.finish_upload(
                    task_id,
                    source_file_id,
                    &normalized_path,
                    &file_name,
                    parent_folder_id,
                    &target_parent_id,
                    UploadResult {
                        file: remote,
                        md5_checksum,
                        size_bytes: size,
                        reconciled: true,
                    },
                );
            }
        }

        let mut properties = BTreeMap::new();
        properties.insert("rb_task".to_owned(), task_id.to_owned());
        properties.insert("rb_file".to_owned(), source_file_id.to_string());
        properties.insert("rb_revision".to_owned(), source_revision.to_string());
        properties.insert("rb_hash".to_owned(), source_hash.to_ascii_lowercase());
        properties.insert("rb_role".to_owned(), "current".to_owned());
        properties.insert("rb_path".to_owned(), truncate_property(&normalized_path));
        let mut body = serde_json::json!({
            "name": truncate_drive_name(&file_name),
            "appProperties": properties,
        });
        if file_id.is_none() {
            body["parents"] = serde_json::json!([target_parent_id]);
        }

        let mut session_uri = self.credentials.get(&session_key)?;
        let mut offset = 0_u64;
        if let Some(uri) = session_uri.as_deref() {
            match self.query_upload_session(uri, size) {
                Ok(SessionStatus::Complete(file)) => {
                    self.credentials.remove(&session_key)?;
                    verify_remote_file(&file, size, &md5_checksum)?;
                    return self.finish_upload(
                        task_id,
                        source_file_id,
                        &normalized_path,
                        &file_name,
                        parent_folder_id,
                        &target_parent_id,
                        UploadResult {
                            file,
                            md5_checksum,
                            size_bytes: size,
                            reconciled: true,
                        },
                    );
                }
                Ok(SessionStatus::Incomplete(next_offset)) => offset = next_offset,
                Err(DriveError::SessionExpired) => {
                    self.credentials.remove(&session_key)?;
                    session_uri = None;
                }
                Err(error) => return Err(error),
            }
        }
        if session_uri.is_none() {
            let uri = self.start_upload_session(file_id.as_deref(), &body, size, &session_key)?;
            session_uri = Some(uri);
            offset = 0;
        }
        let session_uri = session_uri.ok_or(DriveError::SessionExpired)?;

        if size == 0 {
            let response = self.put_chunk(&session_uri, 0, 0, &[])?;
            if response.status == 200 || response.status == 201 {
                let file = response.json::<DriveFile>()?;
                self.credentials.remove(&session_key)?;
                verify_remote_file(&file, 0, &md5_checksum)?;
                return self.finish_upload(
                    task_id,
                    source_file_id,
                    &normalized_path,
                    &file_name,
                    parent_folder_id,
                    &target_parent_id,
                    UploadResult {
                        file,
                        md5_checksum,
                        size_bytes: 0,
                        reconciled: false,
                    },
                );
            }
            return Err(DriveError::InvalidResponse);
        }

        let mut source = File::open(source_path).map_err(|_| DriveError::LocalFileChanged)?;
        let mut stalled_attempts = 0_u32;
        while offset < size {
            source
                .seek(SeekFrom::Start(offset))
                .map_err(|_| DriveError::LocalFileChanged)?;
            let count = usize::try_from((size - offset).min(CHUNK_BYTES as u64))
                .map_err(|_| DriveError::InvalidResponse)?;
            let mut chunk = vec![0_u8; count];
            source
                .read_exact(&mut chunk)
                .map_err(|_| DriveError::LocalFileChanged)?;
            let end = offset + count as u64 - 1;
            let response = match self.put_chunk(&session_uri, offset, size, &chunk) {
                Ok(response) => response,
                Err(DriveError::Network) => {
                    stalled_attempts += 1;
                    if stalled_attempts >= MAX_ATTEMPTS {
                        return Err(DriveError::RetryExhausted);
                    }
                    match self.reconcile_after_uncertain_upload(
                        &session_uri,
                        size,
                        &session_key,
                        &md5_checksum,
                    )? {
                        SessionStatus::Complete(file) => {
                            verify_remote_file(&file, size, &md5_checksum)?;
                            self.credentials.remove(&session_key)?;
                            return self.finish_upload(
                                task_id,
                                source_file_id,
                                &normalized_path,
                                &file_name,
                                parent_folder_id,
                                &target_parent_id,
                                UploadResult {
                                    file,
                                    md5_checksum,
                                    size_bytes: size,
                                    reconciled: true,
                                },
                            );
                        }
                        SessionStatus::Incomplete(next_offset) => {
                            if next_offset > size {
                                return Err(DriveError::InvalidResponse);
                            }
                            offset = next_offset;
                        }
                    }
                    retry_delay(stalled_attempts - 1, None);
                    continue;
                }
                Err(error) => return Err(error),
            };
            if response.status == 308 {
                let next_offset = response
                    .headers
                    .get("range")
                    .and_then(|range| range.rsplit('-').next())
                    .and_then(|last| last.parse::<u64>().ok())
                    .map_or(0, |last| last.saturating_add(1));
                if next_offset > end.saturating_add(1) || next_offset > size {
                    return Err(DriveError::InvalidResponse);
                }
                if next_offset == offset {
                    stalled_attempts += 1;
                    if stalled_attempts >= MAX_ATTEMPTS {
                        return Err(DriveError::RetryExhausted);
                    }
                    retry_delay(stalled_attempts - 1, response.headers.get("retry-after"));
                } else {
                    stalled_attempts = 0;
                }
                offset = next_offset;
                continue;
            }
            if response.status == 200 || response.status == 201 {
                let file = response.json::<DriveFile>()?;
                self.credentials.remove(&session_key)?;
                verify_remote_file(&file, size, &md5_checksum)?;
                return self.finish_upload(
                    task_id,
                    source_file_id,
                    &normalized_path,
                    &file_name,
                    parent_folder_id,
                    &target_parent_id,
                    UploadResult {
                        file,
                        md5_checksum,
                        size_bytes: size,
                        reconciled: false,
                    },
                );
            }
            if response.status == 404 || response.status == 410 {
                self.credentials.remove(&session_key)?;
                return Err(DriveError::SessionExpired);
            }
            if is_retryable(&response) {
                stalled_attempts += 1;
                if stalled_attempts >= MAX_ATTEMPTS {
                    return Err(DriveError::RetryExhausted);
                }
                match self.reconcile_after_uncertain_upload(
                    &session_uri,
                    size,
                    &session_key,
                    &md5_checksum,
                )? {
                    SessionStatus::Complete(file) => {
                        return Ok(UploadResult {
                            file,
                            md5_checksum,
                            size_bytes: size,
                            reconciled: true,
                        });
                    }
                    SessionStatus::Incomplete(next_offset) => offset = next_offset,
                }
                retry_delay(stalled_attempts - 1, response.headers.get("retry-after"));
                continue;
            }
            return Err(status_error(&response));
        }
        Err(DriveError::InvalidResponse)
    }

    /// Downloads one managed binary and atomically publishes it to a new path
    /// only after its BLAKE3 digest and length match the cloud manifest.
    pub fn download_verified_to(
        &self,
        drive_file_id: &str,
        expected_hash: &str,
        expected_size: u64,
        destination: &Path,
    ) -> Result<(), DriveError> {
        if !valid_identifier(drive_file_id)
            || expected_hash.len() != 64
            || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(DriveError::InvalidConfiguration);
        }
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .ok_or(DriveError::InvalidRestoreDestination)?;
        let parent_metadata =
            fs::symlink_metadata(parent).map_err(|_| DriveError::InvalidRestoreDestination)?;
        if !parent_metadata.is_dir() || is_reparse_point(&parent_metadata) || destination.exists() {
            return Err(DriveError::InvalidRestoreDestination);
        }
        for ancestor in parent.ancestors() {
            let metadata = fs::symlink_metadata(ancestor)
                .map_err(|_| DriveError::InvalidRestoreDestination)?;
            if is_reparse_point(&metadata) {
                return Err(DriveError::InvalidRestoreDestination);
            }
        }
        let url = format!(
            "{}/files/{}?alt=media",
            self.api_root(),
            encode_path_segment(drive_file_id)
        );
        let response = self.api_stream(&url)?;
        if !(200..300).contains(&response.status()) {
            return Err(DriveError::HttpStatus(response.status()));
        }
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .map_err(|_| DriveError::InvalidRestoreDestination)?;
        let mut hasher = blake3::Hasher::new();
        let mut length = 0_u64;
        let mut reader = response.into_reader();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer).map_err(|_| DriveError::Network)?;
            if read == 0 {
                break;
            }
            length = length.saturating_add(read as u64);
            if length > expected_size {
                return Err(DriveError::IntegrityMismatch);
            }
            hasher.update(&buffer[..read]);
            temp.write_all(&buffer[..read])
                .map_err(|_| DriveError::Network)?;
        }
        if length != expected_size
            || hasher.finalize().to_hex().as_str() != expected_hash.to_ascii_lowercase()
        {
            return Err(DriveError::IntegrityMismatch);
        }
        temp.as_file().sync_all().map_err(|_| DriveError::Network)?;
        let file = temp
            .persist_noclobber(destination)
            .map_err(|_| DriveError::InvalidRestoreDestination)?;
        file.sync_all().map_err(|_| DriveError::Network)?;
        Ok(())
    }

    fn ensure_relative_folders(
        &self,
        task_id: &str,
        directory_parts: &[String],
        root_folder_id: &str,
    ) -> Result<String, DriveError> {
        if !valid_identifier(task_id) || !valid_identifier(root_folder_id) {
            return Err(DriveError::InvalidConfiguration);
        }
        let mut cache = self
            .folder_cache
            .lock()
            .map_err(|_| DriveError::InvalidResponse)?;
        let mut parent_id = root_folder_id.to_owned();
        let mut relative_path = String::new();
        for part in directory_parts {
            if !valid_drive_name(part) || part.contains(['/', '\\']) {
                return Err(DriveError::InvalidConfiguration);
            }
            if !relative_path.is_empty() {
                relative_path.push('/');
            }
            relative_path.push_str(part);
            let path_hash = blake3::hash(relative_path.as_bytes()).to_hex().to_string();
            let cache_key = format!("{task_id}:{parent_id}:{path_hash}");
            if let Some(folder_id) = cache.get(&cache_key) {
                parent_id.clone_from(folder_id);
                continue;
            }
            let role = format!("source-dir:{path_hash}");
            let folder = self.ensure_folder(task_id, &role, part, Some(&parent_id))?;
            parent_id.clone_from(&folder.id);
            cache.insert(cache_key, folder.id);
        }
        Ok(parent_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_upload(
        &self,
        task_id: &str,
        source_file_id: i64,
        normalized_path: &str,
        file_name: &str,
        task_folder_id: &str,
        parent_folder_id: &str,
        mut result: UploadResult,
    ) -> Result<UploadResult, DriveError> {
        result.file = self.ensure_current_file_location(
            task_id,
            source_file_id,
            normalized_path,
            file_name,
            task_folder_id,
            parent_folder_id,
            result.file,
        )?;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    fn ensure_current_file_location(
        &self,
        task_id: &str,
        source_file_id: i64,
        normalized_path: &str,
        file_name: &str,
        task_folder_id: &str,
        target_parent_id: &str,
        remote: DriveFile,
    ) -> Result<DriveFile, DriveError> {
        let path_marker = truncate_property(normalized_path);
        validate_current_file_identity(&remote, task_id, source_file_id)?;
        let parents_to_remove = remote
            .parents
            .iter()
            .filter(|parent| parent.as_str() != target_parent_id)
            .cloned()
            .collect::<Vec<_>>();
        for parent_id in &parents_to_remove {
            if parent_id == task_folder_id {
                continue;
            }
            self.validate_managed_source_parent(parent_id, task_id)?;
        }
        let path_matches = remote.app_properties.get("rb_path") == Some(&path_marker);
        if remote.name == truncate_drive_name(file_name)
            && remote
                .parents
                .iter()
                .any(|parent| parent == target_parent_id)
            && parents_to_remove.is_empty()
            && path_matches
        {
            return Ok(remote);
        }

        let mut url = Url::parse(&format!(
            "{}/files/{}",
            self.api_root(),
            encode_path_segment(&remote.id)
        ))
        .map_err(|_| DriveError::InvalidConfiguration)?;
        url.query_pairs_mut().append_pair(
            "fields",
            "id,name,mimeType,size,md5Checksum,appProperties,parents",
        );
        if !remote
            .parents
            .iter()
            .any(|parent| parent == target_parent_id)
        {
            url.query_pairs_mut()
                .append_pair("addParents", target_parent_id);
        }
        if !parents_to_remove.is_empty() {
            url.query_pairs_mut()
                .append_pair("removeParents", &parents_to_remove.join(","));
        }
        let mut properties = remote.app_properties.clone();
        properties.insert("rb_path".to_owned(), path_marker);
        let body = serde_json::json!({
            "name": truncate_drive_name(file_name),
            "appProperties": properties,
        });
        let updated: DriveFile = self.api_json("PATCH", url.as_str(), Some(&body))?;
        validate_remote_file(&updated)?;
        if updated.id != remote.id
            || updated.mime_type == FOLDER_MIME_TYPE
            || updated.name != truncate_drive_name(file_name)
            || !updated
                .parents
                .iter()
                .any(|parent| parent == target_parent_id)
            || updated
                .parents
                .iter()
                .any(|parent| parent != target_parent_id)
            || updated.app_properties.get("rb_task").map(String::as_str) != Some(task_id)
            || updated.app_properties.get("rb_file").map(String::as_str)
                != Some(source_file_id.to_string().as_str())
            || updated.app_properties.get("rb_role").map(String::as_str) != Some("current")
            || updated.app_properties.get("rb_path") != Some(&truncate_property(normalized_path))
        {
            return Err(DriveError::InvalidResponse);
        }
        Ok(updated)
    }

    fn validate_managed_source_parent(
        &self,
        parent_id: &str,
        task_id: &str,
    ) -> Result<(), DriveError> {
        let parent = self
            .get_file(parent_id)?
            .ok_or(DriveError::InvalidResponse)?;
        let role = parent
            .app_properties
            .get("rb_role")
            .map(String::as_str)
            .unwrap_or_default();
        if parent.mime_type != FOLDER_MIME_TYPE
            || parent.app_properties.get("rb_task").map(String::as_str) != Some(task_id)
            || !role.starts_with("folder:source-dir:")
        {
            return Err(DriveError::InvalidResponse);
        }
        Ok(())
    }

    fn ensure_folder(
        &self,
        task_id: &str,
        role: &str,
        name: &str,
        parent_id: Option<&str>,
    ) -> Result<DriveFile, DriveError> {
        let matches = self.list_by_properties(&[
            ("rb_task", task_id.to_owned()),
            ("rb_role", format!("folder:{role}")),
        ])?;
        if let Some(folder) = matches
            .into_iter()
            .min_by(|left, right| left.id.cmp(&right.id))
        {
            if folder.mime_type != FOLDER_MIME_TYPE
                || parent_id
                    .is_some_and(|parent| !folder.parents.iter().any(|value| value == parent))
            {
                return Err(DriveError::InvalidResponse);
            }
            if folder.name == name {
                return Ok(folder);
            }

            let url = format!(
                "{}/files/{}?fields=id,name,mimeType,appProperties,parents",
                self.api_root(),
                encode_path_segment(&folder.id)
            );
            let body = serde_json::json!({ "name": name });
            let renamed: DriveFile = self.api_json("PATCH", &url, Some(&body))?;
            validate_remote_file(&renamed)?;
            let expected_role = format!("folder:{role}");
            if renamed.id != folder.id
                || renamed.name != name
                || renamed.mime_type != FOLDER_MIME_TYPE
                || renamed.app_properties.get("rb_task").map(String::as_str) != Some(task_id)
                || renamed.app_properties.get("rb_role").map(String::as_str)
                    != Some(expected_role.as_str())
                || parent_id
                    .is_some_and(|parent| !renamed.parents.iter().any(|value| value == parent))
            {
                return Err(DriveError::InvalidResponse);
            }
            return Ok(renamed);
        }
        let mut properties = BTreeMap::new();
        properties.insert("rb_task".to_owned(), task_id.to_owned());
        properties.insert("rb_role".to_owned(), format!("folder:{role}"));
        let mut body = serde_json::json!({
            "name": name,
            "mimeType": FOLDER_MIME_TYPE,
            "appProperties": properties,
        });
        if let Some(parent_id) = parent_id {
            body["parents"] = serde_json::json!([parent_id]);
        }
        let url = format!(
            "{}/files?fields=id,name,mimeType,appProperties,parents",
            self.api_root()
        );
        let folder: DriveFile = self.api_json("POST", &url, Some(&body))?;
        validate_remote_file(&folder)?;
        let expected_role = format!("folder:{role}");
        if folder.mime_type != FOLDER_MIME_TYPE
            || folder.app_properties.get("rb_task").map(String::as_str) != Some(task_id)
            || folder.app_properties.get("rb_role").map(String::as_str)
                != Some(expected_role.as_str())
            || parent_id.is_some_and(|parent| !folder.parents.iter().any(|value| value == parent))
        {
            return Err(DriveError::InvalidResponse);
        }
        Ok(folder)
    }

    fn list_by_properties(
        &self,
        properties: &[(&str, String)],
    ) -> Result<Vec<DriveFile>, DriveError> {
        let terms = properties
            .iter()
            .map(|(key, value)| {
                format!(
                    "appProperties has {{ key='{}' and value='{}' }}",
                    quote_query(key),
                    quote_query(value)
                )
            })
            .collect::<Vec<_>>();
        let query = format!("{} and trashed = false", terms.join(" and "));
        let base_url = Url::parse(&format!("{}/files", self.api_root()))
            .map_err(|_| DriveError::InvalidConfiguration)?;
        let mut page_token = None;
        let mut seen_page_tokens = HashSet::new();
        let mut files = Vec::new();
        for _ in 0..MAX_LIST_PAGES {
            let mut url = base_url.clone();
            {
                let mut pairs = url.query_pairs_mut();
                pairs
                    .append_pair("q", &query)
                    .append_pair("spaces", "drive")
                    .append_pair("pageSize", "1000")
                    .append_pair(
                        "fields",
                        "nextPageToken,files(id,name,mimeType,size,md5Checksum,appProperties,parents)",
                    );
                if let Some(token) = page_token.as_deref() {
                    pairs.append_pair("pageToken", token);
                }
            }
            let page: FileList = self.api_json("GET", url.as_str(), None)?;
            files.extend(page.files);
            let Some(next) = page.next_page_token else {
                return Ok(files);
            };
            if next.is_empty()
                || next.len() > 4096
                || next.chars().any(char::is_control)
                || !seen_page_tokens.insert(next.clone())
            {
                return Err(DriveError::InvalidResponse);
            }
            page_token = Some(next);
        }
        Err(DriveError::InvalidResponse)
    }

    fn find_revision_file(
        &self,
        task_id: &str,
        source_file_id: i64,
        source_revision: i64,
    ) -> Result<Option<DriveFile>, DriveError> {
        let matches = self.list_by_properties(&[
            ("rb_task", task_id.to_owned()),
            ("rb_file", source_file_id.to_string()),
            ("rb_revision", source_revision.to_string()),
            ("rb_role", "current".to_owned()),
        ])?;
        Ok(matches
            .into_iter()
            .min_by(|left, right| left.id.cmp(&right.id)))
    }

    fn get_file(&self, file_id: &str) -> Result<Option<DriveFile>, DriveError> {
        let url = format!(
            "{}/files/{}?fields=id,name,mimeType,size,md5Checksum,appProperties,parents",
            self.api_root(),
            encode_path_segment(file_id)
        );
        let response = self.api_response("GET", &url, None)?;
        if response.status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&response.status) {
            return Err(status_error(&response));
        }
        let file = response.json()?;
        validate_remote_file(&file)?;
        Ok(Some(file))
    }

    fn start_upload_session(
        &self,
        existing_file_id: Option<&str>,
        metadata: &serde_json::Value,
        size: u64,
        session_key: &str,
    ) -> Result<String, DriveError> {
        let action = existing_file_id.map_or_else(
            || format!("{}/files", self.upload_root()),
            |file_id| {
                format!(
                    "{}/files/{}",
                    self.upload_root(),
                    encode_path_segment(file_id)
                )
            },
        );
        let mut url = Url::parse(&action).map_err(|_| DriveError::InvalidConfiguration)?;
        url.query_pairs_mut()
            .append_pair("uploadType", "resumable")
            .append_pair(
                "fields",
                "id,name,mimeType,size,md5Checksum,appProperties,parents",
            );
        let method = if existing_file_id.is_some() {
            "PATCH"
        } else {
            "POST"
        };
        let response = self.api_response_with_retry(
            method,
            url.as_str(),
            RequestBody::Json(metadata),
            &[
                ("X-Upload-Content-Type", "application/octet-stream"),
                ("X-Upload-Content-Length", &size.to_string()),
            ],
        )?;
        if !(200..300).contains(&response.status) {
            return Err(status_error(&response));
        }
        let uri = response
            .headers
            .get("location")
            .cloned()
            .ok_or(DriveError::InvalidResponse)?;
        validate_session_uri(&uri)?;
        self.credentials.put(session_key, &uri)?;
        Ok(uri)
    }

    fn query_upload_session(&self, uri: &str, size: u64) -> Result<SessionStatus, DriveError> {
        validate_session_uri(uri)?;
        let response = self.api_response_with_retry(
            "PUT",
            uri,
            RequestBody::Bytes(&[]),
            &[("Content-Range", &format!("bytes */{size}"))],
        )?;
        match response.status {
            200 | 201 => Ok(SessionStatus::Complete(response.json()?)),
            308 => {
                let next = response
                    .headers
                    .get("range")
                    .and_then(|range| range.rsplit('-').next())
                    .and_then(|last| last.parse::<u64>().ok())
                    .map_or(0, |last| last.saturating_add(1));
                if next > size {
                    return Err(DriveError::InvalidResponse);
                }
                Ok(SessionStatus::Incomplete(next))
            }
            404 | 410 => Err(DriveError::SessionExpired),
            _ => Err(status_error(&response)),
        }
    }

    fn reconcile_after_uncertain_upload(
        &self,
        uri: &str,
        size: u64,
        session_key: &str,
        expected_md5: &str,
    ) -> Result<SessionStatus, DriveError> {
        let status = match self.query_upload_session(uri, size) {
            Ok(status) => status,
            Err(DriveError::SessionExpired) => {
                self.credentials.remove(session_key)?;
                return Err(DriveError::SessionExpired);
            }
            Err(error) => return Err(error),
        };
        if let SessionStatus::Complete(file) = &status {
            self.credentials.remove(session_key)?;
            verify_remote_file(file, size, expected_md5)?;
        }
        Ok(status)
    }

    fn put_chunk(
        &self,
        uri: &str,
        start: u64,
        size: u64,
        bytes: &[u8],
    ) -> Result<ApiResponse, DriveError> {
        validate_session_uri(uri)?;
        let range = if size == 0 {
            "bytes */0".to_owned()
        } else {
            format!("bytes {start}-{}/{size}", start + bytes.len() as u64 - 1)
        };
        self.api_response_once(
            "PUT",
            uri,
            RequestBody::Bytes(bytes),
            &[
                ("Content-Range", &range),
                ("Content-Type", "application/octet-stream"),
            ],
        )
    }

    fn api_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, DriveError> {
        let body = body.map_or(RequestBody::None, RequestBody::Json);
        let response = self.api_response_with_retry(method, url, body, &[])?;
        if !(200..300).contains(&response.status) {
            return Err(status_error(&response));
        }
        response.json()
    }

    fn api_response(
        &self,
        method: &str,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<ApiResponse, DriveError> {
        let body = body.map_or(RequestBody::None, RequestBody::Json);
        self.api_response_with_retry(method, url, body, &[])
    }

    fn api_response_with_retry(
        &self,
        method: &str,
        url: &str,
        body: RequestBody<'_>,
        headers: &[(&str, &str)],
    ) -> Result<ApiResponse, DriveError> {
        let mut refreshed = false;
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            let access_token = self.oauth.access_token(&self.account_id)?;
            let mut request = self
                .agent
                .request(method, url)
                .set("Authorization", &format!("Bearer {access_token}"));
            for (name, value) in headers {
                request = request.set(name, value);
            }
            let response = match body {
                RequestBody::Json(body) => request.send_json(body),
                RequestBody::Bytes(bytes) => request.send_bytes(bytes),
                RequestBody::None => request.call(),
            };
            let response = match response {
                Ok(response) => ApiResponse::from_response(response)?,
                Err(ureq::Error::Status(_, response)) => ApiResponse::from_response(response)?,
                Err(ureq::Error::Transport(_)) => {
                    last_error = Some(DriveError::Network);
                    if attempt + 1 == MAX_ATTEMPTS {
                        break;
                    }
                    retry_delay(attempt, None);
                    continue;
                }
            };
            if response.status == 401 && !refreshed {
                self.oauth.force_refresh_token(&self.account_id)?;
                refreshed = true;
                continue;
            }
            if is_retryable(&response) {
                if attempt + 1 == MAX_ATTEMPTS {
                    last_error = Some(DriveError::RetryExhausted);
                    break;
                }
                retry_delay(attempt, response.headers.get("retry-after"));
                continue;
            }
            return Ok(response);
        }
        Err(last_error.unwrap_or(DriveError::RetryExhausted))
    }

    fn api_response_once(
        &self,
        method: &str,
        url: &str,
        body: RequestBody<'_>,
        headers: &[(&str, &str)],
    ) -> Result<ApiResponse, DriveError> {
        let mut refreshed = false;
        loop {
            let access_token = self.oauth.access_token(&self.account_id)?;
            let mut request = self
                .agent
                .request(method, url)
                .set("Authorization", &format!("Bearer {access_token}"));
            for (name, value) in headers {
                request = request.set(name, value);
            }
            let response = match body {
                RequestBody::Json(body) => request.send_json(body),
                RequestBody::Bytes(bytes) => request.send_bytes(bytes),
                RequestBody::None => request.call(),
            };
            let response = match response {
                Ok(response) => ApiResponse::from_response(response)?,
                Err(ureq::Error::Status(_, response)) => ApiResponse::from_response(response)?,
                Err(ureq::Error::Transport(_)) => return Err(DriveError::Network),
            };
            if response.status == 401 && !refreshed {
                self.oauth.force_refresh_token(&self.account_id)?;
                refreshed = true;
                continue;
            }
            return Ok(response);
        }
    }

    fn api_stream(&self, url: &str) -> Result<ureq::Response, DriveError> {
        let mut refreshed = false;
        let mut last_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            let access_token = self.oauth.access_token(&self.account_id)?;
            let response = self
                .agent
                .get(url)
                .set("Authorization", &format!("Bearer {access_token}"))
                .call();
            let response = match response {
                Ok(response) => response,
                Err(ureq::Error::Status(status, response)) => {
                    if status == 401 && !refreshed {
                        self.oauth.force_refresh_token(&self.account_id)?;
                        refreshed = true;
                        continue;
                    }
                    if status == 429 || status >= 500 {
                        if attempt + 1 == MAX_ATTEMPTS {
                            last_error = Some(DriveError::RetryExhausted);
                            break;
                        }
                        retry_delay(
                            attempt,
                            response.header("Retry-After").map(str::to_owned).as_ref(),
                        );
                        continue;
                    }
                    return Err(DriveError::HttpStatus(status));
                }
                Err(ureq::Error::Transport(_)) => {
                    last_error = Some(DriveError::Network);
                    if attempt + 1 == MAX_ATTEMPTS {
                        break;
                    }
                    retry_delay(attempt, None);
                    continue;
                }
            };
            return Ok(response);
        }
        Err(last_error.unwrap_or(DriveError::RetryExhausted))
    }

    fn api_root(&self) -> &str {
        self.endpoints.api.trim_end_matches('/')
    }

    fn upload_root(&self) -> &str {
        self.endpoints.upload.trim_end_matches('/')
    }
}

enum SessionStatus {
    Incomplete(u64),
    Complete(DriveFile),
}

struct ApiResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

impl ApiResponse {
    fn from_response(response: ureq::Response) -> Result<Self, DriveError> {
        let status = response.status();
        let mut headers = BTreeMap::new();
        for name in ["location", "range", "retry-after"] {
            if let Some(value) = response.header(name) {
                headers.insert(name.to_owned(), value.to_owned());
            }
        }
        let mut reader = response.into_reader();
        let mut body = Vec::new();
        reader
            .by_ref()
            .take((MAX_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut body)
            .map_err(|_| DriveError::Network)?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(DriveError::InvalidResponse);
        }
        Ok(Self {
            status,
            headers,
            body,
        })
    }

    fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, DriveError> {
        serde_json::from_slice(&self.body).map_err(|_| DriveError::InvalidResponse)
    }
}

#[derive(Clone, Copy)]
enum RequestBody<'a> {
    None,
    Json(&'a serde_json::Value),
    Bytes(&'a [u8]),
}

fn hash_local_file(path: &Path) -> Result<(String, String), DriveError> {
    let mut file = File::open(path).map_err(|_| DriveError::LocalFileChanged)?;
    let mut blake3 = blake3::Hasher::new();
    let mut md5 = Md5::new();
    let mut buffer = [0_u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| DriveError::LocalFileChanged)?;
        if read == 0 {
            break;
        }
        blake3.update(&buffer[..read]);
        md5.update(&buffer[..read]);
    }
    Ok((
        blake3.finalize().to_hex().to_string(),
        format!("{:x}", md5.finalize()),
    ))
}

fn verify_remote_file(
    file: &DriveFile,
    expected_size: u64,
    expected_md5: &str,
) -> Result<(), DriveError> {
    validate_remote_file(file)?;
    if file.size_bytes()? != Some(expected_size)
        || file
            .md5_checksum
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case(expected_md5))
    {
        return Err(DriveError::IntegrityMismatch);
    }
    Ok(())
}

fn is_same_revision(file: &DriveFile, revision: i64, hash: &str, size: u64) -> bool {
    file.app_properties
        .get("rb_revision")
        .is_some_and(|value| value == &revision.to_string())
        && file
            .app_properties
            .get("rb_hash")
            .is_some_and(|value| value.eq_ignore_ascii_case(hash))
        && file
            .size
            .as_deref()
            .and_then(|value| value.parse::<u64>().ok())
            == Some(size)
}

fn validate_remote_file(file: &DriveFile) -> Result<(), DriveError> {
    if !valid_identifier(&file.id) {
        return Err(DriveError::InvalidResponse);
    }
    Ok(())
}

fn validate_current_file_identity(
    file: &DriveFile,
    task_id: &str,
    source_file_id: i64,
) -> Result<(), DriveError> {
    let source_file_marker = source_file_id.to_string();
    if !valid_identifier(&file.id)
        || file.mime_type == FOLDER_MIME_TYPE
        || file.app_properties.get("rb_task").map(String::as_str) != Some(task_id)
        || file.app_properties.get("rb_file").map(String::as_str)
            != Some(source_file_marker.as_str())
        || file.app_properties.get("rb_role").map(String::as_str) != Some("current")
    {
        return Err(DriveError::InvalidResponse);
    }
    Ok(())
}

fn status_error(response: &ApiResponse) -> DriveError {
    if response.status == 0 {
        DriveError::Network
    } else {
        DriveError::HttpStatus(response.status)
    }
}

fn is_retryable(response: &ApiResponse) -> bool {
    if response.status == 429 || response.status >= 500 {
        return true;
    }
    if response.status != 403 {
        return false;
    }
    let text = String::from_utf8_lossy(&response.body).to_ascii_lowercase();
    text.contains("ratelimitexceeded")
        || text.contains("userratelimitexceeded")
        || text.contains("quotaexceeded")
}

fn retry_delay(attempt: u32, retry_after: Option<&String>) {
    let delay = retry_after
        .and_then(|value| {
            value
                .parse::<u64>()
                .map(Duration::from_secs)
                .ok()
                .or_else(|| {
                    httpdate::parse_http_date(value).ok().map(|time| {
                        time.duration_since(std::time::SystemTime::now())
                            .unwrap_or_default()
                    })
                })
        })
        .map(|duration| duration.min(Duration::from_secs(30)))
        .unwrap_or_else(|| Duration::from_millis(50_u64.saturating_mul(1_u64 << attempt.min(6))));
    thread::sleep(delay);
}

fn validate_drive_endpoint(value: &str, expected_path: &str) -> Result<(), DriveError> {
    let url = Url::parse(value).map_err(|_| DriveError::InvalidConfiguration)?;
    let secure = url.scheme() == "https" && url.host_str() == Some("www.googleapis.com");
    let local_test = cfg!(test)
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if (!secure && !local_test)
        || url.path().trim_end_matches('/') != expected_path
        || (secure && url.port().is_some())
        || url.host_str().is_none()
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(DriveError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_session_uri(value: &str) -> Result<(), DriveError> {
    let url = Url::parse(value).map_err(|_| DriveError::InvalidResponse)?;
    let path = url.path();
    let google = url.scheme() == "https"
        && url.host_str() == Some("www.googleapis.com")
        && (path == "/upload/drive/v3/files" || path.starts_with("/upload/drive/v3/files/"))
        && url.port().is_none()
        && url.fragment().is_none();
    let local = cfg!(test)
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    if (!google && !local) || url.username() != "" || url.password().is_some() {
        return Err(DriveError::InvalidResponse);
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn split_relative_file_path(value: &str) -> Result<(Vec<String>, String, String), DriveError> {
    if value.is_empty()
        || value.len() > 4096
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.chars().any(char::is_control)
    {
        return Err(DriveError::InvalidConfiguration);
    }
    let parts = value.split(['/', '\\']).collect::<Vec<_>>();
    if parts.iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || part.contains(':')
            || !valid_drive_name(part)
    }) {
        return Err(DriveError::InvalidConfiguration);
    }
    let file_name = parts
        .last()
        .copied()
        .ok_or(DriveError::InvalidConfiguration)?
        .to_owned();
    let directories = parts[..parts.len() - 1]
        .iter()
        .map(|part| (*part).to_owned())
        .collect();
    let normalized_path = parts.join("/");
    Ok((directories, file_name, normalized_path))
}

fn valid_drive_name(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
}

fn quote_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

fn encode_path_segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

fn upload_session_key(account_id: &str, task_id: &str, file_id: i64, revision: i64) -> String {
    format!("upload:{account_id}:{task_id}:{file_id}:{revision}")
}

fn truncate_property(value: &str) -> String {
    value.chars().take(120).collect()
}

fn truncate_drive_name(value: &str) -> String {
    value.chars().take(240).collect()
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::thread::JoinHandle;

    use super::*;
    use crate::oauth::mock_oauth_client;

    #[test]
    fn drive_endpoints_and_upload_sessions_reject_untrusted_hosts() {
        assert!(
            validate_drive_endpoint("https://www.googleapis.com:8443/drive/v3", "/drive/v3")
                .is_err()
        );
        assert!(validate_session_uri(
            "https://attacker.googleapis.com/upload/drive/v3/files?upload_id=x"
        )
        .is_err());
        assert!(validate_session_uri(
            "https://www.googleapis.com:8443/upload/drive/v3/files?upload_id=x"
        )
        .is_err());
        assert!(validate_session_uri("https://www.googleapis.com/other/path?upload_id=x").is_err());
        assert!(validate_session_uri(
            "https://www.googleapis.com/upload/drive/v3/files-evil?upload_id=x"
        )
        .is_err());
        assert!(validate_session_uri(
            "https://www.googleapis.com/upload/drive/v3/files?upload_id=x"
        )
        .is_ok());
    }

    #[test]
    fn property_lookup_reads_every_drive_page() {
        let (endpoints, server) = mock_server(2, |index, request| {
            assert_eq!(request.method, "GET");
            if index == 0 {
                assert!(!request.target.contains("pageToken"));
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "files": [{"id": "remote-a", "name": "a.txt"}],
                        "nextPageToken": "page-2"
                    }),
                )
            } else {
                assert!(request.target.contains("pageToken=page-2"));
                MockResponse::json(
                    200,
                    serde_json::json!({"files": [{"id": "remote-b", "name": "b.txt"}]}),
                )
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let files = client
            .list_by_properties(&[("rb_task", "task-1".to_owned())])
            .unwrap();
        server.join().unwrap();
        assert_eq!(
            files
                .iter()
                .map(|file| file.id.as_str())
                .collect::<Vec<_>>(),
            ["remote-a", "remote-b"]
        );
    }

    #[test]
    fn history_deletion_checks_manifest_markers_and_parent_folder() {
        let version = DriveHistoryVersion {
            manifest_id: "drive:5:2".to_owned(),
            task_id: "task-1".to_owned(),
            file_id: 5,
            source_revision: 2,
            drive_file_id: "history-file".to_owned(),
            account_id: "account-123".to_owned(),
            relative_path_key: "docs/readme.txt".to_owned(),
            display_relative_path: "Docs/Readme.txt".to_owned(),
            content_hash: "a".repeat(64),
            size_bytes: 12,
            created_at_unix_ms: 1,
        };
        let (endpoints, server) = mock_server(2, |index, request| {
            if index == 0 {
                assert_eq!(request.method, "GET");
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "history-file",
                        "name": "Docs/Readme.txt (revision 2)",
                        "mimeType": "text/plain",
                        "size": "12",
                        "parents": ["history-folder"],
                        "appProperties": {
                            "rb_role": "history",
                            "rb_task": "task-1",
                            "rb_file": "5",
                            "rb_revision": "2",
                            "rb_version": "5:2",
                            "rb_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                            "rb_size": "12"
                        }
                    }),
                )
            } else {
                assert_eq!(request.method, "DELETE");
                assert!(request.target.starts_with("/drive/v3/files/history-file"));
                MockResponse::raw(204, Vec::<u8>::new())
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        assert!(client
            .delete_history_version(&version, "history-folder")
            .unwrap());
        server.join().unwrap();
    }

    #[test]
    fn history_deletion_refuses_a_file_outside_the_managed_history_folder() {
        let version = DriveHistoryVersion {
            manifest_id: "drive:5:2".to_owned(),
            task_id: "task-1".to_owned(),
            file_id: 5,
            source_revision: 2,
            drive_file_id: "history-file".to_owned(),
            account_id: "account-123".to_owned(),
            relative_path_key: "docs/readme.txt".to_owned(),
            display_relative_path: "Docs/Readme.txt".to_owned(),
            content_hash: "a".repeat(64),
            size_bytes: 12,
            created_at_unix_ms: 1,
        };
        let (endpoints, server) = mock_server(1, |_index, request| {
            assert_eq!(request.method, "GET");
            MockResponse::json(
                200,
                serde_json::json!({
                    "id": "history-file",
                    "parents": ["another-folder"],
                    "appProperties": {
                        "rb_role": "history",
                        "rb_task": "task-1",
                        "rb_file": "5",
                        "rb_revision": "2",
                        "rb_version": "5:2",
                        "rb_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "rb_size": "12"
                    }
                }),
            )
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        assert!(matches!(
            client.delete_history_version(&version, "history-folder"),
            Err(DriveError::InvalidResponse)
        ));
        server.join().unwrap();
    }

    #[derive(Default)]
    struct MemoryCredentials(Mutex<HashMap<String, String>>);

    impl CredentialStore for MemoryCredentials {
        fn get(&self, key: &str) -> Result<Option<String>, OAuthError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        fn put(&self, key: &str, value: &str) -> Result<(), OAuthError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }

        fn remove(&self, key: &str) -> Result<(), OAuthError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }
    }

    struct Request {
        method: String,
        target: String,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    }

    struct MockResponse {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl MockResponse {
        fn json(status: u16, body: serde_json::Value) -> Self {
            Self {
                status,
                headers: vec![("Content-Type".to_owned(), "application/json".to_owned())],
                body: serde_json::to_vec(&body).unwrap(),
            }
        }

        fn raw(status: u16, body: impl Into<Vec<u8>>) -> Self {
            Self {
                status,
                headers: vec![(
                    "Content-Type".to_owned(),
                    "application/octet-stream".to_owned(),
                )],
                body: body.into(),
            }
        }

        fn header(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.to_owned(), value.to_owned()));
            self
        }
    }

    fn mock_server<F>(request_count: usize, mut respond: F) -> (DriveEndpoints, JoinHandle<()>)
    where
        F: FnMut(usize, Request) -> MockResponse + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            for index in 0..request_count {
                let (mut stream, _) = listener.accept().unwrap();
                let request = read_request(&mut stream);
                let response = respond(index, request);
                if response.status != 0 {
                    write_response(&mut stream, response);
                }
            }
        });
        let endpoint = format!("http://{address}");
        (
            DriveEndpoints {
                api: format!("{endpoint}/drive/v3"),
                upload: format!("{endpoint}/upload/drive/v3"),
            },
            thread,
        )
    }

    fn test_client(endpoints: DriveEndpoints, credentials: Arc<MemoryCredentials>) -> DriveClient {
        let credential_store: Arc<dyn CredentialStore> = credentials.clone();
        let oauth = mock_oauth_client("account-123", "test-access-token", credential_store.clone());
        DriveClient::new(oauth, credential_store, "account-123", endpoints).unwrap()
    }

    #[test]
    fn folder_ids_are_reused_and_private_to_the_app() {
        let (endpoints, server) = mock_server(6, |index, request| {
            assert_eq!(request.method, if index % 2 == 0 { "GET" } else { "POST" });
            if request.method == "GET" {
                assert!(request.target.starts_with("/drive/v3/files?"));
                assert!(String::from_utf8_lossy(&request.body).is_empty());
                MockResponse::json(200, serde_json::json!({"files": []}))
            } else {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["mimeType"], FOLDER_MIME_TYPE);
                let role = body["appProperties"]["rb_role"].as_str().unwrap();
                let id = match role {
                    "folder:root" => "root-folder",
                    "folder:task" => "task-folder",
                    "folder:history" => "history-folder",
                    _ => panic!("unexpected folder role"),
                };
                let mut response = serde_json::json!({
                    "id": id,
                    "name": body["name"],
                    "mimeType": FOLDER_MIME_TYPE,
                    "appProperties": body["appProperties"],
                });
                if let Some(parents) = body.get("parents") {
                    response["parents"] = parents.clone();
                }
                MockResponse::json(200, response)
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let folders = client
            .ensure_task_folders("work-documents", "RustBackup/Work")
            .unwrap();
        assert_eq!(folders.root_id, "root-folder");
        assert_eq!(folders.task_id, "task-folder");
        assert_eq!(folders.history_id, "history-folder");
        server.join().unwrap();
    }

    #[test]
    fn ensure_task_folders_renames_managed_directory_and_keeps_folder_ids() {
        let (endpoints, server) = mock_server(10, |index, request| match index {
            0..=5 if request.method == "GET" => {
                MockResponse::json(200, serde_json::json!({"files": []}))
            }
            0..=5 => {
                assert_eq!(request.method, "POST");
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let role = body["appProperties"]["rb_role"].as_str().unwrap();
                let id = match role {
                    "folder:root" => "root-folder",
                    "folder:task" => "task-folder",
                    "folder:history" => "history-folder",
                    _ => panic!("unexpected folder role"),
                };
                let mut response = serde_json::json!({
                    "id": id,
                    "name": body["name"],
                    "mimeType": FOLDER_MIME_TYPE,
                    "appProperties": body["appProperties"],
                });
                if let Some(parents) = body.get("parents") {
                    response["parents"] = parents.clone();
                }
                MockResponse::json(200, response)
            }
            6 => MockResponse::json(
                200,
                serde_json::json!({"files": [{
                    "id": "root-folder",
                    "name": "RustBackup",
                    "mimeType": FOLDER_MIME_TYPE,
                    "appProperties": {"rb_task": "__rustbackup__", "rb_role": "folder:root"}
                }]}),
            ),
            7 => MockResponse::json(
                200,
                serde_json::json!({"files": [{
                    "id": "task-folder",
                    "name": "Work",
                    "mimeType": FOLDER_MIME_TYPE,
                    "appProperties": {"rb_task": "work-documents", "rb_role": "folder:task"},
                    "parents": ["root-folder"]
                }]}),
            ),
            8 => {
                assert_eq!(request.method, "PATCH");
                assert!(request.target.starts_with("/drive/v3/files/task-folder?"));
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["name"], "Archive");
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "task-folder",
                        "name": "Archive",
                        "mimeType": FOLDER_MIME_TYPE,
                        "appProperties": {"rb_task": "work-documents", "rb_role": "folder:task"},
                        "parents": ["root-folder"]
                    }),
                )
            }
            9 => MockResponse::json(
                200,
                serde_json::json!({"files": [{
                    "id": "history-folder",
                    "name": ".RustBackupHistory",
                    "mimeType": FOLDER_MIME_TYPE,
                    "appProperties": {"rb_task": "work-documents", "rb_role": "folder:history"},
                    "parents": ["task-folder"]
                }]}),
            ),
            _ => panic!("unexpected Drive request"),
        });

        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let first = client
            .ensure_task_folders("work-documents", "RustBackup/Work")
            .unwrap();
        let renamed = client
            .ensure_task_folders("work-documents", "RustBackup/Archive")
            .unwrap();

        assert_eq!(first, renamed);
        assert_eq!(renamed.task_id, "task-folder");
        server.join().unwrap();
    }

    #[test]
    fn resumable_upload_retries_quota_and_commits_remote_file() {
        let contents = b"new file contents".to_vec();
        let md5 = format!("{:x}", Md5::digest(&contents));
        let hash = blake3::hash(&contents).to_hex().to_string();
        let expected_md5 = md5.clone();
        let expected_hash = hash.clone();
        let expected_contents = contents.clone();
        let (endpoints, server) = mock_server(9, move |index, request| match index {
            0 => {
                assert_eq!(request.method, "GET");
                MockResponse::json(
                    403,
                    serde_json::json!({"error": {"errors": [{"reason": "rateLimitExceeded"}]}}),
                )
            }
            1 | 2 | 4 | 6 => {
                assert_eq!(request.method, "GET");
                MockResponse::json(200, serde_json::json!({"files": []}))
            }
            3 | 5 => {
                assert_eq!(request.method, "POST");
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["mimeType"], FOLDER_MIME_TYPE);
                assert!(body["appProperties"]["rb_role"]
                    .as_str()
                    .unwrap()
                    .starts_with("folder:source-dir:"));
                if index == 3 {
                    assert_eq!(body["name"], "Dept");
                    assert_eq!(body["parents"][0], "task-folder");
                    MockResponse::json(
                        200,
                        serde_json::json!({
                            "id": "source-dir-1",
                            "name": "Dept",
                            "mimeType": FOLDER_MIME_TYPE,
                            "appProperties": body["appProperties"],
                            "parents": ["task-folder"]
                        }),
                    )
                } else {
                    assert_eq!(body["name"], "2026");
                    assert_eq!(body["parents"][0], "source-dir-1");
                    MockResponse::json(
                        200,
                        serde_json::json!({
                            "id": "source-dir-2",
                            "name": "2026",
                            "mimeType": FOLDER_MIME_TYPE,
                            "appProperties": body["appProperties"],
                            "parents": ["source-dir-1"]
                        }),
                    )
                }
            }
            7 => {
                assert_eq!(request.method, "POST");
                assert!(request
                    .target
                    .starts_with("/upload/drive/v3/files?uploadType=resumable"));
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["name"], "source.txt");
                assert_eq!(body["parents"][0], "source-dir-2");
                assert_eq!(body["appProperties"]["rb_path"], "Dept/2026/source.txt");
                assert!(String::from_utf8_lossy(&request.body).contains("rb_revision"));
                MockResponse::json(200, serde_json::json!({})).header(
                    "Location",
                    &format!("http://{}/session/new", request.headers["host"]),
                )
            }
            _ => {
                assert_eq!(request.method, "PUT");
                assert_eq!(request.target, "/session/new");
                assert_eq!(request.body, expected_contents);
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "remote-new",
                        "name": "source.txt",
                        "mimeType": "application/octet-stream",
                        "size": expected_contents.len().to_string(),
                        "md5Checksum": expected_md5,
                        "parents": ["source-dir-2"],
                        "appProperties": {
                            "rb_task": "work-documents",
                            "rb_file": "11",
                            "rb_revision": "1",
                            "rb_hash": expected_hash,
                            "rb_role": "current",
                            "rb_path": "Dept/2026/source.txt"
                        }
                    }),
                )
            }
        });
        let credentials = Arc::new(MemoryCredentials::default());
        let client = test_client(endpoints, credentials.clone());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.txt");
        fs::write(&path, &contents).unwrap();
        let result = client
            .upload_file(
                "work-documents",
                11,
                1,
                "Dept/2026/source.txt",
                &hash,
                &path,
                "task-folder",
                None,
            )
            .unwrap();
        assert_eq!(result.file.id, "remote-new");
        assert_eq!(result.md5_checksum, md5);
        assert_eq!(result.size_bytes, contents.len() as u64);
        assert!(!result.reconciled);
        assert!(credentials.0.lock().unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn source_relative_directories_are_created_and_file_uses_leaf_name() {
        let contents = b"nested cloud file".to_vec();
        let md5 = format!("{:x}", Md5::digest(&contents));
        let hash = blake3::hash(&contents).to_hex().to_string();
        let expected_contents = contents.clone();
        let expected_md5 = md5.clone();
        let expected_hash = hash.clone();
        let (endpoints, server) = mock_server(8, move |index, request| match index {
            0 | 1 | 3 | 5 => {
                assert_eq!(request.method, "GET");
                MockResponse::json(200, serde_json::json!({"files": []}))
            }
            2 | 4 => {
                assert_eq!(request.method, "POST");
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["mimeType"], FOLDER_MIME_TYPE);
                assert!(body["appProperties"]["rb_role"]
                    .as_str()
                    .unwrap()
                    .starts_with("folder:source-dir:"));
                if index == 2 {
                    assert_eq!(body["name"], "Dept");
                    assert_eq!(body["parents"][0], "task-folder");
                    MockResponse::json(
                        200,
                        serde_json::json!({
                            "id": "source-dir-1",
                            "name": "Dept",
                            "mimeType": FOLDER_MIME_TYPE,
                            "appProperties": body["appProperties"],
                            "parents": ["task-folder"]
                        }),
                    )
                } else {
                    assert_eq!(body["name"], "2026");
                    assert_eq!(body["parents"][0], "source-dir-1");
                    MockResponse::json(
                        200,
                        serde_json::json!({
                            "id": "source-dir-2",
                            "name": "2026",
                            "mimeType": FOLDER_MIME_TYPE,
                            "appProperties": body["appProperties"],
                            "parents": ["source-dir-1"]
                        }),
                    )
                }
            }
            6 => {
                assert_eq!(request.method, "POST");
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["name"], "report.txt");
                assert_eq!(body["parents"][0], "source-dir-2");
                assert_eq!(body["appProperties"]["rb_path"], "Dept/2026/report.txt");
                MockResponse::json(200, serde_json::json!({})).header(
                    "Location",
                    &format!("http://{}/session/nested", request.headers["host"]),
                )
            }
            _ => {
                assert_eq!(request.method, "PUT");
                assert_eq!(request.target, "/session/nested");
                assert_eq!(request.body, expected_contents);
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "nested-file",
                        "name": "report.txt",
                        "mimeType": "application/octet-stream",
                        "size": expected_contents.len().to_string(),
                        "md5Checksum": expected_md5,
                        "parents": ["source-dir-2"],
                        "appProperties": {
                            "rb_task": "work-documents",
                            "rb_file": "11",
                            "rb_revision": "1",
                            "rb_hash": expected_hash,
                            "rb_role": "current",
                            "rb_path": "Dept/2026/report.txt"
                        }
                    }),
                )
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("report.txt");
        fs::write(&path, &contents).unwrap();
        let result = client
            .upload_file(
                "work-documents",
                11,
                1,
                "Dept/2026/report.txt",
                &hash,
                &path,
                "task-folder",
                None,
            )
            .unwrap();
        assert_eq!(result.file.name, "report.txt");
        assert_eq!(result.file.parents, ["source-dir-2"]);
        server.join().unwrap();
    }

    #[test]
    fn flattened_current_file_is_moved_into_its_source_directory() {
        let contents = b"unchanged file".to_vec();
        let hash = blake3::hash(&contents).to_hex().to_string();
        let expected_hash = hash.clone();
        let size_bytes = contents.len() as u64;
        let md5_checksum = format!("{:x}", Md5::digest(&contents));
        let expected_md5 = md5_checksum.clone();
        let remote_file = serde_json::json!({
            "id": "legacy-flat",
            "name": "Dept\\report.txt",
            "mimeType": "application/octet-stream",
            "size": size_bytes.to_string(),
            "md5Checksum": md5_checksum,
            "parents": ["task-folder"],
            "appProperties": {
                "rb_task": "work-documents",
                "rb_file": "11",
                "rb_revision": "3",
                "rb_hash": hash,
                "rb_role": "current",
                "rb_path": "Dept/report.txt"
            }
        });
        let path_marker = "Dept/report.txt".to_owned();
        let (endpoints, server) = mock_server(4, move |index, request| match index {
            0 => {
                assert_eq!(request.method, "GET");
                assert!(request.target.starts_with("/drive/v3/files/legacy-flat?"));
                MockResponse::json(200, remote_file.clone())
            }
            1 => {
                assert_eq!(request.method, "GET");
                MockResponse::json(200, serde_json::json!({"files": []}))
            }
            2 => {
                assert_eq!(request.method, "POST");
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["name"], "Dept");
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "nested-folder",
                        "name": "Dept",
                        "mimeType": FOLDER_MIME_TYPE,
                        "appProperties": body["appProperties"],
                        "parents": ["task-folder"]
                    }),
                )
            }
            _ => {
                assert_eq!(request.method, "PATCH");
                assert!(request.target.contains("addParents=nested-folder"));
                assert!(request.target.contains("removeParents=task-folder"));
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["name"], "report.txt");
                assert_eq!(body["appProperties"]["rb_path"], path_marker);
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "legacy-flat",
                        "name": "report.txt",
                        "mimeType": "application/octet-stream",
                        "size": size_bytes.to_string(),
                        "md5Checksum": expected_md5,
                        "parents": ["nested-folder"],
                        "appProperties": {
                            "rb_task": "work-documents",
                            "rb_file": "11",
                            "rb_revision": "3",
                            "rb_hash": expected_hash,
                            "rb_role": "current",
                            "rb_path": "Dept/report.txt"
                        }
                    }),
                )
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let moved = client
            .ensure_current_file_path(
                "work-documents",
                11,
                "Dept/report.txt",
                "task-folder",
                "legacy-flat",
            )
            .unwrap();
        assert_eq!(moved.name, "report.txt");
        assert_eq!(moved.parents, ["nested-folder"]);
        server.join().unwrap();
    }

    #[test]
    fn refuses_to_move_current_file_out_of_an_unmanaged_parent() {
        let (endpoints, server) = mock_server(2, |index, request| match index {
            0 => {
                assert_eq!(request.method, "GET");
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "managed-current",
                        "name": "report.txt",
                        "mimeType": "application/octet-stream",
                        "parents": ["foreign-folder"],
                        "appProperties": {
                            "rb_task": "work-documents",
                            "rb_file": "11",
                            "rb_role": "current"
                        }
                    }),
                )
            }
            _ => {
                assert_eq!(request.method, "GET");
                assert!(request
                    .target
                    .starts_with("/drive/v3/files/foreign-folder?"));
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "foreign-folder",
                        "name": "User folder",
                        "mimeType": FOLDER_MIME_TYPE,
                        "appProperties": {
                            "rb_task": "another-task",
                            "rb_role": "folder:source-dir:foreign"
                        }
                    }),
                )
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        assert!(matches!(
            client.ensure_current_file_path(
                "work-documents",
                11,
                "Dept/report.txt",
                "task-folder",
                "managed-current"
            ),
            Err(DriveError::InvalidResponse)
        ));
        server.join().unwrap();
    }

    #[test]
    fn relative_path_validation_blocks_rooted_and_traversal_paths() {
        let (directories, file_name, normalized) =
            split_relative_file_path("Dept\\2026/report.txt").unwrap();
        assert_eq!(directories, ["Dept", "2026"]);
        assert_eq!(file_name, "report.txt");
        assert_eq!(normalized, "Dept/2026/report.txt");

        for invalid in [
            "",
            "/report.txt",
            "\\report.txt",
            "C:\\report.txt",
            "Dept/../report.txt",
            "Dept//report.txt",
        ] {
            assert!(matches!(
                split_relative_file_path(invalid),
                Err(DriveError::InvalidConfiguration)
            ));
        }
    }

    #[test]
    fn nested_folder_ids_are_cached_and_reused_within_a_backup_run() {
        let (endpoints, server) = mock_server(2, |index, request| match index {
            0 => {
                assert_eq!(request.method, "GET");
                MockResponse::json(200, serde_json::json!({"files": []}))
            }
            _ => {
                assert_eq!(request.method, "POST");
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                MockResponse::json(
                    200,
                    serde_json::json!({
                        "id": "cached-folder",
                        "name": body["name"],
                        "mimeType": FOLDER_MIME_TYPE,
                        "appProperties": body["appProperties"],
                        "parents": ["task-folder"]
                    }),
                )
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let directory_parts = vec!["Dept".to_owned()];
        let first = client
            .ensure_relative_folders("work-documents", &directory_parts, "task-folder")
            .unwrap();
        let second = client
            .ensure_relative_folders("work-documents", &directory_parts, "task-folder")
            .unwrap();
        assert_eq!(first, "cached-folder");
        assert_eq!(second, first);
        server.join().unwrap();
    }

    #[test]
    fn update_resumes_session_and_reconciles_uncertain_completion() {
        let contents = b"hello drive".to_vec();
        let md5 = format!("{:x}", Md5::digest(&contents));
        let hash = blake3::hash(&contents).to_hex().to_string();
        let expected_md5 = md5.clone();
        let expected_hash = hash.clone();
        let expected_contents = contents.clone();
        let (endpoints, server) = mock_server(3, move |index, request| match index {
            0 => MockResponse::json(
                200,
                serde_json::json!({
                    "id":"remote-current",
                    "name":"doc.txt",
                    "size":"3",
                    "md5Checksum":"old",
                    "parents":["task-folder"],
                    "appProperties":{
                        "rb_task":"work-documents",
                        "rb_file":"11",
                        "rb_revision":"1",
                        "rb_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "rb_role":"current",
                        "rb_path":"doc.txt"
                    }
                }),
            ),
            1 => {
                assert_eq!(request.method, "PUT");
                assert_eq!(request.headers.get("content-range").unwrap(), "bytes */11");
                MockResponse::raw(308, Vec::new()).header("Range", "bytes=0-3")
            }
            _ => {
                assert_eq!(request.method, "PUT");
                assert_eq!(
                    request.headers.get("content-range").unwrap(),
                    "bytes 4-10/11"
                );
                assert_eq!(request.body, expected_contents[4..]);
                MockResponse::json(
                    200,
                    file_json(
                        "remote-current",
                        &expected_hash,
                        &expected_md5,
                        expected_contents.len() as u64,
                        2,
                    ),
                )
            }
        });
        let credentials = Arc::new(MemoryCredentials::default());
        credentials
            .put(
                "upload:account-123:work-documents:11:2",
                &format!("http://{}/session/resume", endpoint_host(&endpoints)),
            )
            .unwrap();
        let client = test_client(endpoints, credentials.clone());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.txt");
        fs::write(&path, &contents).unwrap();
        let result = client
            .upload_file(
                "work-documents",
                11,
                2,
                "doc.txt",
                &hash,
                &path,
                "task-folder",
                Some("remote-current"),
            )
            .unwrap();
        assert_eq!(result.file.id, "remote-current");
        assert_eq!(result.file.app_properties.get("rb_revision").unwrap(), "2");
        assert!(credentials.0.lock().unwrap().is_empty());
        server.join().unwrap();

        let reconciled_hash = hash.clone();
        let reconciled_md5 = md5.clone();
        let (endpoints, server) = mock_server(1, move |_index, _request| {
            MockResponse::json(
                200,
                serde_json::json!({
                    "id":"remote-current",
                    "name":"doc.txt",
                    "size":"11",
                    "md5Checksum":reconciled_md5,
                    "parents":["task-folder"],
                    "appProperties":{
                        "rb_task":"work-documents",
                        "rb_file":"11",
                        "rb_revision":"2",
                        "rb_hash":reconciled_hash,
                        "rb_role":"current",
                        "rb_path":"doc.txt"
                    }
                }),
            )
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let result = client
            .upload_file(
                "work-documents",
                11,
                2,
                "doc.txt",
                &hash,
                &path,
                "task-folder",
                Some("remote-current"),
            )
            .unwrap();
        assert!(result.reconciled);
        server.join().unwrap();
    }

    #[test]
    fn expired_resumable_session_is_rebuilt_without_creating_a_second_file() {
        let contents = b"updated".to_vec();
        let md5 = format!("{:x}", Md5::digest(&contents));
        let hash = blake3::hash(&contents).to_hex().to_string();
        let expected_md5 = md5.clone();
        let expected_hash = hash.clone();
        let expected_contents = contents.clone();
        let (endpoints, server) = mock_server(4, move |index, request| match index {
            0 => MockResponse::json(
                200,
                serde_json::json!({
                    "id":"remote-current",
                    "name":"doc.txt",
                    "size":"3",
                    "parents":["task-folder"],
                    "appProperties":{
                        "rb_task":"work-documents",
                        "rb_file":"11",
                        "rb_revision":"1",
                        "rb_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "rb_role":"current",
                        "rb_path":"doc.txt"
                    }
                }),
            ),
            1 => {
                assert_eq!(request.method, "PUT");
                MockResponse::raw(410, Vec::new())
            }
            2 => {
                assert_eq!(request.method, "PATCH");
                MockResponse::json(200, serde_json::json!({})).header(
                    "Location",
                    &format!("http://{}/session/rebuilt", request.headers["host"]),
                )
            }
            _ => {
                assert_eq!(request.method, "PUT");
                assert_eq!(request.target, "/session/rebuilt");
                assert_eq!(request.body, expected_contents);
                MockResponse::json(
                    200,
                    file_json(
                        "remote-current",
                        &expected_hash,
                        &expected_md5,
                        expected_contents.len() as u64,
                        2,
                    ),
                )
            }
        });
        let credentials = Arc::new(MemoryCredentials::default());
        credentials
            .put(
                "upload:account-123:work-documents:11:2",
                &format!("http://{}/session/expired", endpoint_host(&endpoints)),
            )
            .unwrap();
        let client = test_client(endpoints, credentials.clone());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.txt");
        fs::write(&path, &contents).unwrap();
        let result = client
            .upload_file(
                "work-documents",
                11,
                2,
                "doc.txt",
                &hash,
                &path,
                "task-folder",
                Some("remote-current"),
            )
            .unwrap();
        assert_eq!(result.file.id, "remote-current");
        assert!(credentials.0.lock().unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn previous_remote_revision_is_copied_once_into_history() {
        let old_hash = "a".repeat(64);
        let request_hash = old_hash.clone();
        let (endpoints, server) = mock_server(3, move |index, request| match index {
            0 => MockResponse::json(
                200,
                serde_json::json!({"id":"remote-current","name":"doc.txt","size":"4","appProperties":{"rb_revision":"3","rb_hash":request_hash}}),
            ),
            1 => MockResponse::json(200, serde_json::json!({"files": []})),
            _ => {
                assert_eq!(request.method, "POST");
                assert!(request.target.contains("/files/remote-current/copy?"));
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                assert_eq!(body["parents"][0], "history-folder");
                assert_eq!(body["appProperties"]["rb_version"], "21:3");
                MockResponse::json(
                    200,
                    serde_json::json!({"id":"history-copy","name":"doc.txt (revision 3)","size":"4","appProperties":body["appProperties"]}),
                )
            }
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let archived = client
            .preserve_current_revision(
                "work-documents",
                21,
                3,
                "doc.txt",
                "remote-current",
                "history-folder",
            )
            .unwrap();
        assert_eq!(archived.file.id, "history-copy");
        assert_eq!(archived.source_revision, 3);
        assert_eq!(archived.content_hash, old_hash);
        assert_eq!(archived.size_bytes, 4);
        server.join().unwrap();
    }

    #[test]
    fn uncertain_chunk_completion_is_reconciled_before_any_resend() {
        let contents = b"hello".to_vec();
        let md5 = format!("{:x}", Md5::digest(&contents));
        let hash = blake3::hash(&contents).to_hex().to_string();
        let expected_hash = hash.clone();
        let expected_md5 = md5.clone();
        let (endpoints, server) = mock_server(4, move |index, request| match index {
            0 => MockResponse::json(
                200,
                serde_json::json!({
                    "id":"remote-current",
                    "name":"doc.txt",
                    "size":"3",
                    "parents":["task-folder"],
                    "appProperties":{
                        "rb_task":"work-documents",
                        "rb_file":"11",
                        "rb_revision":"1",
                        "rb_hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "rb_role":"current",
                        "rb_path":"doc.txt"
                    }
                }),
            ),
            1 => MockResponse::raw(308, Vec::new()).header("Range", "bytes=0-1"),
            2 => {
                assert_eq!(request.headers.get("content-range").unwrap(), "bytes 2-4/5");
                assert_eq!(request.body, b"llo");
                // The server accepted the body, then the response was lost.
                MockResponse::raw(0, Vec::new())
            }
            _ => MockResponse::json(
                200,
                file_json("remote-current", &expected_hash, &expected_md5, 5, 2),
            ),
        });
        let credentials = Arc::new(MemoryCredentials::default());
        credentials
            .put(
                "upload:account-123:work-documents:11:2",
                &format!("http://{}/session/uncertain", endpoint_host(&endpoints)),
            )
            .unwrap();
        let client = test_client(endpoints, credentials.clone());
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.txt");
        fs::write(&path, &contents).unwrap();
        let result = client
            .upload_file(
                "work-documents",
                11,
                2,
                "doc.txt",
                &hash,
                &path,
                "task-folder",
                Some("remote-current"),
            )
            .unwrap();
        assert!(result.reconciled);
        assert_eq!(result.file.id, "remote-current");
        assert!(credentials.0.lock().unwrap().is_empty());
        server.join().unwrap();
    }

    #[test]
    fn cloud_restore_verifies_bytes_before_publishing() {
        let contents = b"verified cloud history".to_vec();
        let expected_hash = blake3::hash(&contents).to_hex().to_string();
        let response_body = contents.clone();
        let (endpoints, server) = mock_server(1, move |_index, request| {
            assert_eq!(request.method, "GET");
            assert_eq!(request.target, "/drive/v3/files/history-file?alt=media");
            MockResponse::raw(200, response_body.clone())
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("restored.txt");
        client
            .download_verified_to(
                "history-file",
                &expected_hash,
                contents.len() as u64,
                &target,
            )
            .unwrap();
        assert_eq!(fs::read(&target).unwrap(), contents);
        server.join().unwrap();

        let (endpoints, server) = mock_server(1, move |_index, _request| {
            MockResponse::raw(200, b"altered".to_vec())
        });
        let client = test_client(endpoints, Arc::new(MemoryCredentials::default()));
        let target = temp.path().join("bad-restore.txt");
        assert!(matches!(
            client.download_verified_to(
                "history-file",
                &expected_hash,
                contents.len() as u64,
                &target
            ),
            Err(DriveError::IntegrityMismatch)
        ));
        assert!(!target.exists());
        server.join().unwrap();
    }

    fn file_json(id: &str, hash: &str, md5: &str, size: u64, revision: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": "doc.txt",
            "mimeType": "application/octet-stream",
            "size": size.to_string(),
            "md5Checksum": md5,
            "parents": ["task-folder"],
            "appProperties": {
                "rb_task": "work-documents",
                "rb_file": "11",
                "rb_revision": revision.to_string(),
                "rb_hash": hash,
                "rb_role": "current",
                "rb_path": "doc.txt"
            },
        })
    }

    fn endpoint_host(endpoints: &DriveEndpoints) -> String {
        Url::parse(&endpoints.api)
            .unwrap()
            .host_str()
            .unwrap()
            .to_owned()
            + ":"
            + Url::parse(&endpoints.api)
                .unwrap()
                .port()
                .unwrap()
                .to_string()
                .as_str()
    }

    fn read_request(stream: &mut TcpStream) -> Request {
        let mut raw = Vec::new();
        let mut byte = [0_u8; 1];
        while !raw.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            raw.push(byte[0]);
        }
        let header_length = raw.len();
        let header = String::from_utf8_lossy(&raw);
        let mut lines = header.lines();
        let mut request_line = lines.next().unwrap().split_ascii_whitespace();
        let method = request_line.next().unwrap().to_owned();
        let target = request_line.next().unwrap().to_owned();
        let headers = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
            .collect::<HashMap<_, _>>();
        let body_length = headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        raw.resize(header_length + body_length, 0);
        stream.read_exact(&mut raw[header_length..]).unwrap();
        Request {
            method,
            target,
            headers,
            body: raw[header_length..].to_vec(),
        }
    }

    fn write_response(stream: &mut TcpStream, response: MockResponse) {
        let reason = match response.status {
            200 => "OK",
            201 => "Created",
            308 => "Resume Incomplete",
            404 => "Not Found",
            410 => "Gone",
            429 => "Too Many Requests",
            503 => "Service Unavailable",
            _ => "Error",
        };
        write!(
            stream,
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            response.status,
            reason,
            response.body.len()
        )
        .unwrap();
        for (name, value) in response.headers {
            write!(stream, "{name}: {value}\r\n").unwrap();
        }
        write!(stream, "\r\n").unwrap();
        stream.write_all(&response.body).unwrap();
    }
}
