//! Phishing hints for the reader, after Nextcloud Mail's checks: a sender name that belongs to
//! a contact with other addresses, a sender name that is itself a different address, a date
//! in the future, links whose text shows another site than they open, a Reply-To in another
//! domain, and the mail server's `$Phishing` keyword. Each finding comes with a plain reason;
//! none of them blocks anything.
//!
//! Everything here is pure; [`Core::mail_security_report`](crate::Core) gathers the inputs.

use super::parse::Addr;
use super::trust::domain_of;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

/// Clock skew tolerated before a date counts as "in the future".
pub const FUTURE_TOLERANCE: Duration = Duration::minutes(10);
/// Links reported per message.
const MAX_LINKS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    ContactMismatch,
    CustomAddress,
    FutureDate,
    MisleadingLink,
    ReplyTo,
    ServerFlag,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub kind: CheckKind,
    pub message: String,
    /// For links: (shown text, real target).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<(String, String)>,
}

/// What the checks look at.
#[derive(Debug, Clone, Default)]
pub struct Input<'a> {
    pub from: Option<&'a Addr>,
    pub reply_to: &'a [Addr],
    pub date: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
    /// Sanitized HTML of the message (ammonia's normalized serialization).
    pub html: Option<&'a str>,
    /// IMAP keywords of the message.
    pub keywords: &'a [String],
    /// Addresses of the user's contacts whose name equals the sender name.
    pub contact_addresses: &'a [String],
}

pub fn check(i: &Input) -> Vec<Finding> {
    let mut out = Vec::new();
    if let Some(from) = i.from {
        let name = from.name.trim();
        if !name.is_empty() && !i.contact_addresses.is_empty() && !i.contact_addresses.iter().any(|a| a.eq_ignore_ascii_case(&from.address)) {
            out.push(Finding {
                kind: CheckKind::ContactMismatch,
                message: format!(
                    "The sender calls themselves “{name}”, like one of your contacts, but writes from {} instead of {}.",
                    from.address,
                    i.contact_addresses.join(", ")
                ),
                links: vec![],
            });
        }
        if let Some(shown) = address_in(name) {
            if !shown.eq_ignore_ascii_case(&from.address) {
                out.push(Finding {
                    kind: CheckKind::CustomAddress,
                    message: format!("The sender name shows the address {shown}, but the message comes from {}.", from.address),
                    links: vec![],
                });
            }
        }
        if let Some(from_domain) = domain_of(&from.address) {
            let other: Vec<&str> = i
                .reply_to
                .iter()
                .filter(|r| domain_of(&r.address).is_some_and(|d| d != from_domain))
                .map(|r| r.address.as_str())
                .collect();
            if !other.is_empty() {
                out.push(Finding {
                    kind: CheckKind::ReplyTo,
                    message: format!("Replies go to {}, in another domain than the sender ({from_domain}).", other.join(", ")),
                    links: vec![],
                });
            }
        }
    }
    if let Some(d) = i.date {
        if d > i.now + FUTURE_TOLERANCE {
            out.push(Finding {
                kind: CheckKind::FutureDate,
                message: format!("The message is dated in the future ({}).", d.format("%-d %b %Y %H:%M UTC")),
                links: vec![],
            });
        }
    }
    if let Some(html) = i.html {
        let links = misleading_links(html);
        if !links.is_empty() {
            out.push(Finding {
                kind: CheckKind::MisleadingLink,
                message: if links.len() == 1 {
                    "A link shows one address but opens another.".into()
                } else {
                    format!("{} links show one address but open another.", links.len())
                },
                links,
            });
        }
    }
    if i.keywords.iter().any(|k| k.eq_ignore_ascii_case("$phishing")) {
        out.push(Finding {
            kind: CheckKind::ServerFlag,
            message: "Your mail server marked this message as a possible phishing attempt.".into(),
            links: vec![],
        });
    }
    out
}

/// An email address written inside a display name ("support@bank.example" or
/// "Bank <support@bank.example>" typed into the name).
fn address_in(name: &str) -> Option<String> {
    name.split(|c: char| c.is_whitespace() || "<>()[]\"',;:".contains(c))
        .map(|t| t.trim_matches('.'))
        .find(|t| {
            t.split_once('@')
                .is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.'))
        })
        .map(str::to_lowercase)
}

