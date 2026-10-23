//! Writing mail: reply/forward templates, composer uploads, recipient suggestions, drafts
//! (IMAP APPEND to Drafts) and sending (the account's SMTP through `mailer`, then APPEND to
//! Sent unless the provider files sent mail itself).

use super::Account;
use super::imap::uid_set;
use super::parse::{self, Addr};
use crate::mailer::{self, Attachment, OutgoingMail};
use crate::util::{new_id, now};
use crate::{Core, Error, Result};
use lettre::message::Mailboxes;
use lettre::message::header::{Bcc, To};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Attachments per message, in total.
pub const MAX_ATTACHMENTS_BYTES: usize = 25 * 1024 * 1024;
/// Composer uploads kept per user (until sent, saved or a day old).
const MAX_UPLOADS_BYTES: i64 = 100 * 1024 * 1024;

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
}

/// A message being written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Compose {
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub text: String,
    /// The cached message this replies to (threading headers, \Answered after sending).
    #[serde(default)]
    pub in_reply_to: Option<String>,
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// A draft this replaces (deleted once the new version is saved or the mail is sent).
    #[serde(default)]
    pub draft_id: Option<String>,
    /// S/MIME signing/encryption or PGP/MIME (Mailvelope) for this message; applied when
    /// sending (drafts are stored as written). See `security::Core::mail_secure_outgoing`.
    #[serde(default)]
    pub security: super::security::OutgoingSecurity,
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
    pub address: String,
    /// contact, user or recent
    pub source: &'static str,
}

fn same_address(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn with_signature(text: String, signature: &str) -> String {
    if signature.trim().is_empty() {
        text
    } else {
        format!("\n\n-- \n{}{}", signature.trim_end(), text)
    }
}

fn bracketed(id: &str) -> String {
    format!("<{id}>")
}

fn addresses(list: &[String], what: &str) -> Result<Vec<String>> {
    list.iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            parse::Addr::parse(s)
                .map(|a| a.display())
                .ok_or_else(|| Error::bad(format!("{what}: {s:?} is not an email address")))
        })
        .collect()
}

