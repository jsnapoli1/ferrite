//! Composing-related parts of reading: read receipts (a sender asked for one with
//! `Disposition-Notification-To`; the reader decides, the receipt is an RFC 8098 report and
//! the decision is remembered with the `$MDNSent` keyword, RFC 3503), attached messages
//! (`message/rfc822`, shown nested in the reader with their own body and attachments) and
//! the print view of a message.

use super::Account;
use super::mime::{self, MdnSpec};
use super::parse::{self, Addr, AttachmentInfo};
use super::store::MessageSummary;
use crate::mailer;
use crate::{Core, Error, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

/// IMAP keyword set once a receipt was sent or declined.
pub const MDN_KEYWORD: &str = "$MDNSent";

#[derive(Debug, Clone, Serialize)]
pub struct MdnStatus {
    /// The sender asked for a read receipt (and this is not one's own message).
    pub requested: bool,
    /// Where the receipt would go.
    pub to: Option<Addr>,
    /// A receipt was sent or declined already.
    pub handled: bool,
}

/// An attached message, parsed.
#[derive(Debug, Clone, Serialize)]
pub struct NestedMessage {
    pub name: String,
    pub subject: String,
    pub from: Option<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub date: Option<DateTime<Utc>>,
    pub text: String,
    pub has_html: bool,
    pub attachments: Vec<AttachmentInfo>,
}

/// The header block of a raw message.
fn header_block(raw: &[u8]) -> &[u8] {
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 2)
        .or_else(|| raw.windows(2).position(|w| w == b"\n\n").map(|p| p + 1))
        .unwrap_or(raw.len());
    &raw[..end]
}

fn receipt_address(header: &[u8]) -> Option<Addr> {
    let m = mail_parser::MessageParser::default().parse_headers(header)?;
    let v = m.header_raw("Disposition-Notification-To")?;
    // The first address of the header (it may carry a display name).
    let first = v.split(',').next()?.trim().replace(['\r', '\n'], "");
    Addr::parse(first.trim())
}

fn escape(s: &str) -> String {
    ammonia::clean_text(s)
}

impl Core {
    /// The header of a cached message: from the cached raw message, else fetched.
    async fn message_header(&self, a: &Account, m: &MessageSummary) -> Result<Vec<u8>> {
        let cached: Option<Option<Vec<u8>>> = sqlx::query_scalar("SELECT raw FROM mail_bodies WHERE message_id = ?")
            .bind(&m.id)
            .fetch_optional(&self.db)
            .await?;
        if let Some(Some(raw)) = cached {
            return Ok(header_block(&raw).to_vec());
        }
        let f = self.folder_row(&m.folder_id).await?;
        let mut conn = self.imap_conn(a).await?;
        conn.ensure_selected(&f.name).await?;
        let fetched = conn.uid_fetch(&m.uid.to_string(), "(UID BODY.PEEK[HEADER])").await?;
        fetched.into_iter().find(|x| x.uid as i64 == m.uid).map(|x| x.header).ok_or(Error::NotFound)
    }

    async fn has_keyword(&self, a: &Account, m: &MessageSummary, keyword: &str) -> Result<bool> {
        let f = self.folder_row(&m.folder_id).await?;
        let mut conn = self.imap_conn(a).await?;
        conn.ensure_selected(&f.name).await?;
        Ok(!conn.uid_search(&format!("UID {} KEYWORD {keyword}", m.uid)).await?.is_empty())
    }

    /// Whether a message asks for a read receipt, and whether that was dealt with.
    pub async fn mail_mdn_status(&self, user_id: &str, id: &str) -> Result<MdnStatus> {
        let (a, m) = self.message_row(user_id, id).await?;
        let none = MdnStatus {
            requested: false,
            to: None,
            handled: false,
        };
        let role = self.folder_row(&m.folder_id).await?.role;
        if matches!(role.as_deref(), Some("sent") | Some("drafts") | Some("junk")) || m.draft {
            return Ok(none);
        }
        let Some(to) = receipt_address(&self.message_header(&a, &m).await?) else {
            return Ok(none);
        };
        // One's own messages (sent to oneself) need no receipt.
        let own: Vec<String> = sqlx::query_scalar("SELECT email FROM mail_aliases WHERE account_id = ?")
            .bind(&a.id)
            .fetch_all(&self.db)
            .await?;
        if to.address.eq_ignore_ascii_case(&a.email) || own.iter().any(|e| e.eq_ignore_ascii_case(&to.address)) {
            return Ok(none);
        }
        let handled = self.has_keyword(&a, &m, MDN_KEYWORD).await?;
        Ok(MdnStatus {
            requested: true,
            to: Some(to),
            handled,
        })
    }