/// The host a piece of link text shows, when it looks like an address at all
/// (`https://bank.example/login`, `www.bank.example`, `bank.example/x`).
fn shown_host(text: &str) -> Option<String> {
    let t = text.trim().trim_matches(|c: char| "<>()[]\"'«»“”‘’.,;:!?".contains(c)).trim();
    if t.is_empty() || t.contains(char::is_whitespace) || t.contains('@') {
        return None;
    }
    let lower = t.to_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        lower
    } else if lower.contains("://") {
        return None;
    } else {
        // A bare host must end in an alphabetic top-level domain to count as a link.
        let host = lower.split(['/', '?', '#']).next().unwrap_or("");
        let host = host.split(':').next().unwrap_or("");
        let tld = host.rsplit('.').next().unwrap_or("");
        if !host.contains('.') || tld.chars().count() < 2 || !tld.chars().all(char::is_alphabetic) {
            return None;
        }
        format!("http://{lower}")
    };
    host_of(&with_scheme)
}

/// The host of a URL in ASCII (IDNs as punycode), without a leading `www.`.
fn host_of(url: &str) -> Option<String> {
    let u = reqwest::Url::parse(url).ok()?;
    let h = u.host_str()?.trim_end_matches('.').to_ascii_lowercase();
    Some(h.strip_prefix("www.").map(str::to_string).unwrap_or(h))
}

/// Links whose visible text looks like an address on another host than the target.
pub fn misleading_links(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (href, text) in anchors(html) {
        let lower = href.trim().to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            continue;
        }
        let (Some(shown), Some(real)) = (shown_host(&text), host_of(href.trim())) else {
            continue;
        };
        if shown != real {
            out.push((text.trim().to_string(), href.trim().to_string()));
            if out.len() >= MAX_LINKS {
                break;
            }
        }
    }
    out
}

