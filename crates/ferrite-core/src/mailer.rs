//! Outgoing mail over SMTP (lettre): the server's own mail (invitations, security notices)
//! through `[smtp]` in the configuration, and the building blocks webmail accounts reuse with
//! their own SMTP settings.

use crate::{Core, Error, Result};
use lettre::message::header::{ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment as LettreAttachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmtpSecurity {
    /// Plain connection upgraded with STARTTLS (port 587). Required, not opportunistic.
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Tls,
    /// No encryption: only for a relay on localhost or a trusted LAN.
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpSettings {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub security: SmtpSecurity,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default, skip_serializing)]
    pub password: Option<String>,
    /// Sender of the server's own mail, e.g. `Ferrite <noreply@example.org>`.
    pub from: String,
}

fn default_port() -> u16 {
    587
}

#[derive(Debug, Clone, Default)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// An iCalendar part (RFC 6047 iMIP), shown by mail clients as an invitation.
#[derive(Debug, Clone)]
pub struct CalendarPart {
    /// REQUEST, REPLY, CANCEL, …
    pub method: String,
    pub data: String,
}

#[derive(Debug, Clone, Default)]
pub struct OutgoingMail {
    /// Sender; defaults to the transport's configured `from`.
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    pub calendar: Option<CalendarPart>,
    /// Threading headers for replies.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

fn mailbox(s: &str) -> Result<Mailbox> {
    s.trim().parse::<Mailbox>().map_err(|e| Error::bad(format!("invalid address {s:?}: {e}")))
}

/// Builds the MIME message: text (and HTML) alternatives, an optional iMIP calendar part,
/// and attachments.
pub fn build_message(m: &OutgoingMail, default_from: &str) -> Result<Message> {
    if m.to.is_empty() && m.cc.is_empty() && m.bcc.is_empty() {
        return Err(Error::bad("no recipients"));
    }
    let mut b = Message::builder()
        .from(mailbox(m.from.as_deref().unwrap_or(default_from))?)
        .subject(m.subject.clone())
        // A Message-ID (generated from the hostname) lets replies and RSVPs thread.
        .message_id(None);
    for t in &m.to {
        b = b.to(mailbox(t)?);
    }
    for c in &m.cc {
        b = b.cc(mailbox(c)?);
    }
    for c in &m.bcc {
        b = b.bcc(mailbox(c)?);
    }
    if let Some(r) = &m.reply_to {
        b = b.reply_to(mailbox(r)?);
    }
    if let Some(id) = &m.in_reply_to {
        b = b.in_reply_to(id.clone());
    }
    if !m.references.is_empty() {
        b = b.references(m.references.join(" "));
    }
    let text = SinglePart::plain(m.text.clone());
    let mut alternative = MultiPart::alternative().singlepart(text);
    if let Some(html) = &m.html {
        alternative = alternative.singlepart(SinglePart::html(html.clone()));
    }
    if let Some(cal) = &m.calendar {
        let ct = format!("text/calendar; charset=utf-8; method={}", cal.method);
        let ct = ContentType::parse(&ct).map_err(|e| Error::internal(format!("calendar content type: {e}")))?;
        alternative = alternative.singlepart(SinglePart::builder().header(ct).body(cal.data.clone()));
    }
    let mut mixed = MultiPart::mixed().multipart(alternative);
    if let Some(cal) = &m.calendar {
        // Also as a file, for clients that only look at attachments.
        let ct = ContentType::parse("application/ics").expect("valid content type");
        mixed = mixed.singlepart(LettreAttachment::new("invite.ics".into()).body(cal.data.clone(), ct));
    }
    for a in &m.attachments {
        let ct = ContentType::parse(&a.content_type).unwrap_or(ContentType::parse("application/octet-stream").expect("valid content type"));
        mixed = mixed.singlepart(LettreAttachment::new(a.filename.clone()).body(a.data.clone(), ct));
    }
    let mut msg = b.multipart(mixed).map_err(|e| Error::bad(format!("building message: {e}")))?;
    // Mark as automatic only for the server's own mail (no `from` override).
    if m.from.is_none() {
        msg.headers_mut()
            .insert_raw(HeaderValue::new(HeaderName::new_from_ascii_str("Auto-Submitted"), "auto-generated".into()));
    }
    Ok(msg)
}

/// An SMTP transport for `settings` (connections are pooled and reused).
pub fn transport(settings: &SmtpSettings) -> Result<AsyncSmtpTransport<Tokio1Executor>> {
    let host = settings.host.trim();
    let tls = || TlsParameters::new(host.to_string()).map_err(|e| Error::bad(format!("TLS for {host}: {e}")));
    let mut b = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)
        .port(settings.port)
        .timeout(Some(Duration::from_secs(30)));
    b = match settings.security {
        SmtpSecurity::Starttls => b.tls(Tls::Required(tls()?)),
        SmtpSecurity::Tls => b.tls(Tls::Wrapper(tls()?)),
        SmtpSecurity::None => b.tls(Tls::None),
    };
    if let (Some(u), Some(p)) = (&settings.username, &settings.password) {
        b = b.credentials(Credentials::new(u.clone(), p.clone()));
    }
    Ok(b.build())
}

/// Sends `msg`, returning its Message-ID.
pub async fn send(transport: &AsyncSmtpTransport<Tokio1Executor>, msg: Message) -> Result<String> {
    let id = msg.headers().get_raw("Message-ID").map(str::to_string).unwrap_or_default();
    transport.send(msg).await.map_err(|e| Error::Unavailable(format!("sending mail failed: {e}")))?;
    Ok(id)
}

