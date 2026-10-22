//! Unified full text search over files, chat messages, cards and documents (SQLite FTS5).
//! Results are permission-filtered for the requesting user.

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
        let Some(query) = fts_query(q) else { return Ok(vec![]) };
        let rows: Vec<(String, String, String, String, String)> = sqlx::query_as(
            "SELECT object_kind, object_id, scope, title, snippet(fts, 4, '[', ']', '…', 12) FROM fts WHERE fts MATCH ? ORDER BY rank LIMIT 200",
        )
        .bind(&query)
        .fetch_all(&self.db)
        .await?;
        let rooms: Vec<String> = sqlx::query_scalar("SELECT room_id FROM participants WHERE user_id = ?").bind(user_id).fetch_all(&self.db).await?;
        let boards = self.accessible_board_ids(user_id).await?;
        let mut hits = Vec::new();
        for (kind, id, scope, title, snippet) in rows {
            if kinds.is_some_and(|k| !k.contains(&kind.as_str())) {
                continue;
            }
            let visible = match kind.as_str() {
                "file" => match self.node(&id).await {
                    Ok(n) if n.trashed_at.is_none() => self.permissions(user_id, &n).await? & perm::READ != 0,
                    _ => false,
                },
                "message" => rooms.contains(&scope),
                "card" => boards.contains(&scope),
                _ => scope == user_id,
            };
            if !visible {
                continue;
            }
            let context = match kind.as_str() {
                "message" => sqlx::query_scalar("SELECT token FROM rooms WHERE id = ?").bind(&scope).fetch_optional(&self.db).await?,
                "card" => Some(scope.clone()),
                "file" => self.node(&id).await.ok().and_then(|n| n.parent_id),
                _ => None,
            };
            hits.push(SearchHit { kind, id, title, snippet, context });
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn query_is_sanitized() {
        assert_eq!(super::fts_query("hello wor\"ld").as_deref(), Some("\"hello\"* \"wor\"* \"ld\"*"));
        assert_eq!(super::fts_query("  ;; "), None);
    }
}
