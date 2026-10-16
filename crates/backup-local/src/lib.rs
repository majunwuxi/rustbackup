//! Safe local snapshot and mirror updates. The source is never modified and
//! destination deletions are deliberately not propagated.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use backup_core::{
    validate_task_paths, BackupMode, BackupTask, FilterSet, PathValidationError, ScanError,
    Scanner, SourceUnavailablePolicy, HISTORY_RETENTION_DAYS,
};
use backup_drive::{DriveClient, DriveError, OAuthError};
use backup_storage::{
    BackupRunStatus, CommitOutcome, DriveHistoryRecord, DriveHistoryVersion, DriveTaskBinding,
    FileObservation, HistoryRepository, IndexedFile, IndexedFileState, StateStore, StorageError,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tempfile::NamedTempFile;
use thiserror::Error;

mod drive_restore;
pub use drive_restore::{restore_drive_history_version, DriveRestoreError};

const SNAPSHOT_ATTEMPTS: usize = 3;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_REPORTED_ISSUES: usize = 100;
const DRIVE_HISTORY_GC_BATCH: usize = 256;
const DRIVE_HISTORY_GC_LEASE_MS: i64 = 30 * 60 * 1_000;
const BACKUP_PROGRESS_FILE_BATCH: u64 = 25;
const BACKUP_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

trait DriveHistoryDeleter {
    fn delete_history_version(
        &self,
        version: &DriveHistoryVersion,
        history_folder_id: &str,
    ) -> Result<bool, DriveError>;
}

impl DriveHistoryDeleter for DriveClient {
    fn delete_history_version(
        &self,
        version: &DriveHistoryVersion,
        history_folder_id: &str,
    ) -> Result<bool, DriveError> {
        DriveClient::delete_history_version(self, version, history_folder_id)
    }
}

#[derive(Debug, Error)]
pub enum LocalBackupError {
    #[error(transparent)]
    Paths(#[from] PathValidationError),
    #[error(transparent)]
    Scan(#[from] ScanError),
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("local destination root must already exist; it is created only after user confirmation: {0}")]
    DestinationMissing(PathBuf),
    #[error("local destination is not a regular directory: {0}")]
    DestinationInvalid(PathBuf),
    #[error("local destination contains an unsupported reparse point: {0}")]
    DestinationReparsePoint(PathBuf),
    #[error("destination path contains an unsafe component: {0}")]
    UnsafeDestinationPath(PathBuf),
    #[error("destination path is occupied by a non-file: {0}")]
    DestinationTargetInvalid(PathBuf),
    #[error("destination contains an untracked file that cannot be overwritten safely: {0}")]
    UntrackedDestinationTarget(PathBuf),
    #[error("backup mode {0} is not implemented by the local M1 service")]
    UnsupportedMode(String),
    #[error("task {0} is not enabled")]
    TaskDisabled(String),
    #[error("filesystem operation failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("source snapshot did not remain stable: {0}")]
    UnstableSource(PathBuf),
    #[error("source snapshot checksum changed after scan: {0}")]
    SnapshotHashMismatch(PathBuf),
    #[error("local mirror path is unsafe or unavailable for Drive upload: {0}")]
    UnsafeMirrorFile(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupSummary {
    pub run_id: String,
    pub status: String,
    pub scanned_files: u64,
    pub copied_files: u64,
    pub source_missing_files: u64,
    pub cloud_pending_files: u64,
    pub cloud_uploaded_files: u64,
    pub cloud_history_versions_created: u64,
    pub cloud_history_versions_removed: u64,
    pub history_versions_removed: u64,
    pub history_objects_removed: u64,
    pub deletion_review_required: bool,
    pub issues: Vec<String>,
    pub omitted_issue_count: u64,
}

impl BackupSummary {
    fn new(run_id: String) -> Self {
        Self {
            run_id,
            status: "running".to_owned(),
            scanned_files: 0,
            copied_files: 0,
            source_missing_files: 0,
            cloud_pending_files: 0,
            cloud_uploaded_files: 0,
            cloud_history_versions_created: 0,
            cloud_history_versions_removed: 0,
            history_versions_removed: 0,
            history_objects_removed: 0,
            deletion_review_required: false,
            issues: Vec::new(),
            omitted_issue_count: 0,
        }
    }

    fn add_issue(&mut self, issue: impl Into<String>) {
        if self.issues.len() < MAX_REPORTED_ISSUES {
            self.issues.push(issue.into());
        } else {
            self.omitted_issue_count += 1;
        }
    }
}

/// Runs a complete reconciliation and commits each stable file independently.
/// The destination root must exist already; this function never creates it.
pub fn backup_once(
    store: &mut StateStore,
    task: &BackupTask,
) -> Result<BackupSummary, LocalBackupError> {
    validate_mode(task)?;
    if !task.enabled {
        return Err(LocalBackupError::TaskDisabled(task.id.clone()));
    }

    store.register_task(&task.id, &task.name, task.enabled)?;
    let run_id = store.begin_backup_run(&task.id)?;
    let result = run_reconciliation(store, task, run_id.clone());
    match result {
        Ok(mut summary) => {
            summary.status = if !summary.issues.is_empty()
                || summary.omitted_issue_count > 0
                || summary.deletion_review_required
                || summary.cloud_pending_files > 0
            {
                "partial_success".to_owned()
            } else {
                "succeeded".to_owned()
            };
            let run_status = if summary.status == "succeeded" {
                BackupRunStatus::Succeeded
            } else {
                BackupRunStatus::PartialSuccess
            };
            store.finish_backup_run(
                &summary.run_id,
                run_status,
                summary.scanned_files,
                summary.copied_files,
                summary_error_text(&summary).as_deref(),
            )?;
            Ok(summary)
        }
        Err(error) => {
            let status = if should_pause_for_source(task, &error) {
                BackupRunStatus::Paused
            } else {
                BackupRunStatus::Error
            };
            store.fail_backup_run(&run_id, status, &error.to_string())?;
            Err(error)
        }
    }
}

/// Runs the local backup first, then reconciles current mirror files to Drive.
/// With no authorized Drive client the local branch still completes and the
/// overall result remains `waiting_auth`/partial.
pub fn backup_once_with_drive(
    store: &mut StateStore,
    task: &BackupTask,
    drive: Option<&DriveClient>,
) -> Result<BackupSummary, LocalBackupError> {
    let mut summary = backup_once(store, task)?;
    if task.mode != BackupMode::Dual || !task.destinations.google_drive.enabled {
        return Ok(summary);
    }

    let Some(drive) = drive else {
        summary.status = "waiting_auth".to_owned();
        summary.add_issue("Google Drive authorization is required; the local backup completed");
        finish_cloud_summary(store, &summary)?;
        return Ok(summary);
    };

    let prior_binding = store.drive_task_binding(&task.id)?;
    if task
        .destinations
        .google_drive
        .account_id
        .as_deref()
        .is_some_and(|account| account != drive.account_id())
        || prior_binding
            .as_ref()
            .is_some_and(|binding| binding.account_id != drive.account_id())
    {
        summary.add_issue("configured Google account does not match the authorized account");
        finish_cloud_summary(store, &summary)?;
        return Ok(summary);
    }
    if task.destinations.google_drive.folder_id.is_some() && prior_binding.is_none() {
        summary.add_issue(
            "configured Drive folder ID is not bound to this task; cloud uploads are paused",
        );
        finish_cloud_summary(store, &summary)?;
        return Ok(summary);
    }
    if prior_binding.as_ref().is_some_and(|binding| {
        task.destinations
            .google_drive
            .folder_id
            .as_deref()
            .is_some_and(|folder| folder != binding.task_folder_id)
    }) {
        summary.add_issue("configured Google Drive folder does not match the saved task binding");
        finish_cloud_summary(store, &summary)?;
        return Ok(summary);
    }

    if let Err(error) = drive.check_authorized() {
        if matches!(
            error,
            DriveError::OAuth(OAuthError::WaitingForAuthorization)
        ) {
            summary.status = "waiting_auth".to_owned();
            summary.add_issue("Google Drive authorization is required; the local backup completed");
        } else {
            summary.add_issue(format!("Google Drive authorization failed: {error}"));
        }
        finish_cloud_summary(store, &summary)?;
        return Ok(summary);
    }

    let folders =
        match drive.ensure_task_folders(&task.id, &task.destinations.google_drive.folder_name) {
            Ok(folders) => folders,
            Err(error) => {
                summary.add_issue(format!("Google Drive folder setup failed: {error}"));
                finish_cloud_summary(store, &summary)?;
                return Ok(summary);
            }
        };
    let binding = DriveTaskBinding {
        task_id: task.id.clone(),
        account_id: drive.account_id().to_owned(),
        root_folder_id: folders.root_id,
        task_folder_id: folders.task_id,
        history_folder_id: folders.history_id,
        folder_name: task.destinations.google_drive.folder_name.clone(),
    };
    if let Err(error) = store.bind_drive_task(&binding) {
        summary.add_issue(format!("Google Drive binding could not be saved: {error}"));
        finish_cloud_summary(store, &summary)?;
        return Ok(summary);
    }

    summary.cloud_pending_files = 0;
    for file in store.indexed_files(&task.id)? {
        if store.destination_revision(file.file_id, "drive")? >= file.source_revision {
            continue;
        }
        summary.cloud_pending_files += 1;
        let Some(content_hash) = file.content_hash.as_deref() else {
            summary.add_issue(format!(
                "{}: source content hash is missing",
                file.display_relative_path
            ));
            continue;
        };
        let Some(size_bytes) = file.size_bytes else {
            summary.add_issue(format!(
                "{}: source file size is missing",
                file.display_relative_path
            ));
            continue;
        };
        if store.destination_revision(file.file_id, "local")? < file.source_revision {
            summary.add_issue(format!(
                "{}: verified local mirror is not current",
                file.display_relative_path
            ));
            continue;
        }
        let mirror_path =
            match safe_mirror_path(&task.destinations.local.path, &file.display_relative_path) {
                Ok(path) => path,
                Err(error) => {
                    summary.add_issue(format!("{}: {error}", file.display_relative_path));
                    continue;
                }
            };
        let cloud_state = store.destination_state(file.file_id, "drive")?;
        let existing_drive_file_id = cloud_state
            .as_ref()
            .and_then(|state| state.target_file_id.as_deref());
        let job_id = store.enqueue_drive_upload(file.file_id, file.source_revision)?;
        if let Err(error) = store.start_job(&job_id) {
            summary.add_issue(format!(
                "{}: Drive upload queue could not start: {error}",
                file.display_relative_path
            ));
            continue;
        }

        let upload_result = (|| -> Result<_, DriveError> {
            if task.versioning.enabled {
                if let Some(previous) = cloud_state
                    .as_ref()
                    .filter(|state| state.committed_revision > 0)
                {
                    let previous_drive_id = previous
                        .target_file_id
                        .as_deref()
                        .ok_or(DriveError::InvalidResponse)?;
                    let archived = drive.preserve_current_revision(
                        &task.id,
                        file.file_id,
                        previous.committed_revision,
                        &file.display_relative_path,
                        previous_drive_id,
                        &binding.history_folder_id,
                    )?;
                    let created_at = i64::try_from(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| DriveError::InvalidResponse)?
                            .as_millis(),
                    )
                    .map_err(|_| DriveError::InvalidResponse)?;
                    store
                        .record_drive_history_version(DriveHistoryRecord {
                            task_id: &task.id,
                            file_id: file.file_id,
                            source_revision: archived.source_revision,
                            drive_file_id: &archived.file.id,
                            account_id: drive.account_id(),
                            relative_path_key: &file.relative_path_key,
                            display_relative_path: &file.display_relative_path,
                            content_hash: &archived.content_hash,
                            size_bytes: archived.size_bytes,
                            created_at_unix_ms: created_at,
                        })
                        .map_err(|_| DriveError::InvalidResponse)?;
                    summary.cloud_history_versions_created += 1;
                }
            }
            let cloud_snapshot =
                create_stable_snapshot(&mirror_path, &task.destinations.local.path, content_hash)
                    .map_err(|_| DriveError::LocalFileChanged)?;
            if fs::metadata(cloud_snapshot.path())
                .map_err(|_| DriveError::LocalFileChanged)?
                .len()
                != size_bytes
            {
                return Err(DriveError::LocalFileChanged);
            }
            drive.upload_file(
                &task.id,
                file.file_id,
                file.source_revision,
                &file.display_relative_path,
                content_hash,
                cloud_snapshot.path(),
                &binding.task_folder_id,
                existing_drive_file_id,
            )
        })();

        match upload_result {
            Ok(upload) => {
                let verified = if upload.file.md5_checksum.is_some() {
                    "md5"
                } else {
                    "size_only"
                };
                let checksum_info = format!(
                    "blake3={content_hash};md5={};size={size_bytes};verified={verified}",
                    upload.md5_checksum
                );
                match store.commit_drive_upload_if_current(
                    &job_id,
                    file.file_id,
                    file.source_revision,
                    &checksum_info,
                    &upload.file.id,
                )? {
                    CommitOutcome::Committed => {
                        summary.cloud_pending_files = summary.cloud_pending_files.saturating_sub(1);
                        summary.cloud_uploaded_files += 1;
                    }
                    CommitOutcome::Stale => summary.add_issue(format!(
                        "{}: a newer source revision superseded this Drive upload",
                        file.display_relative_path
                    )),
                }
            }
            Err(error) => {
                let error_text = drive_error_summary(&error);
                let _ = store.retry_job(&job_id, &error_text);
                if matches!(
                    error,
                    DriveError::OAuth(OAuthError::WaitingForAuthorization)
                ) {
                    summary.status = "waiting_auth".to_owned();
                }
                summary.add_issue(format!(
                    "{}: Drive upload failed: {error_text}",
                    file.display_relative_path
                ));
            }
        }
    }

    collect_expired_drive_history(store, drive, &binding, task, &mut summary)?;

    if summary.cloud_pending_files > 0 && summary.status != "waiting_auth" {
        summary.status = "partial_success".to_owned();
    } else if summary.cloud_pending_files == 0
        && summary.issues.is_empty()
        && !summary.deletion_review_required
        && summary.omitted_issue_count == 0
    {
        summary.status = "succeeded".to_owned();
    } else if summary.status != "waiting_auth" {
        summary.status = "partial_success".to_owned();
    }
    finish_cloud_summary(store, &summary)?;
    Ok(summary)
}

fn collect_expired_drive_history(
    store: &mut StateStore,
    drive: &impl DriveHistoryDeleter,
    binding: &DriveTaskBinding,
    task: &BackupTask,
    summary: &mut BackupSummary,
) -> Result<(), LocalBackupError> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok());
    let Some(now_ms) = now_ms else {
        summary.add_issue(
            "Drive history retention cleanup skipped because the system clock is outside the supported range",
        );
        return Ok(());
    };
    let retention_ms = i64::from(HISTORY_RETENTION_DAYS).saturating_mul(24 * 60 * 60 * 1_000);
    let cutoff_ms = now_ms.saturating_sub(retention_ms);
    let reclaim_before_ms = now_ms.saturating_sub(DRIVE_HISTORY_GC_LEASE_MS).max(0);

    loop {
        let expired = store.claim_expired_drive_history_versions(
            &task.id,
            cutoff_ms,
            reclaim_before_ms,
            now_ms,
            DRIVE_HISTORY_GC_BATCH,
        )?;
        if expired.is_empty() {
            break;
        }
        let batch_len = expired.len();
        let mut had_error = false;
        for version in expired {
            match drive.delete_history_version(&version, &binding.history_folder_id) {
                Ok(_) => {
                    store.finish_drive_history_gc_delete(&version.manifest_id)?;
                    summary.cloud_history_versions_removed += 1;
                }
                Err(error) => {
                    store.release_drive_history_gc_claim(&version.manifest_id)?;
                    summary.add_issue(format!(
                        "{}: Drive history retention cleanup failed: {}",
                        version.display_relative_path,
                        drive_error_summary(&error)
                    ));
                    had_error = true;
                }
            }
        }
        if had_error || batch_len < DRIVE_HISTORY_GC_BATCH {
            break;
        }
    }
    Ok(())
}

fn finish_cloud_summary(
    store: &StateStore,
    summary: &BackupSummary,
) -> Result<(), LocalBackupError> {
    let status = if summary.status == "succeeded" {
        BackupRunStatus::Succeeded
    } else {
        BackupRunStatus::PartialSuccess
    };
    store.finish_backup_run(
        &summary.run_id,
        status,
        summary.scanned_files,
        summary.copied_files,
        summary_error_text(summary).as_deref(),
    )?;
    Ok(())
}

fn drive_error_summary(error: &DriveError) -> String {
    match error {
        DriveError::OAuth(OAuthError::WaitingForAuthorization) => {
            "waiting for authorization".to_owned()
        }
        DriveError::OAuth(OAuthError::HttpStatus(status)) | DriveError::HttpStatus(status) => {
            format!("HTTP {status}")
        }
        DriveError::OAuth(OAuthError::Network) | DriveError::Network => {
            "network unavailable".to_owned()
        }
        DriveError::OAuth(OAuthError::SecureStoreUnavailable)
        | DriveError::OAuth(OAuthError::SecureStoreFailure) => {
            "secure credential storage unavailable".to_owned()
        }
        DriveError::RetryExhausted => "retry limit reached".to_owned(),
        DriveError::IntegrityMismatch => "remote integrity check failed".to_owned(),
        _ => "Drive operation failed".to_owned(),
    }
}

fn safe_mirror_path(root: &Path, relative: &str) -> Result<PathBuf, LocalBackupError> {
    let relative_path = Path::new(relative);
    if relative_path.as_os_str().is_empty()
        || relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(LocalBackupError::UnsafeMirrorFile(root.join(relative_path)));
    }
    let canonical_root = fs::canonicalize(root)
        .map_err(|_| LocalBackupError::UnsafeMirrorFile(root.to_path_buf()))?;
    let mut candidate = canonical_root.clone();
    for component in relative_path.components() {
        let Component::Normal(name) = component else {
            return Err(LocalBackupError::UnsafeMirrorFile(root.join(relative_path)));
        };
        candidate.push(name);
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|_| LocalBackupError::UnsafeMirrorFile(candidate.clone()))?;
        if is_reparse_point(&metadata) {
            return Err(LocalBackupError::UnsafeMirrorFile(candidate));
        }
    }
    let canonical = fs::canonicalize(&candidate)
        .map_err(|_| LocalBackupError::UnsafeMirrorFile(candidate.clone()))?;
    if !canonical.starts_with(&canonical_root)
        || !fs::metadata(&canonical).is_ok_and(|metadata| metadata.is_file())
    {
        return Err(LocalBackupError::UnsafeMirrorFile(canonical));
    }
    Ok(canonical)
}