/// Sends prepared bytes (a signed or encrypted message) to the envelope's recipients.
pub async fn send_raw(transport: &AsyncSmtpTransport<Tokio1Executor>, envelope: &lettre::address::Envelope, raw: &[u8]) -> Result<()> {
    transport
        .send_raw(envelope, raw)
        .await
        .map_err(|e| Error::Unavailable(format!("sending mail failed: {e}")))?;
    Ok(())
}

impl Core {
    /// Whether the server can send its own mail (`[smtp]` is configured).
    pub fn mail_configured(&self) -> bool {
        self.config.smtp.is_some()
    }

    /// Sends mail from the server itself (invitations, security notices). Returns the
    /// Message-ID. Fails with `Unavailable` when `[smtp]` is not configured.
    pub async fn send_system_mail(&self, m: &OutgoingMail) -> Result<String> {
        let settings = self
            .config
            .smtp
            .as_ref()
            .ok_or_else(|| Error::Unavailable("outgoing mail is not configured (set [smtp] in ferrite.toml)".into()))?;
        let transport = self.smtp_transport.get_or_try_init(|| async { transport(settings) }).await?;
        send(transport, build_message(m, &settings.from)?).await
    }
}

/// A minimal SMTP server that accepts every message, for tests: `sink().await` returns its
/// port and a channel of received messages (envelope recipients and raw data).
#[cfg(any(test, feature = "test-util"))]
pub mod sink {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::mpsc;

    #[derive(Debug, Clone)]
    pub struct Received {
        pub from: String,
        pub to: Vec<String>,
        pub data: String,
    }

    pub async fn sink() -> (u16, mpsc::UnboundedReceiver<Received>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind SMTP sink");
        let port = listener.local_addr().expect("local addr").port();
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (r, mut w) = sock.into_split();
                    let mut lines = BufReader::new(r).lines();
                    let _ = w.write_all(b"220 sink ESMTP\r\n").await;
                    let mut msg = Received {
                        from: String::new(),
                        to: Vec::new(),
                        data: String::new(),
                    };
                    while let Ok(Some(line)) = lines.next_line().await {
                        let upper = line.to_ascii_uppercase();
                        let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                            b"250-sink\r\n250 8BITMIME\r\n"
                        } else if upper.starts_with("MAIL FROM:") {
                            msg.from = line[10..].trim().trim_matches(['<', '>']).to_string();
                            b"250 OK\r\n"
                        } else if upper.starts_with("RCPT TO:") {
                            msg.to.push(line[8..].trim().trim_matches(['<', '>']).to_string());
                            b"250 OK\r\n"
                        } else if upper == "DATA" {
                            let _ = w.write_all(b"354 go ahead\r\n").await;
                            let mut data = String::new();
                            while let Ok(Some(l)) = lines.next_line().await {
                                if l == "." {
                                    break;
                                }
                                data.push_str(l.strip_prefix('.').unwrap_or(&l));
                                data.push_str("\r\n");
                            }
                            msg.data = data;
                            let _ = tx.send(std::mem::replace(
                                &mut msg,
                                Received {
                                    from: String::new(),
                                    to: Vec::new(),
                                    data: String::new(),
                                },
                            ));
                            b"250 queued\r\n"
                        } else if upper == "QUIT" {
                            let _ = w.write_all(b"221 bye\r\n").await;
                            break;
                        } else {
                            b"250 OK\r\n"
                        };
                        if w.write_all(reply).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (port, rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invite() -> OutgoingMail {
        OutgoingMail {
            to: vec!["Bob <bob@example.org>".into()],
            subject: "Invitation: Planning".into(),
            text: "You are invited.".into(),
            calendar: Some(CalendarPart {
                method: "REQUEST".into(),
                data: "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n".into(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn builds_imip_messages() {
        let raw = String::from_utf8(build_message(&invite(), "Ferrite <noreply@example.org>").unwrap().formatted()).unwrap();
        assert!(raw.contains("From: Ferrite <noreply@example.org>"));
        assert!(raw.contains("To: Bob <bob@example.org>"));
        assert!(raw.contains("text/calendar; charset=utf-8; method=REQUEST"));
        assert!(raw.contains("filename=\"invite.ics\""));
        assert!(raw.contains("Auto-Submitted: auto-generated"));
        assert!(
            build_message(
                &OutgoingMail {
                    subject: "x".into(),
                    ..Default::default()
                },
                "a@b.c"
            )
            .is_err()
        );
        assert!(
            build_message(
                &OutgoingMail {
                    to: vec!["not an address".into()],
                    ..Default::default()
                },
                "a@b.c"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn sends_through_smtp() {
        let (port, mut rx) = sink::sink().await;
        let settings = SmtpSettings {
            host: "127.0.0.1".into(),
            port,
            security: SmtpSecurity::None,
            username: None,
            password: None,
            from: "Ferrite <noreply@example.org>".into(),
        };
        let id = send(&transport(&settings).unwrap(), build_message(&invite(), &settings.from).unwrap())
            .await
            .unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.to, vec!["bob@example.org"]);
        assert_eq!(got.from, "noreply@example.org");
        assert!(got.data.contains("method=REQUEST"));
        assert!(!id.is_empty() && got.data.contains(&id), "Message-ID returned and sent");
    }
}
