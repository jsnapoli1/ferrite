//! "Report spam" and "Not spam": the message is forwarded as an attachment (message/rfc822)
//! to the address the administrator configured for that verdict (a spam filter's training
//! mailbox), through the account's own SMTP server, then moved to Junk or back to the Inbox
//! and tagged `$Junk` / `$NotJunk` on the server. Without a configured address the message is
//! only moved and tagged.

use super::delegation::MailRight;
use super::imap::uid_set;
use crate::{Core, Error, Result};
use lettre::message::header::{ContentDisposition, ContentTransferEncoding, ContentType};
use lettre::message::{Body, Mailbox, MultiPart, SinglePart};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Spam,
    Ham,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReportOutcome {
    /// Messages forwarded to the report address.
    pub reported: usize,
    /// Messages moved to Junk (spam) or the Inbox (not spam).
    pub moved: usize,
}

/// The report: a short note and the original as an RFC 822 attachment (8bit where it can be,
/// as RFC 2046 asks for message/rfc822 bodies).
pub fn report_message(from: &str, to: &str, verdict: Verdict, subject: &str, raw: Vec<u8>) -> Result<lettre::Message> {
    let parse = |s: &str| s.parse::<Mailbox>().map_err(|e| Error::bad(format!("invalid address {s:?}: {e}")));
    let (label, note) = match verdict {
        Verdict::Spam => ("Spam report", "This message was reported as spam."),
        Verdict::Ham => ("Not spam", "This message was reported as not spam (a false positive)."),
    };
    let body = match String::from_utf8(raw) {
        Ok(text) => Body::new_with_encoding(text, ContentTransferEncoding::EightBit).unwrap_or_else(Body::new),
        Err(e) => Body::new(e.into_bytes()),
    };
    let attached = SinglePart::builder()
        .header(ContentType::parse("message/rfc822").expect("valid content type"))
        .header(ContentDisposition::attachment("message.eml"))
        .body(body);
    lettre::Message::builder()
        .from(parse(from)?)
        .to(parse(to)?)
        .subject(format!("{label}: {subject}"))
        .message_id(None)
        .multipart(MultiPart::mixed().singlepart(SinglePart::plain(note.to_string())).singlepart(attached))
        .map_err(|e| Error::bad(format!("building the report: {e}")))
}

impl Core {
    /// Reports messages as spam or not spam (see the module documentation).
    pub async fn mail_report_spam(&self, user_id: &str, ids: &[String], verdict: Verdict) -> Result<ReportOutcome> {
        let settings = self.mail_settings().await;
        let address = match verdict {
            Verdict::Spam => settings.spam_report_address.clone(),
            Verdict::Ham => settings.ham_report_address.clone(),
        };
        let mut out = ReportOutcome::default();
        for (a, f, msgs) in self.group_messages(user_id, ids).await? {
            // Reporting sends mail from the account.
            self.mail_access(user_id, &a.id, MailRight::Send).await?;
            if !address.is_empty() {
                let transport = self.mail_smtp_transport(&a).await?;
                for m in &msgs {
                    let (_, raw) = self.mail_raw(user_id, &m.id).await?;
                    let report = report_message(&a.sender(), &address, verdict, &m.subject, raw)?;
                    crate::mailer::send(&transport, report).await?;
                    out.reported += 1;
                }
                tracing::info!(account = %a.id, count = msgs.len(), verdict = ?verdict, "mail reported");
            }
            let uids: Vec<u32> = msgs.iter().map(|m| m.uid as u32).collect();
            let (add, remove) = match verdict {
                Verdict::Spam => ("$Junk", "$NotJunk"),
                Verdict::Ham => ("$NotJunk", "$Junk"),
            };
            {
                // Keywords are a hint for the server's filter; servers that refuse custom
                // keywords still get the move.
                let mut conn = self.imap_conn(&a).await?;
                conn.ensure_selected(&f.name).await?;
                let set = uid_set(&uids);
                if let Err(e) = conn.uid_store(&set, &format!("+FLAGS.SILENT ({add})")).await {
                    tracing::debug!(error = %e, "setting {add} failed");
                }
                let _ = conn.uid_store(&set, &format!("-FLAGS.SILENT ({remove})")).await;
            }
            let dest = match verdict {
                Verdict::Spam => self.ensure_role_folder(&a, "junk", "Junk").await?,
                Verdict::Ham => self
                    .folder_by_role(&a.id, "inbox")
                    .await?
                    .ok_or_else(|| Error::Unavailable("the account has no INBOX".into()))?,
            };
            // The account may keep reported mail where it is (inbox settings).
            if dest.id != f.id && self.account_prefs_row(&a.id).await?.move_junk {
                self.move_group(&a, &f, &msgs, &dest).await?;
                out.moved += msgs.len();
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_attach_the_original() {
        let raw = b"From: x@spam.example\r\nSubject: Win\r\n\r\nYou won \xc3\xa9!\r\n".to_vec();
        let m = report_message("Ada <ada@example.org>", "spam@example.org", Verdict::Spam, "Win", raw).unwrap();
        let s = String::from_utf8(m.formatted()).unwrap();
        assert!(s.contains("Subject: Spam report: Win"));
        assert!(s.contains("Content-Type: message/rfc822"));
        assert!(s.contains("Content-Transfer-Encoding: 8bit"));
        assert!(s.contains("filename=\"message.eml\""));
        assert!(s.contains("From: x@spam.example"), "the original is attached unencoded");
        let ham = report_message("ada@example.org", "ham@example.org", Verdict::Ham, "Hello", b"\xff\xfe binary".to_vec()).unwrap();
        let s = String::from_utf8(ham.formatted()).unwrap();
        assert!(s.contains("Subject: Not spam: Hello"));
        assert!(s.contains("Content-Transfer-Encoding: base64"), "non-UTF-8 originals fall back to base64");
        assert!(report_message("ada@example.org", "not an address", Verdict::Spam, "x", vec![]).is_err());
    }
}
