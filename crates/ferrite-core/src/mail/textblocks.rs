//! Text blocks: reusable snippets (rich text) inserted in the composer by typing `!` and part
//! of the title, or from the menu. Their owner can share them, read-only, with users and
//! groups.

use super::richtext;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct TextBlock {
    pub id: String,
    pub owner_id: String,
    pub owner_name: String,
    pub title: String,
    pub html: String,
    pub text: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TextBlockShare {
    /// `user` or `group`.
    pub kind: String,
    pub target_id: String,
    pub target_name: String,
}

/// A block as listed for its owner (with shares) or for someone it is shared with.
#[derive(Debug, Clone, Serialize)]
pub struct TextBlockView {
    #[serde(flatten)]
    pub block: TextBlock,
    /// Only the owner may change or share it.
    pub editable: bool,
    pub shares: Vec<TextBlockShare>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TextBlockInput {
    pub title: Option<String>,
    pub html: Option<String>,
}

const COLS: &str = "b.id, b.owner_id, u.display_name AS owner_name, b.title, b.html, b.text, b.created_at, b.updated_at";

fn clean(input: &TextBlockInput, cur: Option<&TextBlock>) -> Result<(String, String, String)> {
    let title = match &input.title {
        Some(t) => t.trim().to_string(),
        None => cur.map(|c| c.title.clone()).unwrap_or_default(),
    };
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Error::bad("a text block needs a title (up to 200 characters)"));
    }
    let html = match &input.html {
        Some(h) => richtext::sanitize(h),
        None => cur.map(|c| c.html.clone()).unwrap_or_default(),
    };
    if !richtext::has_content(&html) {
        return Err(Error::bad("a text block needs some text"));
    }
    if html.len() > 256 * 1024 {
        return Err(Error::bad("text blocks can be at most 256 KB"));
    }
    let text = richtext::html_to_text(&html);
    Ok((title, html, text))
}

impl Core {
    async fn text_block_row(&self, id: &str) -> Result<TextBlock> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM mail_text_blocks b JOIN users u ON u.id = b.owner_id WHERE b.id = ?"
        )))
        .bind(id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(Error::NotFound)
    }

    /// A block owned by `user_id` (NotFound otherwise, also for blocks only shared with them).
    async fn own_text_block(&self, user_id: &str, id: &str) -> Result<TextBlock> {
        let b = self.text_block_row(id).await?;
        if b.owner_id != user_id {
            return Err(Error::NotFound);
        }
        Ok(b)
    }

    async fn text_block_shares(&self, id: &str) -> Result<Vec<TextBlockShare>> {
        let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT s.kind, s.target_id, COALESCE(u.display_name, g.name) FROM mail_text_block_shares s
             LEFT JOIN users u ON s.kind = 'user' AND u.id = s.target_id LEFT JOIN groups g ON s.kind = 'group' AND g.id = s.target_id
             WHERE s.block_id = ? ORDER BY s.kind DESC, 3",
        )
        .bind(id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(kind, target_id, name)| TextBlockShare {
                kind,
                target_name: name.unwrap_or_else(|| target_id.clone()),
                target_id,
            })
            .collect())
    }

    /// The user's own blocks, then the ones shared with them (directly or through a group).
    pub async fn mail_text_blocks(&self, user_id: &str) -> Result<Vec<TextBlockView>> {
        let mine: Vec<TextBlock> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {COLS} FROM mail_text_blocks b JOIN users u ON u.id = b.owner_id WHERE b.owner_id = ? ORDER BY lower(b.title)"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let shared: Vec<TextBlock> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT {COLS} FROM mail_text_blocks b JOIN users u ON u.id = b.owner_id JOIN mail_text_block_shares s ON s.block_id = b.id
             WHERE b.owner_id != ? AND ((s.kind = 'user' AND s.target_id = ?)
                OR (s.kind = 'group' AND s.target_id IN (SELECT group_id FROM group_members WHERE user_id = ?)))
             ORDER BY lower(b.title)"
        )))
        .bind(user_id)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::with_capacity(mine.len() + shared.len());
        for b in mine {
            let shares = self.text_block_shares(&b.id).await?;
            out.push(TextBlockView {
                block: b,
                editable: true,
                shares,
            });
        }
        out.extend(shared.into_iter().map(|b| TextBlockView {
            block: b,
            editable: false,
            shares: vec![],
        }));
        Ok(out)
    }

    pub async fn create_mail_text_block(&self, user_id: &str, input: &TextBlockInput) -> Result<TextBlockView> {
        let (title, html, text) = clean(input, None)?;
        let id = new_id();
        let t = now();
        sqlx::query("INSERT INTO mail_text_blocks (id, owner_id, title, html, text, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(&title)
            .bind(&html)
            .bind(&text)
            .bind(t)
            .bind(t)
            .execute(&self.db)
            .await?;
        Ok(TextBlockView {
            block: self.text_block_row(&id).await?,
            editable: true,
            shares: vec![],
        })
    }

    pub async fn update_mail_text_block(&self, user_id: &str, id: &str, input: &TextBlockInput) -> Result<TextBlockView> {
        let cur = self.own_text_block(user_id, id).await?;
        let (title, html, text) = clean(input, Some(&cur))?;
        sqlx::query("UPDATE mail_text_blocks SET title = ?, html = ?, text = ?, updated_at = ? WHERE id = ?")
            .bind(&title)
            .bind(&html)
            .bind(&text)
            .bind(now())
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(TextBlockView {
            block: self.text_block_row(id).await?,
            editable: true,
            shares: self.text_block_shares(id).await?,
        })
    }

    pub async fn delete_mail_text_block(&self, user_id: &str, id: &str) -> Result<()> {
        self.own_text_block(user_id, id).await?;
        sqlx::query("DELETE FROM mail_text_blocks WHERE id = ?").bind(id).execute(&self.db).await?;
        Ok(())
    }

    /// Shares a block read-only with a user (by username) or a group (by name).
    pub async fn share_mail_text_block(&self, user_id: &str, id: &str, kind: &str, target: &str) -> Result<Vec<TextBlockShare>> {
        self.own_text_block(user_id, id).await?;
        let target = target.trim();
        let target_id = match kind {
            "user" => {
                let u = self.user_by_name(target).await.map_err(|_| Error::bad(format!("no user named {target}")))?;
                if u.id == user_id {
                    return Err(Error::bad("you already own this text block"));
                }
                u.id
            }
            "group" => self
                .list_groups()
                .await?
                .into_iter()
                .find(|g| g.name == target || g.id == target)
                .map(|g| g.id)
                .ok_or_else(|| Error::bad(format!("no group named {target}")))?,
            _ => return Err(Error::bad("share with a user or a group")),
        };
        sqlx::query("INSERT OR IGNORE INTO mail_text_block_shares (block_id, kind, target_id) VALUES (?, ?, ?)")
            .bind(id)
            .bind(kind)
            .bind(&target_id)
            .execute(&self.db)
            .await?;
        self.text_block_shares(id).await
    }

    pub async fn unshare_mail_text_block(&self, user_id: &str, id: &str, kind: &str, target_id: &str) -> Result<Vec<TextBlockShare>> {
        self.own_text_block(user_id, id).await?;
        sqlx::query("DELETE FROM mail_text_block_shares WHERE block_id = ? AND kind = ? AND target_id = ?")
            .bind(id)
            .bind(kind)
            .bind(target_id)
            .execute(&self.db)
            .await?;
        self.text_block_shares(id).await
    }
}