    /// Sends the read receipt a message asked for (`send`), or declines it; either way the
    /// message gets `$MDNSent` so nobody is asked again.
    pub async fn mail_mdn_respond(&self, user_id: &str, id: &str, send: bool) -> Result<MdnStatus> {
        let status = self.mail_mdn_status(user_id, id).await?;
        let (a, m) = self.message_row(user_id, id).await?;
        let Some(to) = status.to.clone().filter(|_| status.requested) else {
            return Err(Error::bad("this message did not ask for a read receipt"));
        };
        if status.handled {
            return Ok(status);
        }
        if send {
            let addrs: Vec<&str> = m.to_addrs.iter().chain(&m.cc_addrs).map(|x| x.address.as_str()).collect();
            let alias = self.alias_for_addresses(&a, &addrs).await?;
            let ident = self.mail_identity(&a, alias.as_deref()).await?;
            let host = url_host(&self.config.public_url);
            let msg = mime::build_mdn(&MdnSpec {
                from: ident.sender(),
                to: to.display(),
                final_recipient: ident.email.clone(),
                original_subject: m.subject.clone(),
                original_message_id: m.message_id.clone(),
                original_date: Some(m.date),
                original_headers: Some(self.message_header(&a, &m).await?),
                host,
            })?;
            let transport = mailer::transport(&self.smtp_settings(&a)?)?;
            mailer::send(&transport, msg).await?;
        }
        let f = self.folder_row(&m.folder_id).await?;
        {
            let mut conn = self.imap_conn(&a).await?;
            conn.ensure_selected(&f.name).await?;
            conn.uid_store(&m.uid.to_string(), &format!("+FLAGS.SILENT ({MDN_KEYWORD})")).await?;
        }
        self.hub.publish(
            [user_id],
            &json!({"type": "mail.changed", "account_id": a.id, "folder_id": f.id, "what": "messages"}),
        );
        Ok(MdnStatus { handled: true, ..status })
    }

    /// The raw bytes of an attached message.
    async fn nested_raw(&self, user_id: &str, id: &str, index: usize) -> Result<(String, Vec<u8>)> {
        let (name, mime, data) = self.mail_attachment(user_id, id, index).await?;
        if mime != "message/rfc822" && !name.to_ascii_lowercase().ends_with(".eml") {
            return Err(Error::bad("that attachment is not an email message"));
        }
        Ok((name, data))
    }

    /// An attached message (`message/rfc822` or an `.eml` file) as the reader shows it.
    pub async fn mail_nested_message(&self, user_id: &str, id: &str, index: usize) -> Result<NestedMessage> {
        let (name, raw) = self.nested_raw(user_id, id, index).await?;
        let p = parse::parse_body(&raw);
        Ok(NestedMessage {
            name,
            subject: p.envelope.subject,
            from: p.envelope.from,
            to: p.envelope.to,
            cc: p.envelope.cc,
            date: p.envelope.date,
            text: p.text,
            has_html: p.html.is_some(),
            attachments: p.attachments,
        })
    }

