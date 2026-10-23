//! Views across folders and accounts: all inboxes, the priority inbox (Important / Others),
//! starred and tag views; oldest-first folder pages; advanced search; the Home widgets; and
//! deep links that survive messages moving between folders.
//!
//! Views list cached messages. Opening one refreshes the inboxes involved in the background,
//! and starred and tag views also ask the server (IMAP SEARCH) for matches older than the
//! cached window, at most every few minutes.

use super::Account;
use super::imap::{quote, uid_set};
use super::prefs::Sort;
use super::store::{Folder, MSG_COLS, MessagePage, MessageSummary};
use super::tags::{IMPORTANT, keyword_like};
use crate::{Core, Error, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

/// How often a starred or tag view asks the server for more matches.
const VIEW_SERVER_REFRESH: Duration = Duration::from_secs(300);
/// Matches taken from each folder's server-side search.
const SERVER_MATCHES: usize = 50;

/// `/mail?m=<id>&mid=<Message-ID>`: the id opens the cached copy; the Message-ID finds the
/// message again after it moved (and got a new id).
pub fn message_link(id: &str, message_id: Option<&str>) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    match message_id {
        Some(mid) => format!("/mail?m={}&mid={}", enc(id), enc(mid)),
        None => format!("/mail?m={}", enc(id)),
    }
}

/// A view across folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    /// Every account's inbox.
    AllInboxes,
    /// Inbox messages tagged Important.
    Important,
    /// Inbox messages not tagged Important.
    Others,
    /// Starred messages anywhere but Trash and Junk.
    Starred,
    /// Messages with a tag, anywhere but Trash and Junk.
    Tag(String),
}

impl View {
    pub fn parse(s: &str) -> Result<View> {
        Ok(match s {
            "all" => View::AllInboxes,
            "important" => View::Important,
            "others" => View::Others,
            "starred" => View::Starred,
            other => match other.strip_prefix("tag:") {
                Some(k) if super::tags::valid_keyword(k) => View::Tag(k.to_ascii_lowercase()),
                _ => return Err(Error::bad(format!("unknown view {s:?}"))),
            },
        })
    }

    /// The SQL condition (on `m` and folder `f`) and its bound value, if any.
    fn condition(&self) -> (&'static str, Option<String>) {
        match self {
            View::AllInboxes => ("f.role = 'inbox'", None),
            View::Important => ("f.role = 'inbox' AND m.keywords LIKE ? ESCAPE '\\'", Some(keyword_like(IMPORTANT))),
            View::Others => ("f.role = 'inbox' AND m.keywords NOT LIKE ? ESCAPE '\\'", Some(keyword_like(IMPORTANT))),
            View::Starred => ("m.flagged = 1 AND (f.role IS NULL OR f.role NOT IN ('trash', 'junk'))", None),
            View::Tag(k) => (
                "m.keywords LIKE ? ESCAPE '\\' AND (f.role IS NULL OR f.role NOT IN ('trash', 'junk'))",
                Some(keyword_like(k)),
            ),
        }
    }

    /// IMAP SEARCH criteria for matches the cache may not have yet.
    fn server_criteria(&self) -> Option<String> {
        match self {
            View::Starred => Some("FLAGGED".into()),
            View::Tag(k) => Some(format!("KEYWORD {k}")),
            _ => None,
        }
    }
}

/// Advanced search: every given criterion must match.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SearchFilter {
    /// Free text: senders, recipients and subject (and the body when body search is on).
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    /// On or after this day (UTC).
    pub since: Option<NaiveDate>,
    /// On or before this day (UTC).
    pub until: Option<NaiveDate>,
    #[serde(default)]
    pub unread: bool,
    #[serde(default)]
    pub flagged: bool,
    #[serde(default)]
    pub attachments: bool,
    /// Comma-separated keywords; any of them matches.
    pub tags: Option<String>,
    pub account_id: Option<String>,
    pub folder_id: Option<String>,
    /// Only the inboxes (searching the unified inbox).
    #[serde(default)]
    pub unified: bool,
}

