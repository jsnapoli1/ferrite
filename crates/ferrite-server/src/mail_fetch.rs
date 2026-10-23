//! Fetching from the network on behalf of mail: remote images shown through the image proxy,
//! and sender favicons (and, when the admin allows it, Gravatar) for avatars.
//!
//! The same guard as Web Push (`push::is_public`) applies to every hop: only `http`/`https`,
//! no credentials in URLs, the host must resolve to public addresses only (the resolver drops
//! private, loopback, link-local and other internal answers, so a name cannot be re-pointed
//! between the check and the connection), redirects are followed by hand (at most
//! [`MAX_REDIRECTS`], each checked again). Requests carry no cookies and no Referer, time out
//! after [`TIMEOUT`], and bodies stop at a size limit; images must really be images (sniffed,
//! whatever the server claims): raster formats, or SVG for the proxy, which serves it with a
//! sandboxing CSP so it can never run as a document on Ferrite's origin.
//!
//! `[mail_security] proxy_allow_private` (tests only) lifts the address check.

use crate::push::is_public;
use ferrite_core::mail::security::sniff_image;
use ferrite_core::{Error, Result};
use reqwest::{StatusCode, Url};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const MAX_REDIRECTS: usize = 3;
/// A proxied image at most.
pub const MAX_IMAGE: usize = 8 * 1024 * 1024;
/// A favicon at most.
pub const MAX_ICON: usize = 200 * 1024;
/// A home page read for its icon links at most.
const MAX_PAGE: usize = 512 * 1024;

/// DNS that only returns public addresses.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let found = tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host.as_str(), 0)))
                .await
                .map_err(|_| format!("{host}: DNS timed out"))??;
            let addrs: Vec<SocketAddr> = found.filter(|a| is_public(a.ip())).collect();
            if addrs.is_empty() {
                return Err(format!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

fn client(allow_private: bool) -> &'static reqwest::Client {
    static GUARDED: OnceLock<reqwest::Client> = OnceLock::new();
    static OPEN: OnceLock<reqwest::Client> = OnceLock::new();
    let build = |guarded: bool| {
        let mut b = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .referer(false)
            .user_agent(concat!("Ferrite/", env!("CARGO_PKG_VERSION"), " (mail image proxy)"));
        if guarded {
            b = b.dns_resolver(Arc::new(PublicOnlyResolver));
        }
        b.build().unwrap_or_default()
    };
    if allow_private {
        OPEN.get_or_init(|| build(false))
    } else {
        GUARDED.get_or_init(|| build(true))
    }
}

/// Checks one URL before it is requested.
pub fn check_url(raw: &str, allow_private: bool) -> Result<Url> {
    if raw.len() > ferrite_core::mail::security::PROXY_MAX_URL {
        return Err(Error::bad("the address is too long"));
    }
    let url = Url::parse(raw.trim()).map_err(|_| Error::bad("not a URL"))?;
    if url.scheme() != "https" && url.scheme() != "http" {
        return Err(Error::bad("only http and https can be fetched"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::bad("addresses with credentials are not fetched"));
    }
    let host = url.host_str().ok_or_else(|| Error::bad("the address has no host"))?;
    if !allow_private {
        // IP literals bypass the resolver, so they are checked here.
        if let Ok(ip) = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
            if !is_public(ip) {
                return Err(Error::Forbidden);
            }
        }
        let lower = host.to_ascii_lowercase();
        if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") || lower.ends_with(".internal") || !lower.contains('.') {
            return Err(Error::Forbidden);
        }
    }
    Ok(url)
}

/// What a fetch returned.
#[derive(Debug, Clone)]
pub struct Fetched {
    pub url: Url,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// GETs `raw` following checked redirects, reading at most `max` bytes.
pub async fn get(raw: &str, accept: &str, max: usize, allow_private: bool) -> Result<Fetched> {
    let mut url = check_url(raw, allow_private)?;
    let http = client(allow_private);
    for _ in 0..=MAX_REDIRECTS {
        let resp = http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, accept)
            .send()
            .await
            .map_err(|e| Error::Unavailable(format!("fetching failed: {}", without_url(&e))))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::Unavailable("a redirect without a location".into()))?;
            let next = url.join(loc).map_err(|_| Error::Unavailable("a redirect to an invalid address".into()))?;
            url = check_url(next.as_str(), allow_private)?;
            continue;
        }
        if status != StatusCode::OK {
            return Err(Error::Unavailable(format!("the server answered {status}")));
        }
        if resp.content_length().is_some_and(|l| l as usize > max) {
            return Err(Error::bad("too large"));
        }
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase())
            .unwrap_or_default();
        let mut resp = resp;
        let mut data = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Unavailable(format!("reading failed: {}", without_url(&e))))? {
            if data.len() + chunk.len() > max {
                return Err(Error::bad("too large"));
            }
            data.extend_from_slice(&chunk);
        }
        return Ok(Fetched { url, content_type, data });
    }
    Err(Error::Unavailable("too many redirects".into()))
}

