//! Unified search over files, chat messages, cards, events and contacts: full text (SQLite
//! FTS5) and, when an embedding model is configured, semantic similarity over vectors of the
//! same rows. Results are permission-filtered for the requesting user.

use crate::files::perm;
use crate::{Core, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub snippet: String,
    /// Where to open it: a room token, board id or folder id.
    pub context: Option<String>,
}

/// What a user can see, loaded once per search.
struct Access {
    rooms: Vec<String>,
    boards: Vec<String>,
}

/// One full-text row that semantic search found similar to the query.
#[derive(Debug, Clone)]
pub struct SemanticCandidate {
    pub object_kind: String,
    pub object_id: String,
    pub fts_rowid: i64,
    pub score: f32,
}

/// Cosine similarity of two vectors (0 when either is empty or zero).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Encodes a vector for the `embeddings.vector` column (little-endian f32).
pub fn vector_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

pub fn vector_from_bytes(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

/// Turns free text into a safe FTS5 query: each word becomes a quoted prefix term.
fn fts_query(q: &str) -> Option<String> {
    let terms: Vec<String> = q
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .filter(|t| !t.is_empty())
        .take(12)
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect();
    if terms.is_empty() { None } else { Some(terms.join(" ")) }
}

impl Core {
    pub async fn search(&self, user_id: &str, q: &str, kinds: Option<&[&str]>, limit: usize) -> Result<Vec<SearchHit>> {
        let Some(query) = fts_query(q) else {
            return Ok(vec![]);
        };
        let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT object_kind, object_id, scope, title, snippet(fts, 4, '[', ']', '…', 12) FROM fts WHERE fts MATCH ? ORDER BY rank LIMIT 200",
        )
        .bind(&query)
        .fetch_all(&self.db)
        .await?;
        let access = self.access(user_id).await?;
        let mut hits = Vec::new();
        for (kind, id, scope, title, snippet) in rows {
            if kinds.is_some_and(|k| !k.contains(&kind.as_str())) {
                continue;
            }
            if let Some(hit) = self.visible_hit(user_id, &access, kind, id, scope, title, snippet).await? {
                hits.push(hit);
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }

    async fn access(&self, user_id: &str) -> Result<Access> {
        let rooms: Vec<String> = sqlx::query_scalar("SELECT room_id FROM participants WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&self.db)
            .await?;
        Ok(Access {
            rooms,
            boards: self.accessible_board_ids(user_id).await?,
        })
    }

    /// The hit for one indexed row if `user_id` may see it.
    #[allow(clippy::too_many_arguments)]
    async fn visible_hit(
        &self,
        user_id: &str,
        access: &Access,
        kind: String,
        id: String,
        scope: String,
        title: String,
        snippet: String,
    ) -> Result<Option<SearchHit>> {
        let visible = match kind.as_str() {
            "file" => match self.node(&id).await {
                Ok(n) if n.trashed_at.is_none() => self.permissions(user_id, &n).await? & perm::READ != 0,
                _ => false,
            },
            "message" => access.rooms.contains(&scope),
            "card" => access.boards.contains(&scope),
            _ => scope == user_id,
        };
        if !visible {
            return Ok(None);
        }
        let context = match kind.as_str() {
            "message" => {
                sqlx::query_scalar("SELECT token FROM rooms WHERE id = ?")
                    .bind(&scope)
                    .fetch_optional(&self.db)
                    .await?
            }
            "card" => Some(scope.clone()),
            "file" => self.node(&id).await.ok().and_then(|n| n.parent_id),
            _ => None,
        };
        Ok(Some(SearchHit {
            kind,
            id,
            title,
            snippet,
            context,
        }))
    }

    /// The `limit` indexed rows most similar to `query` among those embedded with `model`
    /// that `user_id` could plausibly see (rooms, boards, own objects and all files; files
    /// are permission-checked when resolved). Vectors are streamed, not loaded at once.
    pub async fn semantic_candidates(&self, user_id: &str, model: &str, query: &[f32], limit: usize) -> Result<Vec<SemanticCandidate>> {
        use futures::TryStreamExt;
        let access = self.access(user_id).await?;
        let mut scopes = access.rooms.clone();
        scopes.extend(access.boards.iter().cloned());
        scopes.push(user_id.to_string());
        let scopes = serde_json::to_string(&scopes).map_err(|e| crate::Error::internal(e.to_string()))?;
        let mut rows = sqlx::query_as::<_, (String, String, i64, Vec<u8>)>(
            "SELECT object_kind, object_id, fts_rowid, vector FROM embeddings WHERE model = ? AND (object_kind = 'file' OR owner_id IN (SELECT value FROM json_each(?)))",
        )
        .bind(model)
        .bind(&scopes)
        .fetch(&self.db);
        let mut best: Vec<SemanticCandidate> = Vec::with_capacity(limit + 1);
        while let Some((object_kind, object_id, fts_rowid, bytes)) = rows.try_next().await? {
            let score = cosine(query, &vector_from_bytes(&bytes));
            if best.len() < limit || best.last().is_some_and(|b| score > b.score) {
                let at = best.partition_point(|b| b.score >= score);
                best.insert(
                    at,
                    SemanticCandidate {
                        object_kind,
                        object_id,
                        fts_rowid,
                        score,
                    },
                );
                best.truncate(limit);
            }
        }
        Ok(best)
    }

    /// Resolves semantic candidates to permission-checked hits, best first.
    pub async fn semantic_hits(&self, user_id: &str, candidates: &[SemanticCandidate], kinds: Option<&[&str]>) -> Result<Vec<SearchHit>> {
        let access = self.access(user_id).await?;
        let mut hits = Vec::new();
        for c in candidates {
            let row: Option<(String, String, String, String, String)> = sqlx::query_as(
                "SELECT object_kind, object_id, scope, title, substr(body, 1, 160) FROM fts WHERE rowid = ? AND object_kind = ? AND object_id = ?",
            )
            .bind(c.fts_rowid)
            .bind(&c.object_kind)
            .bind(&c.object_id)
            .fetch_optional(&self.db)
            .await?;
            // A missing row was deleted or re-indexed since (its rowid may even belong to
            // another object now); the indexer catches up.
            let Some((kind, id, scope, title, excerpt)) = row else { continue };
            if kinds.is_some_and(|k| !k.contains(&kind.as_str())) {
                continue;
            }
            let snippet = excerpt.split_whitespace().collect::<Vec<_>>().join(" ");
            if let Some(hit) = self.visible_hit(user_id, &access, kind, id, scope, title, snippet).await? {
                hits.push(hit);
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn vectors_round_trip_and_compare() {
        let v = vec![0.5f32, -1.25, 3.0];
        assert_eq!(super::vector_from_bytes(&super::vector_bytes(&v)), v);
        assert!((super::cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(super::cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(super::cosine(&[1.0], &[1.0, 2.0]), 0.0);
    }

    #[test]
    fn query_is_sanitized() {
        assert_eq!(super::fts_query("hello wor\"ld").as_deref(), Some("\"hello\"* \"wor\"* \"ld\"*"));
        assert_eq!(super::fts_query("  ;; "), None);
    }
}
