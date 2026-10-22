//! Folder and message sync, and IMAP IDLE for online users.
//!
//! A folder sync SELECTs the folder and compares it with what is cached:
//! * UIDVALIDITY changed: the cache of that folder is dropped and refetched.
//! * Nothing cached yet: the newest [`INITIAL_WINDOW`] envelopes are fetched (older ones on
//!   demand, as the user scrolls).
//! * Otherwise only UIDs above the newest cached one are fetched, and flags/expunges of the
//!   cached window are reconciled with one `UID FETCH min:max (FLAGS)`. On CONDSTORE servers
//!   an unchanged HIGHESTMODSEQ (with unchanged UIDNEXT and message count) skips even that.
//!
//! Envelopes come from `BODY.PEEK[HEADER]` plus the first [`PREVIEW_BYTES`] of the text, so
//! nothing is marked read and no body is downloaded until a message is opened.

use super::imap::{self, Fetched, Flags, ImapConn, Selected};
use super::parse::{self, Envelope};
use super::store::Folder;
use super::{Account, ServerSettings};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use serde_json::json;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Envelopes fetched when a folder is first opened (or after its cache was reset).
pub const INITIAL_WINDOW: u32 = 200;
/// More envelopes fetched per "load older" step.
pub const OLDER_PAGE: usize = 100;
/// Body bytes fetched with each envelope for the list preview.
pub const PREVIEW_BYTES: u32 = 2048;
/// Envelopes per FETCH command (bounds memory per round trip).
const FETCH_BATCH: usize = 100;
/// The folder list is refreshed in the background at most this often.
pub const FOLDERS_STALE: Duration = Duration::from_secs(300);
/// A folder being viewed is re-synced in the background at most this often.
pub const FOLDER_STALE: Duration = Duration::from_secs(60);
/// IDLE is re-issued this often (RFC 2177 asks for less than 29 minutes).
const IDLE_WAIT: Duration = Duration::from_secs(25 * 60);
/// Without IDLE support the inbox is polled this often while the owner is online.
const POLL_EVERY: Duration = Duration::from_secs(60);
/// A watcher keeps running this long after its owner went offline (page reloads).
const OFFLINE_GRACE: Duration = Duration::from_secs(60);

fn envelope_items() -> String {
    format!("(UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.{PREVIEW_BYTES}>)")
}

/// What a folder sync has to do, decided from the cached and the server state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    /// UIDVALIDITY changed: drop the folder's cache first.
    pub reset: bool,
    pub fetch: FetchNew,
    /// Reconcile flags and expunges of the cached window.
    pub check_flags: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchNew {
    None,
    /// The newest N messages, by sequence number.
    Newest(u32),
    /// Every UID from this one up.
    From(u32),
}

/// The cached side of a folder, as of the last sync.
#[derive(Debug, Clone, Copy, Default)]
pub struct CachedState {
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
    pub modseq: Option<u64>,
    pub total: u32,
    pub count: u32,
    pub max_uid: u32,
}

pub fn plan(cached: &CachedState, server: &Selected) -> SyncPlan {
    let reset = cached.uidvalidity.is_some_and(|v| v != server.uidvalidity);
    if reset || cached.count == 0 {
        return SyncPlan {
            reset,
            fetch: if server.exists == 0 {
                FetchNew::None
            } else {
                FetchNew::Newest(server.exists.min(INITIAL_WINDOW))
            },
            check_flags: false,
        };
    }
    let has_new = match server.uidnext {
        Some(next) => next > cached.max_uid + 1,
        // Without UIDNEXT, ask for anything above the newest cached UID.
        None => true,
    };
    let unchanged = server.modseq.is_some() && server.modseq == cached.modseq && server.exists == cached.total && server.uidnext == cached.uidnext;
    SyncPlan {
        reset: false,
        fetch: if has_new { FetchNew::From(cached.max_uid + 1) } else { FetchNew::None },
        check_flags: !unchanged,
    }
}

/// Compares cached (uid, flags) with what the server reported for the same UID range:
/// returns the expunged UIDs and the messages whose flags changed.
pub fn reconcile(cached: &[(u32, Flags)], server: &[(u32, Flags)]) -> (Vec<u32>, Vec<(u32, Flags)>) {
    let on_server: std::collections::HashMap<u32, Flags> = server.iter().copied().collect();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for (uid, flags) in cached {
        match on_server.get(uid) {
            None => removed.push(*uid),
            Some(f) if f.deleted => removed.push(*uid),
            Some(f) if f != flags => changed.push((*uid, *f)),
            Some(_) => {}
        }
    }
    (removed, changed)
}

