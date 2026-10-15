//! Persistent SQLite state for source revisions and per-destination transfers.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use tempfile::NamedTempFile;
use thiserror::Error;

const INITIAL_MIGRATION: &str = include_str!("../../../sql/001_init.sql");
const HISTORY_MIGRATION: &str = include_str!("../../../sql/002_history.sql");
const SCHEMA_VERSION: i64 = 2;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
type IndexedFileRow = (i64, i64, String, Option<i64>, Option<String>);

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("SQLite operation failed: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("state database I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("state database schema version {0} is not supported")]
    UnsupportedSchema(i64),
    #[error("relative path key is invalid: {0}")]
    InvalidPathKey(String),
    #[error("display path is invalid: {0}")]
    InvalidDisplayPath(String),
    #[error("case-insensitive path collision in one scan: {0}")]
    PathCollision(String),
    #[error("backup run {0} does not exist")]
    MissingRun(String),
    #[error("transfer job {0} does not exist or is not running")]
    InvalidJobState(String),
    #[error("backup run file count exceeds SQLite's supported integer range")]
    CountOverflow,
    #[error("file revision counter is exhausted")]
    RevisionOverflow,
    #[error("task id is invalid: {0}")]
    InvalidTaskId(String),
    #[error("content hash is invalid")]
    InvalidHash,
    #[error("history repository path is unsafe or unavailable: {0}")]
    InvalidHistoryRoot(PathBuf),
    #[error("history version {0} does not exist or is not recoverable")]
    VersionNotRecoverable(String),
    #[error("history version {0} conflicts with an existing immutable version")]
    VersionConflict(String),
    #[error("history object failed integrity verification: {0}")]
    HistoryIntegrity(PathBuf),
    #[error("restore destination must be an absolute path with an existing parent: {0}")]
    InvalidRestoreDestination(PathBuf),
    #[error("restore destination already exists and overwrite was not confirmed: {0}")]
    RestoreDestinationExists(PathBuf),
    #[error("restore destination is not a safe directory: {0}")]
    InvalidRestoreDirectory(PathBuf),
    #[error("restore target is occupied by a non-file or reparse point: {0}")]
    InvalidRestoreTarget(PathBuf),
    #[error("retention cutoff is invalid")]
    InvalidRetentionCutoff,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedFile {
    pub file_id: i64,
    pub source_revision: i64,
    pub changed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedFileState {
    pub file_id: i64,
    pub source_revision: i64,
    pub relative_path_key: String,
    pub display_relative_path: String,
    pub size_bytes: Option<u64>,
    pub content_hash: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnseenFile {
    pub file_id: i64,
    pub relative_path_key: String,
    pub display_relative_path: String,
}

#[derive(Clone, Copy, Debug)]
pub struct FileObservation<'a> {
    pub relative_path_key: &'a str,
    pub display_relative_path: &'a str,
    pub size_bytes: u64,
    pub modified_at_unix_ms: i64,
    pub blake3_hex: &'a str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommitOutcome {
    Committed,
    Stale,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackupRunStatus {
    Succeeded,
    PartialSuccess,
    Error,
    Paused,
}

impl BackupRunStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::PartialSuccess => "partial_success",
            Self::Error => "error",
            Self::Paused => "paused",
        }
    }
}

