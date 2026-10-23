//! Writing mail: reply/forward templates (plain text and HTML quoting), composer uploads,
//! recipient suggestions (contacts, users, groups, past correspondents) and recipient
//! details, drafts (IMAP APPEND to Drafts) and sending (the account's SMTP through `mime`,
//! then APPEND to Sent unless the provider files sent mail itself). Messages are sent as the
//! account or one of its aliases; group recipients expand to the members' addresses; local
//! images of HTML bodies become `cid:` parts; large files go out as share links.
//!
//! The web composer queues messages in the outbox (`outbox.rs`) rather than sending them
//! here directly; both end in [`Core::send_composed`].

use super::Account;
use super::aliases::Identity;
use super::imap::uid_set;
use super::mime::{self, InlineImage, MessageSpec};
use super::parse::{self, Addr};
use super::richtext::{self, QUOTE_CLASS};
use crate::files::{Node, WriteOpts, perm};
use crate::groupware::ContactView;
use crate::mailer::{self, Attachment};
use crate::shares::NewShare;
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use lettre::message::Mailboxes;
use lettre::message::header::{Bcc, To};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use tokio::io::AsyncRead;

/// Attachments (and inline images) per message, in total.
pub const MAX_ATTACHMENTS_BYTES: usize = 25 * 1024 * 1024;
/// Composer uploads kept per user (until sent, saved or a day old).
const MAX_UPLOADS_BYTES: i64 = 100 * 1024 * 1024;
/// Where large attachments shared as links are kept in the user's Files.
pub const LINK_FOLDER: &str = "Mail attachments";
/// How long attachment links work unless the user picks another expiry.
pub const LINK_DAYS: i64 = 14;

/// Where an attachment of an outgoing message comes from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AttachmentRef {
    /// Uploaded in the composer.
    Upload { id: String },
    /// A file in the user's Files.
    File { id: String },
    /// An attachment of a cached message (forwarding, or reopening a draft).
    Message { message_id: String, index: usize },
    /// A whole cached message, attached as `message/rfc822` (forward as attachment).
    Eml { message_id: String },
    /// A copy kept with a message in the outbox (reopened for editing).
    Outbox { id: String },
}

/// A message being written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Compose {
    /// Recipients: `Name <a@b.c>`, `a@b.c`, or `group:<id>` for a Ferrite group.
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub subject: String,
    /// The plain-text body (ignored when `html` is set: the text alternative is rendered
    /// from the HTML).
    #[serde(default)]
    pub text: String,
    /// The rich body (sanitized on the server).
    #[serde(default)]
    pub html: Option<String>,
    /// The cached message this replies to (threading headers, \Answered after sending).
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// A draft this replaces (deleted once the new version is saved or the mail is sent).
    #[serde(default)]
    pub draft_id: Option<String>,
    /// Send as this alias of the account (else as the account itself).
    #[serde(default)]
    pub alias_id: Option<String>,
    /// Ask for a read receipt (`Disposition-Notification-To`).
    #[serde(default)]
    pub request_mdn: bool,
}