fn validate_mode(task: &BackupTask) -> Result<(), LocalBackupError> {
    match task.mode {
        BackupMode::Local if task.destinations.local.enabled => Ok(()),
        BackupMode::Dual
            if task.destinations.local.enabled && task.destinations.google_drive.enabled =>
        {
            Ok(())
        }
        BackupMode::Bidirectional => Err(LocalBackupError::UnsupportedMode(
            "bidirectional".to_owned(),
        )),
        BackupMode::Cloud => Err(LocalBackupError::UnsupportedMode("cloud".to_owned())),
        BackupMode::Local | BackupMode::Dual => Err(LocalBackupError::UnsupportedMode(
            "task has no enabled local destination".to_owned(),
        )),
    }
}

fn run_reconciliation(
    store: &mut StateStore,
    task: &BackupTask,
    run_id: String,
) -> Result<BackupSummary, LocalBackupError> {
    let paths = validate_task_paths(task)?;
    let destination_root = paths.local_destination.ok_or_else(|| {
        LocalBackupError::DestinationMissing(task.destinations.local.path.clone())
    })?;
    let root_metadata = match fs::symlink_metadata(&destination_root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(LocalBackupError::DestinationMissing(destination_root));
        }
        Err(source) => {
            return Err(LocalBackupError::Io {
                path: destination_root,
                source,
            });
        }
    };
    if is_reparse_point(&root_metadata) {
        return Err(LocalBackupError::DestinationReparsePoint(destination_root));
    }
    if !root_metadata.is_dir() {
        return Err(LocalBackupError::DestinationInvalid(destination_root));
    }
    let destination_root =
        fs::canonicalize(&destination_root).map_err(|source| LocalBackupError::Io {
            path: destination_root,
            source,
        })?;
    let history_repository = if task.versioning.enabled {
        let repository_parent = destination_root.parent().unwrap_or(&destination_root);
        Some(HistoryRepository::open_or_create(
            &repository_parent.join(".rustbackup").join(&task.id),
        )?)
    } else {
        None
    };

    let mut summary = BackupSummary::new(run_id.clone());
    let scanner = if let Some(repository) = &history_repository {
        let managed_root = repository.root().parent().unwrap_or(repository.root());
        Scanner::new_excluding(task, &[managed_root.to_path_buf()])?
    } else {
        Scanner::new(task)?
    };
    let filters = FilterSet::new(&task.filters).map_err(ScanError::from)?;
    let mut scan_complete = true;
    let mut last_progress_update = Instant::now();
    let mut last_persisted_scanned_files = 0;

    for item in scanner {
        let scanned = match item {
            Ok(scanned) => scanned,
            Err(error) => {
                scan_complete = false;
                summary.add_issue(error.to_string());
                update_backup_progress_if_due(
                    store,
                    &summary,
                    &mut last_progress_update,
                    &mut last_persisted_scanned_files,
                    false,
                )?;
                continue;
            }
        };
        summary.scanned_files += 1;
        update_backup_progress_if_due(
            store,
            &summary,
            &mut last_progress_update,
            &mut last_persisted_scanned_files,
            false,
        )?;
        let display_path = scanned.relative_path.to_string_lossy().into_owned();
        let previous = store.indexed_file_state(&task.id, &scanned.relative_path_key)?;
        let indexed = match store.record_file_seen(
            &task.id,
            &run_id,
            FileObservation {
                relative_path_key: &scanned.relative_path_key,
                display_relative_path: &display_path,
                size_bytes: scanned.size_bytes,
                modified_at_unix_ms: scanned.modified_at_unix_ms,
                blake3_hex: &scanned.blake3_hex,
            },
        ) {
            Ok(indexed) => indexed,
            Err(error) => {
                scan_complete = false;
                summary.add_issue(error.to_string());
                update_backup_progress_if_due(
                    store,
                    &summary,
                    &mut last_progress_update,
                    &mut last_persisted_scanned_files,
                    false,
                )?;
                continue;
            }
        };

        if task.mode == BackupMode::Dual {
            summary.cloud_pending_files += 1;
        }
        let destination_is_current =
            store.destination_revision(indexed.file_id, "local")? >= indexed.source_revision;
        let history_is_current = match &history_repository {
            Some(_) => {
                store.has_recoverable_history_version(indexed.file_id, indexed.source_revision)?
            }
            None => true,
        };
        if destination_is_current && history_is_current {
            continue;
        }

        let job_id = if destination_is_current {
            None
        } else {
            let job_id = store.enqueue_local_copy(indexed.file_id, indexed.source_revision)?;
            if let Err(error) = store.start_job(&job_id) {
                summary.add_issue(format!("{}: {error}", display_path));
                continue;
            }
            Some(job_id)
        };

        let result = if let Some(job_id) = job_id.as_deref() {
            copy_scanned_file(
                store,
                history_repository.as_ref(),
                &destination_root,
                &scanned,
                &indexed,
                previous.as_ref(),
                job_id,
            )
            .map(Some)
        } else if let Some(repository) = &history_repository {
            save_history_only(
                repository,
                store,
                &scanned,
                indexed.file_id,
                indexed.source_revision,
            )
            .map(|()| None)
        } else {
            Ok(None)
        };
        match result {
            Ok(Some(CommitOutcome::Committed)) => summary.copied_files += 1,
            Ok(Some(CommitOutcome::Stale)) => summary.add_issue(format!(
                "{}: source revision was superseded before local commit",
                display_path
            )),
            Ok(None) => {}
            Err(error) => {
                let message = error.to_string();
                if let Some(job_id) = job_id.as_deref() {
                    store.retry_job(job_id, &message)?;
                }
                summary.add_issue(format!("{}: {message}", display_path));
            }
        }
        update_backup_progress_if_due(
            store,
            &summary,
            &mut last_progress_update,
            &mut last_persisted_scanned_files,
            false,
        )?;
    }

    update_backup_progress_if_due(
        store,
        &summary,
        &mut last_progress_update,
        &mut last_persisted_scanned_files,
        true,
    )?;

    if scan_complete {
        let unseen = store.unseen_files(&task.id, &run_id)?;
        let mut candidates = Vec::new();
        for file in unseen {
            if !filters.is_excluded(Path::new(&file.display_relative_path), false) {
                candidates.push(file);
            }
        }
        let tracked_count = summary.scanned_files as usize + candidates.len();
        let mass_deletion_suspected = candidates.len() >= 10
            && tracked_count > 0
            && candidates.len().saturating_mul(2) >= tracked_count;
        if mass_deletion_suspected {
            summary.deletion_review_required = true;
            summary.add_issue(format!(
                "{} of {} previously indexed files were not found; source-missing states require review",
                candidates.len(), tracked_count
            ));
        } else {
            for file in candidates {
                store.mark_source_missing(file.file_id)?;
                summary.source_missing_files += 1;
            }
        }
    } else {
        summary.add_issue("source scan was incomplete; missing-file states were left unchanged");
    }

    if scan_complete {
        if let Some(repository) = &history_repository {
            let retention_ms =
                i64::from(HISTORY_RETENTION_DAYS).saturating_mul(24 * 60 * 60 * 1_000);
            let now_ms = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .and_then(|duration| i64::try_from(duration.as_millis()).ok());
            if let Some(now_ms) = now_ms {
                let cutoff_ms = now_ms.saturating_sub(retention_ms);
                match repository.collect_expired_before(store, cutoff_ms) {
                    Ok(gc) => {
                        summary.history_versions_removed = gc.versions_removed;
                        summary.history_objects_removed = gc.objects_removed;
                    }
                    Err(error) => {
                        summary.add_issue(format!("history retention cleanup failed: {error}"))
                    }
                }
            } else {
                summary.add_issue("history retention cleanup skipped because the system clock is outside the supported range");
            }
        }
    }

    Ok(summary)
}

