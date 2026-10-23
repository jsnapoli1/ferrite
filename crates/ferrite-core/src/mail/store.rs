//! Reading and acting on cached mail: folders, message lists, bodies (fetched on demand and
//! cached under a per-account cap), conversations, attachments, flag/move/delete actions
//! (applied on the server first, then to the cache) and search.

use super::Account;
use super::imap::{quote, uid_set};
use super::parse::{self, Addr, AttachmentInfo};
use super::sync::index_text;
use crate::files::{Node, WriteOpts};
use crate::util::now;
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Body text kept per message.
const TEXT_CAP: usize = 512 * 1024;
/// Original HTML kept per message.
const HTML_CAP: usize = 1024 * 1024;
/// Whole messages up to this size are kept (for attachments and inline images).
const RAW_CAP: usize = 2 * 1024 * 1024;
/// Body cache per account; the least recently read bodies go first.
pub const ACCOUNT_CACHE_CAP: i64 = 64 * 1024 * 1024;
/// Text indexed for search per message.
const INDEX_CAP: usize = 32 * 1024;

pub(crate) const FOLDER_COLS: &str = "id, account_id, name, display_name, delimiter, role, selectable, uidvalidity, uidnext, modseq, total, unread, window_uid, complete, synced_at, subscribed, shared";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Folder {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub display_name: String,
    pub delimiter: Option<String>,
    pub role: Option<String>,
    pub selectable: bool,
    #[serde(skip)]
    pub uidvalidity: Option<i64>,
    #[serde(skip)]
    pub uidnext: Option<i64>,
    #[serde(skip)]
    pub modseq: Option<i64>,
    pub total: i64,
    pub unread: i64,
    #[serde(skip)]
    pub window_uid: Option<i64>,
    #[serde(skip)]
    pub complete: bool,
    pub synced_at: Option<DateTime<Utc>>,
    /// In the server's subscription list.
    pub subscribed: bool,
    /// In another user's or a shared namespace.
    pub shared: bool,
}

pub(crate) const MSG_COLS: &str = "m.id, m.account_id, m.folder_id, m.uid, m.message_id, m.in_reply_to, m.refs, m.thread_id, m.subject, m.from_name, m.from_addr, \
     m.to_addrs, m.cc_addrs, m.date, m.size, m.seen, m.flagged, m.answered, m.draft, m.has_attachments, m.preview, m.keywords, \
     (SELECT COUNT(*) FROM mail_messages t WHERE t.account_id = m.account_id AND t.thread_id = m.thread_id) AS thread_size";

/// A cached message as lists show it.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct MessageSummary {
    pub id: String,
    pub account_id: String,
    pub folder_id: String,
    #[serde(skip)]
    pub uid: i64,
    pub message_id: Option<String>,
    #[serde(skip)]
    pub in_reply_to: Option<String>,
    #[serde(skip)]
    pub refs: String,
    pub thread_id: String,
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    #[sqlx(json)]
    #[serde(rename = "to")]
    pub to_addrs: Vec<Addr>,
    #[sqlx(json)]
    #[serde(rename = "cc")]
    pub cc_addrs: Vec<Addr>,
    pub date: DateTime<Utc>,
    pub size: i64,
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub draft: bool,
    pub has_attachments: bool,
    pub preview: String,
    /// IMAP keywords (tags), stored space-padded; serialized as a list.
    #[serde(serialize_with = "super::tags::serialize_keywords")]
    pub keywords: String,
    /// Messages in this conversation (across folders).
    pub thread_size: i64,
}

impl MessageSummary {
    pub fn from(&self) -> Addr {
        Addr {
            name: self.from_name.clone(),
            address: self.from_addr.clone(),
        }
    }
}