impl Core {
    /// A prefilled reply, reply-all or forward of a message (quoting it), or a draft to edit.
    pub async fn mail_compose_template(&self, user_id: &str, id: &str, mode: ReplyMode) -> Result<ComposeTemplate> {
        let view = self.mail_message(user_id, id).await?;
        let a = self.mail_account(user_id, &view.summary.account_id).await?;
        let m = &view.summary;
        let from = m.from();
        let from_label = from.display();
        let mut compose = Compose::default();
        let mut attachment_names = Vec::new();
        match mode {
            ReplyMode::Reply | ReplyMode::ReplyAll => {
                // Replying to one's own sent message goes to its recipients again.
                let own = same_address(&from.address, &a.email);
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
                            || primary.iter().any(|p| same_address(&p.address, &x.address))
                            || cc.iter().any(|c| same_address(&c.address, &x.address));
                        if !dup {
                            cc.push(x.clone());
                        }
                    }
                    compose.cc = cc.iter().map(Addr::display).collect();
                }
                compose.subject = parse::prefixed_subject("Re: ", &m.subject);
                compose.text = with_signature(parse::quote_reply(&view.text, &from_label, Some(m.date)), &a.signature);
                compose.in_reply_to = Some(m.id.clone());
            }
            ReplyMode::Forward => {
                compose.subject = parse::prefixed_subject("Fwd: ", &m.subject);
                let to = m.to_addrs.iter().map(Addr::display).collect::<Vec<_>>().join(", ");
                compose.text = with_signature(parse::quote_forward(&view.text, &from_label, Some(m.date), &m.subject, &to), &a.signature);
                for att in view.attachments.iter().filter(|x| !x.inline) {
                    compose.attachments.push(AttachmentRef::Message {
                        message_id: m.id.clone(),
                        index: att.index,
                    });
                    attachment_names.push((att.name.clone(), att.size));
                }
            }
            ReplyMode::Edit => {
                compose.to = m.to_addrs.iter().map(Addr::display).collect();
                compose.cc = m.cc_addrs.iter().map(Addr::display).collect();
                compose.bcc = view.bcc.iter().map(Addr::display).collect();
                compose.subject = m.subject.clone();
                compose.text = view.text.clone();
                compose.draft_id = Some(m.id.clone());
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
            return Err(Error::bad(format!("attachments can be at most {} MB", MAX_ATTACHMENTS_BYTES / (1024 * 1024))));
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

    pub async fn mail_discard_upload(&self, user_id: &str, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM mail_uploads WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Address suggestions for `q`: contacts, other users, then people the user has mail from.
    pub async fn mail_recipients(&self, user_id: &str, q: &str) -> Result<Vec<Recipient>> {
        let q = q.trim();
        let mut out: Vec<Recipient> = Vec::new();
        let push = |out: &mut Vec<Recipient>, name: &str, address: &str, source: &'static str| {
            if address.contains('@') && !out.iter().any(|r| same_address(&r.address, address)) {
                out.push(Recipient {
                    name: name.to_string(),
                    address: address.to_string(),
                    source,
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

    async fn load_attachments(&self, user_id: &str, refs: &[AttachmentRef]) -> Result<Vec<Attachment>> {
        let mut out = Vec::new();
        let mut total = 0usize;
        for r in refs {
            let (filename, content_type, data) = match r {
                AttachmentRef::Upload { id } => {
                    let row: Option<(String, String, Vec<u8>)> = sqlx::query_as("SELECT name, mime, data FROM mail_uploads WHERE id = ? AND user_id = ?")
                        .bind(id)
                        .bind(user_id)
                        .fetch_optional(&self.db)
                        .await?;
                    row.ok_or_else(|| Error::bad("an uploaded attachment expired; attach it again"))?
                }
                AttachmentRef::File { id } => {
                    let (node, data) = self.read_file(user_id, id).await?;
                    (node.name, node.mime, data)
                }
                AttachmentRef::Message { message_id, index } => self.mail_attachment(user_id, message_id, *index).await?,
            };
            total += data.len();
            if total > MAX_ATTACHMENTS_BYTES {
                return Err(Error::bad(format!(
                    "attachments can be at most {} MB in total",
                    MAX_ATTACHMENTS_BYTES / (1024 * 1024)
                )));
            }
            out.push(Attachment { filename, content_type, data });
        }
        Ok(out)
    }

    /// Builds the message. Returns it with the raw bytes for the Sent/Drafts copy (which keep
    /// the Bcc header that SMTP delivery leaves out) and its Message-ID.
    async fn build_outgoing(&self, user_id: &str, a: &Account, c: &Compose, draft: bool) -> Result<(lettre::Message, Vec<u8>, String)> {
        let to = addresses(&c.to, "To")?;
        let cc = addresses(&c.cc, "Cc")?;
        let bcc = addresses(&c.bcc, "Bcc")?;
        let mut m = OutgoingMail {
            from: Some(a.sender()),
            to: to.clone(),
            cc,
            bcc: bcc.clone(),
            subject: c.subject.trim().to_string(),
            text: c.text.clone(),
            attachments: self.load_attachments(user_id, &c.attachments).await?,
            ..Default::default()
        };
        if let Some(parent_id) = &c.in_reply_to {
            let (_, parent) = self.message_row(user_id, parent_id).await?;
            if let Some(mid) = &parent.message_id {
                m.in_reply_to = Some(bracketed(mid));
                let mut refs: Vec<String> = parent.refs.split_whitespace().map(bracketed).collect();
                refs.push(bracketed(mid));
                m.references = refs;
            }
        }
        // A draft may have no recipients yet; lettre insists on one, so the sender stands in
        // and the header is removed again.
        let placeholder = draft && m.to.is_empty() && m.cc.is_empty() && m.bcc.is_empty();
        if placeholder {
            m.to = vec![a.email.clone()];
        }
        if !draft && to.is_empty() && m.cc.is_empty() && bcc.is_empty() {
            return Err(Error::bad("add at least one recipient"));
        }
        let mut msg = mailer::build_message(&m, &a.sender())?;
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
        Ok((msg, copy.formatted(), message_id))
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
        let (_, raw, message_id) = self.build_outgoing(user_id, &a, c, true).await?;
        let id = self
            .file_copy(&a, "drafts", "Drafts", &raw, "(\\Draft \\Seen)", &message_id)
            .await?
            .ok_or_else(|| Error::Unavailable("the draft was saved but could not be found in Drafts".into()))?;
        if let Some(old) = &c.draft_id {
            if old != &id {
                self.drop_draft(user_id, old).await?;
            }
        }
        Ok(id)
    }

    /// Sends a message through the account's SMTP server, files a copy in Sent (unless the
    /// account says the provider does), marks the replied-to message answered and removes the
    /// draft it came from. Returns the Message-ID.
    pub async fn mail_send(&self, user_id: &str, account_id: &str, c: &Compose) -> Result<String> {
        let a = self.mail_account(user_id, account_id).await?;
        let (msg, raw, message_id) = self.build_outgoing(user_id, &a, c, false).await?;
        let transport = mailer::transport(&self.smtp_settings(&a)?)?;
        // Security hook: signing/encrypting happens on the built message, before anything is
        // sent; a missing certificate stops the send with a clear error.
        let raw = if c.security.any() {
            let recipients: Vec<String> = c.to.iter().chain(&c.cc).chain(&c.bcc).cloned().collect();
            let (wire, copy) = self.mail_secure_outgoing(user_id, &a.id, &c.security, &recipients, &msg.formatted(), &raw).await?;
            mailer::send_raw(&transport, msg.envelope(), &wire).await?;
            copy
        } else {
            mailer::send(&transport, msg).await?;
            raw
        };
        // The mail is out: everything below is bookkeeping and must not report failure.
        if a.save_sent {
            if let Err(e) = self.file_copy(&a, "sent", "Sent", &raw, "(\\Seen)", &message_id).await {
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
        let uploads: Vec<&String> = c
            .attachments
            .iter()
            .filter_map(|r| match r {
                AttachmentRef::Upload { id } => Some(id),
                _ => None,
            })
            .collect();
        if !uploads.is_empty() {
            let _ = sqlx::query("DELETE FROM mail_uploads WHERE user_id = ? AND id IN (SELECT value FROM json_each(?))")
                .bind(user_id)
                .bind(serde_json::to_string(&uploads).unwrap_or_default())
                .execute(&self.db)
                .await;
        }
        self.hub
            .publish([user_id], &json!({"type": "mail.changed", "account_id": a.id, "what": "sent"}));
        Ok(message_id)
    }
}
