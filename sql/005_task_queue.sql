CREATE TABLE IF NOT EXISTS task_run_queue (
  run_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL REFERENCES backup_tasks(task_id),
  requested_at TEXT NOT NULL,
  started_at TEXT,
  finished_at TEXT,
  status TEXT NOT NULL CHECK(status IN ('queued','running','completed')),
  result_status TEXT CHECK(result_status IS NULL OR result_status IN
    ('succeeded','partial_success','waiting_auth','paused','retry_wait','error')),
  error_summary TEXT
);

CREATE INDEX IF NOT EXISTS idx_task_run_queue_pending
  ON task_run_queue(task_id,status,requested_at);