fn update_backup_progress_if_due(
    store: &StateStore,
    summary: &BackupSummary,
    last_progress_update: &mut Instant,
    last_persisted_scanned_files: &mut u64,
    force: bool,
) -> Result<(), StorageError> {
    if force
        || summary
            .scanned_files
            .saturating_sub(*last_persisted_scanned_files)
            >= BACKUP_PROGRESS_FILE_BATCH
        || last_progress_update.elapsed() >= BACKUP_PROGRESS_INTERVAL
    {
        store.update_backup_run_progress(
            &summary.run_id,
            summary.scanned_files,
            summary.copied_files,
        )?;
        *last_progress_update = Instant::now();
        *last_persisted_scanned_files = summary.scanned_files;
    }
    Ok(())
}

fn copy_scanned_file(
    store: &mut StateStore,
    history_repository: Option<&HistoryRepository>,
    destination_root: &Path,
    scanned: &backup_core::ScannedFile,
    indexed: &IndexedFile,
    previous: Option<&IndexedFileState>,
    job_id: &str,
) -> Result<CommitOutcome, LocalBackupError> {
    let file_id = indexed.file_id;
    let revision = indexed.source_revision;
    let target = target_path(destination_root, &scanned.relative_path)?;
    let parent = target
        .parent()
        .ok_or_else(|| LocalBackupError::UnsafeDestinationPath(target.clone()))?;
    ensure_safe_target_parent(destination_root, parent)?;
    validate_existing_target(&target)?;

    let destination_revision = store.destination_revision(file_id, "local")?;
    let target_exists = target.exists();
    if target_exists && destination_revision == 0 {
        return Err(LocalBackupError::UntrackedDestinationTarget(target));
    }
    let staged = create_stable_snapshot(&scanned.source_path, parent, &scanned.blake3_hex)?;
    if target_exists {
        if let Some(repository) = history_repository {
            let old_relative_path = previous
                .map(|state| Path::new(&state.display_relative_path))
                .unwrap_or(&scanned.relative_path);
            let old_target = target_path(destination_root, old_relative_path)?;
            let old_parent = old_target
                .parent()
                .ok_or_else(|| LocalBackupError::UnsafeDestinationPath(old_target.clone()))?;
            ensure_safe_target_parent(destination_root, old_parent)?;
            validate_existing_target(&old_target)?;
            let (actual_hash, actual_size) = hash_existing_target(&old_target)?;
            if actual_hash != scanned.blake3_hex {
                let old_hash = store
                    .destination_checksum(file_id, "local")?
                    .ok_or_else(|| {
                        LocalBackupError::UntrackedDestinationTarget(old_target.clone())
                    })?;
                let relative_path_key = previous
                    .map(|state| state.relative_path_key.as_str())
                    .unwrap_or(&scanned.relative_path_key);
                let display_relative_path = previous
                    .map(|state| state.display_relative_path.clone())
                    .unwrap_or_else(|| scanned.relative_path.to_string_lossy().into_owned());
                repository.save_version(
                    store,
                    file_id,
                    destination_revision,
                    relative_path_key,
                    &display_relative_path,
                    &old_hash,
                    actual_size,
                    &old_target,
                )?;
            }
        }
    }
    if let Some(repository) = history_repository {
        repository.save_version(
            store,
            file_id,
            revision,
            &scanned.relative_path_key,
            &scanned.relative_path.to_string_lossy(),
            &scanned.blake3_hex,
            scanned.size_bytes,
            staged.path(),
        )?;
    }
    let checksum = scanned.blake3_hex.clone();
    let target_file_id = target.to_string_lossy().into_owned();
    store
        .commit_local_if_current(
            job_id,
            file_id,
            revision,
            &checksum,
            &target_file_id,
            || {
                ensure_safe_target_parent(destination_root, parent).map_err(local_error_to_io)?;
                validate_existing_target(&target).map_err(local_error_to_io)?;
                staged
                    .persist(&target)
                    .map(|_| ())
                    .map_err(|error| error.error)
            },
        )
        .map_err(LocalBackupError::from)
}

