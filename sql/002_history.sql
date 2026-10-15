CREATE TABLE IF NOT EXISTS history_objects (
  content_hash TEXT PRIMARY KEY CHECK(length(content_hash)=64),
  storage_ref TEXT NOT NULL UNIQUE,
  size_bytes INTEGER NOT NULL CHECK(size_bytes>=0),
  created_at_ms INTEGER NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('writing','recoverable','gc_pending','removed'))
);

CREATE TABLE IF NOT EXISTS history_manifests (
  manifest_id TEXT PRIMARY KEY REFERENCES file_versions(version_id),
  relative_path_key TEXT NOT NULL,
  display_relative_path TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('writing','recoverable','gc_pending','removed'))
);

CREATE TABLE IF NOT EXISTS history_object_refs (
  manifest_id TEXT PRIMARY KEY REFERENCES history_manifests(manifest_id),
  content_hash TEXT NOT NULL REFERENCES history_objects(content_hash)
);

CREATE TABLE IF NOT EXISTS restore_audit (
  restore_id TEXT PRIMARY KEY,
  manifest_id TEXT NOT NULL REFERENCES history_manifests(manifest_id),
  destination_path TEXT NOT NULL,
  overwrite_confirmed INTEGER NOT NULL CHECK(overwrite_confirmed IN (0,1)),
  status TEXT NOT NULL CHECK(status IN ('running','succeeded','failed')),
  started_at_ms INTEGER NOT NULL,
  finished_at_ms INTEGER,
  error_summary TEXT
);

CREATE INDEX IF NOT EXISTS idx_history_manifests_retention
  ON history_manifests(status, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_history_refs_hash ON history_object_refs(content_hash);
CREATE INDEX IF NOT EXISTS idx_restore_audit_active ON restore_audit(manifest_id, status);