fn without_url(e: &reqwest::Error) -> String {
    let mut s = e.to_string();
    if let Some(u) = e.url() {
        s = s.replace(u.as_str(), "the address");
    }
    s
}

/// A remote raster image: (sniffed type, bytes).
pub async fn image(url: &str, max: usize, allow_private: bool) -> Result<(&'static str, Vec<u8>)> {
    let f = get(url, "image/avif,image/webp,image/png,image/jpeg,image/gif,image/*;q=0.8", max, allow_private).await?;
    let kind = sniff_image(&f.data).ok_or_else(|| Error::bad("not an image"))?;
    Ok((kind, f.data))
}

/// A remote image for the proxy: raster formats, or SVG (which the proxy serves sandboxed).
pub async fn proxied_image(url: &str, max: usize, allow_private: bool) -> Result<(&'static str, Vec<u8>)> {
    let f = get(url, "image/avif,image/webp,image/png,image/jpeg,image/gif,image/svg+xml,image/*;q=0.8", max, allow_private).await?;
    if let Some(kind) = sniff_image(&f.data) {
        return Ok((kind, f.data));
    }
    if looks_like_svg(&f.data) {
        return Ok(("image/svg+xml", f.data));
    }
    Err(Error::bad("not an image"))
}

/// SVG: UTF-8 whose first element (after an XML declaration, comments or a doctype) is `<svg`.
pub fn looks_like_svg(data: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(data) else { return false };
    let mut rest = text.trim_start_matches('\u{feff}').trim_start();
    loop {
        if let Some(r) = rest.strip_prefix("<?") {
            let Some(e) = r.find("?>") else { return false };
            rest = r[e + 2..].trim_start();
        } else if let Some(r) = rest.strip_prefix("<!--") {
            let Some(e) = r.find("-->") else { return false };
            rest = r[e + 3..].trim_start();
        } else if rest.len() >= 9 && rest[..9].eq_ignore_ascii_case("<!doctype") {
            let Some(e) = rest.find('>') else { return false };
            rest = rest[e + 1..].trim_start();
        } else {
            return rest.len() >= 5 && rest[..4].eq_ignore_ascii_case("<svg") && (rest.as_bytes()[4].is_ascii_whitespace() || rest.as_bytes()[4] == b'>');
        }
    }
}

/// Icon links of a home page, best first: apple-touch-icon, then icons by declared size.
pub fn icon_links(html: &str, base: &Url) -> Vec<Url> {
    let lower = html.to_ascii_lowercase();
    let mut found: Vec<(i64, Url)> = Vec::new();
    let mut i = 0;
    while let Some(p) = lower[i..].find("<link") {
        let start = i + p;
        let Some(end) = lower[start..].find('>').map(|e| start + e) else { break };
        let tag = &html[start..end];
        let tag_l = &lower[start..end];
        i = end;
        let rel = attr(tag, tag_l, "rel").unwrap_or_default().to_ascii_lowercase();
        if !rel.split_whitespace().any(|r| r == "icon" || r == "apple-touch-icon" || r == "apple-touch-icon-precomposed") {
            continue;
        }
        let Some(href) = attr(tag, tag_l, "href") else { continue };
        let ty = attr(tag, tag_l, "type").unwrap_or_default().to_ascii_lowercase();
        if ty.contains("svg") || href.to_ascii_lowercase().ends_with(".svg") {
            continue;
        }
        let Ok(u) = base.join(href.trim()) else { continue };
        let size: i64 = attr(tag, tag_l, "sizes")
            .and_then(|s| s.split(['x', 'X']).next().and_then(|n| n.trim().parse().ok()))
            .unwrap_or(16);
        let score = if rel.contains("apple-touch-icon") { 1000 } else { size.min(512) };
        found.push((score, u));
    }
    found.sort_by_key(|(s, _)| -s);
    found.into_iter().map(|(_, u)| u).take(3).collect()
}

fn attr(tag: &str, lower: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(p) = lower[from..].find(name) {
        let at = from + p;
        from = at + name.len();
        if !lower[..at].ends_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let rest = lower[at + name.len()..].trim_start();
        let Some(after_eq) = rest.strip_prefix('=') else { continue };
        let offset = tag.len() - after_eq.len();
        let v = tag[offset..].trim_start();
        let value = match v.chars().next() {
            Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or(""),
            _ => v.split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or(""),
        };
        return Some(value.replace("&amp;", "&"));
    }
    None
}

