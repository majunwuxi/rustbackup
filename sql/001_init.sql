PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS backup_tasks (
  task_id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS files (
  file_id INTEGER PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES backup_tasks(task_id),
  relative_path_key TEXT NOT NULL,
  display_relative_path TEXT NOT NULL,
  size_bytes INTEGER,
  modified_at TEXT,
  blake3_hex TEXT,
  source_revision INTEGER NOT NULL DEFAULT 0,
  source_missing INTEGER NOT NULL DEFAULT 0,
  UNIQUE(task_id, relative_path_key)
);

CREATE TABLE IF NOT EXISTS file_versions (
  version_id TEXT PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(file_id),
  source_revision INTEGER NOT NULL,
  storage_target TEXT NOT NULL CHECK(storage_target IN ('local','drive')),
  storage_ref TEXT NOT NULL,
  content_hash TEXT NOT NULL,
  size_bytes INTEGER NOT NULL,
  created_at TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('writing','recoverable','gc_pending','removed')),
  is_latest INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS transfer_jobs (
  job_id TEXT PRIMARY KEY,
  file_id INTEGER NOT NULL REFERENCES files(file_id),
  source_revision INTEGER NOT NULL,
  destination TEXT NOT NULL CHECK(destination IN ('local','drive')),
  operation TEXT NOT NULL CHECK(operation IN ('upload','restore','copy','verify')),
  status TEXT NOT NULL CHECK(status IN ('pending','running','retry_wait','succeeded','failed')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_retry_at TEXT,
  transfer_session TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(file_id, source_revision, destination, operation)
);

CREATE TABLE IF NOT EXISTS destination_states (
  file_id INTEGER NOT NULL REFERENCES files(file_id),
  destination TEXT NOT NULL CHECK(destination IN ('local','drive')),
  committed_revision INTEGER NOT NULL DEFAULT 0,
  target_file_id TEXT,
  checksum_info TEXT,
  last_error TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(file_id, destination)
);

CREATE TABLE IF NOT EXISTS backup_runs (
  run_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES backup_tasks(task_id),
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL,
  scanned_files INTEGER NOT NULL DEFAULT 0,
  changed_files INTEGER NOT NULL DEFAULT 0,
  error_summary TEXT
);
CREATE INDEX IF NOT EXISTS idx_transfer_jobs_status_retry ON transfer_jobs(status, next_retry_at);
CREATE INDEX IF NOT EXISTS idx_versions_retention ON file_versions(status, created_at);
