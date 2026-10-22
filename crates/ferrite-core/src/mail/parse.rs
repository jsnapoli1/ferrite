//! Message parsing (mail-parser), conversation threading, HTML sanitizing, reply/forward
//! quoting and IMAP folder-name decoding. Everything here is pure, so it is unit tested
//! without a server.

use chrono::{DateTime, TimeZone, Utc};
use mail_parser::{HeaderValue, MessageParser, MessagePart, MimeHeaders, PartType};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Characters of body text kept for list previews.
pub const PREVIEW_CHARS: usize = 180;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Addr {
    #[serde(default)]
    pub name: String,
    pub address: String,
}

impl Addr {
    /// `Name <address>` (or the bare address), quoting the name when it needs it.
    pub fn display(&self) -> String {
        if self.name.is_empty() || self.name == self.address {
            return self.address.clone();
        }
        let needs_quotes = self.name.chars().any(|c| "()<>[]:;@\\,.\"".contains(c));
        if needs_quotes {
            format!("\"{}\" <{}>", self.name.replace('\\', "\\\\").replace('"', "\\\""), self.address)
        } else {
            format!("{} <{}>", self.name, self.address)
        }
    }

    /// Name if known, else the address.
    pub fn label(&self) -> &str {
        if self.name.is_empty() { &self.address } else { &self.name }
    }

    /// Parses `Name <a@b>`, `"Name" <a@b>` or `a@b`.
    pub fn parse(s: &str) -> Option<Addr> {
        let s = s.trim();
        if let (Some(lt), true) = (s.rfind('<'), s.ends_with('>')) {
            let address = s[lt + 1..s.len() - 1].trim().to_string();
            let name = s[..lt].trim().trim_matches('"').replace("\\\"", "\"").trim().to_string();
            return valid_address(&address).then_some(Addr { name, address });
        }
        valid_address(s).then(|| Addr {
            name: String::new(),
            address: s.to_string(),
        })
    }
}

fn valid_address(a: &str) -> bool {
    match a.split_once('@') {
        Some((local, domain)) => !local.is_empty() && !domain.is_empty() && !a.chars().any(|c| c.is_whitespace() || "<>\",;".contains(c)),
        None => false,
    }
}

