//! Persistent SQLite state for source revisions and per-destination transfers.

use std::fs;
use std::io;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use thiserror::Error;

const INITIAL_MIGRATION: &str = include_str!("../../../sql/001_init.sql");
const SCHEMA_VERSION: i64 = 1;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedFile {
    pub file_id: i64,
    pub source_revision: i64,
    pub changed: bool,
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

        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(StorageError::UnsupportedSchema(version));
        }
        if version == 0 {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(INITIAL_MIGRATION)?;
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
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
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
        let table_count: i64 = store.connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('files','transfer_jobs','destination_states')",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        assert_eq!(table_count, 3);
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
