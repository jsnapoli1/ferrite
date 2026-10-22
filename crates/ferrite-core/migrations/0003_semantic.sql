-- Semantic search: each embedding remembers the full-text row it was computed from, so hits
-- can be resolved (and stale vectors detected) without scanning the FTS table.
ALTER TABLE embeddings ADD COLUMN fts_rowid INTEGER NOT NULL DEFAULT 0;
CREATE INDEX embeddings_model ON embeddings (model);
