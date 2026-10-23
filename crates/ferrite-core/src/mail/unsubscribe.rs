//! Unsubscribing from mailing lists: `List-Unsubscribe` (RFC 2369) with one-click
//! `List-Unsubscribe-Post` (RFC 8058).
//!
//! The reader offers, in order of preference: a one-click unsubscribe (an HTTPS POST of
//! `List-Unsubscribe=One-Click`, done by the server through a fetch that only reaches public
//! addresses, see the API), a `mailto:` unsubscribe sent from the user's account, or a link
//! to open.

use crate::mailer::OutgoingMail;
use crate::{Core, Error, Result};
use ammonia::url::Url;
use mail_parser::MessageParser;
use serde::Serialize;

/// A mail to send to unsubscribe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Mailto {
    pub to: String,
    pub subject: String,
    pub body: String,
}

/// How a message can be unsubscribed from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Unsubscribe {
    /// HTTPS URL that accepts the RFC 8058 one-click POST.
    pub one_click: Option<String>,
    /// HTTP(S) URL to open in a browser.
    pub url: Option<String>,
    pub mailto: Option<Mailto>,
    /// The list's name (`List-Id`), if any.
    pub list: Option<String>,
}

fn parse_mailto(u: &Url) -> Option<Mailto> {
    let to = percent_decode(u.path());
    if !to.contains('@') || to.contains(['\r', '\n', ',', ';']) {
        return None;
    }
    let mut m = Mailto {
        to,
        subject: "unsubscribe".into(),
        body: String::new(),
    };
    for (k, v) in u.query_pairs() {
        match k.to_ascii_lowercase().as_str() {
            "subject" => m.subject = v.replace(['\r', '\n'], " "),
            "body" => m.body = v.into_owned(),
            _ => {}
        }
    }
    Some(m)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Tests only (`FERRITE_MAIL_ALLOW_PRIVATE_FETCH=1`): one-click unsubscribe may use plain
/// http and reach local servers.
pub fn allow_private_fetch() -> bool {
    std::env::var("FERRITE_MAIL_ALLOW_PRIVATE_FETCH").is_ok_and(|v| v == "1")
}

/// `Weekly news <news.example.org>` → "Weekly news"; `<news.example.org>` → its id.
fn list_name(list_id: &str) -> Option<String> {
    let l = list_id.trim();
    let name = match l.find('<') {
        Some(0) => l.trim_matches(['<', '>']).trim(),
        Some(i) => l[..i].trim().trim_matches('"').trim(),
        None => l,
    };
    (!name.is_empty()).then(|| name.to_string())
}

/// Parses the header values. `post` is `List-Unsubscribe-Post`.
pub fn parse(list_unsubscribe: &str, post: Option<&str>, list_id: Option<&str>) -> Option<Unsubscribe> {
    let http_ok = allow_private_fetch();
    let one_click_allowed = post.is_some_and(|p| p.split_whitespace().collect::<String>().eq_ignore_ascii_case("List-Unsubscribe=One-Click"));
    let mut out = Unsubscribe {
        one_click: None,
        url: None,
        mailto: None,
        list: list_id.and_then(list_name),
    };
    // Each URI is in angle brackets; anything outside them is ignored (RFC 2369 §2).
    let mut rest = list_unsubscribe;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start + 1..].find('>') else { break };
        let raw: String = rest[start + 1..start + 1 + len].chars().filter(|c| !c.is_whitespace()).collect();
        rest = &rest[start + 1 + len + 1..];
        let Ok(u) = Url::parse(&raw) else { continue };
        match u.scheme() {
            "https" if out.one_click.is_none() && one_click_allowed => out.one_click = Some(u.to_string()),
            "http" if out.one_click.is_none() && one_click_allowed && http_ok => out.one_click = Some(u.to_string()),
            "https" | "http" if out.url.is_none() => out.url = Some(u.to_string()),
            "mailto" if out.mailto.is_none() => out.mailto = parse_mailto(&u),
            _ => {}
        }
    }
    if out.one_click.is_some() && out.url.is_none() {
        out.url = out.one_click.clone();
    }
    (out.one_click.is_some() || out.url.is_some() || out.mailto.is_some()).then_some(out)
}

