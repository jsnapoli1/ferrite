//! The file tree: folders, files, versions, trash, locks and share-aware access control.
//!
//! Every user owns one root folder. Files shared with a user appear as extra entries in the
//! user's root ("mounts"), exactly like Nextcloud, so paths such as `/Projects/plan.md` work
//! for both owners and recipients over WebDAV, MCP and the REST API.

use crate::blobs::{BlobStore, StoredBlob};
use crate::util::{guess_mime, new_id, now, short_token, split_path, validate_name};
use crate::{Core, Error, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Sqlite, Transaction};
use tokio::io::AsyncRead;

pub mod perm {
    pub const READ: i64 = 1;
    pub const UPDATE: i64 = 2;
    pub const CREATE: i64 = 4;
    pub const DELETE: i64 = 8;
    pub const SHARE: i64 = 16;
    pub const ALL: i64 = 31;
}

/// Largest file whose text is put in the full text index.
const MAX_INDEXED_TEXT: i64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Node {
    pub id: String,
    pub owner_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub kind: String,
    #[serde(skip_serializing)]
    pub blob_hash: Option<String>,
    pub size: i64,
    pub mime: String,
    pub etag: String,
    pub created_at: DateTime<Utc>,
    pub modified_at: DateTime<Utc>,
    pub trashed_at: Option<DateTime<Utc>>,
    pub favorite: bool,
}

const NODE_COLS: &str = "id, owner_id, parent_id, name, kind, blob_hash, size, mime, etag, created_at, modified_at, trashed_at, favorite";

impl Node {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }
}

