//! Sharing: with users, with groups, and public links (optionally password protected and
//! expiring). Permission bits match Nextcloud's so mental models carry over.

use crate::files::{Node, perm};
use crate::util::{hash_password, new_id, now, sha256_hex, short_token, split_path, verify_password};
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Share {
    pub id: String,
    pub node_id: String,
    pub owner_id: String,
    pub kind: String,
    pub target_user: Option<String>,
    pub target_group: Option<String>,
    pub target_room: Option<String>,
    pub token: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    pub permissions: i64,
    pub label: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

const SHARE_COLS: &str =
    "id, node_id, owner_id, kind, target_user, target_group, target_room, token, password_hash, permissions, label, expires_at, created_at";

impl Share {
    pub fn has_password(&self) -> bool {
        self.password_hash.is_some()
    }
    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|e| e <= now())
    }
}

/// Share as returned by the API, with display information resolved.
#[derive(Debug, Clone, Serialize)]
pub struct ShareView {
    #[serde(flatten)]
    pub share: Share,
    pub has_password: bool,
    pub node_name: String,
    pub node_kind: String,
    pub target_name: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewShare {
    /// `user`, `group` or `link`.
    pub kind: String,
    /// Username for `user` shares, group name for `group` shares.
    pub target: Option<String>,
    pub permissions: Option<i64>,
    pub password: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ShareUpdate {
    pub permissions: Option<i64>,
    /// `Some("")` removes the password.
    pub password: Option<String>,
    pub expires_at: Option<Option<DateTime<Utc>>>,
    pub label: Option<String>,
}

impl Core {
    async fn share(&self, id: &str) -> Result<Share> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {SHARE_COLS} FROM shares WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    pub async fn share_view(&self, share: Share) -> Result<ShareView> {
        let node = self.node(&share.node_id).await?;
        let target_name = if let Some(u) = &share.target_user {
            Some(self.user(u).await?.display_name)
        } else if let Some(g) = &share.target_group {
            sqlx::query_scalar("SELECT name FROM groups WHERE id = ?")
                .bind(g)
                .fetch_optional(&self.db)
                .await?
        } else if let Some(r) = &share.target_room {
            sqlx::query_scalar("SELECT name FROM rooms WHERE id = ?")
                .bind(r)
                .fetch_optional(&self.db)
                .await?
        } else {
            None
        };
        let url = share.token.as_ref().map(|t| format!("{}/s/{t}", self.config.public_url));
        Ok(ShareView {
            has_password: share.has_password(),
            node_name: node.name,
            node_kind: node.kind,
            target_name,
            url,
            share,
        })
    }

    pub async fn create_share(&self, user_id: &str, node_id: &str, ns: NewShare) -> Result<ShareView> {
        let (node, my_perms) = self.node_for(user_id, node_id, perm::SHARE).await?;
        if node.parent_id.is_none() {
            return Err(Error::bad("the root folder cannot be shared"));
        }
        let default = if node.is_dir() { perm::ALL } else { perm::READ | perm::UPDATE | perm::SHARE };
        let mut permissions = ns.permissions.unwrap_or(default) & my_perms;
        if !node.is_dir() {
            permissions &= !(perm::CREATE | perm::DELETE);
        }
        if permissions & perm::READ == 0 {
            return Err(Error::bad("a share must at least allow reading"));
        }
        let (mut target_user, mut target_group, mut target_room, mut token, mut password_hash) = (None, None, None, None, None);
        match ns.kind.as_str() {
            "user" => {
                let name = ns.target.as_deref().ok_or_else(|| Error::bad("target username required"))?;
                let target = self.user_by_name(name).await.map_err(|_| Error::bad(format!("no user named {name}")))?;
                if target.id == node.owner_id || target.id == user_id {
                    return Err(Error::bad("cannot share with the owner"));
                }
                target_user = Some(target.id);
            }
            "group" => {
                let name = ns.target.as_deref().ok_or_else(|| Error::bad("target group required"))?;
                let gid: String = sqlx::query_scalar("SELECT id FROM groups WHERE name = ?")
                    .bind(name)
                    .fetch_optional(&self.db)
                    .await?
                    .ok_or_else(|| Error::bad(format!("no group named {name}")))?;
                target_group = Some(gid);
            }
            "room" => {
                let room_token = ns.target.as_deref().ok_or_else(|| Error::bad("target room token required"))?;
                let room = self.room_for(user_id, room_token).await?;
                target_room = Some(room.id);
            }
            "link" => {
                token = Some(short_token(15));
                if let Some(pw) = ns.password.as_deref().filter(|p| !p.is_empty()) {
                    password_hash = Some(hash_password(pw)?);
                }
                // Links never grant re-sharing.
                permissions &= !perm::SHARE;
            }
            other => return Err(Error::bad(format!("unknown share kind {other}"))),
        }
        if ns.expires_at.is_some_and(|e| e <= now()) {
            return Err(Error::bad("expiry must be in the future"));
        }
        let id = new_id();
        sqlx::query(
            "INSERT INTO shares (id, node_id, owner_id, kind, target_user, target_group, target_room, token, password_hash, permissions, label, expires_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&node.id)
        .bind(user_id)
        .bind(&ns.kind)
        .bind(&target_user)
        .bind(&target_group)
        .bind(&target_room)
        .bind(&token)
        .bind(&password_hash)
        .bind(permissions)
        .bind(&ns.label)
        .bind(ns.expires_at)
        .bind(now())
        .execute(&self.db)
        .await?;
        let share = self.share(&id).await?;
        let sharer = self.user(user_id).await?;
        // Room shares are announced in the chat itself instead of as notifications.
        let recipients: Vec<String> = match (&share.target_user, &share.target_group) {
            (Some(u), _) => vec![u.clone()],
            (_, Some(g)) => self.group_members(g).await?.into_iter().map(|u| u.id).filter(|u| u != user_id).collect(),
            _ => vec![],
        };
        for r in &recipients {
            self.notify(
                r,
                "share",
                &format!("{} shared “{}” with you", sharer.display_name, node.name),
                "",
                &format!("/files?node={}", node.id),
            )
            .await?;
        }
        self.record_activity(
            &[user_id.to_string()],
            Some(user_id),
            "shared",
            "file",
            &node.id,
            &format!("Shared {} ({})", node.name, share.kind),
        )
        .await?;
        self.share_view(share).await
    }

    pub async fn shares_of_node(&self, user_id: &str, node_id: &str) -> Result<Vec<ShareView>> {
        self.node_for(user_id, node_id, perm::READ).await?;
        let rows: Vec<Share> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SHARE_COLS} FROM shares WHERE node_id = ? AND (owner_id = ? OR (SELECT owner_id FROM nodes WHERE id = node_id) = ?) ORDER BY created_at"
        )))
        .bind(node_id)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::new();
        for s in rows {
            out.push(self.share_view(s).await?);
        }
        Ok(out)
    }

    pub async fn outgoing_shares(&self, user_id: &str) -> Result<Vec<ShareView>> {
        let rows: Vec<Share> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SHARE_COLS} FROM shares WHERE owner_id = ? ORDER BY created_at DESC"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::new();
        for s in rows {
            out.push(self.share_view(s).await?);
        }
        Ok(out)
    }

    pub async fn incoming_shares(&self, user_id: &str) -> Result<Vec<ShareView>> {
        let rows: Vec<Share> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SHARE_COLS} FROM shares WHERE kind != 'link'
               AND (target_user = ?1 OR target_group IN (SELECT group_id FROM group_members WHERE user_id = ?1)
                    OR target_room IN (SELECT room_id FROM participants WHERE user_id = ?1))
             ORDER BY created_at DESC"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::new();
        for s in rows.into_iter().filter(|s| !s.expired()) {
            out.push(self.share_view(s).await?);
        }
        Ok(out)
    }

    async fn share_for_manage(&self, user_id: &str, share_id: &str) -> Result<Share> {
        let share = self.share(share_id).await?;
        let node = self.node(&share.node_id).await?;
        if share.owner_id != user_id && node.owner_id != user_id {
            return Err(Error::NotFound);
        }
        Ok(share)
    }

    pub async fn update_share(&self, user_id: &str, share_id: &str, up: ShareUpdate) -> Result<ShareView> {
        let share = self.share_for_manage(user_id, share_id).await?;
        let node = self.node(&share.node_id).await?;
        let my_perms = self.permissions(user_id, &node).await?;
        if let Some(mut p) = up.permissions {
            p &= my_perms;
            if !node.is_dir() {
                p &= !(perm::CREATE | perm::DELETE);
            }
            if share.kind == "link" {
                p &= !perm::SHARE;
            }
            if p & perm::READ == 0 {
                return Err(Error::bad("a share must at least allow reading"));
            }
            sqlx::query("UPDATE shares SET permissions = ? WHERE id = ?")
                .bind(p)
                .bind(share_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(pw) = up.password {
            if share.kind != "link" {
                return Err(Error::bad("only link shares have passwords"));
            }
            let h = if pw.is_empty() { None } else { Some(hash_password(&pw)?) };
            sqlx::query("UPDATE shares SET password_hash = ? WHERE id = ?")
                .bind(h)
                .bind(share_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(exp) = up.expires_at {
            sqlx::query("UPDATE shares SET expires_at = ? WHERE id = ?")
                .bind(exp)
                .bind(share_id)
                .execute(&self.db)
                .await?;
        }
        if let Some(l) = up.label {
            sqlx::query("UPDATE shares SET label = ? WHERE id = ?")
                .bind(l)
                .bind(share_id)
                .execute(&self.db)
                .await?;
        }
        let s = self.share(share_id).await?;
        self.share_view(s).await
    }

    pub async fn delete_share(&self, user_id: &str, share_id: &str) -> Result<()> {
        let share = self.share(share_id).await?;
        let node = self.node(&share.node_id).await?;
        // Owners and sharers can delete; a recipient may also leave a share.
        let is_recipient = share.target_user.as_deref() == Some(user_id);
        if share.owner_id != user_id && node.owner_id != user_id && !is_recipient {
            return Err(Error::NotFound);
        }
        sqlx::query("DELETE FROM shares WHERE id = ?").bind(share_id).execute(&self.db).await?;
        Ok(())
    }

    // ---- public links --------------------------------------------------------

    /// Looks up an active link share by its token.
    pub async fn link_share(&self, token: &str) -> Result<Share> {
        let s: Share = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {SHARE_COLS} FROM shares WHERE token = ? AND kind = 'link'"
        )))
        .bind(token)
        .fetch_optional(&self.db)
        .await?
        .ok_or(Error::NotFound)?;
        let node = self.node(&s.node_id).await?;
        if s.expired() || node.trashed_at.is_some() {
            return Err(Error::NotFound);
        }
        Ok(s)
    }

    pub fn check_link_password(&self, share: &Share, password: &str) -> bool {
        share.password_hash.as_deref().is_none_or(|h| verify_password(password, h))
    }

    /// Opaque proof that a visitor entered the link password, stored in a cookie. It changes
    /// whenever the password changes, which invalidates earlier unlocks.
    pub fn link_unlock_proof(&self, share: &Share) -> String {
        sha256_hex(format!("{}:{}:{}", self.secret, share.id, share.password_hash.as_deref().unwrap_or("")).as_bytes())
    }

    /// Resolves a path below a link share's root.
    pub async fn link_node(&self, share: &Share, subpath: &str) -> Result<Node> {
        let mut cur = self.node(&share.node_id).await?;
        for part in split_path(subpath)? {
            if !cur.is_dir() {
                return Err(Error::NotFound);
            }
            cur = sqlx::query_as(
                "SELECT id, owner_id, parent_id, name, kind, blob_hash, size, mime, etag, created_at, modified_at, trashed_at, favorite
                 FROM nodes WHERE parent_id = ? AND name = ? AND trashed_at IS NULL",
            )
            .bind(&cur.id)
            .bind(part)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)?;
        }
        Ok(cur)
    }

    /// Children of a folder previously resolved with [`Core::link_node`].
    pub async fn link_children(&self, dir: &Node) -> Result<Vec<Node>> {
        Ok(sqlx::query_as(
            "SELECT id, owner_id, parent_id, name, kind, blob_hash, size, mime, etag, created_at, modified_at, trashed_at, favorite
             FROM nodes WHERE parent_id = ? AND trashed_at IS NULL ORDER BY kind = 'file', name COLLATE NOCASE",
        )
        .bind(&dir.id)
        .fetch_all(&self.db)
        .await?)
    }
}
