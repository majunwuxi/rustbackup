CREATE TABLE IF NOT EXISTS drive_history_gc_claims (
  manifest_id TEXT PRIMARY KEY REFERENCES drive_history_manifests(manifest_id),
  claimed_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_drive_history_gc_lease
  ON drive_history_gc_claims(claimed_at_ms);