/// A node as seen by a particular user.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    #[serde(flatten)]
    pub node: Node,
    pub permissions: i64,
    /// Display name of the owner when the entry is shared with (not owned by) the viewer.
    pub shared_by: Option<String>,
    pub lock: Option<LockInfo>,
    /// Craft app that edits this file natively, if any.
    pub craft_app: Option<&'static str>,
    /// Built-in live editor for this file, if any (`text`, `sheet`, `mindmap`, `canvas`).
    pub live_editor: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct LockInfo {
    pub node_id: String,
    pub user_id: String,
    pub owner_app: String,
    #[serde(skip_serializing)]
    pub token: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Version {
    pub id: String,
    pub node_id: String,
    pub size: i64,
    pub author_id: Option<String>,
    pub label: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct WriteOpts {
    /// Fail with `PreconditionFailed` unless the current etag matches (optimistic concurrency).
    pub if_match: Option<String>,
    /// Fail with `Conflict` if the file exists.
    pub create_only: bool,
    /// Keep the previous content as a version (default for user writes).
    pub keep_version: bool,
    /// Lock token that allows writing a locked file.
    pub lock_token: Option<String>,
    pub mime: Option<String>,
    /// Set by live editing sessions writing back their own document.
    pub from_collab: bool,
}

impl WriteOpts {
    pub fn user() -> Self {
        Self {
            keep_version: true,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Quota {
    pub used: i64,
    pub limit: Option<i64>,
}

fn etag() -> String {
    short_token(12)
}

impl Core {
    // ---- lookup & access -------------------------------------------------

    pub async fn ensure_root(&self, user_id: &str) -> Result<String> {
        if let Some(id) = sqlx::query_scalar::<_, String>("SELECT id FROM nodes WHERE owner_id = ? AND parent_id IS NULL")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
        {
            return Ok(id);
        }
        let id = new_id();
        let t = now();
        sqlx::query("INSERT INTO nodes (id, owner_id, parent_id, name, kind, mime, etag, created_at, modified_at) VALUES (?, ?, NULL, '', 'dir', 'inode/directory', ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(etag())
            .bind(t)
            .bind(t)
            .execute(&self.db)
            .await?;
        Ok(id)
    }

    pub async fn node(&self, id: &str) -> Result<Node> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {NODE_COLS} FROM nodes WHERE id = ?")))
            .bind(id)
            .fetch_one(&self.db)
            .await?)
    }

    /// Effective permissions of `user_id` on `node`: everything for the owner, otherwise the
    /// union of all unexpired user/group shares on the node or any ancestor.
    pub async fn permissions(&self, user_id: &str, node: &Node) -> Result<i64> {
        if node.owner_id == user_id {
            return Ok(perm::ALL);
        }
        let rows: Vec<(i64, Option<DateTime<Utc>>)> = sqlx::query_as(
            "WITH RECURSIVE anc(id, parent_id) AS (
                 SELECT id, parent_id FROM nodes WHERE id = ?1
                 UNION ALL SELECT n.id, n.parent_id FROM nodes n JOIN anc a ON n.id = a.parent_id)
             SELECT s.permissions, s.expires_at FROM shares s
             WHERE s.node_id IN (SELECT id FROM anc)
               AND (s.target_user = ?2 OR s.target_group IN (SELECT group_id FROM group_members WHERE user_id = ?2)
                    OR s.target_room IN (SELECT room_id FROM participants WHERE user_id = ?2))",
        )
        .bind(&node.id)
        .bind(user_id)
        .fetch_all(&self.db)
        .await?;
        let t = now();
        Ok(rows.iter().filter(|(_, exp)| exp.is_none_or(|e| e > t)).fold(0, |acc, (p, _)| acc | p))
    }

    /// Loads a node and checks that `user_id` holds `need` on it. Hidden nodes report NotFound.
    pub async fn node_for(&self, user_id: &str, id: &str, need: i64) -> Result<(Node, i64)> {
        let node = self.node(id).await?;
        if node.trashed_at.is_some() && node.owner_id != user_id {
            return Err(Error::NotFound);
        }
        let p = self.permissions(user_id, &node).await?;
        if p & perm::READ == 0 {
            return Err(Error::NotFound);
        }
        if p & need != need {
            return Err(Error::Forbidden);
        }
        Ok((node, p))
    }

    async fn child(&self, parent_id: &str, name: &str) -> Result<Option<Node>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {NODE_COLS} FROM nodes WHERE parent_id = ? AND name = ? AND trashed_at IS NULL"
        )))
        .bind(parent_id)
        .bind(name)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Nodes shared directly with the user (shown in the user's root).
    async fn share_mounts(&self, user_id: &str) -> Result<Vec<Node>> {
        let cols = NODE_COLS.split(", ").map(|c| format!("n.{c}")).collect::<Vec<_>>().join(", ");
        let rows: Vec<Node> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT {cols} FROM nodes n JOIN shares s ON s.node_id = n.id
             WHERE n.trashed_at IS NULL AND n.owner_id != ?1 AND s.kind != 'link'
               AND (s.expires_at IS NULL OR s.expires_at > ?2)
               AND (s.target_user = ?1 OR s.target_group IN (SELECT group_id FROM group_members WHERE user_id = ?1)
                    OR s.target_room IN (SELECT room_id FROM participants WHERE user_id = ?1))
             ORDER BY n.name"
        )))
        .bind(user_id)
        .bind(now())
        .fetch_all(&self.db)
        .await?;
        Ok(rows)
    }

    /// Resolves a slash separated path in the user's namespace.
    pub async fn resolve_path(&self, user_id: &str, path: &str) -> Result<Node> {
        let parts = split_path(path)?;
        let root = self.ensure_root(user_id).await?;
        let mut cur = self.node(&root).await?;
        for (i, part) in parts.iter().enumerate() {
            let next = match self.child(&cur.id, part).await? {
                Some(n) => Some(n),
                None if i == 0 => self.share_mounts(user_id).await?.into_iter().find(|n| n.name == *part),
                None => None,
            };
            cur = next.ok_or(Error::NotFound)?;
        }
        if self.permissions(user_id, &cur).await? & perm::READ == 0 {
            return Err(Error::NotFound);
        }
        Ok(cur)
    }

    /// Accepts either a node id or a path.
    pub async fn resolve(&self, user_id: &str, id_or_path: &str) -> Result<Node> {
        if uuid::Uuid::parse_str(id_or_path).is_ok() {
            let (n, _) = self.node_for(user_id, id_or_path, perm::READ).await?;
            Ok(n)
        } else {
            self.resolve_path(user_id, id_or_path).await
        }
    }

    /// Splits `path` into (parent folder, final name), resolving the parent.
    pub async fn resolve_parent(&self, user_id: &str, path: &str) -> Result<(Node, String)> {
        let parts = split_path(path)?;
        let (name, dirs) = parts.split_last().ok_or_else(|| Error::bad("path must name a file or folder"))?;
        let parent = self.resolve_path(user_id, &dirs.join("/")).await?;
        Ok((parent, name.to_string()))
    }

    /// Path of `node` as `user_id` sees it ("/" for the root).
    pub async fn display_path(&self, user_id: &str, node: &Node) -> Result<String> {
        let chain: Vec<(String, Option<String>, String)> = sqlx::query_as(
            "WITH RECURSIVE anc(id, parent_id, name, depth) AS (
                 SELECT id, parent_id, name, 0 FROM nodes WHERE id = ?
                 UNION ALL SELECT n.id, n.parent_id, n.name, a.depth + 1 FROM nodes n JOIN anc a ON n.id = a.parent_id)
             SELECT id, parent_id, name FROM anc ORDER BY depth",
        )
        .bind(&node.id)
        .fetch_all(&self.db)
        .await?;
        let mut names = Vec::new();
        if node.owner_id == user_id {
            for (_, parent, name) in &chain {
                if parent.is_some() {
                    names.push(name.clone());
                }
            }
        } else {
            let mounts: Vec<String> = self.share_mounts(user_id).await?.into_iter().map(|n| n.id).collect();
            for (id, _, name) in &chain {
                names.push(name.clone());
                if mounts.contains(id) {
                    break;
                }
            }
        }
        names.reverse();
        Ok(format!("/{}", names.join("/")))
    }

    pub async fn lock_of(&self, node_id: &str) -> Result<Option<LockInfo>> {
        let l: Option<LockInfo> = sqlx::query_as("SELECT node_id, user_id, owner_app, token, created_at, expires_at FROM locks WHERE node_id = ?")
            .bind(node_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(l.filter(|l| l.expires_at > now()))
    }

    pub async fn entry(&self, user_id: &str, node: Node, permissions: i64) -> Result<Entry> {
        let shared_by = if node.owner_id != user_id {
            Some(self.user(&node.owner_id).await?.display_name)
        } else {
            None
        };
        let lock = if node.is_dir() { None } else { self.lock_of(&node.id).await? };
        let craft_app = if node.is_dir() {
            None
        } else {
            crate::craft::app_for_name(&node.name).map(|a| a.id)
        };
        let live_editor = if node.is_dir() {
            None
        } else {
            crate::collab::DocKind::for_name(&node.name).map(|k| k.as_str())
        };
        Ok(Entry {
            node,
            permissions,
            shared_by,
            lock,
            craft_app,
            live_editor,
        })
    }

    pub async fn list_dir(&self, user_id: &str, dir_id: &str) -> Result<Vec<Entry>> {
        let (dir, p) = self.node_for(user_id, dir_id, perm::READ).await?;
        if !dir.is_dir() {
            return Err(Error::bad("not a folder"));
        }
        let mut children: Vec<Node> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {NODE_COLS} FROM nodes WHERE parent_id = ? AND trashed_at IS NULL ORDER BY kind = 'file', name COLLATE NOCASE"
        )))
        .bind(&dir.id)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::with_capacity(children.len());
        let is_own_root = dir.parent_id.is_none() && dir.owner_id == user_id;
        for n in children.drain(..) {
            out.push(self.entry(user_id, n, p).await?);
        }
        if is_own_root {
            for m in self.share_mounts(user_id).await? {
                if out.iter().any(|e| e.node.name == m.name) {
                    continue;
                }
                let mp = self.permissions(user_id, &m).await?;
                out.push(self.entry(user_id, m, mp).await?);
            }
        }
        Ok(out)
    }

    /// Users who can see `node` (owner plus share recipients on it or its ancestors).
    pub async fn node_audience(&self, node: &Node) -> Result<Vec<String>> {
        let mut users: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE anc(id, parent_id) AS (
                 SELECT id, parent_id FROM nodes WHERE id = ?1
                 UNION ALL SELECT n.id, n.parent_id FROM nodes n JOIN anc a ON n.id = a.parent_id)
             SELECT s.target_user FROM shares s WHERE s.node_id IN (SELECT id FROM anc) AND s.target_user IS NOT NULL
             UNION SELECT gm.user_id FROM shares s JOIN group_members gm ON gm.group_id = s.target_group
             WHERE s.node_id IN (SELECT id FROM anc)
             UNION SELECT p.user_id FROM shares s JOIN participants p ON p.room_id = s.target_room
             WHERE s.node_id IN (SELECT id FROM anc)",
        )
        .bind(&node.id)
        .fetch_all(&self.db)
        .await?;
        users.push(node.owner_id.clone());
        users.sort();
        users.dedup();
        Ok(users)
    }

    async fn notify_changed(&self, node: &Node, action: &str) {
        if let Ok(audience) = self.node_audience(node).await {
            let ev = json!({"type": "files.changed", "action": action, "node_id": node.id, "parent_id": node.parent_id, "name": node.name});
            self.hub.publish(audience.iter().map(String::as_str), &ev);
        }
    }

    /// Bumps etag/modified time on every ancestor so sync clients notice changes below.
    async fn touch_ancestors(tx: &mut Transaction<'_, Sqlite>, parent_id: &str) -> Result<()> {
        sqlx::query(
            "WITH RECURSIVE anc(id, parent_id) AS (
                 SELECT id, parent_id FROM nodes WHERE id = ?1
                 UNION ALL SELECT n.id, n.parent_id FROM nodes n JOIN anc a ON n.id = a.parent_id)
             UPDATE nodes SET etag = lower(hex(randomblob(6))), modified_at = ?2 WHERE id IN (SELECT id FROM anc)",
        )
        .bind(parent_id)
        .bind(now())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    // ---- quota -----------------------------------------------------------

    pub async fn quota(&self, user_id: &str) -> Result<Quota> {
        let used: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size), 0) FROM nodes WHERE owner_id = ? AND kind = 'file'")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        let limit: Option<i64> = sqlx::query_scalar("SELECT quota_bytes FROM users WHERE id = ?")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        Ok(Quota { used, limit })
    }

    async fn check_quota(&self, owner_id: &str, extra: i64) -> Result<()> {
        let q = self.quota(owner_id).await?;
        if let Some(limit) = q.limit {
            if extra > 0 && q.used + extra > limit {
                return Err(Error::QuotaExceeded);
            }
        }
        Ok(())
    }

    // ---- mutations -------------------------------------------------------

    pub async fn mkdir(&self, user_id: &str, parent_id: &str, name: &str) -> Result<Node> {
        validate_name(name)?;
        let (parent, _) = self.node_for(user_id, parent_id, perm::CREATE).await?;
        if !parent.is_dir() {
            return Err(Error::bad("parent is not a folder"));
        }
        if self.child(&parent.id, name).await?.is_some() {
            return Err(Error::conflict(format!("{name} already exists")));
        }
        let id = new_id();
        let t = now();
        let mut tx = self.db.begin().await?;
        sqlx::query("INSERT INTO nodes (id, owner_id, parent_id, name, kind, mime, etag, created_at, modified_at) VALUES (?, ?, ?, ?, 'dir', 'inode/directory', ?, ?, ?)")
            .bind(&id)
            .bind(&parent.owner_id)
            .bind(&parent.id)
            .bind(name)
            .bind(etag())
            .bind(t)
            .bind(t)
            .execute(&mut *tx)
            .await?;
        Self::touch_ancestors(&mut tx, &parent.id).await?;
        tx.commit().await?;
        let node = self.node(&id).await?;
        self.index_file(&node, None).await?;
        self.notify_changed(&node, "created").await;
        Ok(node)
    }

    /// Creates every missing folder along `path` and returns the last one.
    pub async fn mkdir_p(&self, user_id: &str, path: &str) -> Result<Node> {
        let mut cur = self.node(&self.ensure_root(user_id).await?).await?;
        for (i, part) in split_path(path)?.into_iter().enumerate() {
            let existing = match self.child(&cur.id, part).await? {
                Some(n) => Some(n),
                None if i == 0 => self.share_mounts(user_id).await?.into_iter().find(|n| n.name == part),
                None => None,
            };
            cur = match existing {
                Some(n) if n.is_dir() => n,
                Some(_) => return Err(Error::conflict(format!("{part} is a file"))),
                None => self.mkdir(user_id, &cur.id, part).await?,
            };
        }
        Ok(cur)
    }

    /// Writes (creates or replaces) a file from a stream.
    pub async fn write_file<R: AsyncRead + Unpin>(&self, user_id: &str, parent_id: &str, name: &str, reader: R, opts: WriteOpts) -> Result<Node> {
        validate_name(name)?;
        let parent = self.node(parent_id).await?;
        if !parent.is_dir() || parent.trashed_at.is_some() {
            return Err(Error::bad("parent is not a folder"));
        }
        let pperm = self.permissions(user_id, &parent).await?;
        if pperm & perm::READ == 0 {
            return Err(Error::NotFound);
        }
        let existing = self.child(&parent.id, name).await?;
        match &existing {
            Some(n) if n.is_dir() => return Err(Error::conflict(format!("{name} is a folder"))),
            Some(_) if opts.create_only => {
                return Err(Error::conflict(format!("{name} already exists")));
            }
            Some(n) => {
                if let Some(want) = &opts.if_match {
                    if want.trim_matches('"') != n.etag {
                        return Err(Error::PreconditionFailed);
                    }
                }
                if self.permissions(user_id, n).await? & perm::UPDATE == 0 {
                    return Err(Error::Forbidden);
                }
                if let Some(lock) = self.lock_of(&n.id).await? {
                    let ok = lock.user_id == user_id && opts.lock_token.as_deref().is_none_or(|t| t == lock.token)
                        || opts.lock_token.as_deref() == Some(lock.token.as_str());
                    if !ok {
                        let who = self.user(&lock.user_id).await.map(|u| u.display_name).unwrap_or_default();
                        return Err(Error::Locked(format!("{who} ({})", lock.owner_app)));
                    }
                }
            }
            None => {
                if opts.if_match.is_some() {
                    return Err(Error::PreconditionFailed);
                }
                if pperm & perm::CREATE == 0 {
                    return Err(Error::Forbidden);
                }
            }
        }

        let pin = self.blobs.pin().await;
        let blob = self.blobs.write_reader(reader, self.config.max_upload).await?;
        let old_size = existing.as_ref().map(|n| n.size).unwrap_or(0);
        if let Err(e) = self.check_quota(&parent.owner_id, blob.size - old_size).await {
            drop(pin);
            // The blob may now be unreferenced; let GC reclaim it.
            let _ = self.blobs.collect_garbage(&self.db).await;
            return Err(e);
        }
        let mime = opts.mime.clone().unwrap_or_else(|| guess_mime(name));
        let t = now();
        let mut tx = self.db.begin().await?;
        let id = match existing {
            Some(n) if n.blob_hash.as_deref() == Some(blob.hash.as_str()) => {
                // Same content: nothing to do but report the file.
                drop(tx);
                drop(pin);
                return Ok(n);
            }
            Some(n) => {
                if let Some(old) = &n.blob_hash {
                    if opts.keep_version {
                        // The node's reference moves to the version; no refcount change.
                        sqlx::query("INSERT INTO versions (id, node_id, blob_hash, size, author_id, created_at) VALUES (?, ?, ?, ?, ?, ?)")
                            .bind(new_id())
                            .bind(&n.id)
                            .bind(old)
                            .bind(n.size)
                            .bind(user_id)
                            .bind(n.modified_at)
                            .execute(&mut *tx)
                            .await?;
                    } else {
                        BlobStore::release(&mut tx, old).await?;
                    }
                }
                BlobStore::add_ref(&mut tx, &blob).await?;
                sqlx::query("UPDATE nodes SET blob_hash = ?, size = ?, mime = ?, etag = ?, modified_at = ? WHERE id = ?")
                    .bind(&blob.hash)
                    .bind(blob.size)
                    .bind(&mime)
                    .bind(etag())
                    .bind(t)
                    .bind(&n.id)
                    .execute(&mut *tx)
                    .await?;
                self.prune_versions(&mut tx, &n.id).await?;
                n.id
            }
            None => {
                let id = new_id();
                BlobStore::add_ref(&mut tx, &blob).await?;
                sqlx::query(
                    "INSERT INTO nodes (id, owner_id, parent_id, name, kind, blob_hash, size, mime, etag, created_at, modified_at)
                     VALUES (?, ?, ?, ?, 'file', ?, ?, ?, ?, ?, ?)",
                )
                .bind(&id)
                .bind(&parent.owner_id)
                .bind(&parent.id)
                .bind(name)
                .bind(&blob.hash)
                .bind(blob.size)
                .bind(&mime)
                .bind(etag())
                .bind(t)
                .bind(t)
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        Self::touch_ancestors(&mut tx, &parent.id).await?;
        tx.commit().await?;
        drop(pin);
        if !opts.from_collab {
            self.collab_external_write(&id);
        }
        let node = self.node(&id).await?;
        self.index_file(&node, Some(&blob)).await?;
        self.notify_changed(&node, "written").await;
        Ok(node)
    }

    pub async fn write_bytes(&self, user_id: &str, parent_id: &str, name: &str, data: &[u8], opts: WriteOpts) -> Result<Node> {
        self.write_file(user_id, parent_id, name, data, opts).await
    }

    /// Writes a file at a path, creating parent folders as needed.
    pub async fn write_path(&self, user_id: &str, path: &str, data: &[u8], opts: WriteOpts) -> Result<Node> {
        let parts = split_path(path)?;
        let (name, dirs) = parts.split_last().ok_or_else(|| Error::bad("path must name a file"))?;
        let parent = self.mkdir_p(user_id, &dirs.join("/")).await?;
        self.write_bytes(user_id, &parent.id, name, data, opts).await
    }

    async fn prune_versions(&self, tx: &mut Transaction<'_, Sqlite>, node_id: &str) -> Result<()> {
        let excess: Vec<(String, String)> = sqlx::query_as("SELECT id, blob_hash FROM versions WHERE node_id = ? ORDER BY created_at DESC LIMIT -1 OFFSET ?")
            .bind(node_id)
            .bind(self.config.max_versions)
            .fetch_all(&mut **tx)
            .await?;
        for (vid, hash) in excess {
            sqlx::query("DELETE FROM versions WHERE id = ?").bind(&vid).execute(&mut **tx).await?;
            BlobStore::release(tx, &hash).await?;
        }
        Ok(())
    }

    /// Opens a file for reading; returns the node and the blob path on disk.
    pub async fn open_file(&self, user_id: &str, id: &str) -> Result<(Node, std::path::PathBuf)> {
        let (node, _) = self.node_for(user_id, id, perm::READ).await?;
        let hash = node.blob_hash.clone().ok_or_else(|| Error::bad("not a file"))?;
        let path = self.blobs.path(&hash);
        Ok((node, path))
    }

    pub async fn read_file(&self, user_id: &str, id: &str) -> Result<(Node, Vec<u8>)> {
        let (node, path) = self.open_file(user_id, id).await?;
        Ok((node, tokio::fs::read(path).await?))
    }

    async fn is_ancestor(&self, ancestor_id: &str, node_id: &str) -> Result<bool> {
        let found: Option<String> = sqlx::query_scalar(
            "WITH RECURSIVE anc(id, parent_id) AS (
                 SELECT id, parent_id FROM nodes WHERE id = ?1
                 UNION ALL SELECT n.id, n.parent_id FROM nodes n JOIN anc a ON n.id = a.parent_id)
             SELECT id FROM anc WHERE id = ?2",
        )
        .bind(node_id)
        .bind(ancestor_id)
        .fetch_optional(&self.db)
        .await?;
        Ok(found.is_some())
    }

    /// Moves and/or renames a node. `overwrite` replaces an existing file at the destination.
    pub async fn move_node(&self, user_id: &str, id: &str, new_parent_id: &str, new_name: &str, overwrite: bool) -> Result<Node> {
        validate_name(new_name)?;
        let (node, p) = self.node_for(user_id, id, perm::READ).await?;
        if node.parent_id.is_none() {
            return Err(Error::bad("cannot move the root folder"));
        }
        let (dest, dp) = self.node_for(user_id, new_parent_id, perm::READ).await?;
        if !dest.is_dir() {
            return Err(Error::bad("destination is not a folder"));
        }
        if self.is_ancestor(&node.id, &dest.id).await? {
            return Err(Error::bad("cannot move a folder into itself"));
        }
        let same_parent = node.parent_id.as_deref() == Some(dest.id.as_str());
        let rename_only = same_parent;
        if rename_only {
            if p & perm::UPDATE == 0 {
                return Err(Error::Forbidden);
            }
        } else if p & perm::DELETE == 0 || dp & perm::CREATE == 0 {
            return Err(Error::Forbidden);
        }
        if let Some(existing) = self.child(&dest.id, new_name).await? {
            if existing.id == node.id {
                return Ok(node);
            }
            if !overwrite || existing.is_dir() != node.is_dir() {
                return Err(Error::conflict(format!("{new_name} already exists")));
            }
            self.delete(user_id, &existing.id).await?;
        }
        if dest.owner_id != node.owner_id {
            // Crossing into another user's tree changes ownership: copy, then trash the source.
            let copied = self.copy_node(user_id, &node.id, &dest.id, new_name).await?;
            self.delete(user_id, &node.id).await?;
            return Ok(copied);
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, etag = ?, modified_at = ? WHERE id = ?")
            .bind(&dest.id)
            .bind(new_name)
            .bind(etag())
            .bind(now())
            .bind(&node.id)
            .execute(&mut *tx)
            .await?;
        if let Some(old_parent) = &node.parent_id {
            Self::touch_ancestors(&mut tx, old_parent).await?;
        }
        Self::touch_ancestors(&mut tx, &dest.id).await?;
        tx.commit().await?;
        let moved = self.node(&node.id).await?;
        self.index_file(&moved, None).await?;
        self.notify_changed(&node, "moved").await;
        self.notify_changed(&moved, "moved").await;
        Ok(moved)
    }

    /// Deep-copies a node; file contents are shared through blob refcounts.
    pub async fn copy_node(&self, user_id: &str, id: &str, dest_parent_id: &str, new_name: &str) -> Result<Node> {
        validate_name(new_name)?;
        let (src, _) = self.node_for(user_id, id, perm::READ).await?;
        let (dest, dp) = self.node_for(user_id, dest_parent_id, perm::READ).await?;
        if dp & perm::CREATE == 0 {
            return Err(Error::Forbidden);
        }
        if !dest.is_dir() {
            return Err(Error::bad("destination is not a folder"));
        }
        if src.is_dir() && self.is_ancestor(&src.id, &dest.id).await? {
            return Err(Error::bad("cannot copy a folder into itself"));
        }
        if self.child(&dest.id, new_name).await?.is_some() {
            return Err(Error::conflict(format!("{new_name} already exists")));
        }
        // Size of everything being copied counts against the destination owner.
        let total: i64 = sqlx::query_scalar(
            "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN d ON n.parent_id = d.id WHERE n.trashed_at IS NULL)
             SELECT COALESCE(SUM(size), 0) FROM nodes WHERE id IN (SELECT id FROM d) AND kind = 'file'",
        )
        .bind(&src.id)
        .fetch_one(&self.db)
        .await?;
        self.check_quota(&dest.owner_id, total).await?;

        let mut tx = self.db.begin().await?;
        let new_root = new_id();
        let mut stack = vec![(src.clone(), dest.id.clone(), new_root.clone(), new_name.to_string())];
        let t = now();
        while let Some((n, parent, nid, name)) = stack.pop() {
            sqlx::query(
                "INSERT INTO nodes (id, owner_id, parent_id, name, kind, blob_hash, size, mime, etag, created_at, modified_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&nid)
            .bind(&dest.owner_id)
            .bind(&parent)
            .bind(&name)
            .bind(&n.kind)
            .bind(&n.blob_hash)
            .bind(n.size)
            .bind(&n.mime)
            .bind(etag())
            .bind(t)
            .bind(t)
            .execute(&mut *tx)
            .await?;
            if let Some(h) = &n.blob_hash {
                BlobStore::add_ref_hash(&mut tx, h).await?;
            }
            if n.is_dir() {
                let kids: Vec<Node> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {NODE_COLS} FROM nodes WHERE parent_id = ? AND trashed_at IS NULL"
                )))
                .bind(&n.id)
                .fetch_all(&mut *tx)
                .await?;
                for k in kids {
                    let kname = k.name.clone();
                    stack.push((k, nid.clone(), new_id(), kname));
                }
            }
        }
        Self::touch_ancestors(&mut tx, &dest.id).await?;
        tx.commit().await?;
        let node = self.node(&new_root).await?;
        self.reindex_subtree(&node).await?;
        self.notify_changed(&node, "created").await;
        Ok(node)
    }

    /// Moves a node (and its subtree) to the owner's trash.
    pub async fn delete(&self, user_id: &str, id: &str) -> Result<()> {
        let (node, p) = self.node_for(user_id, id, perm::READ).await?;
        if node.parent_id.is_none() {
            return Err(Error::bad("cannot delete the root folder"));
        }
        if node.trashed_at.is_some() {
            return Err(Error::NotFound);
        }
        if p & perm::DELETE == 0 {
            return Err(Error::Forbidden);
        }
        if let Some(lock) = self.lock_of(&node.id).await? {
            if lock.user_id != user_id {
                return Err(Error::Locked(lock.owner_app));
            }
        }
        let t = now();
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN d ON n.parent_id = d.id WHERE n.trashed_at IS NULL)
             UPDATE nodes SET trashed_at = ?2 WHERE id IN (SELECT id FROM d)",
        )
        .bind(&node.id)
        .bind(t)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET trash_origin_parent = parent_id WHERE id = ?")
            .bind(&node.id)
            .execute(&mut *tx)
            .await?;
        if let Some(parent) = &node.parent_id {
            Self::touch_ancestors(&mut tx, parent).await?;
        }
        tx.commit().await?;
        self.notify_changed(&node, "deleted").await;
        Ok(())
    }

    pub async fn list_trash(&self, user_id: &str) -> Result<Vec<Node>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {NODE_COLS} FROM nodes WHERE owner_id = ? AND trash_origin_parent IS NOT NULL AND trashed_at IS NOT NULL ORDER BY trashed_at DESC"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn restore(&self, user_id: &str, id: &str) -> Result<Node> {
        let node = self.node(id).await?;
        if node.owner_id != user_id || node.trashed_at.is_none() {
            return Err(Error::NotFound);
        }
        let parent_id: String = sqlx::query_scalar("SELECT trash_origin_parent FROM nodes WHERE id = ?")
            .bind(id)
            .fetch_one(&self.db)
            .await?;
        // Restore into the original folder if it still exists, otherwise into the root.
        let parent = match self.node(&parent_id).await {
            Ok(p) if p.trashed_at.is_none() => p,
            _ => self.node(&self.ensure_root(user_id).await?).await?,
        };
        let mut name = node.name.clone();
        if self.child(&parent.id, &name).await?.is_some() {
            let (stem, ext) = match name.rsplit_once('.') {
                Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
                _ => (name.clone(), String::new()),
            };
            name = format!("{stem} (restored){ext}");
        }
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN d ON n.parent_id = d.id)
             UPDATE nodes SET trashed_at = NULL WHERE id IN (SELECT id FROM d) AND trashed_at = ?2",
        )
        .bind(&node.id)
        .bind(node.trashed_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET parent_id = ?, name = ?, trash_origin_parent = NULL WHERE id = ?")
            .bind(&parent.id)
            .bind(&name)
            .bind(&node.id)
            .execute(&mut *tx)
            .await?;
        Self::touch_ancestors(&mut tx, &parent.id).await?;
        tx.commit().await?;
        let restored = self.node(&node.id).await?;
        self.notify_changed(&restored, "restored").await;
        Ok(restored)
    }

    /// Permanently deletes a trashed node and its subtree.
    pub async fn purge(&self, user_id: &str, id: &str) -> Result<()> {
        let node = self.node(id).await?;
        if node.owner_id != user_id || node.trashed_at.is_none() {
            return Err(Error::NotFound);
        }
        self.purge_subtree(&node.id).await?;
        self.blobs.collect_garbage(&self.db).await?;
        Ok(())
    }

    async fn purge_subtree(&self, id: &str) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let hashes: Vec<String> = sqlx::query_scalar(
            "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN d ON n.parent_id = d.id)
             SELECT blob_hash FROM nodes WHERE id IN (SELECT id FROM d) AND blob_hash IS NOT NULL
             UNION ALL SELECT blob_hash FROM versions WHERE node_id IN (SELECT id FROM d)",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        for h in &hashes {
            BlobStore::release(&mut tx, h).await?;
        }
        sqlx::query(
            "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN d ON n.parent_id = d.id)
             DELETE FROM fts WHERE object_kind = 'file' AND object_id IN (SELECT id FROM d)",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM nodes WHERE id = ?").bind(id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn empty_trash(&self, user_id: &str) -> Result<usize> {
        let items = self.list_trash(user_id).await?;
        for n in &items {
            self.purge_subtree(&n.id).await?;
        }
        self.blobs.collect_garbage(&self.db).await?;
        Ok(items.len())
    }

    /// Purges trash older than the retention period for all users. Returns purged count.
    pub async fn expire_trash(&self) -> Result<usize> {
        let cutoff = now() - Duration::days(self.config.trash_retention_days);
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM nodes WHERE trash_origin_parent IS NOT NULL AND trashed_at < ?")
            .bind(cutoff)
            .fetch_all(&self.db)
            .await?;
        for id in &ids {
            self.purge_subtree(id).await?;
        }
        self.blobs.collect_garbage(&self.db).await?;
        Ok(ids.len())
    }

    // ---- versions ----------------------------------------------------------

    pub async fn list_versions(&self, user_id: &str, id: &str) -> Result<Vec<Version>> {
        self.node_for(user_id, id, perm::READ).await?;
        Ok(
            sqlx::query_as("SELECT id, node_id, size, author_id, label, created_at FROM versions WHERE node_id = ? ORDER BY created_at DESC")
                .bind(id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    pub async fn read_version(&self, user_id: &str, id: &str, version_id: &str) -> Result<Vec<u8>> {
        self.node_for(user_id, id, perm::READ).await?;
        let hash: String = sqlx::query_scalar("SELECT blob_hash FROM versions WHERE id = ? AND node_id = ?")
            .bind(version_id)
            .bind(id)
            .fetch_one(&self.db)
            .await?;
        self.blobs.read(&hash).await
    }

    /// Makes a version current again; the current content becomes a version itself.
    pub async fn restore_version(&self, user_id: &str, id: &str, version_id: &str) -> Result<Node> {
        let (node, _) = self.node_for(user_id, id, perm::UPDATE).await?;
        let data = self.read_version(user_id, id, version_id).await?;
        let parent = node.parent_id.clone().ok_or(Error::NotFound)?;
        self.write_bytes(user_id, &parent, &node.name, &data, WriteOpts::user()).await
    }

    // ---- locks -------------------------------------------------------------

    pub async fn lock(&self, user_id: &str, id: &str, app: &str, ttl_secs: i64) -> Result<LockInfo> {
        let (node, _) = self.node_for(user_id, id, perm::UPDATE).await?;
        if node.is_dir() {
            return Err(Error::bad("folders cannot be locked"));
        }
        if let Some(l) = self.lock_of(&node.id).await? {
            if l.user_id != user_id {
                return Err(Error::Locked(l.owner_app));
            }
        }
        let t = now();
        let ttl = ttl_secs.clamp(30, 7 * 24 * 3600);
        if let Some(l) = self.lock_of(&node.id).await? {
            // Refreshing your own lock keeps its token, so holders' tokens stay valid.
            sqlx::query("UPDATE locks SET owner_app = ?, expires_at = ? WHERE node_id = ? AND token = ?")
                .bind(app)
                .bind(t + Duration::seconds(ttl))
                .bind(&node.id)
                .bind(&l.token)
                .execute(&self.db)
                .await?;
            return self.lock_of(&node.id).await?.ok_or(Error::NotFound);
        }
        sqlx::query(
            "INSERT INTO locks (node_id, user_id, owner_app, token, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)
                     ON CONFLICT(node_id) DO UPDATE SET user_id = excluded.user_id, owner_app = excluded.owner_app,
                     token = excluded.token, created_at = excluded.created_at, expires_at = excluded.expires_at",
        )
        .bind(&node.id)
        .bind(user_id)
        .bind(app)
        .bind(format!("opaquelocktoken:{}", new_id()))
        .bind(t)
        .bind(t + Duration::seconds(ttl))
        .execute(&self.db)
        .await?;
        self.notify_changed(&node, "locked").await;
        self.lock_of(&node.id).await?.ok_or(Error::NotFound)
    }

    pub async fn unlock(&self, user_id: &str, id: &str) -> Result<()> {
        let (node, p) = self.node_for(user_id, id, perm::READ).await?;
        if let Some(l) = self.lock_of(&node.id).await? {
            // The lock holder or the file owner (with full rights) may unlock.
            if l.user_id != user_id && !(node.owner_id == user_id && p == perm::ALL) {
                return Err(Error::Forbidden);
            }
        }
        sqlx::query("DELETE FROM locks WHERE node_id = ?").bind(&node.id).execute(&self.db).await?;
        self.notify_changed(&node, "unlocked").await;
        Ok(())
    }

    // ---- misc --------------------------------------------------------------

    pub async fn set_favorite(&self, user_id: &str, id: &str, favorite: bool) -> Result<()> {
        let r = sqlx::query("UPDATE nodes SET favorite = ? WHERE id = ? AND owner_id = ?")
            .bind(favorite)
            .bind(id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        if r.rows_affected() == 0 { Err(Error::NotFound) } else { Ok(()) }
    }

    pub async fn recent_files(&self, user_id: &str, limit: i64) -> Result<Vec<Node>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {NODE_COLS} FROM nodes WHERE owner_id = ? AND kind = 'file' AND trashed_at IS NULL ORDER BY modified_at DESC LIMIT ?"
        )))
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn favorites(&self, user_id: &str) -> Result<Vec<Node>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {NODE_COLS} FROM nodes WHERE owner_id = ? AND favorite = 1 AND trashed_at IS NULL ORDER BY name"
        )))
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Keeps the full text index in sync for a file: its name always, its text when small.
    async fn index_file(&self, node: &Node, blob: Option<&StoredBlob>) -> Result<()> {
        let body = match (blob, &node.blob_hash) {
            (_, Some(hash)) if is_text_mime(&node.mime) && node.size <= MAX_INDEXED_TEXT => String::from_utf8_lossy(&self.blobs.read(hash).await?).into_owned(),
            _ => String::new(),
        };
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM fts WHERE object_kind = 'file' AND object_id = ?")
            .bind(&node.id)
            .execute(&mut *tx)
            .await?;
        if node.parent_id.is_some() {
            sqlx::query("INSERT INTO fts (object_kind, object_id, scope, title, body) VALUES ('file', ?, ?, ?, ?)")
                .bind(&node.id)
                .bind(&node.owner_id)
                .bind(&node.name)
                .bind(body)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn reindex_subtree(&self, root: &Node) -> Result<()> {
        let nodes: Vec<Node> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "WITH RECURSIVE d(id) AS (SELECT ?1 UNION ALL SELECT n.id FROM nodes n JOIN d ON n.parent_id = d.id)
             SELECT {NODE_COLS} FROM nodes WHERE id IN (SELECT id FROM d)"
        )))
        .bind(&root.id)
        .fetch_all(&self.db)
        .await?;
        for n in &nodes {
            self.index_file(n, None).await?;
        }
        Ok(())
    }
}

pub fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/xml" | "application/javascript" | "application/toml" | "application/x-yaml"
        )
        || mime == crate::collab::MIME_MINDMAP
        || mime == crate::collab::MIME_CANVAS
}
