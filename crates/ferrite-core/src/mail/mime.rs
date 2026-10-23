//! MIME structure of webmail messages (lettre): a `format=flowed` text part alone, or
//! `multipart/alternative` (text, HTML) with `multipart/related` around the HTML when it
//! shows inline images (`cid:`), inside `multipart/mixed` when there are attachments.
//! Forwarded messages travel as `message/rfc822` (8bit when possible, as RFC 2046 wants).
//! Headers with non-ASCII text are encoded words (RFC 2047) and attachment names use
//! RFC 2231 parameters; lettre does both. Also read receipts (MDN, RFC 8098).
//!
//! Signing or encrypting (S/MIME, the [security] workstream) wraps the result of
//! [`body_part`]: sign it into `multipart/signed` or encrypt it before it becomes the
//! message body in [`build`]; the headers stay as they are.

use super::richtext;
use crate::mailer::Attachment;
use crate::{Error, Result};
use chrono::{DateTime, Utc};
use lettre::Message;
use lettre::message::header::{ContentDisposition, ContentTransferEncoding, ContentType, HeaderName, HeaderValue};
use lettre::message::{Attachment as LettreAttachment, Body, Mailbox, MultiPart, SinglePart};

/// An image shown in the HTML body, referenced there as `cid:<content_id>`.
#[derive(Debug, Clone)]
pub struct InlineImage {
    pub content_id: String,
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Everything an outgoing message is made of.
#[derive(Debug, Clone, Default)]
pub struct MessageSpec {
    /// `Name <address>` of the sender (the account or an alias).
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Option<String>,
    pub subject: String,
    /// The plain text (the whole body in plain-text mode, the alternative of HTML mail).
    pub text: String,
    /// Sanitized HTML whose local images already point at `cid:` parts.
    pub html: Option<String>,
    pub inline: Vec<InlineImage>,
    pub attachments: Vec<Attachment>,
    /// Bracketed Message-IDs for threading.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// Asks for a read receipt to this address (`Disposition-Notification-To`).
    pub read_receipt_to: Option<String>,
}

fn mailbox(s: &str) -> Result<Mailbox> {
    s.trim().parse::<Mailbox>().map_err(|e| Error::bad(format!("invalid address {s:?}: {e}")))
}

fn content_type(s: &str) -> ContentType {
    ContentType::parse(s).unwrap_or_else(|_| ContentType::parse("application/octet-stream").expect("valid content type"))
}

/// The text part: UTF-8, `format=flowed`.
fn text_part(text: &str) -> SinglePart {
    SinglePart::builder()
        .header(ContentType::parse("text/plain; charset=utf-8; format=flowed").expect("valid content type"))
        .body(richtext::flowed(text))
}

/// The HTML document sent: the sanitized body in a minimal page.
pub fn html_document(body: &str) -> String {
    format!("<!DOCTYPE html><html><head><meta charset=\"utf-8\"></head><body>{body}</body></html>")
}

/// Line endings as CRLF (an uploaded .eml may use bare LF).
fn crlf(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 40);
    let mut prev = 0u8;
    for &b in raw {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// A whole message as a body part: as it is (7bit or 8bit, as RFC 2046 §5.2.1 wants for
/// message/rfc822; quoted-printable would hide its structure from readers), or base64 when
/// it has lines no transport could carry.
fn message_body(raw: &[u8]) -> Body {
    let raw = crlf(raw);
    let fits = !raw.contains(&0) && raw.split(|b| *b == b'\n').all(|l| l.len() <= 998);
    if fits {
        let encoding = if raw.is_ascii() {
            ContentTransferEncoding::SevenBit
        } else {
            ContentTransferEncoding::EightBit
        };
        Body::dangerous_pre_encoded(raw, encoding)
    } else {
        Body::new_with_encoding(raw, ContentTransferEncoding::Base64).unwrap_or_else(Body::new)
    }
}

fn attachment_part(a: &Attachment) -> SinglePart {
    if a.content_type.eq_ignore_ascii_case("message/rfc822") {
        let body = message_body(&a.data);
        return SinglePart::builder()
            .header(ContentType::parse("message/rfc822").expect("valid content type"))
            .header(ContentDisposition::attachment(&a.filename))
            .body(body);
    }
    LettreAttachment::new(a.filename.clone()).body(a.data.clone(), content_type(&a.content_type))
}

/// The body without attachments: text, or text and HTML (with inline images).
pub enum BodyPart {
    Single(SinglePart),
    Multi(MultiPart),
}

/// The message body: `text/plain`, or `multipart/alternative` (with `multipart/related`
/// around the HTML for inline images), in `multipart/mixed` with the attachments.
pub fn body_part(spec: &MessageSpec) -> BodyPart {
    let content = match &spec.html {
        None => BodyPart::Single(text_part(&spec.text)),
        Some(html) => {
            let html_part = SinglePart::html(html_document(html));
            let alternative = MultiPart::alternative().singlepart(text_part(&spec.text));
            let alternative = if spec.inline.is_empty() {
                alternative.singlepart(html_part)
            } else {
                let mut related = MultiPart::related().singlepart(html_part);
                for img in &spec.inline {
                    related = related.singlepart(
                        LettreAttachment::new_inline_with_name(img.content_id.clone(), img.filename.clone())
                            .body(img.data.clone(), content_type(&img.content_type)),
                    );
                }
                alternative.multipart(related)
            };
            BodyPart::Multi(alternative)
        }
    };
    if spec.attachments.is_empty() {
        return content;
    }
    let mut mixed = match content {
        BodyPart::Single(s) => MultiPart::mixed().singlepart(s),
        BodyPart::Multi(m) => MultiPart::mixed().multipart(m),
    };
    for a in &spec.attachments {
        mixed = mixed.singlepart(attachment_part(a));
    }
    BodyPart::Multi(mixed)
}

/// Builds the message. Needs at least one recipient (lettre derives the envelope from the
/// headers: MAIL FROM is the sender, so an alias is also the envelope sender).
pub fn build(spec: &MessageSpec) -> Result<Message> {
    if spec.to.is_empty() && spec.cc.is_empty() && spec.bcc.is_empty() {
        return Err(Error::bad("add at least one recipient"));
    }
    let mut b = Message::builder().from(mailbox(&spec.from)?).subject(spec.subject.clone()).message_id(None);
    for t in &spec.to {
        b = b.to(mailbox(t)?);
    }
    for c in &spec.cc {
        b = b.cc(mailbox(c)?);
    }
    for c in &spec.bcc {
        b = b.bcc(mailbox(c)?);
    }
    if let Some(r) = &spec.reply_to {
        b = b.reply_to(mailbox(r)?);
    }
    if let Some(id) = &spec.in_reply_to {
        b = b.in_reply_to(id.clone());
    }
    if !spec.references.is_empty() {
        b = b.references(spec.references.join(" "));
    }
    let mut msg = match body_part(spec) {
        BodyPart::Single(s) => b.singlepart(s),
        BodyPart::Multi(m) => b.multipart(m),
    }
    .map_err(|e| Error::bad(format!("building the message: {e}")))?;
    if let Some(to) = &spec.read_receipt_to {
        let mb = mailbox(to)?;
        msg.headers_mut()
            .insert_raw(HeaderValue::new(HeaderName::new_from_ascii_str("Disposition-Notification-To"), mb.to_string()));
    }
    Ok(msg)
}

/// What a read receipt is about.
#[derive(Debug, Clone)]
pub struct MdnSpec {
    /// The reader's mailbox (`Name <address>`), the receipt's sender.
    pub from: String,
    /// Where the sender asked for the receipt (`Disposition-Notification-To`).
    pub to: String,
    /// The address the original was delivered to (`Final-Recipient`).
    pub final_recipient: String,
    pub original_subject: String,
    /// Message-ID of the original, without brackets.
    pub original_message_id: Option<String>,
    pub original_date: Option<DateTime<Utc>>,
    /// The original's header block, included as `text/rfc822-headers`.
    pub original_headers: Option<Vec<u8>>,
    /// The reporting user agent's host, for `Reporting-UA`.
    pub host: String,
}

/// A read receipt (RFC 8098): `multipart/report; report-type=disposition-notification` with
/// a human-readable part, the `message/disposition-notification` part ("displayed", sent
/// manually after the user agreed) and the original's headers.
pub fn build_mdn(m: &MdnSpec) -> Result<Message> {
    let when = m
        .original_date
        .map(|d| format!(" on {}", d.format("%a, %-d %b %Y %H:%M UTC")))
        .unwrap_or_default();
    let human = format!(
        "This is a read receipt for the message sent{when} to {} with the subject \"{}\".\r\n\r\n\
         It was displayed on the recipient's screen. This is no guarantee that it has been read or understood.\r\n",
        m.final_recipient, m.original_subject
    );
    let final_rcpt = m.final_recipient.trim();
    let mut report = format!(
        "Reporting-UA: {}; Ferrite\r\nFinal-Recipient: rfc822;{final_rcpt}\r\n",
        m.host.trim().trim_end_matches('.')
    );
    if let Some(id) = &m.original_message_id {
        report.push_str(&format!("Original-Message-ID: <{id}>\r\n"));
    }
    report.push_str("Disposition: manual-action/MDN-sent-manually; displayed\r\n");
    let boundary: String = format!("mdn-{}", crate::util::random_token(12));
    let mut parts = MultiPart::builder()
        .header(content_type(&format!(
            "multipart/report; report-type=disposition-notification; boundary=\"{boundary}\""
        )))
        .singlepart(SinglePart::plain(human))
        .singlepart(
            SinglePart::builder()
                .header(content_type("message/disposition-notification"))
                .body(Body::new_with_encoding(report, ContentTransferEncoding::SevenBit).map_err(|_| Error::bad("the receipt has a non-ASCII field"))?),
        );
    if let Some(h) = &m.original_headers {
        parts = parts.singlepart(SinglePart::builder().header(content_type("text/rfc822-headers")).body(message_body(h)));
    }
    let mut b = Message::builder()
        .from(mailbox(&m.from)?)
        .to(mailbox(&m.to)?)
        .subject(format!("Read: {}", m.original_subject))
        .message_id(None);
    if let Some(id) = &m.original_message_id {
        b = b.in_reply_to(format!("<{id}>")).references(format!("<{id}>"));
    }
    let mut msg = b.multipart(parts).map_err(|e| Error::bad(format!("building the receipt: {e}")))?;
    // RFC 3834: automatic responses say so (also keeps vacation responders from answering).
    msg.headers_mut()
        .insert_raw(HeaderValue::new(HeaderName::new_from_ascii_str("Auto-Submitted"), "auto-replied".into()));
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::{MessageParser, MimeHeaders, PartType};

    fn spec() -> MessageSpec {
        MessageSpec {
            from: "Jürgen Müller <juergen@example.org>".into(),
            to: vec!["Zoë <zoe@example.org>".into()],
            subject: "Grüße aus Köln – Planung".into(),
            text:
                "Hallo Zoë,\n\nhere is a line long enough to be wrapped by the flowed encoder because it keeps going past the limit.\n\n> quoted\n-- \nJürgen"
                    .into(),
            ..Default::default()
        }
    }

    #[test]
    fn plain_messages_are_one_flowed_part_with_encoded_headers() {
        let msg = build(&spec()).unwrap();
        let raw = msg.formatted();
        let s = String::from_utf8_lossy(&raw);
        assert!(s.contains("Subject: =?utf-8?"), "encoded-word subject: {s}");
        assert!(s.contains("From: =?utf-8?"), "encoded-word name: {s}");
        assert!(s.contains("format=flowed"));
        assert!(!s.contains("multipart"), "no multipart for a plain message");
        let m = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(m.subject(), Some("Grüße aus Köln – Planung"));
        assert_eq!(m.from().unwrap().first().unwrap().name(), Some("Jürgen Müller"));
        let text = m.body_text(0).unwrap().replace("\r\n", "\n");
        assert!(text.contains("Hallo Zoë"));
        assert!(text.lines().all(|l| l.chars().count() <= 78), "{text}");
        assert!(text.contains("\n-- \n"), "the signature separator keeps its space: {text:?}");
    }

    #[test]
    fn html_messages_are_alternative_related_mixed() {
        let mut s = spec();
        s.html = Some("<p>Hallo <b>Zoë</b></p><img src=\"cid:logo@ferrite\" alt=\"Logo\">".into());
        s.inline = vec![InlineImage {
            content_id: "logo@ferrite".into(),
            filename: "logo.png".into(),
            content_type: "image/png".into(),
            data: vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3],
        }];
        s.attachments = vec![
            Attachment {
                filename: "Übersicht März.pdf".into(),
                content_type: "application/pdf".into(),
                data: b"%PDF-1.4\n".to_vec(),
            },
            Attachment {
                filename: "Original.eml".into(),
                content_type: "message/rfc822".into(),
                data: format!(
                    "From: a@example.org\nSubject: inner\n\nInner body with a line far longer than seventy-eight characters, which quoted-printable would wrap.\n{}\n",
                    "x".repeat(900)
                )
                .into_bytes(),
            },
        ];
        s.read_receipt_to = Some("Jürgen Müller <juergen@example.org>".into());
        let raw = build(&s).unwrap().formatted();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("Disposition-Notification-To: =?utf-8?"), "{text}");
        assert!(
            text.contains("filename*0*=utf-8''") || text.contains("filename*=utf-8''"),
            "RFC 2231 file name: {text}"
        );
        let m = MessageParser::default().parse(&raw).unwrap();
        let root = m.root_part().content_type().unwrap();
        assert_eq!((root.ctype(), root.subtype()), ("multipart", Some("mixed")));
        // mixed > [alternative > [text, related > [html, image]], pdf, rfc822]
        let kinds: Vec<String> = m
            .parts
            .iter()
            .map(|p| {
                p.content_type()
                    .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("")))
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "multipart/mixed",
                "multipart/alternative",
                "text/plain",
                "multipart/related",
                "text/html",
                "image/png",
                "application/pdf",
                "message/rfc822",
            ]
        );
        let img = &m.parts[5];
        assert_eq!(img.content_id(), Some("logo@ferrite"));
        assert_eq!(img.contents(), &[0x89, b'P', b'N', b'G', 0, 1, 2, 3]);
        assert_eq!(m.parts[6].attachment_name(), Some("Übersicht März.pdf"));
        let PartType::Message(inner) = &m.parts[7].body else {
            panic!("not a nested message")
        };
        assert_eq!(inner.subject(), Some("inner"));
        // As it is, not quoted-printable (which would hide the structure) or base64.
        assert!(
            text.contains("Content-Type: message/rfc822\r\nContent-Disposition: attachment; filename=\"Original.eml\"\r\nContent-Transfer-Encoding: 7bit"),
            "{text}"
        );
        assert!(inner.body_text(0).unwrap().contains("which quoted-printable would wrap"));
        assert!(m.body_html(0).unwrap().contains("<b>Zoë</b>"));
        assert!(m.body_text(0).unwrap().contains("Hallo Zoë"));
    }

    #[test]
    fn read_receipts_follow_rfc_8098() {
        let raw = build_mdn(&MdnSpec {
            from: "Bob <bob@example.org>".into(),
            to: "Alice <alice@example.org>".into(),
            final_recipient: "bob@example.org".into(),
            original_subject: "Plans".into(),
            original_message_id: Some("orig@example.org".into()),
            original_date: None,
            original_headers: Some(b"From: Alice <alice@example.org>\nSubject: Plans\n".to_vec()),
            host: "ferrite.example.org".into(),
        })
        .unwrap()
        .formatted();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("multipart/report; report-type=disposition-notification;"), "{text}");
        assert!(text.contains("Auto-Submitted: auto-replied"));
        assert!(text.contains("In-Reply-To: <orig@example.org>"));
        let m = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(m.subject(), Some("Read: Plans"));
        let kinds: Vec<String> = m
            .parts
            .iter()
            .map(|p| {
                p.content_type()
                    .map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or("")))
                    .unwrap_or_default()
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["multipart/report", "text/plain", "message/disposition-notification", "text/rfc822-headers"]
        );
        let report = String::from_utf8_lossy(m.parts[2].contents()).to_string();
        assert!(report.contains("Reporting-UA: ferrite.example.org; Ferrite\r\n"), "{report}");
        assert!(report.contains("Final-Recipient: rfc822;bob@example.org\r\n"));
        assert!(report.contains("Original-Message-ID: <orig@example.org>\r\n"));
        assert!(report.contains("Disposition: manual-action/MDN-sent-manually; displayed\r\n"));
    }

    #[test]
    fn a_message_needs_recipients() {
        let mut s = spec();
        s.to.clear();
        assert!(build(&s).is_err());
        s.to = vec!["not an address".into()];
        assert!(build(&s).is_err());
    }
}