    /// The sanitized HTML document of an attached message (for a sandboxed frame).
    pub async fn mail_nested_html(&self, user_id: &str, id: &str, index: usize, allow_remote: bool) -> Result<String> {
        let (_, raw) = self.nested_raw(user_id, id, index).await?;
        let p = parse::parse_body(&raw);
        let html = p.html.unwrap_or_else(|| format!("<pre>{}</pre>", escape(&p.text)));
        let base = format!("/api/mail/messages/{id}/attachments/{index}/cid/");
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
            format!("{base}{enc}")
        });
        Ok(super::store::html_document(&s))
    }

    /// An attachment of an attached message.
    pub async fn mail_nested_part(&self, user_id: &str, id: &str, index: usize, part: usize) -> Result<(String, String, Vec<u8>)> {
        let (_, raw) = self.nested_raw(user_id, id, index).await?;
        parse::attachment(&raw, part).ok_or(Error::NotFound)
    }

    /// An inline image of an attached message.
    pub async fn mail_nested_cid(&self, user_id: &str, id: &str, index: usize, cid: &str) -> Result<(String, String, Vec<u8>)> {
        let (_, raw) = self.nested_raw(user_id, id, index).await?;
        parse::attachment_by_cid(&raw, cid).ok_or(Error::NotFound)
    }

    /// A message laid out for printing: its headers, the body (sanitized HTML, remote images
    /// only when asked) and the list of attachments.
    pub async fn mail_print_document(&self, user_id: &str, id: &str, allow_remote: bool) -> Result<String> {
        let view = self.mail_message(user_id, id).await?;
        let m = &view.summary;
        let body = if view.has_html {
            let doc = self.mail_message_html(user_id, id, allow_remote).await?;
            // The message's own document is embedded in the printed page.
            let start = doc.find("<body>").map(|p| p + 6).unwrap_or(0);
            let end = doc.rfind("</body>").unwrap_or(doc.len());
            let head_css: String = doc
                .match_indices("<style>")
                .filter_map(|(p, _)| doc[p + 7..].find("</style>").map(|e| doc[p + 7..p + 7 + e].to_string()))
                .collect::<Vec<_>>()
                .join("\n");
            format!("<style>{head_css}</style><div class=\"body\">{}</div>", &doc[start..end])
        } else {
            format!("<pre class=\"body\">{}</pre>", escape(&view.text))
        };
        let list = |v: &[Addr]| escape(&v.iter().map(Addr::display).collect::<Vec<_>>().join(", "));
        let mut rows = format!(
            "<tr><th>From</th><td>{}</td></tr><tr><th>Date</th><td>{}</td></tr>",
            escape(&m.from().display()),
            m.date.format("%a, %-d %b %Y %H:%M UTC")
        );
        if !m.to_addrs.is_empty() {
            rows.push_str(&format!("<tr><th>To</th><td>{}</td></tr>", list(&m.to_addrs)));
        }
        if !m.cc_addrs.is_empty() {
            rows.push_str(&format!("<tr><th>Cc</th><td>{}</td></tr>", list(&m.cc_addrs)));
        }
        let files: Vec<String> = view
            .attachments
            .iter()
            .filter(|x| !x.inline)
            .map(|x| format!("<li>{} <span>({})</span></li>", escape(&x.name), human_size(x.size)))
            .collect();
        let attachments = if files.is_empty() {
            String::new()
        } else {
            format!("<section class=\"files\"><h2>Attachments</h2><ul>{}</ul></section>", files.join(""))
        };
        const CSS: &str = "html{color-scheme:light}body{margin:24px;font:13px/1.5 ui-sans-serif,system-ui,-apple-system,\"Segoe UI\",Roboto,Arial,sans-serif;color:#111;background:#fff}\
            h1{font-size:20px;margin:0 0 10px}table.head{border-collapse:collapse;margin-bottom:14px;width:100%}table.head th{text-align:left;color:#555;font-weight:600;padding:2px 12px 2px 0;width:1%;white-space:nowrap;vertical-align:top}\
            table.head td{padding:2px 0;overflow-wrap:anywhere}hr{border:0;border-top:1px solid #bbb;margin:12px 0}.body{overflow-wrap:anywhere}pre.body{white-space:pre-wrap;font:inherit}\
            img{max-width:100%;height:auto}.files{margin-top:18px;border-top:1px solid #bbb;padding-top:8px}.files h2{font-size:14px;margin:0 0 4px}.files span{color:#555}\
            @page{margin:16mm}@media print{body{margin:0}}";
        Ok(format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"referrer\" content=\"no-referrer\"><title>{}</title><style>{CSS}</style></head>\
             <body><h1>{}</h1><table class=\"head\">{rows}</table><hr>{body}{attachments}</body></html>",
            escape(&m.subject),
            escape(if m.subject.is_empty() { "(no subject)" } else { &m.subject })
        ))
    }
}

fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    if host.is_empty() { "localhost".into() } else { host.to_string() }
}

fn human_size(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else if n >= 1024 {
        format!("{:.0} KB", n as f64 / 1024.0)
    } else {
        format!("{n} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_receipt_request() {
        let raw = b"From: Ada <ada@example.org>\r\nDisposition-Notification-To: \"Ada L.\" <ada@example.org>\r\nSubject: x\r\n\r\nbody\r\n";
        let h = header_block(raw);
        assert!(h.ends_with(b"Subject: x\r\n"));
        let a = receipt_address(h).unwrap();
        assert_eq!((a.name.as_str(), a.address.as_str()), ("Ada L.", "ada@example.org"));
        assert!(receipt_address(b"From: a@example.org\r\n\r\n").is_none());
        assert_eq!(url_host("https://cloud.example.org:8443/x"), "cloud.example.org");
        assert_eq!(url_host(""), "localhost");
    }
}
