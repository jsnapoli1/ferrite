//! `mailto:` URIs (RFC 6068), for the browser's mail link handler: recipients from the path
//! and `to=`, plus `cc`, `bcc`, `subject` and `body` (other header fields are ignored, as the
//! RFC allows). Everything is percent-decoded as UTF-8; `+` is a plus, not a space.

use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Mailto {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
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

/// Splits an address list (`a@b.c,d@e.f`) after decoding; keeps "Name <a@b.c>" forms whole.
fn addresses(decoded: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut angle = false;
    for c in decoded.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '<' if !quoted => {
                angle = true;
                cur.push(c);
            }
            '>' if !quoted => {
                angle = false;
                cur.push(c);
            }
            ',' | ';' if !quoted && !angle => {
                let t = cur.trim();
                if !t.is_empty() {
                    out.push(t.to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let t = cur.trim();
    if !t.is_empty() {
        out.push(t.to_string());
    }
    out
}

/// Parses a `mailto:` URI (the scheme is optional; it is matched case-insensitively).
pub fn parse(uri: &str) -> Mailto {
    let uri = uri.trim();
    let rest = match uri.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("mailto:") => &uri[7..],
        _ => uri,
    };
    // A fragment is not part of a mailto URI's meaning.
    let rest = rest.split('#').next().unwrap_or("");
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let mut m = Mailto {
        to: addresses(&percent_decode(path)),
        ..Default::default()
    };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(v);
        match percent_decode(k).to_ascii_lowercase().as_str() {
            "to" => m.to.extend(addresses(&value)),
            "cc" => m.cc.extend(addresses(&value)),
            "bcc" => m.bcc.extend(addresses(&value)),
            "subject" if m.subject.is_empty() => m.subject = value.replace(['\r', '\n'], " "),
            "body" if m.body.is_empty() => m.body = value.replace("\r\n", "\n"),
            _ => {}
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6068_examples() {
        assert_eq!(parse("mailto:chris@example.com").to, vec!["chris@example.com"]);
        let m = parse("mailto:infobot@example.com?subject=current-issue");
        assert_eq!(m.subject, "current-issue");
        let m = parse("mailto:infobot@example.com?body=send%20current-issue%0D%0Asend%20index");
        assert_eq!(m.body, "send current-issue\nsend index");
        let m = parse("mailto:list@example.org?In-Reply-To=%3C3469A91.D10AF4C@example.com%3E");
        assert_eq!(m.to, vec!["list@example.org"]);
        assert_eq!(m.subject, "");
        let m = parse("mailto:joe@example.com?cc=bob@example.com&body=hello");
        assert_eq!((m.cc, m.body.as_str()), (vec!["bob@example.com".to_string()], "hello"));
        let m = parse("mailto:?to=addr1@an.example,addr2@an.example");
        assert_eq!(m.to, vec!["addr1@an.example", "addr2@an.example"]);
        let m = parse("mailto:addr1@an.example?to=addr2@an.example");
        assert_eq!(m.to, vec!["addr1@an.example", "addr2@an.example"]);
        // Percent-encoded UTF-8 in the subject; "+" stays a plus.
        let m = parse("MAILTO:user@example.org?subject=%E3%81%BB%E3%81%92&body=1+1");
        assert_eq!(m.subject, "ほげ");
        assert_eq!(m.body, "1+1");
        let m = parse("mailto:%22not%40me%22@example.org");
        assert_eq!(m.to, vec!["\"not@me\"@example.org"]);
    }

    #[test]
    fn robust_to_odd_input() {
        let m = parse("mailto:a@b.c?bcc=x@y.z;w@v.u&Subject=Hi%0Athere&subject=ignored&body=%zz#frag");
        assert_eq!(m.bcc, vec!["x@y.z", "w@v.u"]);
        assert_eq!(m.subject, "Hi there", "header fields can't carry line breaks");
        assert_eq!(m.body, "%zz", "invalid escapes are kept");
        assert_eq!(parse("").to, Vec::<String>::new());
        assert_eq!(
            parse("mailto:Ada%20Lovelace%20%3Cada@example.org%3E,%22Doe,%20J%22%20%3Cj@d.e%3E").to,
            vec!["Ada Lovelace <ada@example.org>", "\"Doe, J\" <j@d.e>"]
        );
        assert_eq!(parse("mailto:a@b.c?body=50%").body, "50%");
    }
}