/// (href, text) of every `<a href>` in sanitized HTML. Ammonia serializes attributes with
/// double quotes and escapes `"`, `<` and `&`, and never nests anchors, so a scanner is
/// exact here.
pub fn anchors(html: &str) -> Vec<(String, String)> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = lower[i..].find("<a") {
        let start = i + p;
        let after = lower.as_bytes().get(start + 2).copied();
        if !matches!(after, Some(b' ') | Some(b'>') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'/')) {
            i = start + 2;
            continue;
        }
        let Some(tag_end) = lower[start..].find('>').map(|e| start + e) else { break };
        let tag = &html[start..tag_end];
        let href = attr(tag, "href");
        let close = lower[tag_end..].find("</a").map(|e| tag_end + e).unwrap_or(html.len());
        let inner = &html[tag_end + 1..close];
        if let Some(href) = href {
            out.push((href, decode(&strip_tags(inner))));
        }
        i = close.max(tag_end + 1);
    }
    out
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        let before = lower.as_bytes().get(at.wrapping_sub(1)).copied();
        let rest = lower[at + name.len()..].trim_start();
        if matches!(before, Some(b' ') | Some(b'\t') | Some(b'\n')) && rest.starts_with('=') {
            let vstart = tag.len() - rest.len() + 1;
            let v = tag[vstart..].trim_start();
            return Some(match v.chars().next() {
                Some(q @ ('"' | '\'')) => decode(v[1..].split(q).next().unwrap_or("")),
                _ => decode(v.split(char::is_whitespace).next().unwrap_or("")),
            });
        }
        from = at + name.len();
    }
    None
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::parse::sanitize_html;

    fn addr(name: &str, address: &str) -> Addr {
        Addr {
            name: name.into(),
            address: address.into(),
        }
    }

    fn kinds(f: &[Finding]) -> Vec<CheckKind> {
        f.iter().map(|x| x.kind.clone()).collect()
    }

    #[test]
    fn clean_message_has_no_findings() {
        let from = addr("Ada Lovelace", "ada@example.org");
        let reply = [addr("", "ada@example.org"), addr("", "list@example.org")];
        let contacts = ["ADA@example.org".to_string()];
        let html = sanitize_html(
            r#"<a href="https://example.org/x">https://www.example.org/x</a> <a href="https://t.example/">Read more</a> <a href="mailto:a@b.c">a@b.c</a>"#,
            false,
            |c| c.to_string(),
        )
        .html;
        let now = Utc::now();
        let f = check(&Input {
            from: Some(&from),
            reply_to: &reply,
            date: Some(now + Duration::minutes(2)),
            now,
            html: Some(&html),
            keywords: &["$Junk".into(), "$label1".into()],
            contact_addresses: &contacts,
        });
        assert!(f.is_empty(), "{f:?}");
    }

    #[test]
    fn contact_name_with_another_address() {
        let from = addr("Ada Lovelace", "ada.lovelace@freemail.example");
        let contacts = ["ada@example.org".to_string()];
        let f = check(&Input {
            from: Some(&from),
            contact_addresses: &contacts,
            now: Utc::now(),
            ..Default::default()
        });
        assert_eq!(kinds(&f), vec![CheckKind::ContactMismatch]);
        assert!(f[0].message.contains("ada@example.org") && f[0].message.contains("ada.lovelace@freemail.example"));
    }

    #[test]
    fn display_name_that_is_another_address() {
        for name in ["support@bank.example", "Bank Support <support@bank.example>", "\"support@bank.example\""] {
            let from = addr(name, "x@attacker.example");
            let f = check(&Input {
                from: Some(&from),
                now: Utc::now(),
                ..Default::default()
            });
            assert_eq!(kinds(&f), vec![CheckKind::CustomAddress], "{name}");
            assert!(f[0].message.contains("support@bank.example") && f[0].message.contains("x@attacker.example"));
        }
        // The same address in the name is fine.
        let from = addr("ada@example.org", "Ada@Example.org");
        assert!(
            check(&Input {
                from: Some(&from),
                now: Utc::now(),
                ..Default::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn future_date_and_reply_to_and_server_flag() {
        let now = Utc::now();
        let from = addr("Bank", "noreply@bank.example");
        let reply = [addr("", "collect@attacker.example")];
        let f = check(&Input {
            from: Some(&from),
            reply_to: &reply,
            date: Some(now + Duration::days(3)),
            now,
            keywords: &["$Phishing".into()],
            ..Default::default()
        });
        assert_eq!(kinds(&f), vec![CheckKind::ReplyTo, CheckKind::FutureDate, CheckKind::ServerFlag]);
        assert!(f[0].message.contains("collect@attacker.example"));
    }

    #[test]
    fn misleading_links_are_found() {
        let html = sanitize_html(
            r#"<p>Log in at <a href="https://attacker.example/login">https://bank.example/login</a>,
            <a href='http://xn--pple-43d.example/'>www.apple.example</a>,
            <a href="https://evil.example/"><b>bank.example</b>/account</a>,
            <a href="https://bank.example/ok">bank.example</a>,
            <a href="https://www.bank.example/ok">https://bank.example</a>,
            <a href="https://evil.example/">Click here</a>,
            <a href="https://evil.example/">(bank.example)</a>,
            <a href="https://evil.example/">1.5</a>,
            <a href="javascript:alert(1)">bank.example</a></p>"#,
            false,
            |c| c.to_string(),
        )
        .html;
        let links = misleading_links(&html);
        let shown: Vec<&str> = links.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(
            shown,
            vec!["https://bank.example/login", "www.apple.example", "bank.example/account", "(bank.example)"],
            "{links:?}"
        );
        assert_eq!(links[0].1, "https://attacker.example/login");
        let f = check(&Input {
            html: Some(&html),
            now: Utc::now(),
            ..Default::default()
        });
        assert_eq!(kinds(&f), vec![CheckKind::MisleadingLink]);
        assert!(f[0].message.starts_with("4 links"));
    }

    #[test]
    fn anchor_scanner_handles_entities_and_attributes() {
        let a = anchors(r#"<a title="x" href="https://a.example/?q=1&amp;r=2" rel="noopener">A &amp; <i>B</i></a><abbr>x</abbr><a>no href</a>"#);
        assert_eq!(a, vec![("https://a.example/?q=1&r=2".to_string(), "A & B".to_string())]);
    }
}