/// A message with its body.
#[derive(Debug, Clone, Serialize)]
pub struct MessageView {
    #[serde(flatten)]
    pub summary: MessageSummary,
    pub folder_role: Option<String>,
    pub reply_to: Vec<Addr>,
    pub bcc: Vec<Addr>,
    pub text: String,
    /// The message has an HTML version (served sanitized by `mail_message_html`).
    pub has_html: bool,
    /// Its HTML loads remote images or styles (blocked unless asked for).
    pub remote_content: bool,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagePage {
    pub messages: Vec<MessageSummary>,
    /// Older messages exist (in the cache or on the server).
    pub has_more: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlagAction {
    Read,
    Unread,
    Flag,
    Unflag,
}

#[derive(sqlx::FromRow)]
struct BodyRow {
    text: String,
    html: Option<String>,
    attachments: String,
    remote_content: bool,
    raw: Option<Vec<u8>>,
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The document served in the message iframe.
fn html_document(s: &parse::Sanitized) -> String {
    const BASE: &str = "html{color-scheme:light}body{margin:0;padding:12px 4px;font:14px/1.5 ui-sans-serif,system-ui,-apple-system,\"Segoe UI\",Roboto,Arial,sans-serif;\
         color:#16191f;background:#fff;overflow-wrap:anywhere}img{max-width:100%;height:auto}table{max-width:100%}pre{white-space:pre-wrap}\
         blockquote{margin:0 0 0 .4em;padding-left:.8em;border-left:3px solid #dde1e8;color:#4a5263}a{color:#c2410c}";
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\"><base target=\"_blank\"><style>{BASE}</style><style>{}</style></head><body>{}</body></html>",
        s.css, s.html
    )
}

impl Core {
    /// An account's folders. The first call lists them from the server; later calls answer
    /// from the cache and refresh in the background when stale.
    pub async fn mail_folders(&self, user_id: &str, account_id: &str) -> Result<Vec<Folder>> {
        let a = self.mail_account(user_id, account_id).await?;
        let any: Option<String> = sqlx::query_scalar("SELECT id FROM mail_folders WHERE account_id = ? LIMIT 1")
            .bind(&a.id)
            .fetch_optional(&self.db)
            .await?;
        if any.is_none() {
            self.sync_mail_folders(&a).await?;
        } else {
            self.refresh_folders_soon(&a);
        }
        let mut folders: Vec<Folder> = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {FOLDER_COLS} FROM mail_folders WHERE account_id = ?")))
            .bind(&a.id)
            .fetch_all(&self.db)
            .await?;
        // Inbox, then the other special folders, then the rest by name.
        let rank = |f: &Folder| match f.role.as_deref() {
            Some("inbox") => 0,
            Some("drafts") => 1,
            Some("sent") => 2,
            Some("archive") => 3,
            Some("junk") => 4,
            Some("trash") => 5,
            Some("snoozed") => 6,
            _ => 7,
        };
        folders.sort_by(|x, y| rank(x).cmp(&rank(y)).then_with(|| x.name.to_lowercase().cmp(&y.name.to_lowercase())));
        Ok(folders)
    }

    /// A folder of one of `user_id`'s accounts.
    pub async fn mail_folder(&self, user_id: &str, folder_id: &str) -> Result<(Account, Folder)> {
        let f = self.folder_row(folder_id).await?;
        let a = self.mail_account(user_id, &f.account_id).await?;
        Ok((a, f))
    }

    /// Finds a folder by id, IMAP name, display name or role (for tools).
    pub async fn find_mail_folder(&self, user_id: &str, account_id: &str, which: &str) -> Result<Folder> {
        let folders = self.mail_folders(user_id, account_id).await?;
        let w = which.trim();
        folders
            .iter()
            .find(|f| f.id == w || f.name == w)
            .or_else(|| folders.iter().find(|f| f.role.as_deref().is_some_and(|r| r.eq_ignore_ascii_case(w))))
            .or_else(|| {
                folders
                    .iter()
                    .find(|f| f.display_name.eq_ignore_ascii_case(w) || f.name.eq_ignore_ascii_case(w))
            })
            .cloned()
            .ok_or_else(|| Error::bad(format!("no folder named {w:?}")))
    }

    /// A page of a folder, newest first. `before` is the id of the last message of the
    /// previous page. Pages past the cached window are fetched from the server.
    pub async fn mail_messages(&self, user_id: &str, folder_id: &str, before: Option<&str>, unread_only: bool, limit: i64) -> Result<MessagePage> {
        let (a, f) = self.mail_folder(user_id, folder_id).await?;
        if !f.selectable {
            return Ok(MessagePage {
                messages: vec![],
                has_more: false,
            });
        }
        if f.synced_at.is_none() {
            self.sync_mail_folder(&a, &f.id).await?;
        } else {
            self.refresh_folder_soon(&a, &f.id);
        }
        let limit = limit.clamp(1, 200);
        let cursor: Option<(DateTime<Utc>, i64)> = match before {
            Some(id) => {
                sqlx::query_as("SELECT date, uid FROM mail_messages WHERE id = ? AND folder_id = ?")
                    .bind(id)
                    .bind(&f.id)
                    .fetch_optional(&self.db)
                    .await?
            }
            None => None,
        };
        // Newest first; messages with the same Date in arrival (UID) order.
        let query = |cursor: &Option<(DateTime<Utc>, i64)>| {
            let sql = format!(
                "SELECT {MSG_COLS} FROM mail_messages m WHERE m.folder_id = ? AND (? = 0 OR m.seen = 0)
                 AND (? IS NULL OR m.date < ? OR (m.date = ? AND m.uid < ?)) ORDER BY m.date DESC, m.uid DESC LIMIT ?"
            );
            let (d, i) = (*cursor).unzip();
            let db = self.db.clone();
            let folder_id = f.id.clone();
            async move {
                sqlx::query_as::<_, MessageSummary>(sqlx::AssertSqlSafe(sql))
                    .bind(folder_id)
                    .bind(unread_only)
                    .bind(d)
                    .bind(d)
                    .bind(d)
                    .bind(i)
                    .bind(limit + 1)
                    .fetch_all(&db)
                    .await
            }
        };
        let mut rows = query(&cursor).await?;
        let mut complete = f.complete;
        while (rows.len() as i64) <= limit && !complete && !unread_only {
            if self.fetch_older(&a, &f).await? == 0 {
                break;
            }
            complete = self.folder_row(&f.id).await?.complete;
            rows = query(&cursor).await?;
        }
        let has_more = rows.len() as i64 > limit || (!complete && !unread_only);
        rows.truncate(limit as usize);
        Ok(MessagePage { messages: rows, has_more })
    }

    /// A cached message of one of `user_id`'s accounts, without its body.
    pub async fn message_row(&self, user_id: &str, id: &str) -> Result<(Account, MessageSummary)> {
        let m: MessageSummary = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {MSG_COLS} FROM mail_messages m WHERE m.id = ?")))
            .bind(id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)?;
        let a = self.mail_account(user_id, &m.account_id).await?;
        Ok((a, m))
    }

    /// Downloads a whole message (`BODY.PEEK[]`, so it stays unread).
    async fn fetch_raw(&self, a: &Account, m: &MessageSummary) -> Result<Vec<u8>> {
        let f = self.folder_row(&m.folder_id).await?;
        let mut conn = self.imap_conn(a).await?;
        let validity = conn.ensure_selected(&f.name).await?;
        if f.uidvalidity.is_some_and(|v| v as u32 != validity) {
            self.refresh_folder_soon(a, &f.id);
            return Err(Error::conflict("the folder changed on the server; refreshing"));
        }
        let mut fetched = conn.uid_fetch(&m.uid.to_string(), "(UID BODY.PEEK[])").await?;
        if fetched.is_empty() {
            // Make sure before calling it gone: a fresh SELECT sees every message.
            conn.select(&f.name).await?;
            fetched = conn.uid_fetch(&m.uid.to_string(), "(UID BODY.PEEK[])").await?;
        }
        match fetched.into_iter().find(|x| x.uid as i64 == m.uid).and_then(|x| x.body) {
            Some(b) => Ok(b),
            None => {
                // Expunged meanwhile.
                sqlx::query("DELETE FROM mail_messages WHERE id = ?").bind(&m.id).execute(&self.db).await?;
                self.hub.publish(
                    [a.user_id.as_str()],
                    &json!({"type": "mail.changed", "account_id": a.id, "folder_id": m.folder_id, "what": "messages"}),
                );
                Err(Error::NotFound)
            }
        }
    }

    /// The cached body, fetching and caching it first if needed.
    async fn body(&self, a: &Account, m: &MessageSummary) -> Result<BodyRow> {
        let cached: Option<BodyRow> = sqlx::query_as("SELECT text, html, attachments, remote_content, raw FROM mail_bodies WHERE message_id = ?")
            .bind(&m.id)
            .fetch_optional(&self.db)
            .await?;
        if let Some(b) = cached {
            sqlx::query("UPDATE mail_bodies SET accessed_at = ? WHERE message_id = ?")
                .bind(now())
                .bind(&m.id)
                .execute(&self.db)
                .await?;
            return Ok(b);
        }
        let raw = self.fetch_raw(a, m).await?;
        let parsed = parse::parse_body(&raw);
        let text = truncate(&parsed.text, TEXT_CAP).to_string();
        let html = parsed.html.as_deref().map(|h| truncate(h, HTML_CAP).to_string());
        let remote_content = html
            .as_deref()
            .is_some_and(|h| parse::sanitize_html(h, false, |_| String::new()).remote_blocked);
        let keep_raw = (raw.len() <= RAW_CAP).then_some(raw);
        let bytes = text.len() + html.as_ref().map_or(0, String::len) + keep_raw.as_ref().map_or(0, Vec::len);
        let attachments = serde_json::to_string(&parsed.attachments)?;
        sqlx::query(
            "INSERT INTO mail_bodies (message_id, account_id, text, html, attachments, remote_content, raw, bytes, accessed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(message_id) DO NOTHING",
        )
        .bind(&m.id)
        .bind(&a.id)
        .bind(&text)
        .bind(&html)
        .bind(&attachments)
        .bind(remote_content)
        .bind(&keep_raw)
        .bind(bytes as i64)
        .bind(now())
        .execute(&self.db)
        .await?;
        let has_attachments = parsed.attachments.iter().any(|x| !x.inline);
        sqlx::query("UPDATE mail_messages SET has_attachments = ?, preview = CASE WHEN ? = '' THEN preview ELSE ? END WHERE id = ?")
            .bind(has_attachments)
            .bind(&parsed.envelope.preview)
            .bind(&parsed.envelope.preview)
            .bind(&m.id)
            .execute(&self.db)
            .await?;
        // Index the full text now that it is known.
        sqlx::query("UPDATE fts SET body = ? WHERE rowid = (SELECT fts_rowid FROM mail_messages WHERE id = ?)")
            .bind(index_text(&parsed.envelope, truncate(&text, INDEX_CAP)))
            .bind(&m.id)
            .execute(&self.db)
            .await?;
        self.trim_body_cache(&a.id).await?;
        Ok(BodyRow {
            text,
            html,
            attachments,
            remote_content,
            raw: keep_raw,
        })
    }

    /// Evicts the least recently read bodies until the account is under its cache cap.
    async fn trim_body_cache(&self, account_id: &str) -> Result<()> {
        let total: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(bytes), 0) FROM mail_bodies WHERE account_id = ?")
            .bind(account_id)
            .fetch_one(&self.db)
            .await?;
        if total <= ACCOUNT_CACHE_CAP {
            return Ok(());
        }
        let mut excess = total - ACCOUNT_CACHE_CAP * 9 / 10;
        let oldest: Vec<(String, i64)> = sqlx::query_as("SELECT message_id, bytes FROM mail_bodies WHERE account_id = ? ORDER BY accessed_at LIMIT 500")
            .bind(account_id)
            .fetch_all(&self.db)
            .await?;
        let mut victims = Vec::new();
        for (id, b) in oldest {
            if excess <= 0 {
                break;
            }
            excess -= b;
            victims.push(id);
        }
        sqlx::query("DELETE FROM mail_bodies WHERE message_id IN (SELECT value FROM json_each(?))")
            .bind(serde_json::to_string(&victims)?)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// A message with its body (fetched from the server the first time).
    pub async fn mail_message(&self, user_id: &str, id: &str) -> Result<MessageView> {
        let (a, m) = self.message_row(user_id, id).await?;
        let body = self.body(&a, &m).await?;
        let folder = self.folder_row(&m.folder_id).await?;
        // Reply-To and Bcc are not cached with the envelope; the header is in the raw message
        // when it was small enough, else they are left out.
        let (reply_to, bcc) = match &body.raw {
            Some(raw) => {
                let p = parse::parse_body(raw);
                let bcc = mail_parser::MessageParser::default()
                    .parse(raw.as_slice())
                    .and_then(|msg| {
                        msg.bcc().map(|b| {
                            b.iter()
                                .filter_map(|x| {
                                    Some(Addr {
                                        name: x.name().unwrap_or("").to_string(),
                                        address: x.address()?.to_string(),
                                    })
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .unwrap_or_default();
                (p.envelope.reply_to, bcc)
            }
            None => (vec![], vec![]),
        };
        Ok(MessageView {
            folder_role: folder.role,
            reply_to,
            bcc,
            text: body.text,
            has_html: body.html.is_some(),
            remote_content: body.remote_content,
            attachments: serde_json::from_str(&body.attachments).unwrap_or_default(),
            summary: m,
        })
    }

    /// The sanitized HTML document of a message, for a sandboxed iframe. Remote images and
    /// styles are removed unless `allow_remote`.
    pub async fn mail_message_html(&self, user_id: &str, id: &str, allow_remote: bool) -> Result<String> {
        let (a, m) = self.message_row(user_id, id).await?;
        let body = self.body(&a, &m).await?;
        let html = match body.html {
            Some(h) => h,
            None => format!("<pre>{}</pre>", ammonia::clean_text(&body.text)),
        };
        let msg_id = m.id.clone();
        let s = parse::sanitize_html(&html, allow_remote, move |cid| {
            let enc: String = cid
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("/api/mail/messages/{msg_id}/cid/{enc}")
        });
        Ok(html_document(&s))
    }

    /// The whole message (from the cache when it was small enough).
    pub async fn mail_raw(&self, user_id: &str, id: &str) -> Result<(MessageSummary, Vec<u8>)> {
        let (a, m) = self.message_row(user_id, id).await?;
        let body = self.body(&a, &m).await?;
        let raw = match body.raw {
            Some(r) => r,
            None => self.fetch_raw(&a, &m).await?,
        };
        Ok((m, raw))
    }

    /// One attachment: (file name, MIME type, bytes).
    pub async fn mail_attachment(&self, user_id: &str, id: &str, index: usize) -> Result<(String, String, Vec<u8>)> {
        let (_, raw) = self.mail_raw(user_id, id).await?;
        parse::attachment(&raw, index).ok_or(Error::NotFound)
    }

    /// The inline part a `cid:` URL refers to.
    pub async fn mail_inline_part(&self, user_id: &str, id: &str, cid: &str) -> Result<(String, String, Vec<u8>)> {
        let (_, raw) = self.mail_raw(user_id, id).await?;
        parse::attachment_by_cid(&raw, cid).ok_or(Error::NotFound)
    }

    /// Saves an attachment into a folder of the user's files (a free name is picked when the
    /// name is taken).
    pub async fn mail_save_attachment(&self, user_id: &str, id: &str, index: usize, folder_id: &str) -> Result<Node> {
        let (name, _, data) = self.mail_attachment(user_id, id, index).await?;
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        for n in 1..=50 {
            let candidate = if n == 1 { name.clone() } else { format!("{stem} ({n}){ext}") };
            let opts = WriteOpts {
                create_only: true,
                ..WriteOpts::user()
            };
            match self.write_bytes(user_id, folder_id, &candidate, &data, opts).await {
                Err(Error::Conflict(_)) | Err(Error::PreconditionFailed) => continue,
                other => return other,
            }
        }
        Err(Error::conflict(format!("too many files named like {name}")))
    }

    /// The conversation of a message: every cached message of the thread, oldest first,
    /// one copy per Message-ID, leaving out Trash and Junk unless the message is there.
    pub async fn mail_thread(&self, user_id: &str, id: &str) -> Result<Vec<MessageSummary>> {
        let (a, m) = self.message_row(user_id, id).await?;
        let rows: Vec<(MessageSummary, Option<String>)> = {
            let list: Vec<MessageSummary> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {MSG_COLS} FROM mail_messages m WHERE m.account_id = ? AND m.thread_id = ? ORDER BY m.date, m.id"
            )))
            .bind(&a.id)
            .bind(&m.thread_id)
            .fetch_all(&self.db)
            .await?;
            let roles: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id, role FROM mail_folders WHERE account_id = ?")
                .bind(&a.id)
                .fetch_all(&self.db)
                .await?;
            list.into_iter()
                .map(|x| {
                    let role = roles.iter().find(|(fid, _)| *fid == x.folder_id).and_then(|(_, r)| r.clone());
                    (x, role)
                })
                .collect()
        };
        let own_role = rows.iter().find(|(x, _)| x.id == m.id).and_then(|(_, r)| r.clone());
        let hidden = |r: &Option<String>| matches!(r.as_deref(), Some("trash") | Some("junk")) && r != &own_role;
        let mut seen_ids = std::collections::HashSet::new();
        let mut out = Vec::new();
        // The opened copy wins over copies of the same message in other folders.
        let (mine, others): (Vec<_>, Vec<_>) = rows.into_iter().partition(|(x, _)| x.id == m.id);
        for (x, role) in mine.into_iter().chain(others) {
            if hidden(&role) {
                continue;
            }
            if let Some(mid) = &x.message_id {
                if !seen_ids.insert(mid.clone()) {
                    continue;
                }
            }
            out.push(x);
        }
        out.sort_by(|x, y| x.date.cmp(&y.date).then_with(|| x.id.cmp(&y.id)));
        Ok(out)
    }

    /// Cached messages by id, grouped per (account, folder), all owned by `user_id`.
    pub(crate) async fn group_messages(&self, user_id: &str, ids: &[String]) -> Result<Vec<(Account, super::store::Folder, Vec<MessageSummary>)>> {
        let mut groups: Vec<(Account, Folder, Vec<MessageSummary>)> = Vec::new();
        for id in ids {
            let (a, m) = self.message_row(user_id, id).await?;
            match groups.iter_mut().find(|g| g.1.id == m.folder_id) {
                Some(g) => g.2.push(m),
                None => {
                    let f = self.folder_row(&m.folder_id).await?;
                    groups.push((a, f, vec![m]));
                }
            }
        }
        Ok(groups)
    }

    pub(crate) fn mail_changed(&self, a: &Account, folder_ids: &[&str]) {
        for f in folder_ids {
            self.hub.publish(
                [a.user_id.as_str()],
                &json!({"type": "mail.changed", "account_id": a.id, "folder_id": f, "what": "messages"}),
            );
        }
    }

    /// Marks messages read/unread or flagged/unflagged, on the server and in the cache.
    pub async fn mail_set_flags(&self, user_id: &str, ids: &[String], action: FlagAction) -> Result<()> {
        let (op, col, value) = match action {
            FlagAction::Read => ("+FLAGS.SILENT (\\Seen)", "seen", true),
            FlagAction::Unread => ("-FLAGS.SILENT (\\Seen)", "seen", false),
            FlagAction::Flag => ("+FLAGS.SILENT (\\Flagged)", "flagged", true),
            FlagAction::Unflag => ("-FLAGS.SILENT (\\Flagged)", "flagged", false),
        };
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            let uids: Vec<u32> = msgs.iter().map(|m| m.uid as u32).collect();
            let mut conn = self.imap_conn(&a).await?;
            conn.ensure_selected(&f.name).await?;
            conn.uid_store(&uid_set(&uids), op).await?;
            drop(conn);
            let changed: Vec<&String> = msgs
                .iter()
                .filter(|m| if col == "seen" { m.seen != value } else { m.flagged != value })
                .map(|m| &m.id)
                .collect();
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE mail_messages SET {col} = ? WHERE id IN (SELECT value FROM json_each(?))"
            )))
            .bind(value)
            .bind(serde_json::to_string(&changed)?)
            .execute(&self.db)
            .await?;
            if col == "seen" && !changed.is_empty() {
                let delta = if value { -(changed.len() as i64) } else { changed.len() as i64 };
                sqlx::query("UPDATE mail_folders SET unread = MAX(0, unread + ?) WHERE id = ?")
                    .bind(delta)
                    .bind(&f.id)
                    .execute(&self.db)
                    .await?;
            }
            self.mail_changed(&a, &[&f.id]);
        }
        Ok(())
    }

    /// The folder with `role`, created on the server (with `default_name`) when missing.
    pub(crate) async fn ensure_role_folder(&self, a: &Account, role: &str, default_name: &str) -> Result<Folder> {
        if let Some(f) = self.folder_by_role(&a.id, role).await? {
            return Ok(f);
        }
        let existing: Option<Folder> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {FOLDER_COLS} FROM mail_folders WHERE account_id = ? AND lower(name) = lower(?)"
        )))
        .bind(&a.id)
        .bind(default_name)
        .fetch_optional(&self.db)
        .await?;
        if existing.is_none() {
            let mut conn = self.imap_conn(a).await?;
            conn.create(default_name).await?;
            self.sync_folders_on(&mut conn, a).await?;
        }
        let f: Folder = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {FOLDER_COLS} FROM mail_folders WHERE account_id = ? AND lower(name) = lower(?)"
        )))
        .bind(&a.id)
        .bind(default_name)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| Error::Unavailable(format!("the server did not create the folder {default_name}")))?;
        sqlx::query("UPDATE mail_folders SET role = ? WHERE id = ?")
            .bind(role)
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        self.folder_row(&f.id).await
    }

    /// Moves messages to a folder of the same account.
    pub async fn mail_move(&self, user_id: &str, ids: &[String], dest_folder_id: &str) -> Result<()> {
        let (dest_account, dest) = self.mail_folder(user_id, dest_folder_id).await?;
        if !dest.selectable {
            return Err(Error::bad("messages cannot be moved into that folder"));
        }
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            if a.id != dest_account.id {
                return Err(Error::bad("messages can only be moved within their account"));
            }
            if f.id == dest.id {
                continue;
            }
            self.move_group(&a, &f, &msgs, &dest).await?;
        }
        Ok(())
    }

    pub(crate) async fn move_group(&self, a: &Account, from: &Folder, msgs: &[MessageSummary], dest: &Folder) -> Result<()> {
        let uids: Vec<u32> = msgs.iter().map(|m| m.uid as u32).collect();
        let mut conn = self.imap_conn(a).await?;
        conn.ensure_selected(&from.name).await?;
        conn.uid_move(&uid_set(&uids), &dest.name).await?;
        drop(conn);
        self.forget_messages(from, msgs).await?;
        self.mail_changed(a, &[&from.id, &dest.id]);
        // The moved copies get new UIDs there; pick them up.
        let (core, a, dest_id) = (self.clone(), a.clone(), dest.id.clone());
        tokio::spawn(async move {
            if let Err(e) = core.sync_mail_folder(&a, &dest_id).await {
                tracing::debug!(error = %e, "syncing the destination folder failed");
            }
        });
        Ok(())
    }

    /// Drops messages from the cache and fixes the folder's counters.
    pub(crate) async fn forget_messages(&self, f: &Folder, msgs: &[MessageSummary]) -> Result<()> {
        let ids: Vec<&String> = msgs.iter().map(|m| &m.id).collect();
        let unread = msgs.iter().filter(|m| !m.seen).count() as i64;
        sqlx::query("DELETE FROM mail_messages WHERE id IN (SELECT value FROM json_each(?))")
            .bind(serde_json::to_string(&ids)?)
            .execute(&self.db)
            .await?;
        sqlx::query("UPDATE mail_folders SET total = MAX(0, total - ?), unread = MAX(0, unread - ?) WHERE id = ?")
            .bind(msgs.len() as i64)
            .bind(unread)
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Moves messages to the account's Archive folder (created when missing).
    pub async fn mail_archive(&self, user_id: &str, ids: &[String]) -> Result<()> {
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            let dest = self.ensure_role_folder(&a, "archive", "Archive").await?;
            if dest.id != f.id {
                self.move_group(&a, &f, &msgs, &dest).await?;
            }
        }
        Ok(())
    }

    /// Moves messages to Trash; messages already in Trash (or Junk) are deleted for good.
    pub async fn mail_delete(&self, user_id: &str, ids: &[String]) -> Result<()> {
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            if matches!(f.role.as_deref(), Some("trash") | Some("junk")) {
                let uids: Vec<u32> = msgs.iter().map(|m| m.uid as u32).collect();
                let set = uid_set(&uids);
                let mut conn = self.imap_conn(&a).await?;
                conn.ensure_selected(&f.name).await?;
                conn.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?;
                conn.expunge(&set).await?;
                drop(conn);
                self.forget_messages(&f, &msgs).await?;
                self.mail_changed(&a, &[&f.id]);
            } else {
                let trash = self.ensure_role_folder(&a, "trash", "Trash").await?;
                self.move_group(&a, &f, &msgs, &trash).await?;
            }
        }
        Ok(())
    }

    /// Searches mail: cached messages through the search index, plus an IMAP SEARCH on the
    /// server (the given folder, else Inbox, Sent and Archive of each account) for messages
    /// whose bodies are not cached or that are older than the cached window.
    pub async fn mail_search(&self, user_id: &str, q: &str, account_id: Option<&str>, folder_id: Option<&str>, limit: usize) -> Result<Vec<MessageSummary>> {
        let q = q.trim();
        if q.is_empty() {
            return Ok(vec![]);
        }
        let accounts: Vec<Account> = match account_id {
            Some(id) => vec![self.mail_account(user_id, id).await?],
            None => self.mail_accounts(user_id).await?,
        };
        let mut ids: Vec<String> = self.search(user_id, q, Some(&["mail"]), 200).await?.into_iter().map(|h| h.id).collect();
        for a in &accounts {
            let folders: Vec<Folder> = match folder_id {
                Some(fid) => {
                    let (fa, f) = self.mail_folder(user_id, fid).await?;
                    if fa.id != a.id {
                        continue;
                    }
                    vec![f]
                }
                None => {
                    let mut v = Vec::new();
                    for role in ["inbox", "sent", "archive"] {
                        if let Some(f) = self.folder_by_role(&a.id, role).await? {
                            v.push(f);
                        }
                    }
                    v
                }
            };
            for f in folders {
                match self.server_search(a, &f, q).await {
                    Ok(found) => ids.extend(found),
                    Err(e) => tracing::info!(account = %a.id, folder = %f.name, error = %e, "server-side mail search failed"),
                }
            }
        }
        let rows: Vec<MessageSummary> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {MSG_COLS} FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id
             WHERE a.user_id = ? AND m.id IN (SELECT value FROM json_each(?)) AND (? IS NULL OR m.account_id = ?) AND (? IS NULL OR m.folder_id = ?)
             ORDER BY m.date DESC LIMIT ?"
        )))
        .bind(user_id)
        .bind(serde_json::to_string(&ids)?)
        .bind(account_id)
        .bind(account_id)
        .bind(folder_id)
        .bind(folder_id)
        .bind(limit as i64)
        .fetch_all(&self.db)
        .await?;
        Ok(rows)
    }

    /// IMAP SEARCH TEXT in one folder; uncached matches are fetched into the cache. Returns
    /// the ids of the (at most 50 newest) matches.
    async fn server_search(&self, a: &Account, f: &Folder, q: &str) -> Result<Vec<String>> {
        let mut conn = self.imap_conn(a).await?;
        let sel = conn.select(&f.name).await?;
        if f.uidvalidity.is_some_and(|v| v as u32 != sel.uidvalidity) {
            return Ok(vec![]);
        }
        let charset = if q.is_ascii() { "" } else { "CHARSET UTF-8 " };
        let uids = conn.uid_search(&format!("{charset}TEXT {}", quote(q))).await?;
        let newest: Vec<u32> = uids.into_iter().rev().take(50).collect();
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
}