/// A prefilled composer: the compose fields plus attachment names for display.
#[derive(Debug, Clone, Serialize)]
pub struct ComposeTemplate {
    pub account_id: String,
    #[serde(flatten)]
    pub compose: Compose,
    pub attachment_names: Vec<(String, usize)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyMode {
    Reply,
    ReplyAll,
    Forward,
    /// Forward the whole message as a `message/rfc822` attachment.
    ForwardAsAttachment,
    /// Reopen a draft for editing.
    Edit,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Upload {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
}

/// A recipient suggestion.
#[derive(Debug, Clone, Serialize)]
pub struct Recipient {
    pub name: String,
    /// The address, or `group:<id>` for a group.
    pub address: String,
    /// contact, user, group or recent
    pub source: &'static str,
    /// Members with an email address (groups).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<usize>,
}

/// What Ferrite knows about a recipient (the composer's side pane).
#[derive(Debug, Clone, Serialize)]
pub struct RecipientInfo {
    pub address: String,
    pub contacts: Vec<ContactView>,
    /// A Ferrite user with this address.
    pub user: Option<serde_json::Value>,
    /// Cached messages from this address, and the latest one's date.
    pub messages: i64,
    pub last_message: Option<DateTime<Utc>>,
}

/// A file shared as a link instead of being attached.
#[derive(Debug, Clone, Serialize)]
pub struct SharedLink {
    pub file_id: String,
    pub name: String,
    pub size: i64,
    pub url: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// A built message: what goes over SMTP, the copy for Sent/Drafts (with Bcc), its
/// Message-ID and the composer uploads it used.
pub(crate) struct Built {
    pub msg: lettre::Message,
    pub copy: Vec<u8>,
    pub message_id: String,
    pub uploads: Vec<String>,
}

fn same_address(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Text body with the signature above or below the quoted original (or alone).
pub fn text_with_signature(quote: &str, signature: &str, above: bool) -> String {
    if signature.trim().is_empty() {
        return if quote.is_empty() { String::new() } else { quote.to_string() };
    }
    let block = format!("-- \n{}", signature.trim_end());
    if above || quote.is_empty() {
        format!("\n\n{block}{quote}")
    } else {
        format!("{quote}\n{block}\n")
    }
}

/// HTML body: an empty first line to write on, then the signature and the quote in order.
pub fn html_with_signature(quote: &str, signature: &str, above: bool) -> String {
    let lead = "<div><br></div>";
    if above {
        format!("{lead}{signature}{quote}")
    } else {
        format!("{lead}{quote}{signature}")
    }
}

fn escape(s: &str) -> String {
    ammonia::clean_text(s)
}

fn attribution(from: &str, date: Option<DateTime<Utc>>) -> String {
    match date {
        Some(d) => format!("On {}, {from} wrote:", d.format("%a, %-d %b %Y at %H:%M UTC")),
        None => format!("{from} wrote:"),
    }
}

/// Percent-encodes a Content-ID for a URL path segment.
fn encode_cid(cid: &str) -> String {
    cid.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn decode_percent(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Which local image a source points at.
enum ImageSource {
    Upload(String),
    OutboxFile(String),
    MessagePart(String, String),
    Data(String, Vec<u8>),
}

fn image_source(src: &str) -> Option<ImageSource> {
    let s = src.trim();
    if let Some(rest) = s.strip_prefix("/api/mail/uploads/") {
        return rest.strip_suffix("/content").map(|id| ImageSource::Upload(id.to_string()));
    }
    if let Some(rest) = s.strip_prefix("/api/mail/outbox/") {
        let (_, file) = rest.split_once("/files/")?;
        return Some(ImageSource::OutboxFile(file.to_string()));
    }
    if let Some(rest) = s.strip_prefix("/api/mail/messages/") {
        let (mid, cid) = rest.split_once("/cid/")?;
        return Some(ImageSource::MessagePart(mid.to_string(), decode_percent(cid)));
    }
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("data:image/") {
        let (meta, data) = s[5..].split_once(',')?;
        let mime = meta.split(';').next()?.to_ascii_lowercase();
        if !meta.to_ascii_lowercase().ends_with(";base64") {
            return None;
        }
        let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).ok()?;
        return Some(ImageSource::Data(mime, bytes));
    }
    None
}

fn ext_for(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    }
}

impl Core {
    /// HTML of a cached message for quoting or editing, sanitized for composing; its `cid:`
    /// images are mapped with `cid_src`. `None` when the message has no HTML part.
    async fn original_html(&self, user_id: &str, id: &str, cid_src: impl Fn(&str) -> String) -> Result<Option<(String, Vec<u8>)>> {
        let (_, raw) = self.mail_raw(user_id, id).await?;
        let Some(html) = parse::parse_body(&raw).html else { return Ok(None) };
        let clean = richtext::sanitize(&html);
        let mapped = richtext::rewrite_images(&clean, |src| {
            if src.to_ascii_lowercase().starts_with("cid:") {
                Some(cid_src(&src[4..]))
            } else {
                Some(src.to_string())
            }
        });
        Ok(Some((mapped, raw)))
    }

    /// A prefilled reply, reply-all or forward of a message (quoting it), or a draft to edit.
    /// Replies and forwards go out from the alias the original was addressed to, if any.
    pub async fn mail_compose_template(&self, user_id: &str, id: &str, mode: ReplyMode) -> Result<ComposeTemplate> {
        let view = self.mail_message(user_id, id).await?;
        let a = self.mail_account(user_id, &view.summary.account_id).await?;
        let prefs = self.compose_prefs_of(&a).await?;
        let above = prefs.signature_above_quote;
        let m = &view.summary;
        let from = m.from();
        let from_label = from.display();
        let mut compose = Compose::default();
        let mut attachment_names = Vec::new();
        if mode != ReplyMode::Edit {
            let addrs: Vec<&str> = m.to_addrs.iter().chain(&m.cc_addrs).map(|x| x.address.as_str()).collect();
            compose.alias_id = self.alias_for_addresses(&a, &addrs).await?;
        }
        let ident = self.mail_identity(&a, compose.alias_id.as_deref()).await?;
        let msg_id = m.id.clone();
        let quoted = || async {
            Ok::<String, Error>(
                match self
                    .original_html(user_id, &msg_id, |cid| format!("/api/mail/messages/{msg_id}/cid/{}", encode_cid(cid)))
                    .await?
                {
                    Some((h, _)) => h,
                    None => richtext::text_to_html(&view.text),
                },
            )
        };
        match mode {
            ReplyMode::Reply | ReplyMode::ReplyAll => {
                // Replying to one's own sent message goes to its recipients again.
                let own = same_address(&from.address, &a.email) || same_address(&from.address, &ident.email);
                let primary: Vec<Addr> = if own {
                    m.to_addrs.clone()
                } else if !view.reply_to.is_empty() {
                    view.reply_to.clone()
                } else {
                    vec![from.clone()]
                };
                compose.to = primary.iter().map(Addr::display).collect();
                if mode == ReplyMode::ReplyAll {
                    let mut cc: Vec<Addr> = Vec::new();
                    for x in m.to_addrs.iter().chain(&m.cc_addrs) {
                        let dup = same_address(&x.address, &a.email)
                            || same_address(&x.address, &ident.email)
                            || primary.iter().any(|p| same_address(&p.address, &x.address))
                            || cc.iter().any(|c| same_address(&c.address, &x.address));
                        if !dup {
                            cc.push(x.clone());
                        }
                    }
                    compose.cc = cc.iter().map(Addr::display).collect();
                }
                compose.subject = parse::prefixed_subject("Re: ", &m.subject);
                compose.text = text_with_signature(&parse::quote_reply(&view.text, &from_label, Some(m.date)), &ident.signature_text, above);
                let quote = format!(
                    "<div class=\"{QUOTE_CLASS}\"><div>{}</div><blockquote type=\"cite\" style=\"margin: 0 0 0 0.8ex; border-left: 2px solid #ccc; padding-left: 1ex\">{}</blockquote></div>",
                    escape(&attribution(&from_label, Some(m.date))),
                    quoted().await?
                );
                compose.html = Some(html_with_signature(&quote, &ident.signature_html, above));
                compose.in_reply_to = Some(m.id.clone());
            }
            ReplyMode::Forward => {
                compose.subject = parse::prefixed_subject("Fwd: ", &m.subject);
                let to = m.to_addrs.iter().map(Addr::display).collect::<Vec<_>>().join(", ");
                compose.text = text_with_signature(
                    &parse::quote_forward(&view.text, &from_label, Some(m.date), &m.subject, &to),
                    &ident.signature_text,
                    above,
                );
                let mut head = format!(
                    "<div>---------- Forwarded message ----------</div><div>From: {}</div><div>Date: {}</div><div>Subject: {}</div>",
                    escape(&from_label),
                    m.date.format("%a, %-d %b %Y %H:%M UTC"),
                    escape(&m.subject)
                );
                if !to.is_empty() {
                    head.push_str(&format!("<div>To: {}</div>", escape(&to)));
                }
                let quote = format!("<div class=\"{QUOTE_CLASS}\">{head}<div><br></div>{}</div>", quoted().await?);
                compose.html = Some(html_with_signature(&quote, &ident.signature_html, above));
                for att in view.attachments.iter().filter(|x| !x.inline) {
                    compose.attachments.push(AttachmentRef::Message {
                        message_id: m.id.clone(),
                        index: att.index,
                    });
                    attachment_names.push((att.name.clone(), att.size));
                }
            }
            ReplyMode::ForwardAsAttachment => {
                compose.subject = parse::prefixed_subject("Fwd: ", &m.subject);
                compose.text = text_with_signature("", &ident.signature_text, above);
                compose.html = Some(html_with_signature("", &ident.signature_html, above));
                compose.attachments.push(AttachmentRef::Eml { message_id: m.id.clone() });
                attachment_names.push((eml_name(&m.subject), m.size as usize));
            }
            ReplyMode::Edit => {
                compose.to = m.to_addrs.iter().map(Addr::display).collect();
                compose.cc = m.cc_addrs.iter().map(Addr::display).collect();
                compose.bcc = view.bcc.iter().map(Addr::display).collect();
                compose.subject = m.subject.clone();
                compose.text = view.text.clone();
                compose.draft_id = Some(m.id.clone());
                compose.alias_id = self.alias_for_addresses(&a, &[from.address.as_str()]).await?;
                // Inline images of the draft become composer uploads: the draft itself is
                // replaced when it is saved again.
                if let Some((html, raw)) = self.original_html(user_id, &m.id, |cid| format!("cid:{cid}")).await? {
                    let mut map: HashMap<String, String> = HashMap::new();
                    for src in richtext::image_sources(&html) {
                        if let Some(cid) = src.strip_prefix("cid:") {
                            if let Some((name, mime, data)) = parse::attachment_by_cid(&raw, cid) {
                                let up = self.mail_upload(user_id, &name, &mime, data).await?;
                                map.insert(src.clone(), format!("/api/mail/uploads/{}/content", up.id));
                            }
                        }
                    }
                    compose.html = Some(richtext::rewrite_images(&html, |src| map.get(src).cloned().or_else(|| Some(src.to_string()))));
                    compose.request_mdn = mail_parser::MessageParser::default()
                        .parse(raw.as_slice())
                        .is_some_and(|p| p.header("Disposition-Notification-To").is_some());
                }
                if let Some(parent) = &m.in_reply_to {
                    compose.in_reply_to = sqlx::query_scalar("SELECT id FROM mail_messages WHERE account_id = ? AND message_id = ? LIMIT 1")
                        .bind(&a.id)
                        .bind(parent)
                        .fetch_optional(&self.db)
                        .await?;
                }
                for att in view.attachments.iter().filter(|x| !x.inline) {
                    compose.attachments.push(AttachmentRef::Message {
                        message_id: m.id.clone(),
                        index: att.index,
                    });
                    attachment_names.push((att.name.clone(), att.size));
                }
            }
        }
        Ok(ComposeTemplate {
            account_id: a.id,
            compose,
            attachment_names,
        })
    }

    /// Stores a file uploaded in the composer.
    pub async fn mail_upload(&self, user_id: &str, name: &str, mime: &str, data: Vec<u8>) -> Result<Upload> {
        let name = name.trim().replace(['/', '\\', '\0'], "_");
        if name.is_empty() {
            return Err(Error::bad("the upload needs a file name"));
        }
        if data.len() > MAX_ATTACHMENTS_BYTES {
            return Err(Error::bad(format!(
                "attachments can be at most {} MB; share larger files as a link",
                MAX_ATTACHMENTS_BYTES / (1024 * 1024)
            )));
        }
        let used: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(size), 0) FROM mail_uploads WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        if used + data.len() as i64 > MAX_UPLOADS_BYTES {
            return Err(Error::bad("too many pending attachments; send or discard some drafts first"));
        }
        let mime = if mime.is_empty() || mime == "application/octet-stream" {
            crate::util::guess_mime(&name)
        } else {
            mime.to_string()
        };
        let id = new_id();
        sqlx::query("INSERT INTO mail_uploads (id, user_id, name, mime, size, data, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
            .bind(&id)
            .bind(user_id)
            .bind(&name)
            .bind(&mime)
            .bind(data.len() as i64)
            .bind(&data)
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(Upload {
            id,
            name,
            mime,
            size: data.len() as i64,
        })
    }

    /// A composer upload: (name, MIME type, bytes).
    pub async fn mail_upload_content(&self, user_id: &str, id: &str) -> Result<(String, String, Vec<u8>)> {
        sqlx::query_as("SELECT name, mime, data FROM mail_uploads WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    pub async fn mail_discard_upload(&self, user_id: &str, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM mail_uploads WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Address suggestions for `q`: contacts, other users, groups (which expand to their
    /// members when sending), then people the user has mail from.
    pub async fn mail_recipients(&self, user_id: &str, q: &str) -> Result<Vec<Recipient>> {
        let q = q.trim();
        let mut out: Vec<Recipient> = Vec::new();
        let push = |out: &mut Vec<Recipient>, name: &str, address: &str, source: &'static str| {
            if address.contains('@') && !out.iter().any(|r| same_address(&r.address, address)) {
                out.push(Recipient {
                    name: name.to_string(),
                    address: address.to_string(),
                    source,
                    members: None,
                });
            }
        };
        if !q.is_empty() {
            for c in self.contacts(user_id, Some(q)).await? {
                for e in &c.emails {
                    push(&mut out, &c.name, e, "contact");
                }
            }
            for u in self.list_users(Some(q), 20).await? {
                if u.disabled || u.id == user_id {
                    continue;
                }
                if let Some(e) = u.email.as_deref() {
                    push(&mut out, &u.display_name, e, "user");
                }
            }
            let lq = q.to_lowercase();
            for g in self.list_groups().await? {
                if !g.name.to_lowercase().contains(&lq) {
                    continue;
                }
                let members = self.group_addresses(&g.id).await?.len();
                if members > 0 {
                    out.push(Recipient {
                        name: g.name.clone(),
                        address: format!("group:{}", g.id),
                        source: "group",
                        members: Some(members),
                    });
                }
            }
        }
        let like = format!("%{}%", q.replace(['%', '_'], ""));
        let recent: Vec<(String, String)> = sqlx::query_as(
            "SELECT m.from_name, m.from_addr FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id
             WHERE a.user_id = ? AND m.from_addr != '' AND lower(m.from_addr) != lower(a.email) AND (m.from_addr LIKE ? OR m.from_name LIKE ?)
             GROUP BY lower(m.from_addr) ORDER BY COUNT(*) DESC, MAX(m.date) DESC LIMIT 10",
        )
        .bind(user_id)
        .bind(&like)
        .bind(&like)
        .fetch_all(&self.db)
        .await?;
        for (name, address) in recent {
            push(&mut out, &name, &address, "recent");
        }
        out.truncate(12);
        Ok(out)
    }

    /// The addresses of a group's members (enabled users with an email address).
    async fn group_addresses(&self, group_id: &str) -> Result<Vec<Addr>> {
        Ok(self
            .group_members(group_id)
            .await?
            .into_iter()
            .filter(|u| !u.disabled)
            .filter_map(|u| {
                let e = u.email.filter(|e| Addr::parse(e).is_some())?;
                Some(Addr {
                    name: u.display_name,
                    address: e,
                })
            })
            .collect())
    }

    /// Recipients as header addresses: `group:<id>` (or `group:<name>`) becomes the
    /// members' addresses; duplicates are dropped.
    async fn expand_recipients(&self, list: &[String], what: &str) -> Result<Vec<String>> {
        let mut out: Vec<Addr> = Vec::new();
        let add = |out: &mut Vec<Addr>, a: Addr| {
            if !out.iter().any(|x| same_address(&x.address, &a.address)) {
                out.push(a);
            }
        };
        for s in list.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if let Some(g) = s.strip_prefix("group:") {
                let groups = self.list_groups().await?;
                let group = groups
                    .iter()
                    .find(|x| x.id == g || x.name == g)
                    .ok_or_else(|| Error::bad(format!("{what}: there is no group {g:?}")))?;
                let members = self.group_addresses(&group.id).await?;
                if members.is_empty() {
                    return Err(Error::bad(format!("{what}: no member of the group {} has an email address", group.name)));
                }
                for m in members {
                    add(&mut out, m);
                }
                continue;
            }
            let a = parse::Addr::parse(s).ok_or_else(|| Error::bad(format!("{what}: {s:?} is not an email address")))?;
            add(&mut out, a);
        }
        Ok(out.iter().map(Addr::display).collect())
    }

    /// Saved contact details, user and correspondence for an address.
    pub async fn mail_recipient_info(&self, user_id: &str, address: &str) -> Result<RecipientInfo> {
        let addr = Addr::parse(address)
            .ok_or_else(|| Error::bad(format!("{address:?} is not an email address")))?
            .address;
        let contacts: Vec<ContactView> = self
            .contacts(user_id, Some(&addr))
            .await?
            .into_iter()
            .filter(|c| c.emails.iter().any(|e| same_address(e, &addr)))
            .collect();
        let user = self
            .list_users(Some(&addr), 20)
            .await?
            .into_iter()
            .find(|u| !u.disabled && u.email.as_deref().is_some_and(|e| same_address(e, &addr)))
            .map(|u| json!({"username": u.username, "display_name": u.display_name}));
        let (messages, last_message): (i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT COUNT(*), MAX(m.date) FROM mail_messages m JOIN mail_accounts a ON a.id = m.account_id WHERE a.user_id = ? AND lower(m.from_addr) = lower(?)",
        )
        .bind(user_id)
        .bind(&addr)
        .fetch_one(&self.db)
        .await?;
        Ok(RecipientInfo {
            address: addr,
            contacts,
            user,
            messages,
            last_message,
        })
    }

    async fn load_attachments(&self, user_id: &str, refs: &[AttachmentRef], mut total: usize) -> Result<Vec<Attachment>> {
        let limit = self.mail_attachment_limit().await? as usize;
        let mut out = Vec::new();
        for r in refs {
            let (filename, content_type, data) = self.attachment_data(user_id, r).await?;
            if limit > 0 && data.len() > limit {
                return Err(Error::bad(format!(
                    "{filename} is larger than the attachment limit of {}; share it as a link instead",
                    human_size(limit as i64)
                )));
            }
            total += data.len();
            if total > MAX_ATTACHMENTS_BYTES {
                return Err(Error::bad(format!(
                    "attachments can be at most {} MB in total; share large files as links",
                    MAX_ATTACHMENTS_BYTES / (1024 * 1024)
                )));
            }
            out.push(Attachment { filename, content_type, data });
        }
        Ok(out)
    }

    /// The bytes of one attachment reference: (file name, MIME type, data).
    pub(crate) async fn attachment_data(&self, user_id: &str, r: &AttachmentRef) -> Result<(String, String, Vec<u8>)> {
        Ok(match r {
            AttachmentRef::Upload { id } => self
                .mail_upload_content(user_id, id)
                .await
                .map_err(|_| Error::bad("an uploaded attachment expired; attach it again"))?,
            AttachmentRef::File { id } => {
                let (node, data) = self.read_file(user_id, id).await?;
                (node.name, node.mime, data)
            }
            AttachmentRef::Message { message_id, index } => self.mail_attachment(user_id, message_id, *index).await?,
            AttachmentRef::Eml { message_id } => {
                let (m, raw) = self.mail_raw(user_id, message_id).await?;
                (eml_name(&m.subject), "message/rfc822".to_string(), raw)
            }
            AttachmentRef::Outbox { id } => sqlx::query_as(
                "SELECT f.name, f.mime, f.data FROM mail_outbox_files f JOIN mail_outbox o ON o.id = f.outbox_id WHERE f.id = ? AND o.user_id = ?",
            )
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?
            .ok_or_else(|| Error::bad("an attachment of the queued message is gone; attach it again"))?,
        })
    }

    /// The bytes behind a local image source of a composed body.
    async fn image_data(&self, user_id: &str, src: &str) -> Result<Option<(String, String, Vec<u8>)>> {
        Ok(match image_source(src) {
            Some(ImageSource::Upload(id)) => self.mail_upload_content(user_id, &id).await.ok(),
            Some(ImageSource::OutboxFile(id)) => self.attachment_data(user_id, &AttachmentRef::Outbox { id }).await.ok(),
            Some(ImageSource::MessagePart(mid, cid)) => self.mail_inline_part(user_id, &mid, &cid).await.ok(),
            Some(ImageSource::Data(mime, data)) => Some((format!("image.{}", ext_for(&mime)), mime, data)),
            None => None,
        })
    }

    /// Turns the local images of sanitized HTML into `cid:` parts: (HTML, parts, upload ids
    /// used). Images that cannot be found are left out.
    async fn inline_images(&self, user_id: &str, html: &str) -> Result<(String, Vec<InlineImage>, Vec<String>, usize)> {
        let mut map: HashMap<String, String> = HashMap::new();
        let mut parts = Vec::new();
        let mut uploads = Vec::new();
        let mut total = 0usize;
        for src in richtext::image_sources(html) {
            let Some((filename, content_type, data)) = self.image_data(user_id, &src).await? else {
                continue;
            };
            if !content_type.starts_with("image/") {
                continue;
            }
            total += data.len();
            if total > MAX_ATTACHMENTS_BYTES {
                return Err(Error::bad("the images in this message are too large"));
            }
            if let Some(ImageSource::Upload(id)) = image_source(&src) {
                uploads.push(id);
            }
            let content_id = format!("{}@ferrite", new_id());
            map.insert(src.clone(), format!("cid:{content_id}"));
            parts.push(InlineImage {
                content_id,
                filename,
                content_type,
                data,
            });
        }
        let html = richtext::rewrite_images(html, |src| map.get(src).cloned());
        Ok((html, parts, uploads, total))
    }

    /// Builds the message. The Sent/Drafts copy keeps the Bcc header that SMTP delivery
    /// leaves out.
    pub(crate) async fn build_outgoing(&self, user_id: &str, a: &Account, c: &Compose, draft: bool) -> Result<Built> {
        let ident: Identity = self.mail_identity(a, c.alias_id.as_deref()).await?;
        let to = self.expand_recipients(&c.to, "To").await?;
        let cc = self.expand_recipients(&c.cc, "Cc").await?;
        let bcc = self.expand_recipients(&c.bcc, "Bcc").await?;
        let (text, html, inline, mut uploads, images_size) = match c.html.as_deref() {
            Some(h) => {
                let clean = richtext::sanitize(h);
                let text = richtext::html_to_text(&clean);
                let (html, inline, uploads, size) = self.inline_images(user_id, &clean).await?;
                (text, Some(html), inline, uploads, size)
            }
            None => (c.text.clone(), None, vec![], vec![], 0),
        };
        let mut spec = MessageSpec {
            from: ident.sender(),
            to: to.clone(),
            cc,
            bcc: bcc.clone(),
            subject: c.subject.trim().to_string(),
            text,
            html,
            inline,
            attachments: self.load_attachments(user_id, &c.attachments, images_size).await?,
            read_receipt_to: c.request_mdn.then(|| ident.sender()),
            ..Default::default()
        };
        uploads.extend(c.attachments.iter().filter_map(|r| match r {
            AttachmentRef::Upload { id } => Some(id.clone()),
            _ => None,
        }));
        if let Some(parent_id) = &c.in_reply_to {
            let (_, parent) = self.message_row(user_id, parent_id).await?;
            if let Some(mid) = &parent.message_id {
                spec.in_reply_to = Some(format!("<{mid}>"));
                let mut refs: Vec<String> = parent.refs.split_whitespace().map(|r| format!("<{r}>")).collect();
                refs.push(format!("<{mid}>"));
                spec.references = refs;
            }
        }
        // A draft may have no recipients yet; lettre insists on one, so the sender stands in
        // and the header is removed again.
        let placeholder = draft && spec.to.is_empty() && spec.cc.is_empty() && spec.bcc.is_empty();
        if placeholder {
            spec.to = vec![ident.email.clone()];
        }
        if !draft && to.is_empty() && spec.cc.is_empty() && bcc.is_empty() {
            return Err(Error::bad("add at least one recipient"));
        }
        let mut msg = mime::build(&spec)?;
        if placeholder {
            msg.headers_mut().remove::<To>();
        }
        let mut copy = msg.clone();
        if !bcc.is_empty() {
            let boxes: Mailboxes = bcc
                .iter()
                .filter_map(|b| b.parse().ok())
                .fold(Mailboxes::new(), |acc: Mailboxes, mb| acc.with(mb));
            copy.headers_mut().set(Bcc::from(boxes));
        }
        let message_id = msg.headers().get_raw("Message-ID").map(str::to_string).unwrap_or_default();
        Ok(Built {
            msg,
            copy: copy.formatted(),
            message_id,
            uploads,
        })
    }

    /// Removes a replaced draft from the server and the cache.
    async fn drop_draft(&self, user_id: &str, draft_id: &str) -> Result<()> {
        let (a, m) = match self.message_row(user_id, draft_id).await {
            Ok(x) => x,
            Err(Error::NotFound) => return Ok(()),
            Err(e) => return Err(e),
        };
        let f = self.folder_row(&m.folder_id).await?;
        let set = uid_set(&[m.uid as u32]);
        let mut conn = self.imap_conn(&a).await?;
        conn.ensure_selected(&f.name).await?;
        conn.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?;
        conn.expunge(&set).await?;
        drop(conn);
        sqlx::query("DELETE FROM mail_messages WHERE id = ?").bind(&m.id).execute(&self.db).await?;
        self.hub.publish(
            [a.user_id.as_str()],
            &json!({"type": "mail.changed", "account_id": a.id, "folder_id": f.id, "what": "messages"}),
        );
        Ok(())
    }

    /// Appends a copy to a role folder and syncs it so the copy is cached; returns the id of
    /// the cached copy (found by Message-ID) when there is one.
    async fn file_copy(&self, a: &Account, role: &str, default_name: &str, raw: &[u8], flags: &str, message_id: &str) -> Result<Option<String>> {
        let folder = self.ensure_role_folder(a, role, default_name).await?;
        {
            let mut conn = self.imap_conn(a).await?;
            conn.append(&folder.name, flags, raw).await?;
        }
        self.sync_mail_folder(a, &folder.id).await?;
        let mid = parse::normalize_id(message_id);
        Ok(
            sqlx::query_scalar("SELECT id FROM mail_messages WHERE folder_id = ? AND message_id = ? ORDER BY uid DESC LIMIT 1")
                .bind(&folder.id)
                .bind(mid)
                .fetch_optional(&self.db)
                .await?,
        )
    }

    /// Saves a draft in the Drafts folder (replacing `draft_id`); returns the new draft's id.
    pub async fn mail_save_draft(&self, user_id: &str, account_id: &str, c: &Compose) -> Result<String> {
        let a = self.mail_account(user_id, account_id).await?;
        let built = self.build_outgoing(user_id, &a, c, true).await?;
        let id = self
            .file_copy(&a, "drafts", "Drafts", &built.copy, "(\\Draft \\Seen)", &built.message_id)
            .await?
            .ok_or_else(|| Error::Unavailable("the draft was saved but could not be found in Drafts".into()))?;
        if let Some(old) = &c.draft_id {
            if old != &id {
                self.drop_draft(user_id, old).await?;
            }
        }
        Ok(id)
    }

    /// Sends a message at once (API clients and the assistant; the web composer queues it in
    /// the outbox). Returns the Message-ID.
    pub async fn mail_send(&self, user_id: &str, account_id: &str, c: &Compose) -> Result<String> {
        self.send_composed(user_id, account_id, c).await
    }

    /// Sends a message through the account's SMTP server, files a copy in Sent (unless the
    /// account says the provider does), marks the replied-to message answered and removes the
    /// draft and uploads it came from. Returns the Message-ID.
    pub(crate) async fn send_composed(&self, user_id: &str, account_id: &str, c: &Compose) -> Result<String> {
        let a = self.mail_account(user_id, account_id).await?;
        let built = self.build_outgoing(user_id, &a, c, false).await?;
        let transport = mailer::transport(&self.smtp_settings(&a)?)?;
        mailer::send(&transport, built.msg).await?;
        let message_id = built.message_id;
        // The mail is out: everything below is bookkeeping and must not report failure.
        if a.save_sent {
            if let Err(e) = self.file_copy(&a, "sent", "Sent", &built.copy, "(\\Seen)", &message_id).await {
                tracing::warn!(account = %a.id, error = %e, "saving to Sent failed");
            }
        } else if let Some(sent) = self.folder_by_role(&a.id, "sent").await? {
            self.refresh_folder_soon(&a, &sent.id);
        }
        if let Some(parent) = &c.in_reply_to {
            if let Ok((pa, p)) = self.message_row(user_id, parent).await {
                if let Ok(f) = self.folder_row(&p.folder_id).await {
                    let r = async {
                        let mut conn = self.imap_conn(&pa).await?;
                        conn.ensure_selected(&f.name).await?;
                        conn.uid_store(&p.uid.to_string(), "+FLAGS.SILENT (\\Answered)").await
                    }
                    .await;
                    if r.is_ok() {
                        let _ = sqlx::query("UPDATE mail_messages SET answered = 1 WHERE id = ?")
                            .bind(&p.id)
                            .execute(&self.db)
                            .await;
                    }
                }
            }
        }
        if let Some(d) = &c.draft_id {
            if let Err(e) = self.drop_draft(user_id, d).await {
                tracing::warn!(error = %e, "removing the sent draft failed");
            }
        }
        if !built.uploads.is_empty() {
            let _ = sqlx::query("DELETE FROM mail_uploads WHERE user_id = ? AND id IN (SELECT value FROM json_each(?))")
                .bind(user_id)
                .bind(serde_json::to_string(&built.uploads).unwrap_or_default())
                .execute(&self.db)
                .await;
        }
        self.hub
            .publish([user_id], &json!({"type": "mail.changed", "account_id": a.id, "what": "sent"}));
        Ok(message_id)
    }

    /// The folder in Files where large attachments are kept.
    pub async fn mail_link_folder(&self, user_id: &str) -> Result<Node> {
        self.mkdir_p(user_id, LINK_FOLDER).await
    }

    /// A free file name in a folder ("name (2).ext" when "name.ext" is taken).
    async fn free_name(&self, user_id: &str, folder_id: &str, name: &str) -> Result<String> {
        let name = name.trim().replace(['/', '\\', '\0'], "_");
        let (stem, ext) = match name.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
            _ => (name.clone(), String::new()),
        };
        for n in 1..=200 {
            let candidate = if n == 1 { name.clone() } else { format!("{stem} ({n}){ext}") };
            if self.child_by_name(user_id, folder_id, &candidate).await?.is_none() {
                return Ok(candidate);
            }
        }
        Err(Error::conflict(format!("too many files named like {name}")))
    }

    /// Stores a large attachment in Files (`Mail attachments`, under a free name) and shares
    /// it as a public link.
    pub async fn mail_store_large_attachment<R: AsyncRead + Unpin>(
        &self,
        user_id: &str,
        name: &str,
        reader: R,
        expire_days: Option<i64>,
    ) -> Result<SharedLink> {
        let folder = self.mail_link_folder(user_id).await?;
        let free = self.free_name(user_id, &folder.id, name).await?;
        let opts = WriteOpts {
            create_only: true,
            ..WriteOpts::user()
        };
        let node = self.write_file(user_id, &folder.id, &free, reader, opts).await?;
        self.mail_share_link(user_id, &node.id, expire_days).await
    }

    /// Shares a file of the user as a read-only public link that expires.
    pub async fn mail_share_link(&self, user_id: &str, file_id: &str, expire_days: Option<i64>) -> Result<SharedLink> {
        let (node, _) = self.node_for(user_id, file_id, perm::READ).await?;
        if node.is_dir() {
            return Err(Error::bad("only files can be shared as attachment links"));
        }
        let days = expire_days.unwrap_or(LINK_DAYS).clamp(1, 365);
        let expires = now() + chrono::Duration::days(days);
        let share = self
            .create_share(
                user_id,
                &node.id,
                NewShare {
                    kind: "link".into(),
                    target: None,
                    permissions: Some(perm::READ),
                    password: None,
                    expires_at: Some(expires),
                    label: Some("Mail attachment".into()),
                },
            )
            .await?;
        Ok(SharedLink {
            file_id: node.id.clone(),
            name: node.name.clone(),
            size: node.size,
            url: share.url.unwrap_or_default(),
            expires_at: share.share.expires_at,
        })
    }

    /// Moves a composer upload into Files and shares it as a link (an attachment over the
    /// size limit).
    pub async fn mail_upload_to_link(&self, user_id: &str, upload_id: &str, expire_days: Option<i64>) -> Result<SharedLink> {
        let (name, _, data) = self.mail_upload_content(user_id, upload_id).await?;
        let link = self.mail_store_large_attachment(user_id, &name, data.as_slice(), expire_days).await?;
        self.mail_discard_upload(user_id, upload_id).await?;
        Ok(link)
    }
}

/// "Subject.eml" for a message attached whole.
pub fn eml_name(subject: &str) -> String {
    let base: String = subject.chars().filter(|c| !"/\\\0\r\n".contains(*c)).take(80).collect();
    if base.trim().is_empty() {
        "message.eml".into()
    } else {
        format!("{}.eml", base.trim())
    }
}

fn human_size(n: i64) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_go_above_or_below_the_quote() {
        let quote = "\n\nOn Mon, Ada wrote:\n> hi\n";
        assert_eq!(text_with_signature(quote, "Alice A.", true), "\n\n-- \nAlice A.\n\nOn Mon, Ada wrote:\n> hi\n");
        assert_eq!(text_with_signature(quote, "Alice A.", false), "\n\nOn Mon, Ada wrote:\n> hi\n\n-- \nAlice A.\n");
        assert_eq!(text_with_signature("", "Alice", false), "\n\n-- \nAlice");
        assert_eq!(text_with_signature(quote, " ", true), quote);
        assert_eq!(html_with_signature("<q>", "<s>", true), "<div><br></div><s><q>");
        assert_eq!(html_with_signature("<q>", "<s>", false), "<div><br></div><q><s>");
    }

    #[test]
    fn image_sources_are_recognized() {
        assert!(matches!(image_source("/api/mail/uploads/u1/content"), Some(ImageSource::Upload(id)) if id == "u1"));
        assert!(matches!(image_source("/api/mail/outbox/o1/files/f1"), Some(ImageSource::OutboxFile(id)) if id == "f1"));
        assert!(matches!(image_source("/api/mail/messages/m1/cid/logo%40x%3D"), Some(ImageSource::MessagePart(m, c)) if m == "m1" && c == "logo@x="));
        assert!(matches!(image_source("data:image/png;base64,iVBORw0KGgo="), Some(ImageSource::Data(m, d)) if m == "image/png" && d.starts_with(b"\x89PNG")));
        assert!(image_source("data:image/png,raw").is_none());
        assert!(image_source("https://example.org/x.png").is_none());
        assert_eq!(decode_percent(&encode_cid("a b@c/d")), "a b@c/d");
        assert_eq!(eml_name("Re: plans/now"), "Re: plansnow.eml");
        assert_eq!(eml_name(""), "message.eml");
    }
}