/// Domains to try for `domain`'s icon: itself, then its registrable part
/// (`news.shop.example.org` → `example.org`, `mail.example.co.uk` → `example.co.uk`).
pub fn icon_domains(domain: &str) -> Vec<String> {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = d.split('.').collect();
    let mut out = vec![d.clone()];
    if labels.len() > 2 {
        let tld = labels[labels.len() - 1];
        let sld = labels[labels.len() - 2];
        let keep = if tld.len() == 2 && sld.len() <= 3 && labels.len() > 3 { 3 } else { 2 };
        let reg = labels[labels.len() - keep..].join(".");
        if reg != d {
            out.push(reg);
        }
    }
    out
}

/// The favicon of a mail domain: icon links of its home page, else /favicon.ico.
pub async fn favicon(domain: &str, allow_private: bool) -> Option<(&'static str, Vec<u8>)> {
    for d in icon_domains(domain) {
        let home = format!("https://{d}/");
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(page) = get(&home, "text/html", MAX_PAGE, allow_private).await {
            if page.content_type.contains("html") {
                let text = String::from_utf8_lossy(&page.data);
                candidates.extend(icon_links(&text, &page.url).into_iter().map(|u| u.to_string()));
            }
            if let Ok(fav) = page.url.join("/favicon.ico") {
                candidates.push(fav.to_string());
            }
        } else {
            candidates.push(format!("https://{d}/favicon.ico"));
        }
        for c in candidates {
            if let Ok(found) = image(&c, MAX_ICON, allow_private).await {
                return Some(found);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_checks() {
        for bad in [
            "file:///etc/passwd",
            "ftp://example.org/x.png",
            "gopher://example.org/",
            "http://user:pw@example.org/x.png",
            "http://127.0.0.1/x.png",
            "http://[::1]/x.png",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://[fd00::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost/x",
            "http://printer.local/x",
            "http://intranet/x",
            "http://metadata.google.internal/",
            "http://0x7f000001/",
            "http://2130706433/",
            "not a url",
        ] {
            assert!(check_url(bad, false).is_err(), "{bad} must be refused");
        }
        for ok in ["https://example.org/a.png", "http://8.8.8.8/x.gif", "https://[2606:4700::1111]/x"] {
            assert!(check_url(ok, false).is_ok(), "{ok} is fine");
        }
        // With the test override, local addresses are allowed (never other schemes).
        assert!(check_url("http://127.0.0.1:8080/x.png", true).is_ok());
        assert!(check_url("file:///etc/passwd", true).is_err());
        assert!(check_url(&format!("https://example.org/{}", "a".repeat(5000)), false).is_err());
    }

    #[tokio::test]
    async fn names_that_resolve_to_private_addresses_are_refused() {
        // localhost.localdomain style names resolve to loopback: the resolver drops them.
        let e = get("http://localhost.localtest.me/x.png", "image/*", 100, false).await.unwrap_err();
        assert!(!matches!(e, Error::NotFound), "{e}");
        let r = PublicOnlyResolver;
        let name: reqwest::dns::Name = "localhost".parse().unwrap();
        assert!(reqwest::dns::Resolve::resolve(&r, name).await.is_err());
    }

    #[test]
    fn svg_detection() {
        assert!(looks_like_svg(b"<svg xmlns=\"http://www.w3.org/2000/svg\"><rect/></svg>"));
        assert!(looks_like_svg(b"<?xml version=\"1.0\"?>\n<!-- c -->\n<!DOCTYPE svg>\n<svg>\n</svg>"));
        assert!(!looks_like_svg(b"<html><svg></svg></html>"));
        assert!(!looks_like_svg(b"<svgx>"));
        assert!(!looks_like_svg(b"\xff\xfe<svg>"));
    }

    #[test]
    fn icon_links_are_ranked() {
        let base = Url::parse("https://example.org/news/").unwrap();
        let html = r#"<html><head><link rel="stylesheet" href="/s.css"><LINK REL="shortcut icon" HREF="/favicon.ico">
<link rel="icon" type="image/png" sizes="32x32" href="icon-32.png"><link rel="icon" href="/logo.svg" type="image/svg+xml">
<link rel=apple-touch-icon href=//cdn.example.org/touch.png></head>"#;
        let links: Vec<String> = icon_links(html, &base).into_iter().map(|u| u.to_string()).collect();
        assert_eq!(links, vec!["https://cdn.example.org/touch.png", "https://example.org/news/icon-32.png", "https://example.org/favicon.ico"]);
    }

    #[test]
    fn icon_domain_candidates() {
        assert_eq!(icon_domains("example.org"), vec!["example.org"]);
        assert_eq!(icon_domains("News.Shop.Example.org."), vec!["news.shop.example.org", "example.org"]);
        assert_eq!(icon_domains("mail.example.co.uk"), vec!["mail.example.co.uk", "example.co.uk"]);
    }
}
