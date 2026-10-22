//! Semantic search. When an administrator assigns a model to the "embed" AI role, a background
//! indexer embeds every row of the full-text index (files' text, chat messages, cards, events,
//! contacts), and searches combine full-text and vector-similarity results with reciprocal
//! rank fusion. Without an embedding model, search is plain full text.
//!
//! The indexer follows the FTS rowid as a cursor for new rows. Because SQLite may reuse the
//! highest rowid after the newest row is replaced, each pass also re-checks a window behind
//! the cursor, and an hourly pass reconciles everything by content hash (only changed text is
//! re-embedded) and drops vectors of deleted rows or of a previous model.

use crate::ai::provider_for_role;
use ferrite_ai::Provider;
use ferrite_core::search::{SearchHit, vector_bytes};
use ferrite_core::util::sha256_hex;
use ferrite_core::{Core, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Rows embedded per request to the provider.
const BATCH: i64 = 32;
/// How far behind the cursor each pass re-checks (see the module docs).
const RECHECK_WINDOW: i64 = 256;
/// Text sent per row; enough for a meaningful vector without huge requests.
const MAX_CHARS: usize = 4000;
/// Similarity below which a semantic neighbour is not considered a match.
const MIN_SCORE: f32 = 0.25;
/// Reciprocal rank fusion constant (Cormack et al., 2009).
const RRF_K: f32 = 60.0;

pub struct Semantic {
    core: Core,
}

impl Semantic {
    pub fn new(core: Core) -> Arc<Self> {
        Arc::new(Semantic { core })
    }

    async fn embedder(&self) -> Option<(Provider, String)> {
        provider_for_role(&self.core, "embed").await.ok()
    }

    /// Full-text search, fused with semantic results when an embedding model is configured.
    /// Returns the hits and whether semantic matching took part.
    pub async fn search(&self, user_id: &str, query: &str, kinds: Option<&[&str]>, limit: usize) -> Result<(Vec<SearchHit>, bool)> {
        let text = self.core.search(user_id, query, kinds, limit * 2).await?;
        let Some((provider, model)) = self.embedder().await else {
            return Ok((truncate(text, limit), false));
        };
        let q = query.trim();
        if q.is_empty() {
            return Ok((truncate(text, limit), false));
        }
        let embedded = tokio::time::timeout(Duration::from_secs(5), provider.embed(&model, &[q.to_string()])).await;
        let qv = match embedded {
            Ok(Ok(mut v)) if v.first().is_some_and(|v| !v.is_empty()) => v.swap_remove(0),
            Ok(Ok(_)) => return Ok((truncate(text, limit), false)),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "query embedding failed; using full-text search only");
                return Ok((truncate(text, limit), false));
            }
            Err(_) => {
                tracing::warn!("query embedding timed out; using full-text search only");
                return Ok((truncate(text, limit), false));
            }
        };
        let candidates: Vec<_> = self
            .core
            .semantic_candidates(user_id, &model, &qv, limit * 2)
            .await?
            .into_iter()
            .filter(|c| c.score >= MIN_SCORE)
            .collect();
        let semantic = self.core.semantic_hits(user_id, &candidates, kinds).await?;
        Ok((fuse(text, semantic, limit), true))
    }

    /// Runs the indexer for the life of the process.
    pub fn spawn_indexer(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut last_full = Instant::now() - Duration::from_secs(3600);
            loop {
                if let Some((provider, model)) = this.embedder().await {
                    let full = last_full.elapsed() >= Duration::from_secs(3600);
                    match this.sync(&provider, &model, full).await {
                        Ok(n) => {
                            if full {
                                last_full = Instant::now();
                            }
                            if n > 0 {
                                tracing::info!(embedded = n, model = %model, "semantic index updated");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "semantic indexing failed; will retry"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }

    /// Brings the embeddings for `model` up to date. A full pass reconciles every row and
    /// removes stale vectors; otherwise only rows past (or just behind) the cursor are seen.
    /// Returns how many rows were embedded.
    pub async fn sync(&self, provider: &Provider, model: &str, full: bool) -> Result<usize> {
        let db = &self.core.db;
        let cursor_key = format!("embed_cursor:{model}");
        let cursor: i64 = sqlx::query_scalar::<_, String>("SELECT value FROM kv WHERE key = ?")
            .bind(&cursor_key)
            .fetch_optional(db)
            .await?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if full {
            sqlx::query("DELETE FROM embeddings WHERE model != ? OR fts_rowid NOT IN (SELECT rowid FROM fts)")
                .bind(model)
                .execute(db)
                .await?;
        }
        let mut after = if full { 0 } else { (cursor - RECHECK_WINDOW).max(0) };
        let mut embedded = 0;
        loop {
            let rows: Vec<(i64, String, String, String, String, String)> =
                sqlx::query_as("SELECT rowid, object_kind, object_id, scope, title, body FROM fts WHERE rowid > ? ORDER BY rowid LIMIT ?")
                    .bind(after)
                    .bind(BATCH)
                    .fetch_all(db)
                    .await?;
            let Some(last) = rows.last().map(|r| r.0) else { break };
            embedded += self.index_rows(provider, model, rows).await?;
            after = last;
            if last > cursor {
                sqlx::query("INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
                    .bind(&cursor_key)
                    .bind(last.to_string())
                    .execute(db)
                    .await?;
            }
        }
        Ok(embedded)
    }

    /// Embeds the rows whose text changed (or is new); re-points unchanged ones at their row.
    async fn index_rows(&self, provider: &Provider, model: &str, rows: Vec<(i64, String, String, String, String, String)>) -> Result<usize> {
        let db = &self.core.db;
        let mut todo = Vec::new();
        for (rowid, kind, id, scope, title, body) in rows {
            let mut text = format!("{title}\n\n{body}");
            if let Some((cut, _)) = text.char_indices().nth(MAX_CHARS) {
                text.truncate(cut);
            }
            if text.trim().is_empty() {
                continue;
            }
            let hash = sha256_hex(text.as_bytes());
            let existing: Option<(String, String, i64)> =
                sqlx::query_as("SELECT model, content_hash, fts_rowid FROM embeddings WHERE object_kind = ? AND object_id = ?")
                    .bind(&kind)
                    .bind(&id)
                    .fetch_optional(db)
                    .await?;
            match existing {
                Some((m, h, r)) if m == model && h == hash => {
                    if r != rowid {
                        sqlx::query("UPDATE embeddings SET fts_rowid = ?, owner_id = ? WHERE object_kind = ? AND object_id = ?")
                            .bind(rowid)
                            .bind(&scope)
                            .bind(&kind)
                            .bind(&id)
                            .execute(db)
                            .await?;
                    }
                }
                _ => todo.push((rowid, kind, id, scope, text, hash)),
            }
        }
        if todo.is_empty() {
            return Ok(0);
        }
        let inputs: Vec<String> = todo.iter().map(|t| t.4.clone()).collect();
        let vectors = provider
            .embed(model, &inputs)
            .await
            .map_err(|e| ferrite_core::Error::Unavailable(format!("embedding failed: {e}")))?;
        let mut done = 0;
        for ((rowid, kind, id, scope, _, hash), v) in todo.into_iter().zip(vectors) {
            if v.is_empty() {
                continue;
            }
            sqlx::query(
                "INSERT INTO embeddings (object_kind, object_id, owner_id, model, vector, content_hash, fts_rowid) VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(object_kind, object_id) DO UPDATE SET owner_id = excluded.owner_id, model = excluded.model,
                   vector = excluded.vector, content_hash = excluded.content_hash, fts_rowid = excluded.fts_rowid",
            )
            .bind(&kind)
            .bind(&id)
            .bind(&scope)
            .bind(model)
            .bind(vector_bytes(&v))
            .bind(&hash)
            .bind(rowid)
            .execute(db)
            .await?;
            done += 1;
        }
        Ok(done)
    }
}

fn truncate(mut v: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    v.truncate(limit);
    v
}

/// Reciprocal rank fusion: each list contributes 1/(k + rank) per hit; hits found by both
/// rank highest. Ties keep full-text order.
fn fuse(text: Vec<SearchHit>, semantic: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    let mut scores: HashMap<(String, String), f32> = HashMap::new();
    let mut order: Vec<SearchHit> = Vec::new();
    for list in [text, semantic] {
        for (rank, hit) in list.into_iter().enumerate() {
            let key = (hit.kind.clone(), hit.id.clone());
            let s = scores.entry(key).or_insert(0.0);
            if *s == 0.0 {
                order.push(hit);
            }
            *s += 1.0 / (RRF_K + rank as f32 + 1.0);
        }
    }
    let score = |h: &SearchHit| scores.get(&(h.kind.clone(), h.id.clone())).copied().unwrap_or(0.0);
    order.sort_by(|a, b| score(b).total_cmp(&score(a)));
    order.truncate(limit);
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str) -> SearchHit {
        SearchHit {
            kind: "file".into(),
            id: id.into(),
            title: id.into(),
            snippet: String::new(),
            context: None,
        }
    }

    #[test]
    fn fusion_prefers_hits_found_by_both() {
        let fused = fuse(vec![hit("a"), hit("b")], vec![hit("c"), hit("b")], 10);
        let ids: Vec<&str> = fused.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, ["b", "a", "c"]);
        assert_eq!(fuse(vec![hit("a"), hit("b")], vec![], 1).len(), 1);
    }
}