/// A new message found by a sync (for notifications).
#[derive(Debug, Clone)]
pub struct NewMail {
    pub id: String,
    pub from: String,
    pub subject: String,
    pub seen: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SyncReport {
    pub new: Vec<NewMail>,
    pub changed: bool,
}

/// A running IDLE watcher.
pub(crate) struct IdleTask {
    pub handle: JoinHandle<()>,
    pub user_id: String,
    pub offline_since: Option<Instant>,
}

impl Core {
    pub(crate) async fn folder_row(&self, folder_id: &str) -> Result<Folder> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM mail_folders WHERE id = ?",
            super::store::FOLDER_COLS
        )))
        .bind(folder_id)
        .fetch_optional(&self.db)
        .await?
        .ok_or(Error::NotFound)
    }

    pub(crate) async fn folder_by_role(&self, account_id: &str, role: &str) -> Result<Option<Folder>> {
        Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM mail_folders WHERE account_id = ? AND role = ? ORDER BY length(name) LIMIT 1",
            super::store::FOLDER_COLS
        )))
        .bind(account_id)
        .bind(role)
        .fetch_optional(&self.db)
        .await?)
    }

    pub(crate) async fn imap_conn(&self, a: &Account) -> Result<imap::PooledConn> {
        let settings = self.imap_settings(a)?;
        self.mail.pool(&a.id).get(&settings).await
    }

    /// Refreshes the folder list (LIST) and every folder's counts (STATUS).
    pub async fn sync_mail_folders(&self, a: &Account) -> Result<()> {
        let r = async {
            let mut conn = self.imap_conn(a).await?;
            self.sync_folders_on(&mut conn, a).await
        }
        .await;
        self.set_mail_error(&a.id, r.as_ref().err().map(|e| e.to_string()).as_deref()).await;
        r
    }

    pub(crate) async fn sync_folders_on(&self, conn: &mut ImapConn, a: &Account) -> Result<()> {
        self.mail.last_sync.insert(format!("folders:{}", a.id), Instant::now());
        let listed = conn.list().await?;
        // One folder per role: special-use attributes win over name matches.
        let mut roles: std::collections::HashMap<&str, (bool, &str)> = std::collections::HashMap::new();
        for f in &listed {
            if let Some(r) = f.role {
                let better = match roles.get(r) {
                    None => true,
                    Some((special, _)) => f.special_use && !special,
                };
                if better {
                    roles.insert(r, (f.special_use, f.name.as_str()));
                }
            }
        }
        let mut tx = self.mail_tx().await?;
        let mut names = Vec::new();
        for f in &listed {
            let role = f.role.filter(|r| roles.get(r).is_some_and(|(_, n)| *n == f.name));
            let leaf = f.delimiter.as_deref().and_then(|d| f.name.rsplit(d).next()).unwrap_or(&f.name);
            let display = if role == Some("inbox") {
                "Inbox".to_string()
            } else {
                parse::decode_mailbox_name(leaf)
            };
            sqlx::query(
                "INSERT INTO mail_folders (id, account_id, name, display_name, delimiter, role, selectable) VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(account_id, name) DO UPDATE SET display_name = excluded.display_name, delimiter = excluded.delimiter,
                 role = excluded.role, selectable = excluded.selectable",
            )
            .bind(new_id())
            .bind(&a.id)
            .bind(&f.name)
            .bind(&display)
            .bind(&f.delimiter)
            .bind(role)
            .bind(f.selectable)
            .execute(&mut *tx)
            .await?;
            names.push(f.name.clone());
        }
        sqlx::query("DELETE FROM mail_folders WHERE account_id = ? AND name NOT IN (SELECT value FROM json_each(?))")
            .bind(&a.id)
            .bind(serde_json::to_string(&names)?)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        for f in listed.iter().filter(|f| f.selectable) {
            match conn.status(&f.name).await {
                Ok(st) => {
                    sqlx::query("UPDATE mail_folders SET total = ?, unread = ? WHERE account_id = ? AND name = ?")
                        .bind(st.total as i64)
                        .bind(st.unread as i64)
                        .bind(&a.id)
                        .bind(&f.name)
                        .execute(&self.db)
                        .await?;
                }
                Err(e) if conn.is_broken() => return Err(e),
                Err(e) => tracing::debug!(folder = %f.name, error = %e, "STATUS failed"),
            }
        }
        self.hub
            .publish([a.user_id.as_str()], &json!({"type": "mail.changed", "account_id": a.id, "what": "folders"}));
        Ok(())
    }

    /// Syncs one folder through a pooled connection.
    pub async fn sync_mail_folder(&self, a: &Account, folder_id: &str) -> Result<SyncReport> {
        let folder = self.folder_row(folder_id).await?;
        if folder.account_id != a.id {
            return Err(Error::NotFound);
        }
        let r = async {
            let mut conn = self.imap_conn(a).await?;
            self.sync_folder_on(&mut conn, a, &folder).await
        }
        .await;
        self.set_mail_error(&a.id, r.as_ref().err().map(|e| e.to_string()).as_deref()).await;
        r
    }

    /// Syncs a folder in the background unless it was synced recently or is syncing now.
    pub(crate) fn refresh_folder_soon(&self, a: &Account, folder_id: &str) {
        let key = format!("folder:{folder_id}");
        if self.mail.last_sync.get(&key).is_some_and(|t| t.elapsed() < FOLDER_STALE) {
            return;
        }
        self.mail.last_sync.insert(key, Instant::now());
        let (core, a, folder_id) = (self.clone(), a.clone(), folder_id.to_string());
        tokio::spawn(async move {
            if let Err(e) = core.sync_mail_folder(&a, &folder_id).await {
                tracing::debug!(account = %a.id, error = %e, "background folder sync failed");
            }
        });
    }

    /// Refreshes the folder list in the background unless that happened recently.
    pub(crate) fn refresh_folders_soon(&self, a: &Account) {
        let key = format!("folders:{}", a.id);
        if self.mail.last_sync.get(&key).is_some_and(|t| t.elapsed() < FOLDERS_STALE) {
            return;
        }
        self.mail.last_sync.insert(key, Instant::now());
        let (core, a) = (self.clone(), a.clone());
        tokio::spawn(async move {
            if let Err(e) = core.sync_mail_folders(&a).await {
                tracing::debug!(account = %a.id, error = %e, "background folder list sync failed");
            }
        });
    }

    pub(crate) async fn sync_folder_on(&self, conn: &mut ImapConn, a: &Account, folder: &Folder) -> Result<SyncReport> {
        let lock = self.mail.folder_lock(&folder.id);
        let _guard = lock.lock().await;
        self.mail.last_sync.insert(format!("folder:{}", folder.id), Instant::now());
        // Re-read under the lock: another sync may just have finished.
        let folder = self.folder_row(&folder.id).await?;
        let sel = conn.select(&folder.name).await?;
        let (count, max_uid): (i64, Option<i64>) = sqlx::query_as("SELECT COUNT(*), MAX(uid) FROM mail_messages WHERE folder_id = ?")
            .bind(&folder.id)
            .fetch_one(&self.db)
            .await?;
        let cached = CachedState {
            uidvalidity: folder.uidvalidity.map(|v| v as u32),
            uidnext: folder.uidnext.map(|v| v as u32),
            modseq: folder.modseq.map(|v| v as u64),
            total: folder.total as u32,
            // Rows cached before the first full sync (from a server search) are refetched.
            count: if folder.uidvalidity.is_some() { count as u32 } else { 0 },
            max_uid: max_uid.unwrap_or(0) as u32,
        };
        let mut p = plan(&cached, &sel);
        let mut report = SyncReport::default();
        // A huge gap (many new messages since the last sync) is cheaper to refetch as a
        // fresh window than to fill completely.
        if let (FetchNew::From(from), Some(next)) = (&p.fetch, sel.uidnext) {
            if next.saturating_sub(*from) > INITIAL_WINDOW * 4 {
                let newer = conn.uid_search(&format!("UID {from}:*")).await?;
                if newer.len() as u32 > INITIAL_WINDOW {
                    p = SyncPlan {
                        reset: true,
                        fetch: FetchNew::Newest(sel.exists.min(INITIAL_WINDOW)),
                        check_flags: false,
                    };
                }
            }
        }
        if p.reset || cached.uidvalidity.is_none() {
            let deleted = sqlx::query("DELETE FROM mail_messages WHERE folder_id = ?")
                .bind(&folder.id)
                .execute(&self.db)
                .await?;
            report.changed |= deleted.rows_affected() > 0;
            sqlx::query("UPDATE mail_folders SET complete = 0, window_uid = NULL WHERE id = ?")
                .bind(&folder.id)
                .execute(&self.db)
                .await?;
        }
        if p.check_flags {
            let rows: Vec<(i64, bool, bool, bool, bool)> = sqlx::query_as("SELECT uid, seen, flagged, answered, draft FROM mail_messages WHERE folder_id = ?")
                .bind(&folder.id)
                .fetch_all(&self.db)
                .await?;
            let cached_flags: Vec<(u32, Flags)> = rows
                .iter()
                .map(|(uid, seen, flagged, answered, draft)| {
                    (
                        *uid as u32,
                        Flags {
                            seen: *seen,
                            flagged: *flagged,
                            answered: *answered,
                            draft: *draft,
                            deleted: false,
                        },
                    )
                })
                .collect();
            let min = cached_flags.iter().map(|c| c.0).min().unwrap_or(1);
            let server: Vec<(u32, Flags)> = conn
                .uid_fetch(&format!("{min}:{}", cached.max_uid.max(min)), "(UID FLAGS)")
                .await?
                .into_iter()
                .map(|f| (f.uid, f.flags))
                .collect();
            let (removed, changed) = reconcile(&cached_flags, &server);
            let mut tx = self.mail_tx().await?;
            for uid in &removed {
                sqlx::query("DELETE FROM mail_messages WHERE folder_id = ? AND uid = ?")
                    .bind(&folder.id)
                    .bind(*uid as i64)
                    .execute(&mut *tx)
                    .await?;
            }
            for (uid, f) in &changed {
                sqlx::query("UPDATE mail_messages SET seen = ?, flagged = ?, answered = ?, draft = ? WHERE folder_id = ? AND uid = ?")
                    .bind(f.seen)
                    .bind(f.flagged)
                    .bind(f.answered)
                    .bind(f.draft)
                    .bind(&folder.id)
                    .bind(*uid as i64)
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            report.changed |= !removed.is_empty() || !changed.is_empty();
        }
        // A fresh window: everything from its oldest UID up is cached, and the folder is
        // complete when the window holds every message.
        let mut window: Option<(u32, bool)> = None;
        let fetched = match p.fetch {
            FetchNew::None => Vec::new(),
            FetchNew::Newest(n) => {
                let start = sel.exists.saturating_sub(n) + 1;
                let mut all = Vec::new();
                let mut s = start;
                while s <= sel.exists {
                    let e = (s + FETCH_BATCH as u32 - 1).min(sel.exists);
                    all.extend(conn.seq_fetch(&format!("{s}:{e}"), &envelope_items()).await?);
                    s = e + 1;
                }
                if let Some(min) = all.iter().map(|f| f.uid).min() {
                    window = Some((min, n >= sel.exists));
                }
                all
            }
            FetchNew::From(from) => conn
                .uid_fetch(&format!("{from}:*"), &envelope_items())
                .await?
                .into_iter()
                // "N:*" also returns the last message when every UID is below N.
                .filter(|f| f.uid >= from)
                .collect(),
        };
        if !fetched.is_empty() {
            report.new = self.store_envelopes(a, &folder, fetched).await?;
            report.changed = true;
        }
        let unread = conn.count_unseen().await?;
        report.changed |= unread as i64 != folder.unread || sel.exists as i64 != folder.total;
        sqlx::query(
            "UPDATE mail_folders SET uidvalidity = ?, uidnext = ?, modseq = ?, total = ?, unread = ?, synced_at = ?,
             window_uid = COALESCE(?, window_uid), complete = COALESCE(?, complete, 0) WHERE id = ?",
        )
        .bind(sel.uidvalidity as i64)
        .bind(sel.uidnext.map(|v| v as i64))
        .bind(sel.modseq.map(|v| v as i64))
        .bind(sel.exists as i64)
        .bind(unread as i64)
        .bind(now())
        .bind(window.map(|w| w.0 as i64))
        .bind(window.map(|w| w.1))
        .bind(&folder.id)
        .execute(&self.db)
        .await?;
        if report.changed {
            self.hub.publish(
                [a.user_id.as_str()],
                &json!({"type": "mail.changed", "account_id": a.id, "folder_id": folder.id, "what": "messages"}),
            );
        }
        Ok(report)
    }

    /// Fetches up to [`OLDER_PAGE`] envelopes older than the cached window. Returns how many
    /// were added (0 when there is nothing older).
    pub(crate) async fn fetch_older(&self, a: &Account, folder: &Folder) -> Result<usize> {
        let lock = self.mail.folder_lock(&folder.id);
        let _guard = lock.lock().await;
        let folder = self.folder_row(&folder.id).await?;
        let Some(window) = folder.window_uid.filter(|w| *w > 1 && !folder.complete) else {
            return Ok(0);
        };
        let mut conn = self.imap_conn(a).await?;
        let sel = conn.select(&folder.name).await?;
        if folder.uidvalidity.is_some_and(|v| v as u32 != sel.uidvalidity) {
            // The folder was reset under us; the next sync starts over.
            return Ok(0);
        }
        let cached: std::collections::HashSet<i64> = sqlx::query_scalar("SELECT uid FROM mail_messages WHERE folder_id = ? AND uid < ?")
            .bind(&folder.id)
            .bind(window)
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .collect();
        let older: Vec<u32> = conn
            .uid_search(&format!("UID 1:{}", window - 1))
            .await?
            .into_iter()
            .filter(|u| (*u as i64) < window && !cached.contains(&(*u as i64)))
            .collect();
        let take: Vec<u32> = older.iter().rev().take(OLDER_PAGE).copied().collect();
        let mut fetched = Vec::new();
        for chunk in take.chunks(FETCH_BATCH) {
            fetched.extend(conn.uid_fetch(&imap::uid_set(chunk), &envelope_items()).await?);
        }
        let n = fetched.len();
        if !fetched.is_empty() {
            self.store_envelopes(a, &folder, fetched).await?;
        }
        let new_window = take.iter().min().map(|m| *m as i64).unwrap_or(1);
        sqlx::query("UPDATE mail_folders SET window_uid = ?, complete = ? WHERE id = ?")
            .bind(new_window)
            .bind(older.len() <= OLDER_PAGE)
            .bind(&folder.id)
            .execute(&self.db)
            .await?;
        Ok(n)
    }

    /// Inserts fetched envelopes (skipping UIDs already cached) and indexes them for search.
    pub(crate) async fn store_envelopes(&self, a: &Account, folder: &Folder, fetched: Vec<Fetched>) -> Result<Vec<NewMail>> {
        let mut out = Vec::new();
        let mut tx = self.mail_tx().await?;
        for f in fetched {
            let env = parse::parse_envelope(&f.header, &f.text_prefix);
            let date = env.date.or(f.internal_date).unwrap_or_else(now);
            let thread_id = thread_for(&mut tx, &a.id, &env).await?;
            let id = new_id();
            let from = env.from.clone().unwrap_or_default();
            let inserted = sqlx::query(
                "INSERT INTO mail_messages (id, account_id, folder_id, uid, message_id, in_reply_to, refs, thread_id, subject, from_name, from_addr,
                 to_addrs, cc_addrs, date, size, seen, flagged, answered, draft, has_attachments, preview)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(folder_id, uid) DO NOTHING",
            )
            .bind(&id)
            .bind(&a.id)
            .bind(&folder.id)
            .bind(f.uid as i64)
            .bind(&env.message_id)
            .bind(&env.in_reply_to)
            .bind(env.references.join(" "))
            .bind(&thread_id)
            .bind(&env.subject)
            .bind(&from.name)
            .bind(&from.address)
            .bind(serde_json::to_string(&env.to)?)
            .bind(serde_json::to_string(&env.cc)?)
            .bind(date)
            .bind(f.size as i64)
            .bind(f.flags.seen)
            .bind(f.flags.flagged)
            .bind(f.flags.answered)
            .bind(f.flags.draft)
            .bind(env.has_attachments)
            .bind(&env.preview)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 0 {
                continue;
            }
            let rowid = sqlx::query("INSERT INTO fts (object_kind, object_id, scope, title, body) VALUES ('mail', ?, ?, ?, ?)")
                .bind(&id)
                .bind(&a.user_id)
                .bind(&env.subject)
                .bind(index_text(&env, &env.preview))
                .execute(&mut *tx)
                .await?
                .last_insert_rowid();
            sqlx::query("UPDATE mail_messages SET fts_rowid = ? WHERE id = ?")
                .bind(rowid)
                .bind(&id)
                .execute(&mut *tx)
                .await?;
            out.push(NewMail {
                id,
                from: from.label().to_string(),
                subject: env.subject.clone(),
                seen: f.flags.seen,
            });
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Starts IDLE watchers for `user_id`'s accounts now (instead of at the next manager tick).
    pub async fn ensure_mail_idle(&self, user_id: &str) {
        if !self.hub.is_online(user_id) {
            return;
        }
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM mail_accounts WHERE user_id = ?")
            .bind(user_id)
            .fetch_all(&self.db)
            .await
            .unwrap_or_default();
        for id in ids {
            self.start_idler(&id, user_id);
        }
    }

    fn start_idler(&self, account_id: &str, user_id: &str) {
        if self.mail.idlers.get(account_id).is_some_and(|t| !t.handle.is_finished()) {
            return;
        }
        let (core, id) = (self.clone(), account_id.to_string());
        let handle = tokio::spawn(async move { core.idle_loop(&id).await });
        self.mail.idlers.insert(
            account_id.to_string(),
            IdleTask {
                handle,
                user_id: user_id.to_string(),
                offline_since: None,
            },
        );
    }

    /// Starts watchers for accounts of online users and stops those of users gone offline.
    pub(crate) async fn manage_idlers(&self) {
        self.mail.idlers.retain(|_, t| {
            if t.handle.is_finished() {
                return false;
            }
            if self.hub.is_online(&t.user_id) {
                t.offline_since = None;
                return true;
            }
            let since = *t.offline_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= OFFLINE_GRACE {
                t.handle.abort();
                return false;
            }
            true
        });
        let accounts: Vec<(String, String)> = match sqlx::query_as("SELECT id, user_id FROM mail_accounts").fetch_all(&self.db).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "listing mail accounts failed");
                return;
            }
        };
        for (id, user_id) in accounts {
            if self.hub.is_online(&user_id) {
                self.start_idler(&id, &user_id);
            }
        }
    }

    /// Watches one account's inbox while its owner is online, retrying with backoff.
    async fn idle_loop(&self, account_id: &str) {
        let mut delay = Duration::from_secs(15);
        loop {
            match self.idle_session(account_id).await {
                Ok(()) => return,
                Err(Error::NotFound) => return,
                Err(e) => {
                    tracing::info!(account = %account_id, error = %e, "mail watcher stopped; retrying");
                    self.set_mail_error(account_id, Some(&e.to_string())).await;
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(600));
                }
            }
        }
    }

    async fn idle_session(&self, account_id: &str) -> Result<()> {
        let a = self.mail_account_by_id(account_id).await?;
        let settings: ServerSettings = self.imap_settings(&a)?;
        let mut conn = ImapConn::connect(&settings).await?;
        if self.folder_by_role(&a.id, "inbox").await?.is_none() {
            self.sync_folders_on(&mut conn, &a).await?;
        }
        let inbox = self
            .folder_by_role(&a.id, "inbox")
            .await?
            .ok_or_else(|| Error::Unavailable("the account has no INBOX".into()))?;
        let mut first = true;
        loop {
            let report = self.sync_folder_on(&mut conn, &a, &inbox).await?;
            self.set_mail_error(&a.id, None).await;
            if !first {
                self.notify_new_mail(&a, &report).await;
            }
            first = false;
            if conn.caps.idle {
                let (c, _changed) = conn.idle(IDLE_WAIT).await?;
                conn = c;
            } else {
                tokio::time::sleep(POLL_EVERY).await;
            }
        }
    }

    async fn notify_new_mail(&self, a: &Account, report: &SyncReport) {
        let unseen: Vec<&NewMail> = report.new.iter().filter(|m| !m.seen).collect();
        let result = match unseen.as_slice() {
            [] => Ok(()),
            [one] => {
                let title = format!("New mail from {}", one.from);
                let subject = if one.subject.is_empty() { "(no subject)" } else { one.subject.as_str() };
                self.notify(&a.user_id, "mail", &title, subject, &format!("/mail?m={}", one.id)).await
            }
            many => {
                let title = format!("{} new messages in {}", many.len(), a.email);
                let body = many.iter().take(3).map(|m| m.from.as_str()).collect::<Vec<_>>().join(", ");
                self.notify(&a.user_id, "mail", &title, &body, &format!("/mail?m={}", many[0].id)).await
            }
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "mail notification failed");
        }
    }
}