fn save_history_only(
    repository: &HistoryRepository,
    store: &mut StateStore,
    scanned: &backup_core::ScannedFile,
    file_id: i64,
    revision: i64,
) -> Result<(), LocalBackupError> {
    let staged =
        create_stable_snapshot(&scanned.source_path, repository.root(), &scanned.blake3_hex)?;
    repository.save_version(
        store,
        file_id,
        revision,
        &scanned.relative_path_key,
        &scanned.relative_path.to_string_lossy(),
        &scanned.blake3_hex,
        scanned.size_bytes,
        staged.path(),
    )?;
    Ok(())
}

fn hash_existing_target(path: &Path) -> Result<(String, u64), LocalBackupError> {
    validate_existing_target(path)?;
    let mut file = open_source_no_follow(path).map_err(|source| LocalBackupError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let before_metadata = file.metadata().map_err(|source| LocalBackupError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let before = FileStamp::from_metadata(&before_metadata)
        .ok_or_else(|| LocalBackupError::UnstableSource(path.to_path_buf()))?;
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut file, &mut hasher).map_err(|source| LocalBackupError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let after_metadata = file.metadata().map_err(|source| LocalBackupError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let after = FileStamp::from_metadata(&after_metadata)
        .ok_or_else(|| LocalBackupError::UnstableSource(path.to_path_buf()))?;
    if before != after {
        return Err(LocalBackupError::UnstableSource(path.to_path_buf()));
    }
    Ok((hasher.finalize().to_hex().to_string(), after.size))
}

fn validate_existing_target(target: &Path) -> Result<(), LocalBackupError> {
    match fs::symlink_metadata(target) {
        Ok(metadata) if is_reparse_point(&metadata) => Err(
            LocalBackupError::DestinationReparsePoint(target.to_path_buf()),
        ),
        Ok(metadata) if !metadata.is_file() => Err(LocalBackupError::DestinationTargetInvalid(
            target.to_path_buf(),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(LocalBackupError::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}

fn local_error_to_io(error: LocalBackupError) -> io::Error {
    io::Error::other(error.to_string())
}

fn target_path(root: &Path, relative_path: &Path) -> Result<PathBuf, LocalBackupError> {
    let mut target = root.to_path_buf();
    let mut found = false;
    for component in relative_path.components() {
        let Component::Normal(name) = component else {
            return Err(LocalBackupError::UnsafeDestinationPath(
                relative_path.to_path_buf(),
            ));
        };
        target.push(name);
        found = true;
    }
    if !found {
        return Err(LocalBackupError::UnsafeDestinationPath(
            relative_path.to_path_buf(),
        ));
    }
    Ok(target)
}

fn ensure_safe_target_parent(root: &Path, parent: &Path) -> Result<(), LocalBackupError> {
    let root_metadata = fs::symlink_metadata(root).map_err(|source| LocalBackupError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    if is_reparse_point(&root_metadata) {
        return Err(LocalBackupError::DestinationReparsePoint(
            root.to_path_buf(),
        ));
    }
    if !root_metadata.is_dir() {
        return Err(LocalBackupError::DestinationInvalid(root.to_path_buf()));
    }
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| LocalBackupError::UnsafeDestinationPath(parent.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(LocalBackupError::UnsafeDestinationPath(
                parent.to_path_buf(),
            ));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if is_reparse_point(&metadata) {
                    return Err(LocalBackupError::DestinationReparsePoint(current));
                }
                if !metadata.is_dir() {
                    return Err(LocalBackupError::DestinationInvalid(current));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|source| LocalBackupError::Io {
                    path: current.clone(),
                    source,
                })?;
                let metadata =
                    fs::symlink_metadata(&current).map_err(|source| LocalBackupError::Io {
                        path: current.clone(),
                        source,
                    })?;
                if is_reparse_point(&metadata) {
                    return Err(LocalBackupError::DestinationReparsePoint(current));
                }
                if !metadata.is_dir() {
                    return Err(LocalBackupError::DestinationInvalid(current));
                }
            }
            Err(source) => {
                return Err(LocalBackupError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn create_stable_snapshot(
    source: &Path,
    temp_parent: &Path,
    expected_hash: &str,
) -> Result<NamedTempFile, LocalBackupError> {
    for _ in 0..SNAPSHOT_ATTEMPTS {
        let path_metadata =
            fs::symlink_metadata(source).map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if is_reparse_point(&path_metadata) || !path_metadata.is_file() {
            return Err(LocalBackupError::UnstableSource(source.to_path_buf()));
        }
        let mut input =
            open_source_no_follow(source).map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        let input_metadata = input
            .metadata()
            .map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if is_reparse_point(&input_metadata) || !input_metadata.is_file() {
            return Err(LocalBackupError::UnstableSource(source.to_path_buf()));
        }
        let before = FileStamp::from_metadata(&input_metadata)
            .ok_or_else(|| LocalBackupError::UnstableSource(source.to_path_buf()))?;
        let mut staged =
            NamedTempFile::new_in(temp_parent).map_err(|source_error| LocalBackupError::Io {
                path: temp_parent.to_path_buf(),
                source: source_error,
            })?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|source_error| LocalBackupError::Io {
                    path: source.to_path_buf(),
                    source: source_error,
                })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            staged
                .write_all(&buffer[..read])
                .map_err(|source_error| LocalBackupError::Io {
                    path: temp_parent.to_path_buf(),
                    source: source_error,
                })?;
        }
        let after = FileStamp::from_metadata(&input.metadata().map_err(|source_error| {
            LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            }
        })?)
        .ok_or_else(|| LocalBackupError::UnstableSource(source.to_path_buf()))?;
        let path_after =
            fs::symlink_metadata(source).map_err(|source_error| LocalBackupError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        if is_reparse_point(&path_after) {
            return Err(LocalBackupError::UnstableSource(source.to_path_buf()));
        }
        let path_after = FileStamp::from_metadata(&path_after)
            .ok_or_else(|| LocalBackupError::UnstableSource(source.to_path_buf()))?;
        if before != after || after != path_after {
            continue;
        }
        let actual_hash = hasher.finalize().to_hex().to_string();
        if actual_hash != expected_hash {
            continue;
        }

        staged
            .as_file()
            .sync_all()
            .map_err(|source_error| LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            })?;
        staged
            .as_file_mut()
            .seek(SeekFrom::Start(0))
            .map_err(|source_error| LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            })?;
        let mut verify = blake3::Hasher::new();
        io::copy(staged.as_file_mut(), &mut verify).map_err(|source_error| {
            LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            }
        })?;
        if verify.finalize().to_hex().as_str() != expected_hash {
            continue;
        }
        staged
            .as_file()
            .sync_all()
            .map_err(|source_error| LocalBackupError::Io {
                path: staged.path().to_path_buf(),
                source: source_error,
            })?;
        return Ok(staged);
    }
    Err(LocalBackupError::UnstableSource(source.to_path_buf()))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileStamp {
    size: u64,
    modified: SystemTime,
}

impl FileStamp {
    fn from_metadata(metadata: &fs::Metadata) -> Option<Self> {
        Some(Self {
            size: metadata.len(),
            modified: metadata.modified().ok()?,
        })
    }
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

fn open_source_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn summary_error_text(summary: &BackupSummary) -> Option<String> {
    if summary.issues.is_empty() && summary.omitted_issue_count == 0 {
        None
    } else {
        let mut text = summary.issues.join("; ");
        if summary.omitted_issue_count > 0 {
            text.push_str(&format!(
                "; {} additional issues omitted",
                summary.omitted_issue_count
            ));
        }
        Some(text)
    }
}

#[derive(Clone, Debug)]
pub struct EventDebouncer {
    delay: Duration,
    deadlines: HashMap<PathBuf, Instant>,
    full_reconcile: bool,
}

impl EventDebouncer {
    pub fn new(delay: Duration) -> Self {
        Self {
            delay,
            deadlines: HashMap::new(),
            full_reconcile: false,
        }
    }

    pub fn observe_path(&mut self, path: PathBuf, now: Instant) {
        self.deadlines.insert(path, now + self.delay);
    }

    pub fn request_full_reconcile(&mut self) {
        self.full_reconcile = true;
    }

    pub fn take_ready(&mut self, now: Instant) -> bool {
        let ready_paths: Vec<_> = self
            .deadlines
            .iter()
            .filter_map(|(path, deadline)| (*deadline <= now).then_some(path.clone()))
            .collect();
        let has_ready_path = !ready_paths.is_empty();
        for path in ready_paths {
            self.deadlines.remove(&path);
        }
        let ready = has_ready_path || self.full_reconcile;
        self.full_reconcile = false;
        ready
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.values().copied().min()
    }
}

struct LocalWatcher {
    _watcher: RecommendedWatcher,
    events: Receiver<notify::Result<Event>>,
}

impl LocalWatcher {
    fn start(source: &Path) -> notify::Result<Self> {
        let (sender, events) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |event| {
            let _ = sender.send(event);
        })?;
        watcher.watch(source, RecursiveMode::Recursive)?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SchedulerReport {
    pub reconciliations: u64,
    pub watcher_error: Option<String>,
    pub last_backup_status: Option<String>,
    pub last_backup_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManualRunCompletion {
    pub run_id: String,
    pub status: String,
    pub error_summary: Option<String>,
}

/// Runs startup, debounced watcher and periodic reconciliations until the
/// caller closes the shutdown channel. Watcher errors force a full scan and
/// remain visible in the returned report.
pub fn run_scheduled(
    store: &mut StateStore,
    task: &BackupTask,
    shutdown: &Receiver<()>,
) -> Result<SchedulerReport, LocalBackupError> {
    run_scheduled_with_drive(store, task, shutdown, None, None, None, None)
}

/// Runs the scheduler with an optional Drive client and durable manual-run
/// requests from the Agent. A request is acknowledged only after its backup
/// attempt has completed, so the caller can safely retry it after a restart.
pub fn run_scheduled_with_drive(
    store: &mut StateStore,
    task: &BackupTask,
    shutdown: &Receiver<()>,
    drive: Option<&DriveClient>,
    manual_runs: Option<&Receiver<String>>,
    completed_manual_runs: Option<&mpsc::Sender<ManualRunCompletion>>,
    mut report_changed: Option<&mut dyn FnMut(&SchedulerReport)>,
) -> Result<SchedulerReport, LocalBackupError> {
    validate_mode(task)?;
    if !task.enabled {
        return Err(LocalBackupError::TaskDisabled(task.id.clone()));
    }
    let mut report = SchedulerReport::default();
    let mut watcher = None;
    let mut source = validated_source_or_pause(task, &mut report)?;
    let mut debouncer = EventDebouncer::new(Duration::from_millis(task.schedule.debounce_ms));
    if source.is_some()
        && matches!(
            task.schedule.schedule_type,
            backup_core::ScheduleType::Realtime
        )
    {
        attach_watcher(&mut watcher, &source, &mut report);
        if watcher.is_none() {
            debouncer.request_full_reconcile();
        }
    }

    if task.schedule.scan_on_startup {
        record_scheduled_backup(store, task, drive, &mut report);
        report.reconciliations += 1;
        if let Some(report_changed) = report_changed.as_deref_mut() {
            report_changed(&report);
        }
    }
    let interval = Duration::from_secs(u64::from(task.schedule.reconcile_interval_minutes) * 60);
    let mut next_reconcile = if matches!(
        task.schedule.schedule_type,
        backup_core::ScheduleType::Manual
    ) {
        None
    } else {
        Some(Instant::now() + reconcile_delay(&report, interval))
    };
    let mut next_source_check = Instant::now() + SOURCE_REAPPEAR_CHECK_INTERVAL;

    loop {
        match shutdown.try_recv() {
            Ok(()) | Err(mpsc::TryRecvError::Disconnected) => break,
            Err(mpsc::TryRecvError::Empty) => {}
        }

        let manual_run_id = manual_runs.and_then(|requests| requests.try_recv().ok());

        let now = Instant::now();
        if now >= next_source_check {
            let was_available = source.is_some();
            source = validated_source_or_pause(task, &mut report)?;
            if source.is_none() {
                watcher = None;
                next_reconcile = if matches!(
                    task.schedule.schedule_type,
                    backup_core::ScheduleType::Manual
                ) {
                    None
                } else {
                    Some(now + SOURCE_REAPPEAR_CHECK_INTERVAL)
                };
            } else if !was_available
                && !matches!(
                    task.schedule.schedule_type,
                    backup_core::ScheduleType::Manual
                )
            {
                debouncer.request_full_reconcile();
            }
            next_source_check = now + SOURCE_REAPPEAR_CHECK_INTERVAL;
        }
        let periodic_reconcile_due = next_reconcile.is_some_and(|deadline| now >= deadline);
        if manual_run_id.is_some() || debouncer.take_ready(now) || periodic_reconcile_due {
            record_scheduled_backup(store, task, drive, &mut report);
            report.reconciliations += 1;
            if !matches!(
                task.schedule.schedule_type,
                backup_core::ScheduleType::Manual
            ) {
                next_reconcile = Some(Instant::now() + reconcile_delay(&report, interval));
            }
            if let (Some(run_id), Some(completed)) = (manual_run_id, completed_manual_runs) {
                let _ = completed.send(ManualRunCompletion {
                    run_id,
                    status: report
                        .last_backup_status
                        .clone()
                        .unwrap_or_else(|| "error".to_owned()),
                    error_summary: report.last_backup_error.clone(),
                });
            }
            if let Some(report_changed) = report_changed.as_deref_mut() {
                report_changed(&report);
            }
            source = validated_source_or_pause(task, &mut report)?;
            if source.is_none() {
                watcher = None;
            } else if watcher.is_none()
                && matches!(
                    task.schedule.schedule_type,
                    backup_core::ScheduleType::Realtime
                )
            {
                attach_watcher(&mut watcher, &source, &mut report);
            }
            continue;
        }

        let fallback_deadline = now + Duration::from_millis(250);
        let next_deadline = match (debouncer.next_deadline(), next_reconcile) {
            (Some(debounce), Some(reconcile)) => debounce.min(reconcile),
            (Some(debounce), None) => debounce,
            (None, Some(reconcile)) => reconcile,
            (None, None) => fallback_deadline,
        };
        let timeout = next_deadline
            .saturating_duration_since(now)
            .min(Duration::from_millis(250));
        let Some(active_watcher) = watcher.as_ref() else {
            std::thread::sleep(timeout);
            continue;
        };
        match active_watcher.events.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if event.need_rescan() || event.paths.is_empty() {
                    debouncer.request_full_reconcile();
                } else {
                    for path in event.paths {
                        if source
                            .as_ref()
                            .is_some_and(|root| path.strip_prefix(root).is_ok())
                        {
                            debouncer.observe_path(path, Instant::now());
                        } else {
                            debouncer.request_full_reconcile();
                        }
                    }
                }
            }
            Ok(Err(error)) => {
                report
                    .watcher_error
                    .get_or_insert_with(|| error.to_string());
                debouncer.request_full_reconcile();
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                report
                    .watcher_error
                    .get_or_insert_with(|| "watcher event channel disconnected".to_owned());
                watcher = None;
                debouncer.request_full_reconcile();
            }
        }
    }
    Ok(report)
}

fn record_scheduled_backup(
    store: &mut StateStore,
    task: &BackupTask,
    drive: Option<&DriveClient>,
    report: &mut SchedulerReport,
) {
    match backup_once_with_drive(store, task, drive) {
        Ok(summary) => {
            report.last_backup_status = Some(summary.status);
            if !summary.issues.is_empty() {
                report.last_backup_error = Some(summary.issues.join("; "));
            } else {
                report.last_backup_error = None;
            }
        }
        Err(error) => {
            report.last_backup_status = Some(
                source_unavailable_status(task, &error)
                    .unwrap_or("error")
                    .to_owned(),
            );
            report.last_backup_error = Some(error.to_string());
        }
    }
}

const SOURCE_REAPPEAR_CHECK_INTERVAL: Duration = Duration::from_secs(30);

fn reconcile_delay(report: &SchedulerReport, normal_interval: Duration) -> Duration {
    if matches!(
        report.last_backup_status.as_deref(),
        Some("paused" | "retry_wait")
    ) {
        SOURCE_REAPPEAR_CHECK_INTERVAL
    } else {
        normal_interval
    }
}

fn validated_source_or_pause(
    task: &BackupTask,
    report: &mut SchedulerReport,
) -> Result<Option<PathBuf>, LocalBackupError> {
    match validate_task_paths(task) {
        Ok(paths) => Ok(Some(paths.source)),
        Err(error) if is_source_unavailable_path_error(&error) => {
            report.last_backup_status = Some(
                if matches!(
                    task.safety.on_source_unavailable,
                    SourceUnavailablePolicy::Pause
                ) {
                    "paused"
                } else {
                    "retry_wait"
                }
                .to_owned(),
            );
            report.last_backup_error = Some(error.to_string());
            Ok(None)
        }
        Err(error) => Err(LocalBackupError::Paths(error)),
    }
}

fn should_pause_for_source(task: &BackupTask, error: &LocalBackupError) -> bool {
    source_unavailable_status(task, error) == Some("paused")
}

fn source_unavailable_status(task: &BackupTask, error: &LocalBackupError) -> Option<&'static str> {
    let unavailable = match error {
        LocalBackupError::Paths(path_error) => is_source_unavailable_path_error(path_error),
        LocalBackupError::Scan(ScanError::PathValidation(path_error)) => {
            is_source_unavailable_path_error(path_error)
        }
        _ => false,
    };
    if !unavailable {
        return None;
    }
    Some(
        if matches!(
            task.safety.on_source_unavailable,
            SourceUnavailablePolicy::Pause
        ) {
            "paused"
        } else {
            "retry_wait"
        },
    )
}

fn is_source_unavailable_path_error(error: &PathValidationError) -> bool {
    matches!(
        error,
        PathValidationError::SourceIo { .. }
            | PathValidationError::SourceNotReadable { .. }
            | PathValidationError::NoExistingAncestor(_)
    )
}

fn attach_watcher(
    watcher: &mut Option<LocalWatcher>,
    source: &Option<PathBuf>,
    report: &mut SchedulerReport,
) {
    let Some(source) = source else {
        return;
    };
    match LocalWatcher::start(source) {
        Ok(started) => *watcher = Some(started),
        Err(error) => {
            report.watcher_error = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use backup_core::{
        DestinationsConfig, FilterConfig, GoogleDriveDestination, LocalDestination,
        PermissionDeniedPolicy, SafetyConfig, ScheduleConfig, ScheduleType, SourceConfig,
        SourceUnavailablePolicy, VersioningConfig,
    };
    use std::sync::Mutex;

    fn task(source: &Path, destination: &Path) -> BackupTask {
        BackupTask {
            id: "local-test".to_owned(),
            name: "local test".to_owned(),
            enabled: true,
            source: SourceConfig {
                path: source.to_path_buf(),
                recursive: true,
                follow_symlinks: false,
            },
            mode: BackupMode::Local,
            schedule: ScheduleConfig {
                schedule_type: ScheduleType::Manual,
                debounce_ms: 2_000,
                reconcile_interval_minutes: 60,
                scan_on_startup: false,
            },
            destinations: DestinationsConfig {
                local: LocalDestination {
                    enabled: true,
                    path: destination.to_path_buf(),
                },
                google_drive: GoogleDriveDestination {
                    enabled: false,
                    account_id: None,
                    folder_id: None,
                    folder_name: String::new(),
                },
            },
            versioning: VersioningConfig {
                enabled: true,
                retention_days: 7,
                keep_latest: true,
            },
            filters: FilterConfig {
                exclude: Vec::new(),
                case_insensitive: true,
            },
            safety: SafetyConfig {
                propagate_deletions: false,
                preserve_deleted_files: true,
                verify_before_commit: true,
                on_source_unavailable: SourceUnavailablePolicy::Pause,
                on_permission_denied: PermissionDeniedPolicy::Retry,
            },
        }
    }

    fn directories() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().expect("create isolated test directory");
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).expect("create temporary source");
        fs::create_dir(&destination).expect("create temporary target");
        (temp, source, destination)
    }

    #[test]
    fn dual_backup_without_drive_authorization_keeps_local_copy_and_waits_for_auth() {
        let (_temp, source, destination) = directories();
        fs::write(source.join("hello.txt"), b"local copy first").unwrap();
        let mut dual_task = task(&source, &destination);
        dual_task.mode = BackupMode::Dual;
        dual_task.destinations.google_drive.enabled = true;
        dual_task.destinations.google_drive.folder_name = "RustBackup/Work".to_owned();
        let mut store = StateStore::open_in_memory().unwrap();

        let summary = backup_once_with_drive(&mut store, &dual_task, None).unwrap();
        assert_eq!(summary.status, "waiting_auth");
        assert_eq!(summary.copied_files, 1);
        assert_eq!(summary.cloud_pending_files, 1);
        assert_eq!(
            fs::read(destination.join("hello.txt")).unwrap(),
            b"local copy first"
        );
        assert!(store.drive_task_binding(&dual_task.id).unwrap().is_none());
    }

    #[test]
    fn drive_history_retention_deletes_only_versions_past_seven_days() {
        #[derive(Default)]
        struct RecordingDeleter(Mutex<Vec<String>>);

        impl DriveHistoryDeleter for RecordingDeleter {
            fn delete_history_version(
                &self,
                version: &DriveHistoryVersion,
                _history_folder_id: &str,
            ) -> Result<bool, DriveError> {
                self.0.lock().unwrap().push(version.drive_file_id.clone());
                Ok(true)
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let mut retention_task = task(temp.path(), temp.path());
        // Runtime cleanup enforces the project policy even for an in-memory task
        // that bypasses JSON configuration validation.
        retention_task.versioning.retention_days = 30;
        let mut store = StateStore::open_in_memory().unwrap();
        store
            .register_task(&retention_task.id, &retention_task.name, true)
            .unwrap();
        let old_hash = blake3::hash(b"first").to_hex().to_string();
        let first_run = store.begin_backup_run(&retention_task.id).unwrap();
        let first = store
            .record_file_seen(
                &retention_task.id,
                &first_run,
                FileObservation {
                    relative_path_key: "sample.txt",
                    display_relative_path: "sample.txt",
                    size_bytes: 5,
                    modified_at_unix_ms: 1,
                    blake3_hex: &old_hash,
                },
            )
            .unwrap();
        let second_run = store.begin_backup_run(&retention_task.id).unwrap();
        let second_hash = blake3::hash(b"second").to_hex().to_string();
        let second = store
            .record_file_seen(
                &retention_task.id,
                &second_run,
                FileObservation {
                    relative_path_key: "sample.txt",
                    display_relative_path: "sample.txt",
                    size_bytes: 6,
                    modified_at_unix_ms: 2,
                    blake3_hex: &second_hash,
                },
            )
            .unwrap();
        assert_eq!(first.file_id, second.file_id);
        let binding = DriveTaskBinding {
            task_id: retention_task.id.clone(),
            account_id: "test-account".to_owned(),
            root_folder_id: "root-folder".to_owned(),
            task_folder_id: "task-folder".to_owned(),
            history_folder_id: "history-folder".to_owned(),
            folder_name: "RustBackup/Acceptance".to_owned(),
        };
        store.bind_drive_task(&binding).unwrap();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        for (revision, remote_id, hash, size, created_at) in [
            (
                1,
                "remote-old",
                old_hash.as_str(),
                5,
                now_ms - 8 * 24 * 60 * 60 * 1_000,
            ),
            (
                2,
                "remote-recent",
                second_hash.as_str(),
                6,
                now_ms - 24 * 60 * 60 * 1_000,
            ),
        ] {
            store
                .record_drive_history_version(DriveHistoryRecord {
                    task_id: &retention_task.id,
                    file_id: first.file_id,
                    source_revision: revision,
                    drive_file_id: remote_id,
                    account_id: &binding.account_id,
                    relative_path_key: "sample.txt",
                    display_relative_path: "sample.txt",
                    content_hash: hash,
                    size_bytes: size,
                    created_at_unix_ms: created_at,
                })
                .unwrap();
        }

        let deleter = RecordingDeleter::default();
        let mut summary = BackupSummary::new("retention-run".to_owned());
        collect_expired_drive_history(
            &mut store,
            &deleter,
            &binding,
            &retention_task,
            &mut summary,
        )
        .unwrap();

        assert_eq!(*deleter.0.lock().unwrap(), ["remote-old"]);
        assert_eq!(summary.cloud_history_versions_removed, 1);
        let retained = store
            .list_drive_history_versions(&retention_task.id, first.file_id)
            .unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].source_revision, 2);
    }

    #[test]
    fn copies_new_and_changed_files_but_does_not_revise_unchanged_content() {
        let (temp, source, destination) = directories();
        fs::write(source.join("hello.txt"), b"first").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        let first = backup_once(&mut store, &task).unwrap();
        assert_eq!(first.status, "succeeded");
        assert_eq!(first.copied_files, 1);
        assert_eq!(fs::read(destination.join("hello.txt")).unwrap(), b"first");

        let second = backup_once(&mut store, &task).unwrap();
        assert_eq!(second.copied_files, 0);
        assert_eq!(second.status, "succeeded");

        fs::write(source.join("hello.txt"), b"second revision").unwrap();
        let third = backup_once(&mut store, &task).unwrap();
        assert_eq!(third.copied_files, 1);
        assert_eq!(
            fs::read(destination.join("hello.txt")).unwrap(),
            b"second revision"
        );
        let file_id = store
            .indexed_file_state("local-test", "hello.txt")
            .unwrap()
            .unwrap()
            .file_id;
        let repository =
            HistoryRepository::open_or_create(&temp.path().join(".rustbackup").join("local-test"))
                .unwrap();
        let versions = repository.list_file_versions(&store, file_id).unwrap();
        assert_eq!(versions.len(), 2);
        assert!(versions[0].is_latest);
        let restored = temp.path().join("restored-old-version");
        repository
            .restore_version(&mut store, &versions[1].version_id, &restored, false)
            .unwrap();
        assert_eq!(fs::read(restored.join("hello.txt")).unwrap(), b"first");
    }

    #[test]
    fn local_scan_excludes_the_managed_history_repository() {
        let temp = tempfile::tempdir().unwrap();
        let output_parent = temp.path().join("output");
        let source = output_parent.join(".rustbackup");
        let destination = output_parent.join("Work");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(source.join("source.txt"), b"user data").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        let first = backup_once(&mut store, &task).unwrap();
        assert_eq!(first.scanned_files, 0);
        let second = backup_once(&mut store, &task).unwrap();
        assert_eq!(second.scanned_files, 0);
        assert!(!destination.join("source.txt").exists());
        assert!(!destination.join("local-test").exists());
    }

    #[test]
    fn migration_archives_an_existing_m1_copy_before_replacing_it() {
        let (temp, source, destination) = directories();
        fs::write(source.join("item.txt"), b"new source version").unwrap();
        fs::write(destination.join("item.txt"), b"previous committed version").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        store.register_task(&task.id, &task.name, true).unwrap();
        let first_run = store.begin_backup_run(&task.id).unwrap();
        let previous_hash = blake3::hash(b"previous committed version")
            .to_hex()
            .to_string();
        let previous = store
            .record_file_seen(
                &task.id,
                &first_run,
                FileObservation {
                    relative_path_key: "item.txt",
                    display_relative_path: "item.txt",
                    size_bytes: b"previous committed version".len() as u64,
                    modified_at_unix_ms: 1,
                    blake3_hex: &previous_hash,
                },
            )
            .unwrap();
        let old_job = store
            .enqueue_local_copy(previous.file_id, previous.source_revision)
            .unwrap();
        store.start_job(&old_job).unwrap();
        store
            .commit_local_if_current(
                &old_job,
                previous.file_id,
                previous.source_revision,
                &previous_hash,
                "item.txt",
                || Ok(()),
            )
            .unwrap();

        let summary = backup_once(&mut store, &task).unwrap();
        assert_eq!(summary.copied_files, 1);
        assert_eq!(
            fs::read(destination.join("item.txt")).unwrap(),
            b"new source version"
        );
        let repository =
            HistoryRepository::open_or_create(&temp.path().join(".rustbackup").join("local-test"))
                .unwrap();
        let versions = repository
            .list_file_versions(&store, previous.file_id)
            .unwrap();
        assert_eq!(versions.len(), 2);
        let old = versions
            .iter()
            .find(|version| version.source_revision == 1)
            .unwrap();
        let restored = temp.path().join("restored-m1-version");
        repository
            .restore_version(&mut store, &old.version_id, &restored, false)
            .unwrap();
        assert_eq!(
            fs::read(restored.join("item.txt")).unwrap(),
            b"previous committed version"
        );
    }

    #[test]
    fn source_deletion_keeps_the_existing_local_copy() {
        let (_temp, source, destination) = directories();
        fs::write(source.join("keep.txt"), b"still available").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        backup_once(&mut store, &task).unwrap();
        fs::remove_file(source.join("keep.txt")).unwrap();

        let result = backup_once(&mut store, &task).unwrap();
        assert_eq!(result.source_missing_files, 1);
        assert_eq!(
            fs::read(destination.join("keep.txt")).unwrap(),
            b"still available"
        );
    }

    #[test]
    fn changing_exclusions_does_not_mark_existing_target_missing() {
        let (_temp, source, destination) = directories();
        fs::write(source.join("retain.txt"), b"value").unwrap();
        let mut task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        backup_once(&mut store, &task).unwrap();
        task.filters.exclude.push("retain.txt".to_owned());

        let result = backup_once(&mut store, &task).unwrap();
        assert_eq!(result.source_missing_files, 0);
        assert_eq!(fs::read(destination.join("retain.txt")).unwrap(), b"value");
    }

    #[test]
    fn missing_destination_is_not_created_automatically() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("not-created");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("data.txt"), b"data").unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        assert!(matches!(
            backup_once(&mut store, &task),
            Err(LocalBackupError::DestinationMissing(_))
        ));
        assert!(!destination.exists());
    }

    #[test]
    fn event_debouncer_waits_two_seconds_and_coalesces_paths() {
        let start = Instant::now();
        let mut debouncer = EventDebouncer::new(Duration::from_secs(2));
        debouncer.observe_path(PathBuf::from("a"), start);
        debouncer.observe_path(PathBuf::from("b"), start + Duration::from_millis(500));
        assert!(!debouncer.take_ready(start + Duration::from_millis(1_999)));
        assert!(debouncer.take_ready(start + Duration::from_millis(2_500)));
        assert!(!debouncer.take_ready(start + Duration::from_secs(3)));
    }

    #[test]
    fn full_reconcile_signal_is_immediately_ready() {
        let mut debouncer = EventDebouncer::new(Duration::from_secs(2));
        debouncer.request_full_reconcile();
        assert!(debouncer.take_ready(Instant::now()));
    }

    #[test]
    fn large_missing_batch_is_left_for_review() {
        let (_temp, source, destination) = directories();
        for index in 0..10 {
            fs::write(source.join(format!("file-{index}.txt")), b"value").unwrap();
        }
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        backup_once(&mut store, &task).unwrap();
        for entry in fs::read_dir(&source).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }

        let result = backup_once(&mut store, &task).unwrap();
        assert!(result.deletion_review_required);
        assert_eq!(result.source_missing_files, 0);
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 10);
    }

    #[test]
    fn failed_snapshot_keeps_existing_target_untouched() {
        let (_temp, source, destination) = directories();
        let original_target = destination.join("item.txt");
        fs::write(&original_target, b"previous good copy").unwrap();
        fs::write(source.join("item.txt"), b"new source value").unwrap();
        let expected_hash = blake3::hash(b"not the source hash").to_hex().to_string();
        let error = create_stable_snapshot(&source.join("item.txt"), &destination, &expected_hash)
            .expect_err("mismatching source snapshot must not be committed");
        assert!(matches!(error, LocalBackupError::UnstableSource(_)));
        assert_eq!(fs::read(original_target).unwrap(), b"previous good copy");
    }

    #[test]
    fn source_unavailable_policy_pauses_without_marking_missing() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&destination).unwrap();
        let mut task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();
        assert!(backup_once(&mut store, &task).is_err());

        let mut report = SchedulerReport::default();
        assert!(validated_source_or_pause(&task, &mut report)
            .unwrap()
            .is_none());
        assert_eq!(report.last_backup_status.as_deref(), Some("paused"));

        task.safety.on_source_unavailable = SourceUnavailablePolicy::Retry;
        let mut retry_report = SchedulerReport::default();
        assert!(validated_source_or_pause(&task, &mut retry_report)
            .unwrap()
            .is_none());
        assert_eq!(
            retry_report.last_backup_status.as_deref(),
            Some("retry_wait")
        );
    }

    #[test]
    fn does_not_follow_destination_symlinked_parent() {
        let (_temp, source, destination) = directories();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/a.txt"), b"value").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(source.join("nested"), destination.join("nested")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(source.join("nested"), destination.join("nested"))
            .unwrap();
        let task = task(&source, &destination);
        let mut store = StateStore::open_in_memory().unwrap();

        let summary = backup_once(&mut store, &task).unwrap();
        assert_eq!(summary.copied_files, 0);
        assert_eq!(summary.status, "partial_success");
        assert!(summary
            .issues
            .iter()
            .any(|issue| issue.contains("reparse point")));
        assert!(
            !source.join("nested/a.txt").is_file()
                || fs::read(source.join("nested/a.txt")).unwrap() == b"value"
        );
    }
}
