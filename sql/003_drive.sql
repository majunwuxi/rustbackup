CREATE TABLE IF NOT EXISTS drive_task_bindings (
  task_id TEXT PRIMARY KEY REFERENCES backup_tasks(task_id) ON DELETE CASCADE,
  account_id TEXT NOT NULL,
  root_folder_id TEXT NOT NULL,
  task_folder_id TEXT NOT NULL,
  history_folder_id TEXT NOT NULL,
  folder_name TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS drive_history_manifests (
  manifest_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES backup_tasks(task_id) ON DELETE CASCADE,
  file_id INTEGER NOT NULL REFERENCES files(file_id),
  source_revision INTEGER NOT NULL CHECK(source_revision>0),
  drive_file_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  relative_path_key TEXT NOT NULL,
  display_relative_path TEXT NOT NULL,
  content_hash TEXT NOT NULL CHECK(length(content_hash)=64),
  size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
  created_at_ms INTEGER NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('recoverable','removed')),
  UNIQUE(file_id, source_revision)
);

CREATE INDEX IF NOT EXISTS idx_drive_history_file_revision
  ON drive_history_manifests(file_id, status, source_revision DESC);
CREATE INDEX IF NOT EXISTS idx_drive_history_retention
  ON drive_history_manifests(status, created_at_ms);

CREATE TABLE IF NOT EXISTS drive_restore_audit (
  restore_id TEXT PRIMARY KEY,
  manifest_id TEXT NOT NULL REFERENCES drive_history_manifests(manifest_id),
  destination_path TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('running','succeeded','failed')),
  started_at_ms INTEGER NOT NULL,
  finished_at_ms INTEGER,
  error_summary TEXT
);

CREATE INDEX IF NOT EXISTS idx_drive_restore_active
  ON drive_restore_audit(manifest_id, status);