/// Text indexed for search: sender, recipients and the body (or its preview).
pub(crate) fn index_text(env: &Envelope, body: &str) -> String {
    let mut s = String::new();
    for a in env.from.iter().chain(&env.to).chain(&env.cc) {
        s.push_str(&a.name);
        s.push(' ');
        s.push_str(&a.address);
        s.push(' ');
    }
    s.push_str(body);
    s
}

/// The conversation a message belongs to: that of any cached message it references (or
/// that replies to it), else its thread root.
async fn thread_for(conn: &mut sqlx::SqliteConnection, account_id: &str, env: &Envelope) -> Result<String> {
    let mut related: Vec<&str> = env.references.iter().map(String::as_str).collect();
    if let Some(r) = &env.in_reply_to {
        related.push(r);
    }
    if !related.is_empty() {
        let found: Option<String> =
            sqlx::query_scalar("SELECT thread_id FROM mail_messages WHERE account_id = ? AND message_id IN (SELECT value FROM json_each(?)) LIMIT 1")
                .bind(account_id)
                .bind(serde_json::to_string(&related)?)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(t) = found {
            return Ok(t);
        }
    }
    if let Some(mid) = &env.message_id {
        let found: Option<String> =
            sqlx::query_scalar("SELECT thread_id FROM mail_messages WHERE account_id = ? AND (in_reply_to = ? OR message_id = ?) LIMIT 1")
                .bind(account_id)
                .bind(mid)
                .bind(mid)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(t) = found {
            return Ok(t);
        }
    }
    Ok(parse::thread_root(env.message_id.as_deref(), env.in_reply_to.as_deref(), &env.references).unwrap_or_else(new_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(exists: u32, uidvalidity: u32, uidnext: Option<u32>, modseq: Option<u64>) -> Selected {
        Selected {
            exists,
            uidvalidity,
            uidnext,
            modseq,
        }
    }

    #[test]
    fn plans_initial_incremental_and_reset() {
        // Nothing cached: the newest window.
        let empty = CachedState::default();
        assert_eq!(plan(&empty, &sel(5000, 7, Some(5001), None)).fetch, FetchNew::Newest(INITIAL_WINDOW));
        assert_eq!(plan(&empty, &sel(3, 7, Some(4), None)).fetch, FetchNew::Newest(3));
        assert_eq!(plan(&empty, &sel(0, 7, Some(1), None)).fetch, FetchNew::None);
        let cached = CachedState {
            uidvalidity: Some(7),
            uidnext: Some(101),
            modseq: Some(40),
            total: 100,
            count: 100,
            max_uid: 100,
        };
        // New mail: only UIDs above the newest cached one.
        let p = plan(&cached, &sel(102, 7, Some(103), Some(42)));
        assert_eq!(
            p,
            SyncPlan {
                reset: false,
                fetch: FetchNew::From(101),
                check_flags: true
            }
        );
        // Nothing changed on a CONDSTORE server: no round trips beyond SELECT.
        let p = plan(&cached, &sel(100, 7, Some(101), Some(40)));
        assert_eq!(
            p,
            SyncPlan {
                reset: false,
                fetch: FetchNew::None,
                check_flags: false
            }
        );
        // Flags changed (modseq moved) but no new mail.
        let p = plan(&cached, &sel(100, 7, Some(101), Some(41)));
        assert_eq!(
            p,
            SyncPlan {
                reset: false,
                fetch: FetchNew::None,
                check_flags: true
            }
        );
        // Without CONDSTORE flags are always reconciled.
        assert!(plan(&cached, &sel(100, 7, Some(101), None)).check_flags);
        // UIDVALIDITY changed: start over.
        let p = plan(&cached, &sel(50, 8, Some(51), None));
        assert_eq!(
            p,
            SyncPlan {
                reset: true,
                fetch: FetchNew::Newest(50),
                check_flags: false
            }
        );
    }

    #[test]
    fn reconciles_flags_and_expunges() {
        let f = |seen, flagged| Flags {
            seen,
            flagged,
            ..Default::default()
        };
        let cached = [(1, f(true, false)), (2, f(false, false)), (3, f(false, true)), (4, f(true, false))];
        let deleted = Flags {
            deleted: true,
            ..f(true, false)
        };
        let server = [(1, f(true, false)), (2, f(true, false)), (4, deleted)];
        let (removed, changed) = reconcile(&cached, &server);
        assert_eq!(removed, vec![3, 4]);
        assert_eq!(changed, vec![(2, f(true, false))]);
    }
}