fn given(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

impl SearchFilter {
    pub fn tag_list(&self) -> Vec<String> {
        given(&self.tags)
            .map(|t| {
                t.split(',')
                    .map(|k| k.trim().to_ascii_lowercase())
                    .filter(|k| super::tags::valid_keyword(k))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        [&self.q, &self.from, &self.to, &self.cc, &self.subject, &self.body]
            .iter()
            .all(|v| given(v).is_none())
            && self.since.is_none()
            && self.until.is_none()
            && !self.unread
            && !self.flagged
            && !self.attachments
            && self.tag_list().is_empty()
    }
}

fn imap_date(d: NaiveDate) -> String {
    d.format("%-d-%b-%Y").to_string()
}

/// `OR a OR b c` for any number of criteria.
fn any_of(items: &[String]) -> Option<String> {
    match items {
        [] => None,
        [one] => Some(one.clone()),
        [first, rest @ ..] => Some(format!("OR {first} {}", any_of(rest)?)),
    }
}

/// The IMAP SEARCH criteria for a filter. `has attachments` has no IMAP criterion and is
/// applied to the results.
pub fn imap_criteria(f: &SearchFilter, body_search: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(q) = given(&f.q) {
        if body_search {
            parts.push(format!("TEXT {}", quote(q)));
        } else {
            let q = quote(q);
            parts.push(format!("OR OR FROM {q} SUBJECT {q} OR TO {q} CC {q}"));
        }
    }
    for (key, v) in [("FROM", &f.from), ("TO", &f.to), ("CC", &f.cc), ("SUBJECT", &f.subject), ("BODY", &f.body)] {
        if let Some(v) = given(v) {
            parts.push(format!("{key} {}", quote(v)));
        }
    }
    if let Some(d) = f.since {
        parts.push(format!("SINCE {}", imap_date(d)));
    }
    if let Some(d) = f.until.and_then(|d| d.succ_opt()) {
        parts.push(format!("BEFORE {}", imap_date(d)));
    }
    if f.unread {
        parts.push("UNSEEN".into());
    }
    if f.flagged {
        parts.push("FLAGGED".into());
    }
    let tags: Vec<String> = f.tag_list().iter().map(|k| format!("KEYWORD {k}")).collect();
    if let Some(t) = any_of(&tags) {
        parts.push(t);
    }
    let all = if parts.is_empty() { "ALL".to_string() } else { parts.join(" ") };
    if all.is_ascii() { all } else { format!("CHARSET UTF-8 {all}") }
}

/// A bound SQL value.
#[derive(Debug, Clone, PartialEq)]
pub enum Bind {
    Text(String),
    Time(DateTime<Utc>),
}

fn like(v: &str) -> String {
    format!("%{}%", v.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

/// The cache-side condition of a filter (on `m`), with its binds. `fts_ids` are the
/// full-text index matches for the free text and body criteria (a JSON array).
pub fn sql_condition(f: &SearchFilter, body_search: bool, fts_q: Option<&str>, fts_body: Option<&str>) -> (String, Vec<Bind>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut binds = Vec::new();
    if let Some(q) = given(&f.q) {
        if body_search {
            clauses.push("m.id IN (SELECT value FROM json_each(?))".into());
            binds.push(Bind::Text(fts_q.unwrap_or("[]").to_string()));
        } else {
            clauses.push(
                "(m.subject LIKE ? ESCAPE '\\' OR m.from_name LIKE ? ESCAPE '\\' OR m.from_addr LIKE ? ESCAPE '\\' OR m.to_addrs LIKE ? ESCAPE '\\' OR m.cc_addrs LIKE ? ESCAPE '\\')"
                    .into(),
            );
            for _ in 0..5 {
                binds.push(Bind::Text(like(q)));
            }
        }
    }
    if let Some(v) = given(&f.from) {
        clauses.push("(m.from_name LIKE ? ESCAPE '\\' OR m.from_addr LIKE ? ESCAPE '\\')".into());
        binds.push(Bind::Text(like(v)));
        binds.push(Bind::Text(like(v)));
    }
    for (col, v) in [("m.to_addrs", &f.to), ("m.cc_addrs", &f.cc), ("m.subject", &f.subject)] {
        if let Some(v) = given(v) {
            clauses.push(format!("{col} LIKE ? ESCAPE '\\'"));
            binds.push(Bind::Text(like(v)));
        }
    }
    if given(&f.body).is_some() {
        clauses.push("m.id IN (SELECT value FROM json_each(?))".into());
        binds.push(Bind::Text(fts_body.unwrap_or("[]").to_string()));
    }
    if let Some(d) = f.since {
        clauses.push("m.date >= ?".into());
        binds.push(Bind::Time(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc()));
    }
    if let Some(d) = f.until.and_then(|d| d.succ_opt()) {
        clauses.push("m.date < ?".into());
        binds.push(Bind::Time(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc()));
    }
    if f.unread {
        clauses.push("m.seen = 0".into());
    }
    if f.flagged {
        clauses.push("m.flagged = 1".into());
    }
    let tags = f.tag_list();
    if !tags.is_empty() {
        clauses.push(format!("({})", vec!["m.keywords LIKE ? ESCAPE '\\'"; tags.len()].join(" OR ")));
        for t in &tags {
            binds.push(Bind::Text(keyword_like(t)));
        }
    }
    if clauses.is_empty() {
        ("1 = 1".into(), binds)
    } else {
        (clauses.join(" AND "), binds)
    }
}

/// The Home page's mail widgets.
#[derive(Debug, Clone, Serialize)]
pub struct Widgets {
    pub accounts: i64,
    pub unread_count: i64,
    pub unread: Vec<MessageSummary>,
    pub important_unread: i64,
    pub important: Vec<MessageSummary>,
}

impl Core {
    /// Ids of the accounts `user_id` may read (own and delegated), as a JSON array for SQL.
    async fn readable_accounts(&self, user_id: &str) -> Result<String> {
        let ids: Vec<String> = self.mail_accounts_for(user_id).await?.into_iter().map(|a| a.id).collect();
        Ok(serde_json::to_string(&ids)?)
    }

    /// Background refresh of the user's inboxes (debounced per folder).
    async fn refresh_inboxes(&self, user_id: &str) -> Result<Vec<Account>> {
        let accounts = self.mail_accounts_for(user_id).await?;
        for a in &accounts {
            if let Some(inbox) = self.folder_by_role(&a.id, "inbox").await? {
                self.refresh_folder_soon(a, &inbox.id);
            }
        }
        Ok(accounts)
    }

    /// A page of a view, newest (or oldest) first.
    pub async fn mail_view(&self, user_id: &str, view: &View, before: Option<&str>, unread_only: bool, sort: Sort, limit: i64) -> Result<MessagePage> {
        let scope = self.readable_accounts(user_id).await?;
        let accounts = self.refresh_inboxes(user_id).await?;
        if let Some(criteria) = view.server_criteria() {
            let key = format!("view:{user_id}:{criteria}");
            if before.is_none() && self.mail.last_sync.get(&key).is_none_or(|t| t.elapsed() >= VIEW_SERVER_REFRESH) {
                self.mail.last_sync.insert(key, Instant::now());
                // The first page waits for the inboxes' matches; archives follow in the background.
                for a in &accounts {
                    if let Some(inbox) = self.folder_by_role(&a.id, "inbox").await? {
                        if let Err(e) = self.server_matches(a, &inbox, &criteria).await {
                            tracing::info!(account = %a.id, error = %e, "server search for a view failed");
                        }
                    }
                    if let Some(archive) = self.folder_by_role(&a.id, "archive").await? {
                        let (core, a, criteria) = (self.clone(), a.clone(), criteria.clone());
                        tokio::spawn(async move {
                            if let Err(e) = core.server_matches(&a, &archive, &criteria).await {
                                tracing::debug!(error = %e, "archive search for a view failed");
                            }
                        });
                    }
                }
            }
        }
        let limit = limit.clamp(1, 200);
        let cursor: Option<(DateTime<Utc>, String)> = match before {
            Some(id) => {
                sqlx::query_as("SELECT date, id FROM mail_messages WHERE id = ?")
                    .bind(id)
                    .fetch_optional(&self.db)
                    .await?
            }
            None => None,
        };
        let (cond, bind) = view.condition();
        let (cmp, order) = match sort {
            Sort::Newest => ("(m.date < ? OR (m.date = ? AND m.id < ?))", "m.date DESC, m.id DESC"),
            Sort::Oldest => ("(m.date > ? OR (m.date = ? AND m.id > ?))", "m.date ASC, m.id ASC"),
        };
        let sql = format!(
            "SELECT {MSG_COLS} FROM mail_messages m JOIN mail_folders f ON f.id = m.folder_id JOIN mail_accounts a ON a.id = m.account_id
             WHERE m.account_id IN (SELECT value FROM json_each(?)) AND f.shared = 0 AND {cond} AND (? = 0 OR m.seen = 0) AND (? IS NULL OR {cmp}) ORDER BY {order} LIMIT ?"
        );
        let mut query = sqlx::query_as::<_, MessageSummary>(sqlx::AssertSqlSafe(sql)).bind(&scope);
        if let Some(b) = bind {
            query = query.bind(b);
        }
        let (d, i) = cursor.unzip();
        let mut rows = query
            .bind(unread_only)
            .bind(d)
            .bind(d)
            .bind(d)
            .bind(i)
            .bind(limit + 1)
            .fetch_all(&self.db)
            .await?;
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        Ok(MessagePage { messages: rows, has_more })
    }

    /// Runs an IMAP SEARCH in one folder and caches the (newest [`SERVER_MATCHES`]) matches
    /// that are not cached yet. Returns the ids of the matches.
    pub(crate) async fn server_matches(&self, a: &Account, f: &Folder, criteria: &str) -> Result<Vec<String>> {
        if !f.selectable {
            return Ok(vec![]);
        }
        let mut conn = self.imap_conn(a).await?;
        let sel = conn.select(&f.name).await?;
        if f.uidvalidity.is_some_and(|v| v as u32 != sel.uidvalidity) || sel.exists == 0 {
            return Ok(vec![]);
        }
        let uids = conn.uid_search(criteria).await?;
        let newest: Vec<u32> = uids.into_iter().rev().take(SERVER_MATCHES).collect();
        if newest.is_empty() {
            return Ok(vec![]);
        }
        let cached: Vec<i64> = sqlx::query_scalar("SELECT uid FROM mail_messages WHERE folder_id = ? AND uid IN (SELECT value FROM json_each(?))")
            .bind(&f.id)
            .bind(serde_json::to_string(&newest)?)
            .fetch_all(&self.db)
            .await?;
        let missing: Vec<u32> = newest.iter().copied().filter(|u| !cached.contains(&(*u as i64))).collect();
        if !missing.is_empty() && f.uidvalidity.is_some() {
            let fetched = conn
                .uid_fetch(
                    &uid_set(&missing),
                    &format!(
                        "(UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.{}>)",
                        super::sync::PREVIEW_BYTES
                    ),
                )
                .await?;
            drop(conn);
            self.store_envelopes(a, f, fetched).await?;
        }
        Ok(
            sqlx::query_scalar("SELECT id FROM mail_messages WHERE folder_id = ? AND uid IN (SELECT value FROM json_each(?))")
                .bind(&f.id)
                .bind(serde_json::to_string(&newest)?)
                .fetch_all(&self.db)
                .await?,
        )
    }

    /// A folder page, oldest first. Pages follow the server's order (SORT by date, else UID
    /// order), so the oldest messages are fetched even when only the newest are cached;
    /// `offset` counts messages.
    pub async fn mail_messages_oldest(&self, user_id: &str, folder_id: &str, offset: usize, unread_only: bool, limit: i64) -> Result<MessagePage> {
        let (a, f) = self.mail_folder(user_id, folder_id).await?;
        if !f.selectable {
            return Ok(MessagePage {
                messages: vec![],
                has_more: false,
            });
        }
        if f.synced_at.is_none() {
            self.sync_mail_folder(&a, &f.id).await?;
        }
        let f = self.folder_row(&f.id).await?;
        let limit = limit.clamp(1, 200) as usize;
        let mut conn = self.imap_conn(&a).await?;
        let sel = conn.select(&f.name).await?;
        if f.uidvalidity.is_some_and(|v| v as u32 != sel.uidvalidity) {
            drop(conn);
            self.refresh_folder_soon(&a, &f.id);
            return Err(Error::conflict("the folder changed on the server; refreshing"));
        }
        let criteria = if unread_only { "UNSEEN" } else { "ALL" };
        // By date where the server can sort (RFC 5256), else in arrival (UID) order.
        let uids = if sel.exists == 0 {
            vec![]
        } else {
            match conn.uid_sort("DATE", criteria).await? {
                Some(sorted) => sorted,
                None => conn.uid_search(criteria).await?,
            }
        };
        let page: Vec<u32> = uids.iter().copied().skip(offset).take(limit).collect();
        let cached: Vec<i64> = sqlx::query_scalar("SELECT uid FROM mail_messages WHERE folder_id = ? AND uid IN (SELECT value FROM json_each(?))")
            .bind(&f.id)
            .bind(serde_json::to_string(&page)?)
            .fetch_all(&self.db)
            .await?;
        let missing: Vec<u32> = page.iter().copied().filter(|u| !cached.contains(&(*u as i64))).collect();
        if !missing.is_empty() {
            let fetched = conn
                .uid_fetch(
                    &uid_set(&missing),
                    &format!(
                        "(UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.{}>)",
                        super::sync::PREVIEW_BYTES
                    ),
                )
                .await?;
            drop(conn);
            self.store_envelopes(&a, &f, fetched).await?;
        } else {
            drop(conn);
        }
        self.refresh_folder_soon(&a, &f.id);
        let rows: Vec<MessageSummary> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {MSG_COLS} FROM mail_messages m WHERE m.folder_id = ? AND m.uid IN (SELECT value FROM json_each(?)) ORDER BY m.date ASC, m.uid ASC"
        )))
        .bind(&f.id)
        .bind(serde_json::to_string(&page)?)
        .fetch_all(&self.db)
        .await?;
        Ok(MessagePage {
            messages: rows,
            has_more: offset + limit < uids.len(),
        })
    }

    /// Advanced search over the cache and the server.
    pub async fn mail_search_advanced(&self, user_id: &str, f: &SearchFilter, limit: usize) -> Result<Vec<MessageSummary>> {
        let scope = self.readable_accounts(user_id).await?;
        if f.is_empty() {
            return Ok(vec![]);
        }
        let body_search = self.mail_prefs(user_id).await?.body_search;
        if given(&f.body).is_some() && !body_search {
            return Err(Error::bad("body search is turned off in the mail settings"));
        }
        let fts = |text: Option<&str>| {
            let core = self.clone();
            let text = text.map(String::from);
            async move {
                match text {
                    Some(t) => {
                        let ids: Vec<String> = core.search(user_id, &t, Some(&["mail"]), 200).await?.into_iter().map(|h| h.id).collect();
                        Ok::<_, Error>(Some(serde_json::to_string(&ids)?))
                    }
                    None => Ok(None),
                }
            }
        };
        let fts_q = if body_search { fts(given(&f.q)).await? } else { None };
        let fts_body = fts(given(&f.body)).await?;
        // Server side.
        let accounts: Vec<Account> = match &f.account_id {
            Some(id) => vec![self.mail_access(user_id, id, super::delegation::MailRight::Read).await?],
            None => self.mail_accounts_for(user_id).await?,
        };
        let criteria = imap_criteria(f, body_search);
        let mut server_ids: Vec<String> = Vec::new();
        for a in &accounts {
            let folders: Vec<Folder> = match &f.folder_id {
                Some(fid) => {
                    let (fa, folder) = self.mail_folder(user_id, fid).await?;
                    if fa.id != a.id {
                        continue;
                    }
                    vec![folder]
                }
                None => {
                    let roles: &[&str] = if f.unified { &["inbox"] } else { &["inbox", "sent", "archive"] };
                    let mut v = Vec::new();
                    for role in roles {
                        if let Some(x) = self.folder_by_role(&a.id, role).await? {
                            v.push(x);
                        }
                    }
                    v
                }
            };
            for folder in folders {
                match self.server_matches(a, &folder, &criteria).await {
                    Ok(found) => server_ids.extend(found),
                    Err(e) => tracing::info!(account = %a.id, folder = %folder.name, error = %e, "server-side mail search failed"),
                }
            }
        }
        let (cond, binds) = sql_condition(f, body_search, fts_q.as_deref(), fts_body.as_deref());
        let sql = format!(
            "SELECT {MSG_COLS} FROM mail_messages m JOIN mail_folders f ON f.id = m.folder_id JOIN mail_accounts a ON a.id = m.account_id
             WHERE m.account_id IN (SELECT value FROM json_each(?)) AND (? IS NULL OR m.account_id = ?) AND (? IS NULL OR m.folder_id = ?) AND (? = 0 OR f.role = 'inbox')
             AND ((({cond})) OR m.id IN (SELECT value FROM json_each(?))) AND (? = 0 OR m.has_attachments = 1)
             ORDER BY m.date DESC LIMIT ?"
        );
        let mut q = sqlx::query_as::<_, MessageSummary>(sqlx::AssertSqlSafe(sql))
            .bind(&scope)
            .bind(&f.account_id)
            .bind(&f.account_id)
            .bind(&f.folder_id)
            .bind(&f.folder_id)
            .bind(f.unified);
        for b in binds {
            q = match b {
                Bind::Text(s) => q.bind(s),
                Bind::Time(t) => q.bind(t),
            };
        }
        Ok(q.bind(serde_json::to_string(&server_ids)?)
            .bind(f.attachments)
            .bind(limit as i64)
            .fetch_all(&self.db)
            .await?)
    }

    pub async fn mail_widgets(&self, user_id: &str, limit: i64) -> Result<Widgets> {
        let scope = self.readable_accounts(user_id).await?;
        let accounts = self.refresh_inboxes(user_id).await?;
        let unread_count: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(f.unread), 0) FROM mail_folders f JOIN mail_accounts a ON a.id = f.account_id WHERE f.account_id IN (SELECT value FROM json_each(?)) AND f.role = 'inbox'",
        )
        .bind(&scope)
        .fetch_one(&self.db)
        .await?;
        let base = format!(
            "SELECT {MSG_COLS} FROM mail_messages m JOIN mail_folders f ON f.id = m.folder_id JOIN mail_accounts a ON a.id = m.account_id
             WHERE f.account_id IN (SELECT value FROM json_each(?)) AND f.role = 'inbox'"
        );
        let unread: Vec<MessageSummary> = sqlx::query_as(sqlx::AssertSqlSafe(format!("{base} AND m.seen = 0 ORDER BY m.date DESC LIMIT ?")))
            .bind(&scope)
            .bind(limit)
            .fetch_all(&self.db)
            .await?;
        let important: Vec<MessageSummary> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "{base} AND m.keywords LIKE ? ESCAPE '\\' ORDER BY m.date DESC LIMIT ?"
        )))
        .bind(&scope)
        .bind(keyword_like(IMPORTANT))
        .bind(limit)
        .fetch_all(&self.db)
        .await?;
        let important_unread: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mail_messages m JOIN mail_folders f ON f.id = m.folder_id JOIN mail_accounts a ON a.id = m.account_id
             WHERE m.account_id IN (SELECT value FROM json_each(?)) AND f.role = 'inbox' AND m.seen = 0 AND m.keywords LIKE ? ESCAPE '\\'",
        )
        .bind(&scope)
        .bind(keyword_like(IMPORTANT))
        .fetch_one(&self.db)
        .await?;
        Ok(Widgets {
            accounts: accounts.len() as i64,
            unread_count,
            unread,
            important_unread,
            important,
        })
    }

    /// Finds a message for a deep link: by cached id, else by Message-ID among the user's
    /// cached messages (preferring the inbox, then anything but Trash and Junk), else by
    /// asking each account's inbox and archive.
    pub async fn mail_resolve(&self, user_id: &str, id: Option<&str>, message_id: Option<&str>) -> Result<String> {
        let scope = self.readable_accounts(user_id).await?;
        if let Some(id) = id {
            if self.message_row(user_id, id).await.is_ok() {
                return Ok(id.to_string());
            }
        }
        let mid = message_id
            .map(|m| m.trim().trim_matches(['<', '>']).to_string())
            .filter(|m| !m.is_empty())
            .ok_or(Error::NotFound)?;
        let find = || async {
            sqlx::query_scalar::<_, String>(
                "SELECT m.id FROM mail_messages m JOIN mail_folders f ON f.id = m.folder_id JOIN mail_accounts a ON a.id = m.account_id
                 WHERE m.account_id IN (SELECT value FROM json_each(?)) AND m.message_id = ?
                 ORDER BY f.role = 'inbox' DESC, (f.role IN ('trash', 'junk')) ASC, m.date DESC LIMIT 1",
            )
            .bind(&scope)
            .bind(&mid)
            .fetch_optional(&self.db)
            .await
        };
        if let Some(found) = find().await? {
            return Ok(found);
        }
        let criteria = format!("HEADER Message-ID {}", quote(&format!("<{mid}>")));
        for a in self.mail_accounts_for(user_id).await? {
            for role in ["inbox", "archive", "snoozed", "sent"] {
                if let Some(f) = self.folder_by_role(&a.id, role).await? {
                    if let Ok(ids) = self.server_matches(&a, &f, &criteria).await {
                        if let Some(first) = ids.into_iter().next() {
                            return Ok(first);
                        }
                    }
                }
            }
        }
        Err(Error::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn imap_queries() {
        let f = SearchFilter {
            q: Some("report".into()),
            ..Default::default()
        };
        assert_eq!(imap_criteria(&f, true), r#"TEXT "report""#);
        assert_eq!(imap_criteria(&f, false), r#"OR OR FROM "report" SUBJECT "report" OR TO "report" CC "report""#);
        let f = SearchFilter {
            from: Some("ada@example.org".into()),
            to: Some(" ".into()),
            subject: Some("Q3 \"numbers\"".into()),
            since: Some(d("2026-02-01")),
            until: Some(d("2026-02-28")),
            unread: true,
            flagged: true,
            tags: Some("$label1, $label4,bad tag".into()),
            ..Default::default()
        };
        assert_eq!(
            imap_criteria(&f, true),
            r#"FROM "ada@example.org" SUBJECT "Q3 \"numbers\"" SINCE 1-Feb-2026 BEFORE 1-Mar-2026 UNSEEN FLAGGED OR KEYWORD $label1 KEYWORD $label4"#
        );
        let f = SearchFilter {
            body: Some("Grüße".into()),
            ..Default::default()
        };
        assert_eq!(imap_criteria(&f, true), "CHARSET UTF-8 BODY \"Grüße\"");
        assert_eq!(imap_criteria(&SearchFilter::default(), true), "ALL");
        assert!(SearchFilter::default().is_empty());
        assert!(
            !SearchFilter {
                attachments: true,
                ..Default::default()
            }
            .is_empty()
        );
        assert_eq!(any_of(&["A".into(), "B".into(), "C".into()]).unwrap(), "OR A OR B C");
    }

    #[test]
    fn sql_filters() {
        let f = SearchFilter {
            q: Some("50%_off".into()),
            from: Some("ada".into()),
            until: Some(d("2026-02-28")),
            tags: Some("$label2".into()),
            ..Default::default()
        };
        let (sql, binds) = sql_condition(&f, false, None, None);
        assert_eq!(sql.matches('?').count(), binds.len());
        assert!(sql.contains("m.subject LIKE ?") && sql.contains("m.from_addr LIKE ?") && sql.contains("m.date < ?"));
        assert_eq!(binds[0], Bind::Text("%50\\%\\_off%".into()), "LIKE wildcards are escaped");
        assert!(binds.contains(&Bind::Time(d("2026-03-01").and_hms_opt(0, 0, 0).unwrap().and_utc())));
        assert!(binds.contains(&Bind::Text("% $label2 %".into())));
        let (sql, binds) = sql_condition(&f, true, Some("[\"a\"]"), None);
        assert!(sql.starts_with("m.id IN (SELECT value FROM json_each(?))"));
        assert_eq!(binds[0], Bind::Text("[\"a\"]".into()));
        let (sql, binds) = sql_condition(&SearchFilter::default(), true, None, None);
        assert_eq!((sql.as_str(), binds.len()), ("1 = 1", 0));
    }

    #[test]
    fn views_and_links() {
        assert_eq!(View::parse("all").unwrap(), View::AllInboxes);
        assert_eq!(View::parse("tag:$Label3").unwrap(), View::Tag("$label3".into()));
        assert!(View::parse("tag:bad tag").is_err());
        assert!(View::parse("everything").is_err());
        assert_eq!(View::Starred.server_criteria().as_deref(), Some("FLAGGED"));
        assert_eq!(View::Tag("$label2".into()).server_criteria().as_deref(), Some("KEYWORD $label2"));
        assert_eq!(message_link("abc", Some("x+1@example.org")), "/mail?m=abc&mid=x%2B1%40example.org");
        assert_eq!(message_link("abc", None), "/mail?m=abc");
    }
}