fn addrs(v: Option<&mail_parser::Address>) -> Vec<Addr> {
    v.map(|a| {
        a.iter()
            .filter_map(|x| {
                let address = x.address()?.trim().to_string();
                Some(Addr {
                    name: x.name().map(|n| n.trim().to_string()).unwrap_or_default(),
                    address,
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// A Message-ID without angle brackets or whitespace.
pub fn normalize_id(id: &str) -> Option<String> {
    let t = id.trim().trim_start_matches('<').trim_end_matches('>').trim();
    (!t.is_empty() && !t.contains(char::is_whitespace)).then(|| t.to_string())
}

fn ids(v: &HeaderValue) -> Vec<String> {
    match v {
        HeaderValue::Text(t) => t.split_whitespace().filter_map(normalize_id).collect(),
        HeaderValue::TextList(l) => l.iter().flat_map(|t| t.split_whitespace()).filter_map(normalize_id).collect(),
        _ => Vec::new(),
    }
}

fn to_utc(d: &mail_parser::DateTime) -> Option<DateTime<Utc>> {
    Utc.timestamp_opt(d.to_timestamp(), 0).single()
}

/// What the message list needs, parsed from the header and the first bytes of the body.
#[derive(Debug, Clone, Default)]
pub struct Envelope {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub from: Option<Addr>,
    pub to: Vec<Addr>,
    pub cc: Vec<Addr>,
    pub reply_to: Vec<Addr>,
    pub date: Option<DateTime<Utc>>,
    pub has_attachments: bool,
    pub preview: String,
}

/// Parses an envelope from `BODY[HEADER]` plus a prefix of `BODY[TEXT]` (which may be cut
/// mid-part; the preview is best effort).
pub fn parse_envelope(header: &[u8], text_prefix: &[u8]) -> Envelope {
    let mut raw = Vec::with_capacity(header.len() + text_prefix.len() + 2);
    raw.extend_from_slice(header);
    if !header.ends_with(b"\r\n\r\n") && !header.ends_with(b"\n\n") {
        raw.extend_from_slice(b"\r\n");
    }
    raw.extend_from_slice(text_prefix);
    let Some(m) = MessageParser::default().parse(&raw) else {
        return Envelope::default();
    };
    let multipart_mixed = m
        .root_part()
        .content_type()
        .is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("multipart") && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("mixed")));
    Envelope {
        message_id: m.message_id().and_then(normalize_id),
        in_reply_to: ids(m.in_reply_to()).into_iter().next(),
        references: ids(m.references()),
        subject: m.subject().unwrap_or("").trim().to_string(),
        from: addrs(m.from()).into_iter().next(),
        to: addrs(m.to()),
        cc: addrs(m.cc()),
        reply_to: addrs(m.reply_to()),
        date: m.date().and_then(to_utc),
        has_attachments: multipart_mixed || m.attachments().any(|p| !is_inline_image(p)),
        preview: preview_of(&m),
    }
}

fn preview_of(m: &mail_parser::Message) -> String {
    let text = readable_text(m);
    let mut out: String = text.split_whitespace().take(PREVIEW_CHARS).collect::<Vec<_>>().join(" ");
    if out.chars().count() > PREVIEW_CHARS {
        out = out.chars().take(PREVIEW_CHARS).collect::<String>() + "…";
    }
    out
}

/// The message as plain text: its text part, else its HTML part converted.
fn readable_text(m: &mail_parser::Message) -> String {
    if let Some(t) = m.text_bodies().find_map(|p| match &p.body {
        PartType::Text(t) => Some(t.to_string()),
        _ => None,
    }) {
        return t;
    }
    match m.html_bodies().find_map(|p| match &p.body {
        PartType::Html(h) => Some(h.to_string()),
        _ => None,
    }) {
        Some(h) => html_to_text(&h),
        None => m.body_text(0).map(Cow::into_owned).unwrap_or_default(),
    }
}

/// Converts HTML to readable text: block elements start new lines, list items get a bullet,
/// head/style/script content and comments are dropped, entities are decoded.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    let mut skip_until: Option<&str> = None;
    while i < html.len() {
        if let Some(end) = skip_until {
            match lower[i..].find(end) {
                Some(p) => {
                    i += p + end.len();
                    // Past the rest of a closing tag ("</style>").
                    if end.starts_with("</") {
                        i = lower[i..].find('>').map(|q| i + q + 1).unwrap_or(html.len());
                    }
                    skip_until = None;
                }
                None => break,
            }
            continue;
        }
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            skip_until = Some("-->");
            continue;
        }
        if let Some(inner) = rest.strip_prefix('<') {
            let close = rest.find('>').map(|p| i + p + 1).unwrap_or(html.len());
            let closing = inner.starts_with('/');
            let tag: String = inner.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
            match tag.as_str() {
                "head" if !closing => skip_until = Some("</head"),
                "style" if !closing => skip_until = Some("</style"),
                "script" if !closing => skip_until = Some("</script"),
                "title" if !closing => skip_until = Some("</title"),
                "br" => out.push('\n'),
                "li" if !closing => out.push_str("\n• "),
                "p" | "div" | "tr" | "table" | "ul" | "ol" | "blockquote" | "pre" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "hr" | "section" | "article"
                | "header" | "footer" => out.push('\n'),
                "td" | "th" => out.push(' '),
                _ => {}
            }
            i = close;
            continue;
        }
        let next = rest.find('<').map(|p| i + p).unwrap_or(html.len());
        out.push_str(&decode_entities(&html[i..next]));
        i = next;
    }
    // Collapse runs of spaces within lines, and blank lines.
    let mut text = String::with_capacity(out.len());
    let mut blank = 0;
    for line in out.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() {
            blank += 1;
            if blank > 1 || text.is_empty() {
                continue;
            }
        } else {
            blank = 0;
        }
        text.push_str(&l);
        text.push('\n');
    }
    text.trim_end().to_string()
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        let decoded = after.find(';').filter(|e| *e <= 10).and_then(|e| {
            let name = &after[..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ if name.starts_with("#x") || name.starts_with("#X") => u32::from_str_radix(&name[2..], 16).ok().and_then(char::from_u32),
                _ if name.starts_with('#') => name[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_inline_image(p: &MessagePart) -> bool {
    p.content_id().is_some() && p.content_type().is_some_and(|ct| ct.ctype().eq_ignore_ascii_case("image"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentInfo {
    /// Position among the message's attachments (stable for a given message).
    pub index: usize,
    pub name: String,
    pub mime: String,
    pub size: usize,
    /// Content-ID of inline parts (referenced as `cid:` from the HTML).
    pub content_id: Option<String>,
    pub inline: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedBody {
    pub envelope: Envelope,
    pub text: String,
    pub html: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
}

fn attachment_name(p: &MessagePart, index: usize) -> String {
    if let Some(n) = p.attachment_name() {
        let n = n.trim().replace(['/', '\\', '\0'], "_");
        if !n.is_empty() {
            return n;
        }
    }
    if let PartType::Message(m) = &p.body {
        if let Some(s) = m.subject() {
            return format!("{}.eml", s.trim().replace(['/', '\\', '\0'], "_"));
        }
        return format!("message-{}.eml", index + 1);
    }
    let ext = p
        .content_type()
        .and_then(|ct| ct.subtype().map(|s| s.to_ascii_lowercase()))
        .unwrap_or_else(|| "bin".into());
    format!("attachment-{}.{ext}", index + 1)
}

fn mime_of(p: &MessagePart) -> String {
    if matches!(p.body, PartType::Message(_)) {
        return "message/rfc822".into();
    }
    p.content_type()
        .map(|ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
            None => ct.ctype().to_ascii_lowercase(),
        })
        .unwrap_or_else(|| "application/octet-stream".into())
}

/// Parses a whole message: envelope, text, HTML and attachments.
pub fn parse_body(raw: &[u8]) -> ParsedBody {
    let Some(m) = MessageParser::default().parse(raw) else {
        return ParsedBody {
            text: String::from_utf8_lossy(raw).into_owned(),
            ..Default::default()
        };
    };
    let envelope = parse_envelope_full(&m);
    // Only a real text/html part counts as HTML (mail-parser also renders text as HTML).
    let html = m.html_bodies().find_map(|p| match &p.body {
        PartType::Html(h) => Some(h.to_string()),
        _ => None,
    });
    let text = readable_text(&m);
    let attachments = m
        .attachments()
        .enumerate()
        .map(|(index, p)| AttachmentInfo {
            index,
            name: attachment_name(p, index),
            mime: mime_of(p),
            size: p.contents().len(),
            content_id: p.content_id().and_then(normalize_id),
            inline: is_inline_image(p),
        })
        .collect();
    ParsedBody {
        envelope,
        text,
        html,
        attachments,
    }
}

fn parse_envelope_full(m: &mail_parser::Message) -> Envelope {
    Envelope {
        message_id: m.message_id().and_then(normalize_id),
        in_reply_to: ids(m.in_reply_to()).into_iter().next(),
        references: ids(m.references()),
        subject: m.subject().unwrap_or("").trim().to_string(),
        from: addrs(m.from()).into_iter().next(),
        to: addrs(m.to()),
        cc: addrs(m.cc()),
        reply_to: addrs(m.reply_to()),
        date: m.date().and_then(to_utc),
        has_attachments: m.attachments().any(|p| !is_inline_image(p)),
        preview: preview_of(m),
    }
}

/// Bytes of one attachment: (file name, MIME type, data).
pub fn attachment(raw: &[u8], index: usize) -> Option<(String, String, Vec<u8>)> {
    let m = MessageParser::default().parse(raw)?;
    let p = m.attachments().nth(index)?;
    Some((attachment_name(p, index), mime_of(p), p.contents().to_vec()))
}

/// The inline part with Content-ID `cid`.
pub fn attachment_by_cid(raw: &[u8], cid: &str) -> Option<(String, String, Vec<u8>)> {
    let m = MessageParser::default().parse(raw)?;
    let want = normalize_id(cid)?;
    let (i, p) = m
        .attachments()
        .enumerate()
        .find(|(_, p)| p.content_id().and_then(normalize_id).as_deref() == Some(want.as_str()))?;
    Some((attachment_name(p, i), mime_of(p), p.contents().to_vec()))
}

/// The id every message of a conversation shares: the first References entry (the root),
/// else In-Reply-To, else the message's own id.
pub fn thread_root(message_id: Option<&str>, in_reply_to: Option<&str>, references: &[String]) -> Option<String> {
    references.first().map(String::as_str).or(in_reply_to).or(message_id).and_then(normalize_id)
}

/// "Re: " / "Fwd: " prefixed subject, without stacking prefixes.
pub fn prefixed_subject(prefix: &str, subject: &str) -> String {
    let s = subject.trim();
    let lower = s.to_ascii_lowercase();
    let p = prefix.trim_end_matches(' ').trim_end_matches(':').to_ascii_lowercase();
    let already = lower.starts_with(&format!("{p}:")) || (p == "fwd" && (lower.starts_with("fw:") || lower.starts_with("fwd:")));
    if already { s.to_string() } else { format!("{prefix}{s}") }
}

/// Body of a reply: an attribution line and the original quoted with "> ".
pub fn quote_reply(text: &str, from: &str, date: Option<DateTime<Utc>>) -> String {
    let when = date.map(|d| d.format("%a, %-d %b %Y at %H:%M UTC").to_string());
    let attribution = match when {
        Some(w) => format!("On {w}, {from} wrote:"),
        None => format!("{from} wrote:"),
    };
    let quoted: Vec<String> = text
        .trim_end()
        .lines()
        .map(|l| {
            if l.starts_with('>') {
                format!(">{l}")
            } else if l.is_empty() {
                ">".into()
            } else {
                format!("> {l}")
            }
        })
        .collect();
    format!("\n\n{attribution}\n{}\n", quoted.join("\n"))
}

/// Body of a forward: the original's headers and text below a separator.
pub fn quote_forward(text: &str, from: &str, date: Option<DateTime<Utc>>, subject: &str, to: &str) -> String {
    let mut out = String::from("\n\n---------- Forwarded message ----------\n");
    out.push_str(&format!("From: {from}\n"));
    if let Some(d) = date {
        out.push_str(&format!("Date: {}\n", d.format("%a, %-d %b %Y %H:%M UTC")));
    }
    out.push_str(&format!("Subject: {subject}\n"));
    if !to.is_empty() {
        out.push_str(&format!("To: {to}\n"));
    }
    out.push('\n');
    out.push_str(text.trim_end());
    out.push('\n');
    out
}

/// Decodes an IMAP mailbox name (RFC 3501 modified UTF-7: `&...-` runs of UTF-16 in a
/// base64 variant with `,` for `/`). Invalid runs are kept as they are.
pub fn decode_mailbox_name(name: &str) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut rest = name;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('-') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let chunk = &after[..end];
        if chunk.is_empty() {
            out.push('&');
        } else {
            let b64 = chunk.replace(',', "/");
            let decoded = base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(b64.as_bytes())
                .ok()
                .filter(|b| b.len() % 2 == 0)
                .and_then(|b| {
                    let units: Vec<u16> = b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
                    String::from_utf16(&units).ok()
                });
            match decoded {
                Some(s) => out.push_str(&s),
                None => out.push_str(&rest[start..start + 1 + end + 1]),
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

// ---- HTML -----------------------------------------------------------------------------

/// CSS properties kept in `style` attributes.
const STYLE_PROPERTIES: &[&str] = &[
    "background",
    "background-color",
    "border",
    "border-bottom",
    "border-collapse",
    "border-color",
    "border-left",
    "border-radius",
    "border-right",
    "border-spacing",
    "border-style",
    "border-top",
    "border-width",
    "color",
    "direction",
    "display",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "height",
    "letter-spacing",
    "line-height",
    "list-style",
    "list-style-type",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "table-layout",
    "text-align",
    "text-decoration",
    "text-indent",
    "text-transform",
    "vertical-align",
    "white-space",
    "width",
    "word-break",
    "word-spacing",
    "word-wrap",
    "overflow-wrap",
];

#[derive(Debug, Clone)]
pub struct Sanitized {
    pub html: String,
    /// Inline `<style>` rules, sanitized, for the document head.
    pub css: String,
    /// Remote images or stylesheets were removed (only when they were not allowed).
    pub remote_blocked: bool,
}

fn is_remote(url: &str) -> bool {
    let u = url.trim_start().to_ascii_lowercase();
    u.starts_with("http:") || u.starts_with("https:") || u.starts_with("//")
}

/// Removes CSS that could load something or run code: `@import`, `url()` (except `data:`
/// and, when allowed, remote http(s)), `expression()`, bindings and behaviors.
fn sanitize_css(css: &str, allow_remote: bool, remote_found: &AtomicBool) -> String {
    let mut out = String::with_capacity(css.len());
    let lower = css.to_ascii_lowercase();
    let mut i = 0;
    while i < css.len() {
        let rest = &lower[i..];
        if rest.starts_with("@import") {
            // Never followed; only reported while remote content is blocked.
            if !allow_remote {
                remote_found.store(true, Ordering::Relaxed);
            }
            // Skip to the end of the rule.
            let end = rest.find(';').map(|e| i + e + 1).unwrap_or(css.len());
            i = end;
            continue;
        }
        if rest.starts_with("url(") {
            let close = rest.find(')').map(|e| i + e + 1).unwrap_or(css.len());
            let inner = css[i + 4..close.saturating_sub(1).max(i + 4)].trim().trim_matches(['"', '\'']);
            let keep = inner.to_ascii_lowercase().starts_with("data:image/") || (allow_remote && is_remote(inner) && !inner.contains(['"', '\'', '\\']));
            if keep {
                out.push_str(&css[i..close]);
            } else {
                if is_remote(inner) {
                    remote_found.store(true, Ordering::Relaxed);
                }
                out.push_str("none");
            }
            i = close;
            continue;
        }
        if rest.starts_with("expression(") || rest.starts_with("-moz-binding") || rest.starts_with("behavior") || rest.starts_with("javascript:") {
            out.push_str("/**/");
            i += rest.find([';', '}']).unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("</") {
            i += 2;
            continue;
        }
        let ch = css[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Splits `<style>` blocks out of an HTML document: (the CSS, the HTML without them).
fn extract_styles(html: &str) -> (String, String) {
    let lower = html.to_ascii_lowercase();
    let mut css = String::new();
    let mut rest_html = String::with_capacity(html.len());
    let mut i = 0;
    while let Some(start) = lower[i..].find("<style").map(|s| s + i) {
        rest_html.push_str(&html[i..start]);
        let Some(open_end) = lower[start..].find('>').map(|e| start + e + 1) else {
            i = html.len();
            break;
        };
        let close = lower[open_end..].find("</style").map(|e| open_end + e).unwrap_or(html.len());
        css.push_str(&html[open_end..close]);
        css.push('\n');
        i = lower[close..].find('>').map(|e| close + e + 1).unwrap_or(html.len());
    }
    if i < html.len() {
        rest_html.push_str(&html[i..]);
    }
    (css, rest_html)
}

/// Sanitizes email HTML for display: no scripts, event handlers, forms, frames or objects;
/// links open in a new tab without a referrer; `cid:` images point at `cid_url(cid)`; remote
/// images and CSS resources are removed unless `allow_remote`.
pub fn sanitize_html(html: &str, allow_remote: bool, cid_url: impl Fn(&str) -> String + Send + Sync + 'static) -> Sanitized {
    let remote_found = Arc::new(AtomicBool::new(false));
    let (raw_css, body) = extract_styles(html);
    let css = sanitize_css(&raw_css, allow_remote, &remote_found);
    let found = remote_found.clone();
    let mut b = ammonia::Builder::default();
    b.add_tags(["font", "center"])
        .add_generic_attributes(["style", "align", "valign", "bgcolor", "width", "height", "border", "dir", "color"])
        .add_tag_attributes("font", ["face", "size", "color"])
        .add_tag_attributes("table", ["cellpadding", "cellspacing"])
        .add_tag_attributes("td", ["colspan", "rowspan", "nowrap"])
        .add_tag_attributes("th", ["colspan", "rowspan", "nowrap"])
        .filter_style_properties(STYLE_PROPERTIES.iter().copied().collect::<HashSet<_>>())
        .url_schemes(["http", "https", "mailto", "tel", "cid", "data"].into_iter().collect())
        .link_rel(Some("noopener noreferrer"))
        .set_tag_attribute_value("a", "target", "_blank")
        .attribute_filter(move |element, attribute, value| match (element, attribute) {
            ("img", "src") => {
                let v = value.trim();
                let lower = v.to_ascii_lowercase();
                if lower.starts_with("cid:") {
                    Some(cid_url(&v[4..]).into())
                } else if lower.starts_with("data:image/") {
                    Some(value.into())
                } else if is_remote(v) {
                    if allow_remote {
                        Some(value.into())
                    } else {
                        found.store(true, Ordering::Relaxed);
                        None
                    }
                } else {
                    None
                }
            }
            ("a", "href") if value.trim().to_ascii_lowercase().starts_with("data:") => None,
            (_, "style") => {
                let lower = value.to_ascii_lowercase();
                if lower.contains("url(") || lower.contains("expression(") {
                    Some(sanitize_css(value, allow_remote, &found).into())
                } else {
                    Some(value.into())
                }
            }
            _ => Some(value.into()),
        });
    let html = b.clean(&body).to_string();
    Sanitized {
        html,
        css,
        remote_blocked: remote_found.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXED: &str = "From: \"Ada Lovelace\" <ada@example.org>\r\n\
To: Bob <bob@example.org>, carol@example.org\r\n\
Cc: =?UTF-8?Q?J=C3=BCrgen?= <juergen@example.org>\r\n\
Subject: =?UTF-8?B?UGxhbnMgZm9yIE3DpHJ6?=\r\n\
Date: Tue, 20 Oct 2026 10:15:00 +0200\r\n\
Message-ID: <child@example.org>\r\n\
In-Reply-To: <parent@example.org>\r\n\
References: <root@example.org>\r\n <parent@example.org>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: multipart/alternative; boundary=\"b2\"\r\n\
\r\n\
--b2\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Hello Bob,\r\nthe plan is attached.\r\n\
--b2\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>Hello <b>Bob</b>,</p><img src=\"cid:logo@x\"><script>alert(1)</script>\r\n\
--b2--\r\n\
--b1\r\n\
Content-Type: image/png\r\n\
Content-ID: <logo@x>\r\n\
Content-Disposition: inline\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
iVBORw0KGgo=\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"plan.pdf\"\r\n\
Content-Disposition: attachment; filename=\"plan.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--b1--\r\n";

    #[test]
    fn parses_envelope_body_and_attachments() {
        let raw = MIXED.as_bytes();
        let split = MIXED.find("\r\n\r\n").unwrap() + 4;
        let env = parse_envelope(&raw[..split], &raw[split..split + 200]);
        assert_eq!(env.subject, "Plans for März");
        assert_eq!(env.from.as_ref().unwrap().name, "Ada Lovelace");
        assert_eq!(env.from.as_ref().unwrap().address, "ada@example.org");
        assert_eq!(env.to.len(), 2);
        assert_eq!(env.cc[0].name, "Jürgen");
        assert_eq!(env.message_id.as_deref(), Some("child@example.org"));
        assert_eq!(env.in_reply_to.as_deref(), Some("parent@example.org"));
        assert_eq!(env.references, vec!["root@example.org", "parent@example.org"]);
        assert_eq!(env.date.unwrap().to_rfc3339(), "2026-10-20T08:15:00+00:00");
        assert!(env.has_attachments);
        assert!(env.preview.starts_with("Hello Bob"), "{}", env.preview);

        let body = parse_body(raw);
        assert!(body.text.contains("the plan is attached"));
        assert!(body.html.as_deref().unwrap().contains("<b>Bob</b>"));
        assert_eq!(body.attachments.len(), 2);
        let logo = &body.attachments[0];
        assert!(logo.inline && logo.content_id.as_deref() == Some("logo@x") && logo.mime == "image/png");
        let pdf = &body.attachments[1];
        assert_eq!((pdf.name.as_str(), pdf.mime.as_str(), pdf.inline), ("plan.pdf", "application/pdf", false));
        let (name, mime, data) = attachment(raw, 1).unwrap();
        assert_eq!(
            (name.as_str(), mime.as_str(), data.as_slice()),
            ("plan.pdf", "application/pdf", b"%PDF-1.4\n".as_slice())
        );
        assert_eq!(attachment_by_cid(raw, "<logo@x>").unwrap().1, "image/png");
        assert!(attachment(raw, 5).is_none());
    }

    #[test]
    fn html_becomes_readable_text() {
        let t = html_to_text(
            "<html><head><title>x</title><style>p{}</style></head><body><h1>October</h1><p>Hello&nbsp;<b>you</b> &amp; me</p><!-- c --><ul><li>one</li><li>two</li></ul><script>bad()</script>A&#233;&#x41;</body></html>",
        );
        assert_eq!(t, "October\n\nHello you & me\n\n• one\n• two\nAéA");
        let raw = b"From: a@example.org\r\nSubject: hi\r\nContent-Type: text/html\r\n\r\n<p>First</p><p>Second</p>";
        let b = parse_body(raw);
        assert_eq!(b.text, "First\n\nSecond");
        assert_eq!(b.envelope.preview, "First Second");
    }

    #[test]
    fn plain_text_message_has_no_html() {
        let raw = b"From: a@example.org\r\nSubject: hi\r\n\r\nJust text.\r\n";
        let b = parse_body(raw);
        assert!(b.html.is_none());
        assert_eq!(b.text.trim(), "Just text.");
        assert!(!b.envelope.has_attachments);
    }

    #[test]
    fn threads_by_root() {
        let refs = vec!["root@x".to_string(), "mid@x".to_string()];
        assert_eq!(thread_root(Some("c@x"), Some("mid@x"), &refs).as_deref(), Some("root@x"));
        assert_eq!(thread_root(Some("c@x"), Some("<p@x>"), &[]).as_deref(), Some("p@x"));
        assert_eq!(thread_root(Some("<c@x>"), None, &[]).as_deref(), Some("c@x"));
        assert_eq!(thread_root(None, None, &[]), None);
        assert_eq!(normalize_id(" <a@b> ").as_deref(), Some("a@b"));
        assert_eq!(normalize_id("<>"), None);
    }

    #[test]
    fn sanitizer_strips_scripts_and_remote_content() {
        let html = r#"<html><head><style>@import url(http://evil/x.css); body{color:red;background:url(https://t.example/p.png)}</style></head>
<body onload="x()"><p style="color:blue;background-image:url('http://t.example/bg.png')">Hi</p>
<img src="https://tracker.example/pixel.gif" width=1><img src="cid:logo@x"><img src="data:image/png;base64,AAAA">
<a href="javascript:alert(1)">bad</a><a href="https://ok.example/">ok</a><a href="data:text/html,x">d</a>
<script>alert(1)</script><iframe src="https://x"></iframe><form action="https://x"><input name=a></form></body></html>"#;
        let s = sanitize_html(html, false, |cid| format!("/cid/{cid}"));
        assert!(s.remote_blocked);
        assert!(!s.html.contains("script") && !s.html.contains("onload") && !s.html.contains("iframe") && !s.html.contains("<form"));
        assert!(!s.html.contains("tracker.example") && !s.html.contains("t.example/bg.png"));
        assert!(!s.html.contains("javascript:") && !s.html.contains("data:text/html"));
        assert!(s.html.contains(r#"src="/cid/logo@x""#), "{}", s.html);
        assert!(s.html.contains("data:image/png;base64,AAAA"));
        assert!(s.html.contains(r#"href="https://ok.example/""#) && s.html.contains(r#"target="_blank""#) && s.html.contains("noopener"));
        assert!(s.html.contains("color:blue"));
        assert!(!s.css.contains("@import") && !s.css.contains("evil") && !s.css.contains("t.example"));
        assert!(s.css.contains("color:red"));

        let allowed = sanitize_html(html, true, |cid| format!("/cid/{cid}"));
        assert!(!allowed.remote_blocked);
        assert!(allowed.html.contains("https://tracker.example/pixel.gif"));
        assert!(allowed.css.contains("https://t.example/p.png"));
        assert!(!allowed.html.contains("script") && !allowed.css.contains("@import"));

        let clean = sanitize_html("<p>plain</p>", false, |c| c.to_string());
        assert!(!clean.remote_blocked && clean.css.is_empty());
    }

    #[test]
    fn quoting_and_subjects() {
        assert_eq!(prefixed_subject("Re: ", "Hello"), "Re: Hello");
        assert_eq!(prefixed_subject("Re: ", "RE: Hello"), "RE: Hello");
        assert_eq!(prefixed_subject("Fwd: ", "Fw: Hello"), "Fw: Hello");
        assert_eq!(prefixed_subject("Fwd: ", "Re: Hello"), "Fwd: Re: Hello");
        let q = quote_reply("line one\n\n> older\n", "Ada <ada@example.org>", None);
        assert!(q.contains("Ada <ada@example.org> wrote:\n> line one\n>\n>> older"));
        let f = quote_forward("body", "Ada", None, "Plans", "Bob");
        assert!(f.contains("Forwarded message") && f.contains("Subject: Plans") && f.ends_with("body\n"));
    }

    #[test]
    fn addresses_parse_and_display() {
        let a = Addr::parse("\"Lovelace, Ada\" <ada@example.org>").unwrap();
        assert_eq!(a.name, "Lovelace, Ada");
        assert_eq!(a.display(), "\"Lovelace, Ada\" <ada@example.org>");
        assert_eq!(Addr::parse("bob@example.org").unwrap().display(), "bob@example.org");
        assert!(Addr::parse("not an address").is_none());
        assert!(Addr::parse("Bob <bob>").is_none());
    }

    #[test]
    fn decodes_modified_utf7_names() {
        assert_eq!(decode_mailbox_name("INBOX"), "INBOX");
        assert_eq!(decode_mailbox_name("Entw&APw-rfe"), "Entwürfe");
        assert_eq!(decode_mailbox_name("&ZeVnLIqe-"), "日本語");
        assert_eq!(decode_mailbox_name("Tom &- Jerry"), "Tom & Jerry");
        assert_eq!(decode_mailbox_name("bad&@@@-x"), "bad&@@@-x");
    }
}