pub struct StateStore {
    connection: Connection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryVersion {
    pub version_id: String,
    pub file_id: i64,
    pub source_revision: i64,
    pub relative_path_key: String,
    pub display_relative_path: String,
    pub content_hash: String,
    pub size_bytes: u64,
    pub created_at_unix_ms: i64,
    pub is_latest: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoreReceipt {
    pub restore_id: String,
    pub version_id: String,
    pub destination_directory: PathBuf,
    pub overwrite_confirmed: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GcSummary {
    pub versions_removed: u64,
    pub objects_removed: u64,
}

#[derive(Clone, Debug)]
pub struct HistoryRepository {
    root: PathBuf,
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StorageError> {
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let journal_mode: String =
            connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") && !journal_mode.eq_ignore_ascii_case("memory")
        {
            return Err(StorageError::Sql(rusqlite::Error::InvalidQuery));
        }

        let mut version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if !(0..=SCHEMA_VERSION).contains(&version) {
            return Err(StorageError::UnsupportedSchema(version));
        }
        if version == 0 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(INITIAL_MIGRATION)?;
            transaction.pragma_update(None, "user_version", 1_i64)?;
            transaction.commit()?;
            version = 1;
        }
        if version == 1 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(HISTORY_MIGRATION)?;
            transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            transaction.commit()?;
        }

        let store = Self { connection };
        store.recover_interrupted_work()?;
        Ok(store)
    }

    fn recover_interrupted_work(&self) -> Result<(), StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE transfer_jobs SET status='retry_wait', next_retry_at=datetime('now'), \
             last_error=COALESCE(last_error, 'recovered after interrupted process'), updated_at=datetime('now') \
             WHERE status='running'",
            [],
        )?;
        transaction.execute(
            "UPDATE backup_runs SET status='reconcile_required', finished_at=NULL, \
             error_summary='recovered after interrupted process' WHERE status IN ('running','scanning','backing_up')",
            [],
        )?;
        transaction.execute(
            "UPDATE restore_audit SET status='failed',finished_at_ms=?1, \
             error_summary=COALESCE(error_summary,'restore interrupted by process exit') \
             WHERE status='running'",
            params![current_time_ms()?],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn register_task(
        &self,
        task_id: &str,
        name: &str,
        enabled: bool,
    ) -> Result<(), StorageError> {
        validate_task_id(task_id)?;
        if name.trim().is_empty() {
            return Err(StorageError::InvalidTaskId(task_id.to_owned()));
        }
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO backup_tasks(task_id,name,enabled,created_at,updated_at) \
             VALUES(?1,?2,?3,datetime('now'),datetime('now')) \
             ON CONFLICT(task_id) DO UPDATE SET name=excluded.name, enabled=excluded.enabled, updated_at=datetime('now')",
            params![task_id, name, enabled],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn begin_backup_run(&self, task_id: &str) -> Result<String, StorageError> {
        let run_id = next_id("run");
        self.connection.execute(
            "INSERT INTO backup_runs(run_id,task_id,started_at,status) VALUES(?1,?2,datetime('now'),'running')",
            params![run_id, task_id],
        )?;
        Ok(run_id)
    }

    pub fn finish_backup_run(
        &self,
        run_id: &str,
        status: BackupRunStatus,
        scanned_files: u64,
        changed_files: u64,
        error_summary: Option<&str>,
    ) -> Result<(), StorageError> {
        let scanned_files =
            i64::try_from(scanned_files).map_err(|_| StorageError::CountOverflow)?;
        let changed_files =
            i64::try_from(changed_files).map_err(|_| StorageError::CountOverflow)?;
        let changed = self.connection.execute(
            "UPDATE backup_runs SET finished_at=datetime('now'),status=?2,scanned_files=?3, \
             changed_files=?4,error_summary=?5 WHERE run_id=?1",
            params![
                run_id,
                status.as_str(),
                scanned_files,
                changed_files,
                error_summary
            ],
        )?;
        if changed == 0 {
            return Err(StorageError::MissingRun(run_id.to_owned()));
        }
        Ok(())
    }

    pub fn record_file_seen(
        &self,
        task_id: &str,
        run_id: &str,
        observation: FileObservation<'_>,
    ) -> Result<IndexedFile, StorageError> {
        let FileObservation {
            relative_path_key,
            display_relative_path,
            size_bytes,
            modified_at_unix_ms,
            blake3_hex,
        } = observation;
        validate_path_key(relative_path_key)?;
        validate_display_path(display_relative_path)?;
        validate_hash(blake3_hex)?;
        let size = i64::try_from(size_bytes)
            .map_err(|_| StorageError::InvalidDisplayPath(display_relative_path.to_owned()))?;
        let transaction = self.connection.unchecked_transaction()?;
        let existing: Option<(i64, i64, String, String)> = transaction
            .query_row(
                "SELECT file_id,source_revision,display_relative_path,COALESCE(blake3_hex,'') \
                 FROM files WHERE task_id=?1 AND relative_path_key=?2",
                params![task_id, relative_path_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;

        let indexed = if let Some((file_id, revision, old_display, old_hash)) = existing {
            let seen_in_this_run: bool = transaction.query_row(
                "SELECT last_seen_run=?2 FROM files WHERE file_id=?1",
                params![file_id, run_id],
                |row| row.get(0),
            )?;
            if seen_in_this_run && old_display != display_relative_path {
                return Err(StorageError::PathCollision(relative_path_key.to_owned()));
            }
            let changed = old_hash != blake3_hex;
            let new_revision = if changed {
                revision
                    .checked_add(1)
                    .ok_or(StorageError::RevisionOverflow)?
                    .max(1)
            } else {
                revision.max(1)
            };
            transaction.execute(
                "UPDATE files SET display_relative_path=?2,size_bytes=?3,modified_at=?4,blake3_hex=?5, \
                 source_revision=?6,source_missing=0,last_seen_run=?7 WHERE file_id=?1",
                params![file_id, display_relative_path, size, modified_at_unix_ms.to_string(), blake3_hex, new_revision, run_id],
            )?;
            IndexedFile {
                file_id,
                source_revision: new_revision,
                changed,
            }
        } else {
            transaction.execute(
                "INSERT INTO files(task_id,relative_path_key,display_relative_path,size_bytes,modified_at,blake3_hex, \
                 source_revision,source_missing,last_seen_run) VALUES(?1,?2,?3,?4,?5,?6,1,0,?7)",
                params![task_id, relative_path_key, display_relative_path, size, modified_at_unix_ms.to_string(), blake3_hex, run_id],
            )?;
            IndexedFile {
                file_id: transaction.last_insert_rowid(),
                source_revision: 1,
                changed: true,
            }
        };
        transaction.commit()?;
        Ok(indexed)
    }

    pub fn indexed_file_state(
        &self,
        task_id: &str,
        relative_path_key: &str,
    ) -> Result<Option<IndexedFileState>, StorageError> {
        validate_path_key(relative_path_key)?;
        let row: Option<IndexedFileRow> = self
            .connection
            .query_row(
                "SELECT file_id,source_revision,display_relative_path,size_bytes,blake3_hex \
                 FROM files WHERE task_id=?1 AND relative_path_key=?2",
                params![task_id, relative_path_key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        row.map(
            |(file_id, source_revision, display_relative_path, size, content_hash)| {
                Ok(IndexedFileState {
                    file_id,
                    source_revision,
                    relative_path_key: relative_path_key.to_owned(),
                    display_relative_path,
                    size_bytes: size
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|_| StorageError::CountOverflow)?,
                    content_hash,
                })
            },
        )
        .transpose()
    }

    pub fn unseen_files(
        &self,
        task_id: &str,
        run_id: &str,
    ) -> Result<Vec<UnseenFile>, StorageError> {
        let mut statement = self.connection.prepare(
            "SELECT file_id,relative_path_key,display_relative_path FROM files \
             WHERE task_id=?1 AND COALESCE(last_seen_run,'')<>?2 ORDER BY file_id",
        )?;
        let rows = statement.query_map(params![task_id, run_id], |row| {
            Ok(UnseenFile {
                file_id: row.get(0)?,
                relative_path_key: row.get(1)?,
                display_relative_path: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    pub fn mark_source_missing(&self, file_id: i64) -> Result<(), StorageError> {
        self.connection.execute(
            "UPDATE files SET source_missing=1 WHERE file_id=?1",
            params![file_id],
        )?;
        Ok(())
    }

    pub fn destination_revision(
        &self,
        file_id: i64,
        destination: &str,
    ) -> Result<i64, StorageError> {
        self.connection
            .query_row(
                "SELECT committed_revision FROM destination_states WHERE file_id=?1 AND destination=?2",
                params![file_id, destination],
                |row| row.get(0),
            )
            .optional()
            .map(|revision| revision.unwrap_or(0))
            .map_err(StorageError::from)
    }

    pub fn destination_checksum(
        &self,
        file_id: i64,
        destination: &str,
    ) -> Result<Option<String>, StorageError> {
        let checksum: Option<Option<String>> = self
            .connection
            .query_row(
                "SELECT checksum_info FROM destination_states WHERE file_id=?1 AND destination=?2",
                params![file_id, destination],
                |row| row.get(0),
            )
            .optional()?;
        Ok(checksum.flatten())
    }

    pub fn has_recoverable_history_version(
        &self,
        file_id: i64,
        revision: i64,
    ) -> Result<bool, StorageError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM file_versions WHERE file_id=?1 AND source_revision=?2 \
             AND storage_target='local' AND status='recoverable')",
            params![file_id, revision],
            |row| row.get(0),
        )?)
    }

    pub fn enqueue_local_copy(&self, file_id: i64, revision: i64) -> Result<String, StorageError> {
        let job_id = format!("{file_id}:{revision}:local:copy");
        self.connection.execute(
            "INSERT INTO transfer_jobs(job_id,file_id,source_revision,destination,operation,status,created_at,updated_at) \
             VALUES(?1,?2,?3,'local','copy','pending',datetime('now'),datetime('now')) \
             ON CONFLICT(file_id,source_revision,destination,operation) DO UPDATE SET \
             status=CASE WHEN transfer_jobs.status='failed' OR \
             (transfer_jobs.status='retry_wait' AND COALESCE(transfer_jobs.next_retry_at,'')<=datetime('now')) \
             THEN 'pending' ELSE transfer_jobs.status END, \
             next_retry_at=NULL,last_error=NULL,updated_at=datetime('now')",
            params![job_id, file_id, revision],
        )?;
        Ok(job_id)
    }

    pub fn start_job(&self, job_id: &str) -> Result<(), StorageError> {
        let changed = self.connection.execute(
            "UPDATE transfer_jobs SET status='running',attempts=attempts+1,updated_at=datetime('now') \
             WHERE job_id=?1 AND (status='pending' OR \
             (status='retry_wait' AND COALESCE(next_retry_at,'')<=datetime('now'))) ",
            params![job_id],
        )?;
        if changed == 0 {
            return Err(StorageError::InvalidJobState(job_id.to_owned()));
        }
        Ok(())
    }

    pub fn retry_job(&self, job_id: &str, error: &str) -> Result<(), StorageError> {
        let transaction = self.connection.unchecked_transaction()?;
        let changed = transaction.execute(
            "UPDATE transfer_jobs SET status='retry_wait',next_retry_at=datetime('now','+1 minute'), \
             last_error=?2,updated_at=datetime('now') WHERE job_id=?1 AND status='running'",
            params![job_id, error],
        )?;
        if changed == 0 {
            return Err(StorageError::InvalidJobState(job_id.to_owned()));
        }
        transaction.execute(
            "INSERT INTO destination_states(file_id,destination,committed_revision,last_error,updated_at) \
             SELECT file_id,'local',0,?2,datetime('now') FROM transfer_jobs WHERE job_id=?1 \
             ON CONFLICT(file_id,destination) DO UPDATE SET last_error=excluded.last_error,updated_at=datetime('now')",
            params![job_id, error],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn commit_local_if_current<F>(
        &mut self,
        job_id: &str,
        file_id: i64,
        revision: i64,
        checksum: &str,
        target_file_id: &str,
        replace_target: F,
    ) -> Result<CommitOutcome, StorageError>
    where
        F: FnOnce() -> io::Result<()>,
    {
        validate_hash(checksum)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !is_job_current(&transaction, job_id, file_id, revision)? {
            transaction.execute(
                "UPDATE transfer_jobs SET status='failed',last_error='stale source revision superseded',updated_at=datetime('now') WHERE job_id=?1",
                params![job_id],
            )?;
            transaction.commit()?;
            return Ok(CommitOutcome::Stale);
        }

        replace_target()?;
        transaction.execute(
            "INSERT INTO destination_states(file_id,destination,committed_revision,target_file_id,checksum_info,last_error,updated_at) \
             VALUES(?1,'local',?2,?3,?4,NULL,datetime('now')) \
             ON CONFLICT(file_id,destination) DO UPDATE SET committed_revision=excluded.committed_revision, \
             target_file_id=excluded.target_file_id,checksum_info=excluded.checksum_info,last_error=NULL,updated_at=datetime('now') \
             WHERE destination_states.committed_revision<=excluded.committed_revision",
            params![file_id, revision, target_file_id, checksum],
        )?;
        transaction.execute(
            "UPDATE transfer_jobs SET status='succeeded',last_error=NULL,updated_at=datetime('now') WHERE job_id=?1",
            params![job_id],
        )?;
        transaction.commit()?;
        Ok(CommitOutcome::Committed)
    }

    #[cfg(test)]
    fn job_status(&self, job_id: &str) -> Result<String, StorageError> {
        Ok(self.connection.query_row(
            "SELECT status FROM transfer_jobs WHERE job_id=?1",
            params![job_id],
            |row| row.get(0),
        )?)
    }
}

impl HistoryRepository {
    /// Opens or initializes a managed history root without following reparse points.
    /// The caller is responsible for invoking this only after the backup target was confirmed.
    pub fn open_or_create(root: &Path) -> Result<Self, StorageError> {
        ensure_safe_directory_tree(root)?;
        let root = fs::canonicalize(root)?;
        let objects = root.join("objects");
        ensure_safe_directory_tree(&objects)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Saves a verified immutable object and then commits its manifest as recoverable.
    /// No database row becomes recoverable until the object has been flushed and verified.
    #[allow(clippy::too_many_arguments)]
    pub fn save_version(
        &self,
        store: &mut StateStore,
        file_id: i64,
        source_revision: i64,
        relative_path_key: &str,
        display_relative_path: &str,
        content_hash: &str,
        size_bytes: u64,
        snapshot_path: &Path,
    ) -> Result<HistoryVersion, StorageError> {
        if source_revision <= 0 {
            return Err(StorageError::VersionConflict(format!(
                "{file_id}:{source_revision}"
            )));
        }
        validate_path_key(relative_path_key)?;
        validate_display_path(display_relative_path)?;
        validate_hash(content_hash)?;
        let size = i64::try_from(size_bytes).map_err(|_| StorageError::CountOverflow)?;
        verify_file(snapshot_path, content_hash, size_bytes)?;

        let storage_ref = object_storage_ref(content_hash);
        let object_path = self
            .root
            .join(storage_ref.replace('/', std::path::MAIN_SEPARATOR_STR));
        let object_parent = object_path
            .parent()
            .ok_or_else(|| StorageError::InvalidHistoryRoot(object_path.clone()))?;
        ensure_safe_directory_tree(object_parent)?;
        materialize_object(snapshot_path, &object_path, content_hash, size_bytes)?;

        let version_id = format!("local:{file_id}:{source_revision}");
        let created_at_ms = current_time_ms()?;
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let file_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM files WHERE file_id=?1)",
            params![file_id],
            |row| row.get(0),
        )?;
        if !file_exists {
            return Err(StorageError::VersionNotRecoverable(version_id));
        }

        let existing: Option<(String, i64)> = transaction
            .query_row(
                "SELECT content_hash,size_bytes FROM file_versions WHERE version_id=?1",
                params![version_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if existing
            .as_ref()
            .is_some_and(|(old_hash, old_size)| old_hash != content_hash || *old_size != size)
        {
            return Err(StorageError::VersionConflict(version_id));
        }

        transaction.execute(
            "INSERT INTO history_objects(content_hash,storage_ref,size_bytes,created_at_ms,status) \
             VALUES(?1,?2,?3,?4,'recoverable') ON CONFLICT(content_hash) DO UPDATE SET \
             storage_ref=excluded.storage_ref,size_bytes=excluded.size_bytes,status='recoverable'",
            params![content_hash, storage_ref, size, created_at_ms],
        )?;
        transaction.execute(
            "INSERT INTO file_versions(version_id,file_id,source_revision,storage_target,storage_ref, \
             content_hash,size_bytes,created_at,status,is_latest) \
             VALUES(?1,?2,?3,'local',?4,?5,?6,datetime('now'),'recoverable',0) \
             ON CONFLICT(version_id) DO UPDATE SET storage_ref=excluded.storage_ref, \
             content_hash=excluded.content_hash,size_bytes=excluded.size_bytes,status='recoverable'",
            params![version_id, file_id, source_revision, storage_ref, content_hash, size],
        )?;
        transaction.execute(
            "INSERT INTO history_manifests(manifest_id,relative_path_key,display_relative_path,created_at_ms,status) \
             VALUES(?1,?2,?3,?4,'recoverable') ON CONFLICT(manifest_id) DO UPDATE SET \
             relative_path_key=excluded.relative_path_key,display_relative_path=excluded.display_relative_path, \
             status='recoverable'",
            params![version_id, relative_path_key, display_relative_path, created_at_ms],
        )?;
        transaction.execute(
            "INSERT INTO history_object_refs(manifest_id,content_hash) VALUES(?1,?2) \
             ON CONFLICT(manifest_id) DO UPDATE SET content_hash=excluded.content_hash",
            params![version_id, content_hash],
        )?;

        let latest_revision: Option<i64> = transaction.query_row(
            "SELECT max(source_revision) FROM file_versions WHERE file_id=?1 AND storage_target='local' \
             AND status='recoverable' AND is_latest=1",
            params![file_id],
            |row| row.get(0),
        )?;
        let is_latest = latest_revision.map_or(true, |latest| source_revision >= latest);
        if is_latest {
            transaction.execute(
                "UPDATE file_versions SET is_latest=0 WHERE file_id=?1 AND storage_target='local' \
                 AND status='recoverable' AND version_id<>?2",
                params![file_id, version_id],
            )?;
            transaction.execute(
                "UPDATE file_versions SET is_latest=1 WHERE version_id=?1 AND status='recoverable'",
                params![version_id],
            )?;
        }
        transaction.commit()?;

        Ok(HistoryVersion {
            version_id,
            file_id,
            source_revision,
            relative_path_key: relative_path_key.to_owned(),
            display_relative_path: display_relative_path.to_owned(),
            content_hash: content_hash.to_owned(),
            size_bytes,
            created_at_unix_ms: created_at_ms,
            is_latest,
        })
    }

    pub fn list_file_versions(
        &self,
        store: &StateStore,
        file_id: i64,
    ) -> Result<Vec<HistoryVersion>, StorageError> {
        let mut statement = store.connection.prepare(
            "SELECT v.version_id,v.file_id,v.source_revision,m.relative_path_key, \
             m.display_relative_path,v.content_hash,v.size_bytes,m.created_at_ms,v.is_latest \
             FROM file_versions v JOIN history_manifests m ON m.manifest_id=v.version_id \
             WHERE v.file_id=?1 AND v.storage_target='local' AND v.status='recoverable' \
             AND m.status='recoverable' ORDER BY v.source_revision DESC,v.created_at DESC",
        )?;
        let rows = statement.query_map(params![file_id], |row| {
            let size: i64 = row.get(6)?;
            let size_bytes = u64::try_from(size)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(6, size))?;
            Ok(HistoryVersion {
                version_id: row.get(0)?,
                file_id: row.get(1)?,
                source_revision: row.get(2)?,
                relative_path_key: row.get(3)?,
                display_relative_path: row.get(4)?,
                content_hash: row.get(5)?,
                size_bytes,
                created_at_unix_ms: row.get(7)?,
                is_latest: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StorageError::from)
    }

    /// Restores one version into a new directory by default. Existing directories require
    /// explicit overwrite confirmation; only the corresponding file can be replaced.
    pub fn restore_version(
        &self,
        store: &mut StateStore,
        version_id: &str,
        destination_directory: &Path,
        overwrite_confirmed: bool,
    ) -> Result<RestoreReceipt, StorageError> {
        let destination = canonical_restore_destination(destination_directory)?;
        let restore_id = next_id("restore");
        let (relative_path, content_hash, size_bytes, storage_ref) = {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let row: Option<(String, String, i64, String)> = transaction
                .query_row(
                    "SELECT m.display_relative_path,r.content_hash,o.size_bytes,o.storage_ref \
                     FROM file_versions v \
                     JOIN history_manifests m ON m.manifest_id=v.version_id \
                     JOIN history_object_refs r ON r.manifest_id=m.manifest_id \
                     JOIN history_objects o ON o.content_hash=r.content_hash \
                     WHERE v.version_id=?1 AND v.storage_target='local' AND v.status='recoverable' \
                     AND m.status='recoverable' AND o.status='recoverable'",
                    params![version_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;
            let Some((relative_path, content_hash, size_bytes, storage_ref)) = row else {
                return Err(StorageError::VersionNotRecoverable(version_id.to_owned()));
            };
            transaction.execute(
                "INSERT INTO restore_audit(restore_id,manifest_id,destination_path,overwrite_confirmed, \
                 status,started_at_ms) VALUES(?1,?2,?3,?4,'running',?5)",
                params![
                    restore_id,
                    version_id,
                    destination.to_string_lossy(),
                    overwrite_confirmed,
                    current_time_ms()?
                ],
            )?;
            transaction.commit()?;
            (relative_path, content_hash, size_bytes, storage_ref)
        };

        let restore_result = self.restore_version_inner(
            &destination,
            &relative_path,
            &content_hash,
            u64::try_from(size_bytes)
                .map_err(|_| StorageError::HistoryIntegrity(self.root.clone()))?,
            &storage_ref,
            overwrite_confirmed,
        );
        let finish_time = current_time_ms()?;
        match restore_result {
            Ok(()) => {
                store.connection.execute(
                    "UPDATE restore_audit SET status='succeeded',finished_at_ms=?2,error_summary=NULL \
                     WHERE restore_id=?1 AND status='running'",
                    params![restore_id, finish_time],
                )?;
                Ok(RestoreReceipt {
                    restore_id,
                    version_id: version_id.to_owned(),
                    destination_directory: destination,
                    overwrite_confirmed,
                })
            }
            Err(error) => {
                store.connection.execute(
                    "UPDATE restore_audit SET status='failed',finished_at_ms=?2,error_summary=?3 \
                     WHERE restore_id=?1 AND status='running'",
                    params![restore_id, finish_time, error.to_string()],
                )?;
                Err(error)
            }
        }
    }

    fn restore_version_inner(
        &self,
        destination: &Path,
        relative_path: &str,
        content_hash: &str,
        size_bytes: u64,
        storage_ref: &str,
        overwrite_confirmed: bool,
    ) -> Result<(), StorageError> {
        let object_path = self.object_path(content_hash, storage_ref)?;
        verify_file(&object_path, content_hash, size_bytes)?;
        match fs::symlink_metadata(destination) {
            Ok(metadata) if is_reparse_point(&metadata) || !metadata.is_dir() => {
                return Err(StorageError::InvalidRestoreDirectory(
                    destination.to_path_buf(),
                ));
            }
            Ok(_) if !overwrite_confirmed => {
                return Err(StorageError::RestoreDestinationExists(
                    destination.to_path_buf(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = destination.parent().ok_or_else(|| {
                    StorageError::InvalidRestoreDestination(destination.to_path_buf())
                })?;
                if !parent.is_dir() {
                    return Err(StorageError::InvalidRestoreDestination(
                        destination.to_path_buf(),
                    ));
                }
                let staged = tempfile::tempdir_in(parent)?;
                let staged_file = staged.path().join(relative_path);
                let staged_parent = staged_file
                    .parent()
                    .ok_or_else(|| StorageError::InvalidRestoreDestination(staged_file.clone()))?;
                fs::create_dir_all(staged_parent)?;
                copy_verified_to_new_file(&object_path, &staged_file, content_hash, size_bytes)?;
                let staged_path = staged.path().to_path_buf();
                std::mem::forget(staged);
                match fs::rename(&staged_path, destination) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        let _ = fs::remove_dir_all(&staged_path);
                        return Err(StorageError::Io(error));
                    }
                }
            }
            Err(error) => return Err(StorageError::Io(error)),
        }

        let destination_file = safe_relative_target(destination, relative_path)?;
        let parent = destination_file
            .parent()
            .ok_or_else(|| StorageError::InvalidRestoreTarget(destination_file.clone()))?;
        ensure_child_directories(destination, parent)?;
        match fs::symlink_metadata(&destination_file) {
            Ok(metadata) if is_reparse_point(&metadata) || !metadata.is_file() => {
                return Err(StorageError::InvalidRestoreTarget(destination_file));
            }
            Ok(_) if !overwrite_confirmed => {
                return Err(StorageError::RestoreDestinationExists(destination_file));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(StorageError::Io(error)),
        }
        let mut staged = tempfile::NamedTempFile::new_in(parent)?;
        copy_verified_to_writer(&object_path, staged.as_file_mut(), content_hash, size_bytes)?;
        staged.as_file().sync_all()?;
        match fs::symlink_metadata(&destination_file) {
            Ok(_) if overwrite_confirmed => staged
                .persist(&destination_file)
                .map(|_| ())
                .map_err(|error| StorageError::Io(error.error))?,
            Ok(_) => return Err(StorageError::RestoreDestinationExists(destination_file)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => staged
                .persist_noclobber(&destination_file)
                .map(|_| ())
                .map_err(|error| StorageError::Io(error.error))?,
            Err(error) => return Err(StorageError::Io(error)),
        }
        Ok(())
    }

    fn object_path(&self, content_hash: &str, storage_ref: &str) -> Result<PathBuf, StorageError> {
        validate_hash(content_hash)?;
        let expected_ref = object_storage_ref(content_hash);
        if storage_ref != expected_ref {
            return Err(StorageError::InvalidHistoryRoot(self.root.clone()));
        }
        let path = self
            .root
            .join(expected_ref.replace('/', std::path::MAIN_SEPARATOR_STR));
        let parent = path
            .parent()
            .ok_or_else(|| StorageError::InvalidHistoryRoot(path.clone()))?;
        ensure_existing_directory_tree(parent)?;
        Ok(path)
    }

    /// Removes expired non-latest versions after atomically excluding active restores.
    pub fn collect_expired_before(
        &self,
        store: &mut StateStore,
        cutoff_unix_ms: i64,
    ) -> Result<GcSummary, StorageError> {
        if cutoff_unix_ms < 0 {
            return Err(StorageError::InvalidRetentionCutoff);
        }
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "UPDATE file_versions SET status='gc_pending' WHERE storage_target='local' \
             AND status='recoverable' AND is_latest=0 AND version_id IN ( \
               SELECT m.manifest_id FROM history_manifests m \
               WHERE m.status='recoverable' AND m.created_at_ms<=?1 \
                 AND NOT EXISTS(SELECT 1 FROM restore_audit a \
                   WHERE a.manifest_id=m.manifest_id AND a.status='running'))",
            params![cutoff_unix_ms],
        )?;
        transaction.execute(
            "UPDATE history_manifests SET status='gc_pending' WHERE status='recoverable' \
             AND manifest_id IN (SELECT version_id FROM file_versions WHERE status='gc_pending')",
            [],
        )?;
        transaction.commit()?;

        let mut summary = GcSummary::default();
        self.finalize_gc(store, &mut summary)?;
        Ok(summary)
    }

    fn finalize_gc(
        &self,
        store: &mut StateStore,
        summary: &mut GcSummary,
    ) -> Result<(), StorageError> {
        let pending_hashes = {
            let mut statement = store.connection.prepare(
                "SELECT DISTINCT r.content_hash FROM history_object_refs r \
                 JOIN file_versions v ON v.version_id=r.manifest_id WHERE v.status='gc_pending'",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        for content_hash in pending_hashes {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let active_reference_count: i64 = transaction.query_row(
                "SELECT count(*) FROM history_object_refs r JOIN file_versions v ON v.version_id=r.manifest_id \
                 WHERE r.content_hash=?1 AND v.status IN ('writing','recoverable')",
                params![content_hash],
                |row| row.get(0),
            )?;
            if active_reference_count > 0 {
                let removed = transaction.execute(
                    "UPDATE file_versions SET status='removed',is_latest=0 WHERE status='gc_pending' \
                     AND version_id IN (SELECT manifest_id FROM history_object_refs WHERE content_hash=?1)",
                    params![content_hash],
                )?;
                transaction.execute(
                    "UPDATE history_manifests SET status='removed' WHERE status='gc_pending' \
                     AND manifest_id IN (SELECT manifest_id FROM history_object_refs WHERE content_hash=?1)",
                    params![content_hash],
                )?;
                transaction.execute(
                    "DELETE FROM history_object_refs WHERE content_hash=?1 AND manifest_id IN \
                     (SELECT version_id FROM file_versions WHERE status='removed')",
                    params![content_hash],
                )?;
                transaction.commit()?;
                summary.versions_removed += u64::try_from(removed).unwrap_or_default();
                continue;
            }
            let storage_ref: Option<String> = transaction
                .query_row(
                    "SELECT storage_ref FROM history_objects WHERE content_hash=?1",
                    params![content_hash],
                    |row| row.get(0),
                )
                .optional()?;
            transaction.execute(
                "UPDATE history_objects SET status='gc_pending' WHERE content_hash=?1",
                params![content_hash],
            )?;
            transaction.commit()?;

            if let Some(storage_ref) = storage_ref {
                let object_path = self.object_path(&content_hash, &storage_ref)?;
                match fs::symlink_metadata(&object_path) {
                    Ok(metadata) if is_reparse_point(&metadata) || !metadata.is_file() => {
                        return Err(StorageError::HistoryIntegrity(object_path));
                    }
                    Ok(_) => fs::remove_file(&object_path)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(StorageError::Io(error)),
                }
                summary.objects_removed += 1;
            }

            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            let removed = transaction.execute(
                "UPDATE file_versions SET status='removed',is_latest=0 WHERE status='gc_pending' \
                 AND version_id IN (SELECT manifest_id FROM history_object_refs WHERE content_hash=?1)",
                params![content_hash],
            )?;
            transaction.execute(
                "UPDATE history_manifests SET status='removed' WHERE status='gc_pending' \
                 AND manifest_id IN (SELECT manifest_id FROM history_object_refs WHERE content_hash=?1)",
                params![content_hash],
            )?;
            transaction.execute(
                "DELETE FROM history_object_refs WHERE content_hash=?1 AND manifest_id IN \
                 (SELECT version_id FROM file_versions WHERE status='removed')",
                params![content_hash],
            )?;
            transaction.execute(
                "UPDATE history_objects SET status='removed' WHERE content_hash=?1",
                params![content_hash],
            )?;
            transaction.commit()?;
            summary.versions_removed += u64::try_from(removed).unwrap_or_default();
        }
        Ok(())
    }
}

fn current_time_ms() -> Result<i64, StorageError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StorageError::InvalidRetentionCutoff)?
        .as_millis();
    i64::try_from(millis).map_err(|_| StorageError::CountOverflow)
}

fn object_storage_ref(content_hash: &str) -> String {
    format!(
        "objects/{}/{}/{}",
        &content_hash[..2],
        &content_hash[2..4],
        content_hash
    )
}

fn ensure_safe_directory_tree(path: &Path) -> Result<(), StorageError> {
    walk_directory_tree(path, true)
}

fn ensure_existing_directory_tree(path: &Path) -> Result<(), StorageError> {
    walk_directory_tree(path, false)
}

fn walk_directory_tree(path: &Path, create_missing: bool) -> Result<(), StorageError> {
    if !path.is_absolute() {
        return Err(StorageError::InvalidHistoryRoot(path.to_path_buf()));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => current.push(component.as_os_str()),
            Component::Normal(name) => {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) if is_reparse_point(&metadata) || !metadata.is_dir() => {
                        return Err(StorageError::InvalidHistoryRoot(current));
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound && create_missing => {
                        match fs::create_dir(&current) {
                            Ok(()) => {}
                            Err(create_error)
                                if create_error.kind() == io::ErrorKind::AlreadyExists => {}
                            Err(create_error) => return Err(StorageError::Io(create_error)),
                        }
                        let metadata = fs::symlink_metadata(&current)?;
                        if is_reparse_point(&metadata) || !metadata.is_dir() {
                            return Err(StorageError::InvalidHistoryRoot(current));
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(StorageError::InvalidHistoryRoot(current));
                    }
                    Err(error) => return Err(StorageError::Io(error)),
                }
            }
            Component::CurDir | Component::ParentDir => {
                return Err(StorageError::InvalidHistoryRoot(path.to_path_buf()));
            }
        }
    }
    Ok(())
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn open_read_no_follow(path: &Path) -> io::Result<File> {
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

fn verify_file(path: &Path, expected_hash: &str, expected_size: u64) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(path)?;
    if is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(StorageError::HistoryIntegrity(path.to_path_buf()));
    }
    let mut source = open_read_no_follow(path)?;
    let handle_metadata = source.metadata()?;
    if is_reparse_point(&handle_metadata) || !handle_metadata.is_file() {
        return Err(StorageError::HistoryIntegrity(path.to_path_buf()));
    }
    copy_verified_reader(
        &mut source,
        &mut io::sink(),
        expected_hash,
        expected_size,
        path,
    )
}

fn materialize_object(
    source_path: &Path,
    object_path: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<(), StorageError> {
    match fs::symlink_metadata(object_path) {
        Ok(_) => return verify_file(object_path, expected_hash, expected_size),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(StorageError::Io(error)),
    }

    let parent = object_path
        .parent()
        .ok_or_else(|| StorageError::InvalidHistoryRoot(object_path.to_path_buf()))?;
    let mut staged = NamedTempFile::new_in(parent)?;
    copy_verified_to_writer(
        source_path,
        staged.as_file_mut(),
        expected_hash,
        expected_size,
    )?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(object_path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            verify_file(object_path, expected_hash, expected_size)
        }
        Err(error) => Err(StorageError::Io(error.error)),
    }
}

fn copy_verified_to_new_file(
    source_path: &Path,
    destination_path: &Path,
    expected_hash: &str,
    expected_size: u64,
) -> Result<(), StorageError> {
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination_path)?;
    copy_verified_to_writer(source_path, &mut destination, expected_hash, expected_size)?;
    destination.sync_all()?;
    Ok(())
}

fn copy_verified_to_writer<W: Write>(
    source_path: &Path,
    destination: &mut W,
    expected_hash: &str,
    expected_size: u64,
) -> Result<(), StorageError> {
    let metadata = fs::symlink_metadata(source_path)?;
    if is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(StorageError::HistoryIntegrity(source_path.to_path_buf()));
    }
    let mut source = open_read_no_follow(source_path)?;
    let handle_metadata = source.metadata()?;
    if is_reparse_point(&handle_metadata) || !handle_metadata.is_file() {
        return Err(StorageError::HistoryIntegrity(source_path.to_path_buf()));
    }
    copy_verified_reader(
        &mut source,
        destination,
        expected_hash,
        expected_size,
        source_path,
    )
}

fn copy_verified_reader<R: Read, W: Write>(
    source: &mut R,
    destination: &mut W,
    expected_hash: &str,
    expected_size: u64,
    source_path: &Path,
) -> Result<(), StorageError> {
    let mut hasher = blake3::Hasher::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(u64::try_from(count).map_err(|_| StorageError::CountOverflow)?)
            .ok_or(StorageError::CountOverflow)?;
        hasher.update(&buffer[..count]);
        destination.write_all(&buffer[..count])?;
    }
    if copied != expected_size || hasher.finalize().to_hex().as_str() != expected_hash {
        return Err(StorageError::HistoryIntegrity(source_path.to_path_buf()));
    }
    Ok(())
}

fn canonical_restore_destination(path: &Path) -> Result<PathBuf, StorageError> {
    if !path.is_absolute() {
        return Err(StorageError::InvalidRestoreDestination(path.to_path_buf()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| StorageError::InvalidRestoreDestination(path.to_path_buf()))?;
    let file_name = path
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| StorageError::InvalidRestoreDestination(path.to_path_buf()))?;
    let file_name_text = file_name.to_string_lossy();
    if file_name_text.contains(':')
        || file_name_text.ends_with('.')
        || file_name_text.ends_with(' ')
        || file_name_text.chars().any(char::is_control)
    {
        return Err(StorageError::InvalidRestoreDestination(path.to_path_buf()));
    }
    ensure_existing_directory_tree(parent)
        .map_err(|_| StorageError::InvalidRestoreDestination(path.to_path_buf()))?;
    let canonical_parent = fs::canonicalize(parent)?;
    let destination = canonical_parent.join(file_name);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) if is_reparse_point(&metadata) => {
            Err(StorageError::InvalidRestoreDirectory(destination))
        }
        Ok(_) => Ok(destination),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(destination),
        Err(error) => Err(StorageError::Io(error)),
    }
}

fn safe_relative_target(root: &Path, relative_path: &str) -> Result<PathBuf, StorageError> {
    validate_display_path(relative_path)?;
    let mut target = root.to_path_buf();
    for component in Path::new(relative_path).components() {
        let Component::Normal(name) = component else {
            return Err(StorageError::InvalidRestoreTarget(PathBuf::from(
                relative_path,
            )));
        };
        target.push(name);
    }
    Ok(target)
}

fn ensure_child_directories(root: &Path, parent: &Path) -> Result<(), StorageError> {
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| StorageError::InvalidRestoreDirectory(parent.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(StorageError::InvalidRestoreDirectory(parent.to_path_buf()));
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if is_reparse_point(&metadata) || !metadata.is_dir() => {
                return Err(StorageError::InvalidRestoreDirectory(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
            }
            Err(error) => return Err(StorageError::Io(error)),
        }
    }
    Ok(())
}

fn is_job_current(
    transaction: &Transaction<'_>,
    job_id: &str,
    file_id: i64,
    revision: i64,
) -> Result<bool, StorageError> {
    let job: Option<(i64, i64, String)> = transaction
        .query_row(
            "SELECT file_id,source_revision,status FROM transfer_jobs WHERE job_id=?1",
            params![job_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let current_revision: Option<i64> = transaction
        .query_row(
            "SELECT source_revision FROM files WHERE file_id=?1",
            params![file_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(
        matches!(job, Some((id, job_revision, status)) if id == file_id && job_revision == revision && status == "running")
            && current_revision == Some(revision),
    )
}

fn validate_path_key(key: &str) -> Result<(), StorageError> {
    if key.is_empty()
        || key.contains('\\')
        || key.starts_with('/')
        || key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.contains(':'))
        || key.chars().any(char::is_control)
        || key != key.to_lowercase()
    {
        return Err(StorageError::InvalidPathKey(key.to_owned()));
    }
    Ok(())
}

fn validate_task_id(task_id: &str) -> Result<(), StorageError> {
    let valid = !task_id.is_empty()
        && task_id.len() <= 64
        && task_id.as_bytes()[0].is_ascii_lowercase()
        && task_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
        });
    if valid {
        Ok(())
    } else {
        Err(StorageError::InvalidTaskId(task_id.to_owned()))
    }
}

fn validate_hash(hash: &str) -> Result<(), StorageError> {
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(StorageError::InvalidHash)
    }
}

fn validate_display_path(display: &str) -> Result<(), StorageError> {
    let path = Path::new(display);
    if display.is_empty()
        || path.is_absolute()
        || path.components().any(|part| {
            let Component::Normal(name) = part else {
                return true;
            };
            let name = name.to_string_lossy();
            name.contains(':') || name.ends_with('.') || name.ends_with(' ')
        })
        || display.chars().any(char::is_control)
    {
        return Err(StorageError::InvalidDisplayPath(display.to_owned()));
    }
    Ok(())
}

fn next_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{millis}-{}-{sequence}", std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: &str = concat!(
        "aaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaa",
        "aaaaaaaaaaaaaaaa"
    );
    const HASH_B: &str = concat!(
        "bbbbbbbbbbbbbbbb",
        "bbbbbbbbbbbbbbbb",
        "bbbbbbbbbbbbbbbb",
        "bbbbbbbbbbbbbbbb"
    );
    const HASH_C: &str = concat!(
        "cccccccccccccccc",
        "cccccccccccccccc",
        "cccccccccccccccc",
        "cccccccccccccccc"
    );
    const HASH_D: &str = concat!(
        "dddddddddddddddd",
        "dddddddddddddddd",
        "dddddddddddddddd",
        "dddddddddddddddd"
    );
    const HASH_E: &str = concat!(
        "eeeeeeeeeeeeeeee",
        "eeeeeeeeeeeeeeee",
        "eeeeeeeeeeeeeeee",
        "eeeeeeeeeeeeeeee"
    );

    fn new_store() -> StateStore {
        StateStore::open_in_memory().expect("initialize isolated in-memory database")
    }

    #[test]
    fn migration_creates_schema_and_sets_version() {
        let store = new_store();
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let table_count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN \
             ('files','transfer_jobs','destination_states','history_objects','history_manifests', \
              'history_object_refs','restore_audit')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(table_count, 7);
    }

    #[test]
    fn existing_m1_database_is_upgraded_to_m2() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection
            .pragma_update(None, "user_version", 1_i64)
            .unwrap();
        let store = StateStore::from_connection(connection).unwrap();
        let version: i64 = store
            .connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let history_table_exists: bool = store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='restore_audit')",
            [],
            |row| row.get(0),
        ).unwrap();
        assert!(history_table_exists);
    }

    #[test]
    fn source_revision_changes_only_when_content_changes() {
        let store = new_store();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        let first = store
            .record_file_seen(
                "task",
                &run,
                observation("report.txt", "Report.txt", 3, 1, HASH_A),
            )
            .unwrap();
        let unchanged = store
            .record_file_seen(
                "task",
                &run,
                observation("report.txt", "Report.txt", 3, 2, HASH_A),
            )
            .unwrap();
        assert_eq!((first.source_revision, first.changed), (1, true));
        assert_eq!((unchanged.source_revision, unchanged.changed), (1, false));
        let second_run = store.begin_backup_run("task").unwrap();
        let changed = store
            .record_file_seen(
                "task",
                &second_run,
                observation("report.txt", "Report.txt", 4, 3, HASH_B),
            )
            .unwrap();
        assert_eq!((changed.source_revision, changed.changed), (2, true));
    }

    #[test]
    fn local_commit_is_per_destination_and_completes_queue_job() {
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        let indexed = store
            .record_file_seen("task", &run, observation("a.txt", "a.txt", 1, 1, HASH_C))
            .unwrap();
        let job = store
            .enqueue_local_copy(indexed.file_id, indexed.source_revision)
            .unwrap();
        store.start_job(&job).unwrap();
        let outcome = store
            .commit_local_if_current(&job, indexed.file_id, 1, HASH_C, "a.txt", || Ok(()))
            .unwrap();
        assert_eq!(outcome, CommitOutcome::Committed);
        assert_eq!(
            store
                .destination_revision(indexed.file_id, "local")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .destination_revision(indexed.file_id, "drive")
                .unwrap(),
            0
        );
        assert_eq!(store.job_status(&job).unwrap(), "succeeded");
    }

    #[test]
    fn a_failed_replace_does_not_mark_destination_committed() {
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        let indexed = store
            .record_file_seen("task", &run, observation("a.txt", "a.txt", 1, 1, HASH_C))
            .unwrap();
        let job = store.enqueue_local_copy(indexed.file_id, 1).unwrap();
        store.start_job(&job).unwrap();
        let result =
            store.commit_local_if_current(&job, indexed.file_id, 1, HASH_C, "a.txt", || {
                Err(io::Error::other("simulated replace failure"))
            });
        assert!(result.is_err());
        assert_eq!(
            store
                .destination_revision(indexed.file_id, "local")
                .unwrap(),
            0
        );
        store.retry_job(&job, "simulated replace failure").unwrap();
        assert_eq!(store.job_status(&job).unwrap(), "retry_wait");
        let destination_error: String = store
            .connection
            .query_row(
                "SELECT last_error FROM destination_states WHERE file_id=?1 AND destination='local'",
                params![indexed.file_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(destination_error, "simulated replace failure");
    }

    #[test]
    fn relative_path_keys_reject_traversal_and_case_collisions_are_detected() {
        let store = new_store();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        assert!(matches!(
            store.record_file_seen("task", &run, observation("../x", "../x", 1, 1, HASH_C)),
            Err(StorageError::InvalidPathKey(_))
        ));
        store
            .record_file_seen(
                "task",
                &run,
                observation("readme.txt", "Readme.txt", 1, 1, HASH_C),
            )
            .unwrap();
        assert!(matches!(
            store.record_file_seen(
                "task",
                &run,
                observation("readme.txt", "README.txt", 1, 1, HASH_C)
            ),
            Err(StorageError::PathCollision(_))
        ));
    }

    #[test]
    fn interrupted_runs_and_jobs_are_recovered_for_reconciliation() {
        let directory = tempfile::tempdir().expect("create isolated state directory");
        let database = directory.path().join("state.db");
        let store = StateStore::open(&database).unwrap();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        let indexed = store
            .record_file_seen("task", &run, observation("a.txt", "a.txt", 1, 1, HASH_C))
            .unwrap();
        let job = store.enqueue_local_copy(indexed.file_id, 1).unwrap();
        store.start_job(&job).unwrap();
        drop(store);

        let recovered = StateStore::open(&database).unwrap();
        assert_eq!(recovered.job_status(&job).unwrap(), "retry_wait");
        let run_status: String = recovered
            .connection
            .query_row(
                "SELECT status FROM backup_runs WHERE run_id=?1",
                params![run],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_status, "reconcile_required");
    }

    #[test]
    fn stale_revision_job_cannot_replace_current_target() {
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();
        let first_run = store.begin_backup_run("task").unwrap();
        let indexed = store
            .record_file_seen(
                "task",
                &first_run,
                observation("a.txt", "a.txt", 1, 1, HASH_D),
            )
            .unwrap();
        let job = store.enqueue_local_copy(indexed.file_id, 1).unwrap();
        store.start_job(&job).unwrap();
        let newer_run = store.begin_backup_run("task").unwrap();
        store
            .record_file_seen(
                "task",
                &newer_run,
                observation("a.txt", "a.txt", 2, 2, HASH_E),
            )
            .unwrap();

        let mut replaced = false;
        let outcome = store
            .commit_local_if_current(&job, indexed.file_id, 1, HASH_D, "a.txt", || {
                replaced = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(outcome, CommitOutcome::Stale);
        assert!(!replaced);
        assert_eq!(store.job_status(&job).unwrap(), "failed");
        assert_eq!(
            store
                .destination_revision(indexed.file_id, "local")
                .unwrap(),
            0
        );
    }

    #[test]
    fn history_versions_restore_to_new_directories_and_require_overwrite_confirmation() {
        let temp = tempfile::tempdir().unwrap();
        let repository = HistoryRepository::open_or_create(&temp.path().join("history")).unwrap();
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();

        let (file_id, first_hash) = save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            None,
            "Report.txt",
            b"first revision",
        );
        let (_, second_hash) = save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            Some(file_id),
            "Report.txt",
            b"second revision",
        );
        assert_ne!(first_hash, second_hash);

        let first_restore = temp.path().join("restored-first");
        let receipt = repository
            .restore_version(&mut store, "local:1:1", &first_restore, false)
            .unwrap();
        assert_eq!(
            fs::read(first_restore.join("Report.txt")).unwrap(),
            b"first revision"
        );
        assert!(!receipt.overwrite_confirmed);

        let second_restore = temp.path().join("restored-second");
        repository
            .restore_version(&mut store, "local:1:2", &second_restore, false)
            .unwrap();
        assert_eq!(
            fs::read(second_restore.join("Report.txt")).unwrap(),
            b"second revision"
        );

        let denied = repository.restore_version(&mut store, "local:1:2", &first_restore, false);
        assert!(matches!(
            denied,
            Err(StorageError::RestoreDestinationExists(_))
        ));
        assert_eq!(
            fs::read(first_restore.join("Report.txt")).unwrap(),
            b"first revision"
        );
        repository
            .restore_version(&mut store, "local:1:2", &first_restore, true)
            .unwrap();
        assert_eq!(
            fs::read(first_restore.join("Report.txt")).unwrap(),
            b"second revision"
        );

        let audit_counts: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT count(*),sum(status='succeeded'),sum(status='failed') FROM restore_audit",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(audit_counts, (4, 3, 1));
        assert_eq!(
            repository
                .list_file_versions(&store, file_id)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn retention_gc_keeps_latest_versions_and_objects_with_other_references() {
        let temp = tempfile::tempdir().unwrap();
        let repository = HistoryRepository::open_or_create(&temp.path().join("history")).unwrap();
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();

        let (first_file_id, shared_hash) = save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            None,
            "one.txt",
            b"shared object",
        );
        let second_file_id = store
            .record_file_seen(
                "task",
                &store.begin_backup_run("task").unwrap(),
                observation("two.txt", "two.txt", 13, 1, &shared_hash),
            )
            .unwrap()
            .file_id;
        let second_snapshot = temp.path().join("second-snapshot");
        fs::write(&second_snapshot, b"shared object").unwrap();
        repository
            .save_version(
                &mut store,
                second_file_id,
                1,
                "two.txt",
                "two.txt",
                &shared_hash,
                13,
                &second_snapshot,
            )
            .unwrap();

        save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            Some(first_file_id),
            "one.txt",
            b"new latest object",
        );

        let gc = repository
            .collect_expired_before(&mut store, i64::MAX)
            .unwrap();
        assert_eq!(gc.versions_removed, 1);
        assert_eq!(gc.objects_removed, 0);
        assert!(repository
            .root()
            .join(object_storage_ref(&shared_hash).replace('/', std::path::MAIN_SEPARATOR_STR))
            .is_file());
        assert_eq!(
            repository
                .list_file_versions(&store, first_file_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            repository
                .list_file_versions(&store, second_file_id)
                .unwrap()
                .len(),
            1
        );

        let restore_dir = temp.path().join("shared-object-still-live");
        repository
            .restore_version(&mut store, "local:2:1", &restore_dir, false)
            .unwrap();
        assert_eq!(
            fs::read(restore_dir.join("two.txt")).unwrap(),
            b"shared object"
        );
    }

    #[test]
    fn retention_gc_does_not_remove_a_version_with_an_active_restore() {
        let temp = tempfile::tempdir().unwrap();
        let repository = HistoryRepository::open_or_create(&temp.path().join("history")).unwrap();
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();
        let (file_id, _) = save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            None,
            "item.txt",
            b"old",
        );
        save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            Some(file_id),
            "item.txt",
            b"new",
        );
        store
            .connection
            .execute(
                "INSERT INTO restore_audit(restore_id,manifest_id,destination_path,overwrite_confirmed, \
                 status,started_at_ms) VALUES('active-restore','local:1:1','temporary',0,'running',1)",
                [],
            )
            .unwrap();

        let protected = repository
            .collect_expired_before(&mut store, i64::MAX)
            .unwrap();
        assert_eq!(protected.versions_removed, 0);
        assert_eq!(
            repository
                .list_file_versions(&store, file_id)
                .unwrap()
                .len(),
            2
        );

        store
            .connection
            .execute(
                "UPDATE restore_audit SET status='failed',finished_at_ms=2 WHERE restore_id='active-restore'",
                [],
            )
            .unwrap();
        let collected = repository
            .collect_expired_before(&mut store, i64::MAX)
            .unwrap();
        assert_eq!(collected.versions_removed, 1);
        assert_eq!(collected.objects_removed, 1);
        assert_eq!(
            repository
                .list_file_versions(&store, file_id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT status FROM file_versions WHERE version_id='local:1:2'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "recoverable"
        );
    }

    #[test]
    fn interrupted_restore_is_audited_as_failed_on_database_reopen() {
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("state.db");
        let repository = HistoryRepository::open_or_create(&temp.path().join("history")).unwrap();
        let mut store = StateStore::open(&database).unwrap();
        store.register_task("task", "test", true).unwrap();
        let (file_id, _) = save_fixture_version(
            &repository,
            &mut store,
            temp.path(),
            None,
            "item.txt",
            b"recoverable",
        );
        assert_eq!(file_id, 1);
        store
            .connection
            .execute(
                "INSERT INTO restore_audit(restore_id,manifest_id,destination_path,overwrite_confirmed, \
                 status,started_at_ms) VALUES('interrupted','local:1:1','temporary',0,'running',1)",
                [],
            )
            .unwrap();
        drop(store);

        let reopened = StateStore::open(&database).unwrap();
        let status: String = reopened
            .connection
            .query_row(
                "SELECT status FROM restore_audit WHERE restore_id='interrupted'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }

    #[test]
    fn a_corrupt_snapshot_never_becomes_recoverable() {
        let temp = tempfile::tempdir().unwrap();
        let repository = HistoryRepository::open_or_create(&temp.path().join("history")).unwrap();
        let mut store = new_store();
        store.register_task("task", "test", true).unwrap();
        let run = store.begin_backup_run("task").unwrap();
        let indexed = store
            .record_file_seen(
                "task",
                &run,
                observation("item.txt", "item.txt", 4, 1, HASH_C),
            )
            .unwrap();
        let snapshot = temp.path().join("corrupt-snapshot");
        fs::write(&snapshot, b"wrong").unwrap();

        assert!(matches!(
            repository.save_version(
                &mut store,
                indexed.file_id,
                indexed.source_revision,
                "item.txt",
                "item.txt",
                HASH_C,
                4,
                &snapshot,
            ),
            Err(StorageError::HistoryIntegrity(_))
        ));
        let versions: i64 = store
            .connection
            .query_row("SELECT count(*) FROM file_versions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(versions, 0);
    }

    fn save_fixture_version(
        repository: &HistoryRepository,
        store: &mut StateStore,
        directory: &Path,
        file_id: Option<i64>,
        display_path: &str,
        contents: &[u8],
    ) -> (i64, String) {
        let run = store.begin_backup_run("task").unwrap();
        let hash = blake3::hash(contents).to_hex().to_string();
        let key = display_path.to_lowercase();
        let indexed = store
            .record_file_seen(
                "task",
                &run,
                observation(&key, display_path, contents.len() as u64, 1, &hash),
            )
            .unwrap();
        let file_id = file_id.unwrap_or(indexed.file_id);
        assert_eq!(indexed.file_id, file_id);
        let revision: i64 = store
            .connection
            .query_row(
                "SELECT source_revision FROM files WHERE file_id=?1",
                params![file_id],
                |row| row.get(0),
            )
            .unwrap();
        let snapshot = directory.join(format!("snapshot-{file_id}-{revision}"));
        fs::write(&snapshot, contents).unwrap();
        repository
            .save_version(
                store,
                file_id,
                revision,
                &key,
                display_path,
                &hash,
                contents.len() as u64,
                &snapshot,
            )
            .unwrap();
        (file_id, hash)
    }

    fn observation<'a>(
        relative_path_key: &'a str,
        display_relative_path: &'a str,
        size_bytes: u64,
        modified_at_unix_ms: i64,
        blake3_hex: &'a str,
    ) -> FileObservation<'a> {
        FileObservation {
            relative_path_key,
            display_relative_path,
            size_bytes,
            modified_at_unix_ms,
            blake3_hex,
        }
    }
}