/// The unsubscribe options of a raw message.
pub fn from_raw(raw: &[u8]) -> Option<Unsubscribe> {
    let m = MessageParser::default().parse(raw)?;
    let header = |name: &str| m.header_raw(name).map(|v| v.trim().to_string());
    parse(
        &header("List-Unsubscribe")?,
        header("List-Unsubscribe-Post").as_deref(),
        header("List-Id").as_deref(),
    )
}

impl Core {
    /// How a cached message can be unsubscribed from (None when it is not from a list).
    pub async fn mail_unsubscribe_info(&self, user_id: &str, message_id: &str) -> Result<Option<Unsubscribe>> {
        let (_, raw) = self.mail_raw(user_id, message_id).await?;
        Ok(from_raw(&raw))
    }

    /// Unsubscribes by sending the list's `mailto:` request from the user's account.
    pub async fn mail_unsubscribe_by_mail(&self, user_id: &str, message_id: &str) -> Result<String> {
        let (a, _) = self.mail_message_access(user_id, message_id, super::delegation::MailRight::Send).await?;
        let info = self
            .mail_unsubscribe_info(user_id, message_id)
            .await?
            .and_then(|u| u.mailto)
            .ok_or_else(|| Error::bad("this message has no unsubscribe address"))?;
        self.send_from_account(
            &a,
            OutgoingMail {
                to: vec![info.to.clone()],
                subject: info.subject,
                text: if info.body.is_empty() { "unsubscribe".into() } else { info.body },
                ..Default::default()
            },
        )
        .await?;
        Ok(info.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_headers() {
        let u = parse(
            "<mailto:unsub-42@lists.example.org?subject=Unsubscribe%20me&body=please>,\r\n <https://lists.example.org/unsub?u=42&t=ab%20c>",
            Some("List-Unsubscribe=One-Click"),
            Some("Weekly news <news.lists.example.org>"),
        )
        .unwrap();
        assert_eq!(u.one_click.as_deref(), Some("https://lists.example.org/unsub?u=42&t=ab%20c"));
        assert_eq!(u.url, u.one_click);
        assert_eq!(
            u.mailto,
            Some(Mailto {
                to: "unsub-42@lists.example.org".into(),
                subject: "Unsubscribe me".into(),
                body: "please".into()
            })
        );
        assert_eq!(u.list.as_deref(), Some("Weekly news"));
        // Without List-Unsubscribe-Post the link is only opened, never posted.
        let u = parse("<https://x.example/u>", None, None).unwrap();
        assert!(u.one_click.is_none() && u.url.as_deref() == Some("https://x.example/u"));
        // Plain http is never one-click.
        let u = parse("<http://x.example/u>", Some("List-Unsubscribe=One-Click"), None).unwrap();
        assert!(u.one_click.is_none() && u.url.is_some());
        // Garbage and other schemes are ignored.
        assert!(parse("no brackets here", None, None).is_none());
        assert!(parse("<javascript:alert(1)>, <ftp://x>", None, None).is_none());
        assert!(parse("<mailto:not-an-address>", None, None).is_none());
    }

    #[test]
    fn reads_raw_messages() {
        let raw = b"From: News <news@example.org>\r\nList-Id: <news.example.org>\r\nList-Unsubscribe: <https://example.org/u/1>, <mailto:u@example.org>\r\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\r\nSubject: x\r\n\r\nbody\r\n";
        let u = from_raw(raw).unwrap();
        assert_eq!(u.one_click.as_deref(), Some("https://example.org/u/1"));
        assert_eq!(u.mailto.unwrap().to, "u@example.org");
        assert_eq!(u.list.as_deref(), Some("news.example.org"));
        assert!(from_raw(b"From: a@b.c\r\nSubject: x\r\n\r\nhi\r\n").is_none());
    }
}
