//! DKIM verification of received mail (RFC 6376; rsa-sha256 and, per RFC 8463,
//! ed25519-sha256) with the `mail-auth` crate. Public keys come from DNS TXT records
//! (`<selector>._domainkey.<domain>`) through a resolver with short timeouts; answers are
//! cached in memory for their TTL and results are stored per message, so a message is
//! verified once (again later only after a temporary failure).
//!
//! The DNS side is injectable: `[mail_security] dns_server` (or `FERRITE_MAIL_DNS`) points
//! the resolver at one name server (host:port, UDP and TCP), and [`Dkim::add_static_record`]
//! pins a record in memory, which tests use.

use dashmap::DashMap;
use mail_auth::common::parse::TxtRecordParser;
use mail_auth::common::verify::DomainKey;
use mail_auth::hickory_resolver::config::{ConnectionConfig, NameServerConfig, ResolverConfig, ResolverOpts};
use mail_auth::{AuthenticatedMessage, DkimResult, MessageAuthenticator, Parameters, ResolverCache, Txt};
use serde::Serialize;
use std::borrow::Borrow;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per DNS query, and attempts per query.
const DNS_TIMEOUT: Duration = Duration::from_secs(4);
const DNS_ATTEMPTS: usize = 2;
/// Upper bound for verifying one message (all of its signatures).
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(12);
/// Cached DNS answers live at most this long, whatever their TTL.
const MAX_TTL: Duration = Duration::from_secs(3600);
/// Messages larger than this are not verified (`none`, with a reason).
pub const MAX_VERIFY_BYTES: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// A signature verified.
    Pass,
    /// Signatures exist but none verified (altered message, wrong key, expired, …).
    Fail,
    /// No signature.
    None,
    /// The key could not be fetched right now (DNS trouble): retried later.
    Temperror,
    /// A signature or key record is malformed or uses something unsupported.
    Permerror,
    /// A signature was not applicable (e.g. unsupported algorithm, revoked key).
    Neutral,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::None => "none",
            Status::Temperror => "temperror",
            Status::Permerror => "permerror",
            Status::Neutral => "neutral",
        }
    }

    pub fn parse(s: &str) -> Status {
        match s {
            "pass" => Status::Pass,
            "fail" => Status::Fail,
            "temperror" => Status::Temperror,
            "permerror" => Status::Permerror,
            "neutral" => Status::Neutral,
            _ => Status::None,
        }
    }
}

/// The verdict for one message: the best signature result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub status: Status,
    /// Signing domain (`d=`) of the signature the status is about.
    pub domain: Option<String>,
    pub selector: Option<String>,
    /// Why it failed, in words.
    pub detail: Option<String>,
}

impl Verdict {
    pub fn none(detail: Option<String>) -> Verdict {
        Verdict {
            status: Status::None,
            domain: None,
            selector: None,
            detail,
        }
    }
}

/// TXT answers: pinned records (tests, never expire) and cached lookups (until their TTL).
#[derive(Default)]
pub struct TxtCache {
    pinned: DashMap<Box<str>, Txt>,
    cached: DashMap<Box<str>, (Txt, Instant)>,
}

impl ResolverCache<Box<str>, Txt> for TxtCache {
    fn get<Q>(&self, name: &Q) -> Option<Txt>
    where
        Box<str>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(v) = self.pinned.get(name) {
            return Some(v.clone());
        }
        let hit = self.cached.get(name).map(|e| (e.0.clone(), e.1));
        match hit {
            Some((v, until)) if until > Instant::now() => Some(v),
            Some(_) => {
                self.cached.remove(name);
                None
            }
            None => None,
        }
    }

    fn remove<Q>(&self, name: &Q) -> Option<Txt>
    where
        Box<str>: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.cached.remove(name).map(|(_, (v, _))| v)
    }

    fn insert(&self, key: Box<str>, value: Txt, valid_until: Instant) {
        // Temporary failures are not cached: the next message asks again.
        if matches!(&value, Txt::Error(mail_auth::Error::Dns(mail_auth::DnsError::Resolver(_)))) {
            return;
        }
        if self.cached.len() > 10_000 {
            let now = Instant::now();
            self.cached.retain(|_, (_, until)| *until > now);
        }
        let until = valid_until.min(Instant::now() + MAX_TTL);
        self.cached.insert(key, (value, until));
    }
}

/// The verifier: a lazily built resolver plus the TXT cache.
pub struct Dkim {
    server: Option<SocketAddr>,
    authenticator: tokio::sync::OnceCell<std::result::Result<Arc<MessageAuthenticator>, String>>,
    cache: TxtCache,
}

impl Dkim {
    /// `server`: one name server to ask instead of the system's (`/etc/resolv.conf`).
    pub fn new(server: Option<SocketAddr>) -> Dkim {
        Dkim {
            server,
            authenticator: tokio::sync::OnceCell::new(),
            cache: TxtCache::default(),
        }
    }

    /// Pins a TXT record (`name` like `sel._domainkey.example.org`) in memory, in place of DNS.
    pub fn add_static_record(&self, name: &str, record: &str) -> std::result::Result<(), String> {
        let key = DomainKey::parse(record.as_bytes()).map_err(|e| e.to_string())?;
        let fqdn = format!("{}.", name.trim_end_matches('.').to_ascii_lowercase());
        self.cache.pinned.insert(fqdn.into_boxed_str(), Txt::DomainKey(Arc::new(key)));
        Ok(())
    }

    async fn authenticator(&self) -> std::result::Result<Arc<MessageAuthenticator>, String> {
        self.authenticator
            .get_or_init(|| async {
                let mut opts = ResolverOpts::default();
                opts.timeout = DNS_TIMEOUT;
                opts.attempts = DNS_ATTEMPTS;
                // Lookups are cached here (with the TTL), not twice.
                opts.cache_size = 0;
                let built = match self.server {
                    Some(addr) => {
                        let mut udp = ConnectionConfig::udp();
                        udp.port = addr.port();
                        let mut tcp = ConnectionConfig::tcp();
                        tcp.port = addr.port();
                        let ns = NameServerConfig::new(addr.ip(), true, vec![udp, tcp]);
                        MessageAuthenticator::new(ResolverConfig::from_parts(None, vec![], vec![ns]), opts)
                    }
                    None => match mail_auth::hickory_resolver::system_conf::read_system_conf() {
                        Ok((config, mut sys)) => {
                            sys.timeout = DNS_TIMEOUT;
                            sys.attempts = DNS_ATTEMPTS;
                            sys.cache_size = 0;
                            MessageAuthenticator::new(config, sys)
                        }
                        Err(e) => return Err(format!("no DNS configuration: {e}")),
                    },
                };
                built.map(Arc::new).map_err(|e| format!("DNS resolver: {e}"))
            })
            .await
            .clone()
    }

    /// Verifies every DKIM signature of `raw`. `from_domain` (the From address's domain)
    /// prefers a passing signature of that domain over third-party ones.
    pub async fn verify(&self, raw: &[u8], from_domain: Option<&str>) -> Verdict {
        if raw.len() > MAX_VERIFY_BYTES {
            return Verdict::none(Some("the message is too large to verify".into()));
        }
        let Some(msg) = AuthenticatedMessage::parse(raw) else {
            return Verdict::none(Some("the message could not be parsed".into()));
        };
        if msg.dkim_headers.is_empty() {
            return Verdict::none(None);
        }
        let auth = match self.authenticator().await {
            Ok(a) => a,
            Err(e) => {
                return Verdict {
                    status: Status::Temperror,
                    domain: None,
                    selector: None,
                    detail: Some(e),
                };
            }
        };
        let outputs = match tokio::time::timeout(VERIFY_TIMEOUT, auth.verify_dkim(Parameters::new(&msg).with_txt_cache(&self.cache))).await {
            Ok(o) => o,
            Err(_) => {
                return Verdict {
                    status: Status::Temperror,
                    domain: None,
                    selector: None,
                    detail: Some("looking up the signing key timed out".into()),
                };
            }
        };
        summarize(
            outputs.iter().map(|o| {
                let sig = o.signature();
                (o.result().clone(), sig.map(|s| s.d.clone()), sig.map(|s| s.s.clone()))
            }),
            from_domain,
        )
    }
}

fn rank(s: &DkimResult) -> u8 {
    match s {
        DkimResult::Pass => 0,
        DkimResult::Fail(_) => 1,
        DkimResult::TempError(_) => 2,
        DkimResult::PermError(_) => 3,
        DkimResult::Neutral(_) => 4,
        DkimResult::None => 5,
    }
}

/// Picks the verdict from per-signature results: a pass (aligned with the From domain
/// first), else a failure, else temporary errors, permanent errors, neutral.
pub fn summarize(results: impl Iterator<Item = (DkimResult, Option<String>, Option<String>)>, from_domain: Option<&str>) -> Verdict {
    let mut all: Vec<(DkimResult, Option<String>, Option<String>)> = results.collect();
    if all.is_empty() {
        return Verdict::none(None);
    }
    let aligned = |d: &Option<String>| {
        from_domain.is_some_and(|f| d.as_deref().is_some_and(|d| f.eq_ignore_ascii_case(d) || f.to_ascii_lowercase().ends_with(&format!(".{}", d.to_ascii_lowercase()))))
    };
    all.sort_by_key(|(r, d, _)| (rank(r), !aligned(d)));
    let (r, domain, selector) = all.swap_remove(0);
    let (status, detail) = match r {
        DkimResult::Pass => (Status::Pass, None),
        DkimResult::Fail(e) => (Status::Fail, Some(e.to_string())),
        DkimResult::TempError(e) => (Status::Temperror, Some(e.to_string())),
        DkimResult::PermError(e) => (Status::Permerror, Some(e.to_string())),
        // A body that no longer matches its hash was changed after signing: that is a failure
        // for the reader, whatever the RFC calls it.
        DkimResult::Neutral(e @ mail_auth::Error::Dkim(mail_auth::dkim::DkimError::FailedBodyHashMatch)) => (Status::Fail, Some(e.to_string())),
        DkimResult::Neutral(e) => (Status::Neutral, Some(e.to_string())),
        DkimResult::None => (Status::None, None),
    };
    Verdict {
        status,
        domain,
        selector,
        detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8463 appendix A: the same message signed with Ed25519 and with RSA.
    const RFC8463: &str = "DKIM-Signature: v=1; a=ed25519-sha256; c=relaxed/relaxed;\r
 d=football.example.com; i=@football.example.com;\r
 q=dns/txt; s=brisbane; t=1528637909; h=from : to :\r
 subject : date : message-id : from : subject : date;\r
 bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r
 b=/gCrinpcQOoIfuHNQIbq4pgh9kyIK3AQUdt9OdqQehSwhEIug4D11Bus\r
 Fa3bT3FY5OsU7ZbnKELq+eXdp1Q1Dw==\r
DKIM-Signature: v=1; a=rsa-sha256; c=relaxed/relaxed;\r
 d=football.example.com; i=@football.example.com;\r
 q=dns/txt; s=test; t=1528637909; h=from : to : subject :\r
 date : message-id : from : subject : date;\r
 bh=2jUSOH9NhtVGCQWNr9BrIAPreKQjO6Sn7XIkfJVOzv8=;\r
 b=F45dVWDfMbQDGHJFlXUNB2HKfbCeLRyhDXgFpEL8GwpsRe0IeIixNTe3\r
 DhCVlUrSjV4BwcVcOF6+FF3Zo9Rpo1tFOeS9mPYQTnGdaSGsgeefOsk2Jz\r
 dA+L10TeYt9BgDfQNZtKdN1WO//KgIqXP7OdEFE4LjFYNcUxZQ4FADY+8=\r
From: Joe SixPack <joe@football.example.com>\r
To: Suzie Q <suzie@shopping.example.net>\r
Subject: Is dinner ready?\r
Date: Fri, 11 Jul 2003 21:00:37 -0700 (PDT)\r
Message-ID: <20030712040037.46341.5F8J@football.example.com>\r
\r
Hi.\r
\r
We lost the game.  Are you hungry yet?\r
\r
Joe.\r
";
    const ED_KEY: &str = "v=DKIM1; k=ed25519; p=11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
    const RSA_KEY: &str = "v=DKIM1; k=rsa; p=MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQDkHlOQoBTzWRiGs5V6NpP3idY6Wk08a5qhdR6wy5bdOKb2jLQiY/J16JYi0Qvx/byYzCNb3W91y3FutACDfzwQ/BC/e/8uBsCR+yz1Lxj+PL6lHvqMKrM3rG4hstT5QjvHO9PzoxZyVYLzBfO2EeC3Ip3G+2kryOTIKT+l/K4w3QIDAQAB";

    /// A verifier that never reaches real DNS: an unreachable name server (TEST-NET-1) with
    /// pinned records.
    fn verifier(records: &[(&str, &str)]) -> Dkim {
        let d = Dkim::new(Some("192.0.2.1:53".parse().unwrap()));
        for (n, r) in records {
            d.add_static_record(n, r).unwrap();
        }
        d
    }

    /// Only the signature with selector `keep` (the other one removed).
    fn only_signature(keep: &str) -> String {
        let (sigs, rest) = RFC8463.split_at(RFC8463.find("From: Joe").unwrap());
        let mut out = String::new();
        for sig in sigs.split("DKIM-Signature:").filter(|s| !s.is_empty()) {
            if sig.contains(&format!("s={keep};")) {
                out.push_str("DKIM-Signature:");
                out.push_str(sig);
            }
        }
        out + rest
    }

    #[tokio::test]
    async fn rfc8463_ed25519_and_rsa_signatures_verify() {
        let d = verifier(&[("brisbane._domainkey.football.example.com", ED_KEY), ("test._domainkey.football.example.com", RSA_KEY)]);
        for (sel, expect) in [("brisbane", "brisbane"), ("test", "test")] {
            let v = d.verify(only_signature(sel).as_bytes(), Some("football.example.com")).await;
            assert_eq!(v.status, Status::Pass, "{sel}: {v:?}");
            assert_eq!(v.domain.as_deref(), Some("football.example.com"));
            assert_eq!(v.selector.as_deref(), Some(expect));
        }
        let both = d.verify(RFC8463.as_bytes(), Some("football.example.com")).await;
        assert_eq!(both.status, Status::Pass);
    }

    #[tokio::test]
    async fn altered_messages_fail() {
        let d = verifier(&[("brisbane._domainkey.football.example.com", ED_KEY), ("test._domainkey.football.example.com", RSA_KEY)]);
        // Body changed.
        let body = RFC8463.replace("We lost the game.", "We won the game.");
        let v = d.verify(body.as_bytes(), None).await;
        assert_eq!(v.status, Status::Fail, "{v:?}");
        assert_eq!(v.domain.as_deref(), Some("football.example.com"));
        // A signed header changed.
        let subject = RFC8463.replace("Subject: Is dinner ready?", "Subject: Wire the money");
        assert_eq!(d.verify(subject.as_bytes(), None).await.status, Status::Fail);
        // From changed to another sender: the signature no longer covers it.
        let from = RFC8463.replace("joe@football.example.com>", "ceo@football.example.com>");
        assert_eq!(d.verify(from.as_bytes(), None).await.status, Status::Fail);
        // The wrong key for the selector.
        let wrong = verifier(&[("test._domainkey.football.example.com", RSA_KEY.replace("MIGfMA0", "MIGfMA1").as_str())]);
        let v = wrong.verify(only_signature("test").as_bytes(), None).await;
        assert_ne!(v.status, Status::Pass, "{v:?}");
    }

    #[tokio::test]
    async fn unsigned_and_wrong_keys() {
        // Another Ed25519 key under the selector (key rotation gone wrong, or a forgery).
        let d = verifier(&[("brisbane._domainkey.football.example.com", "v=DKIM1; k=ed25519; p=MCowBQYDK2VwAyEAGb9ECWmEzf6FQbrBZ9w7lshQhqowtrbLDFw4rXAxZuE=")]);
        let unsigned = &RFC8463[RFC8463.find("From: Joe").unwrap()..];
        assert_eq!(d.verify(unsigned.as_bytes(), None).await, Verdict::none(None));
        let v = d.verify(only_signature("brisbane").as_bytes(), None).await;
        assert_ne!(v.status, Status::Pass, "a different key must not pass: {v:?}");
        assert_eq!(d.verify(&vec![b'a'; MAX_VERIFY_BYTES + 1], None).await.status, Status::None);
        assert!(d.add_static_record("x._domainkey.example.org", "not a key record").is_err());
    }

    #[tokio::test]
    async fn unreachable_dns_is_a_temporary_error() {
        let d = Dkim::new(Some("127.0.0.1:9".parse().unwrap()));
        let v = d.verify(only_signature("test").as_bytes(), None).await;
        assert_eq!(v.status, Status::Temperror, "{v:?}");
    }

    #[test]
    fn prefers_aligned_passes() {
        let r = summarize(
            [
                (DkimResult::Pass, Some("mailer.example".into()), Some("s1".into())),
                (DkimResult::Fail(mail_auth::Error::Base64), Some("bank.example".into()), Some("s2".into())),
                (DkimResult::Pass, Some("bank.example".into()), Some("s3".into())),
            ]
            .into_iter(),
            Some("news.bank.example"),
        );
        assert_eq!((r.status, r.domain.as_deref()), (Status::Pass, Some("bank.example")));
        let r = summarize([(DkimResult::Pass, Some("mailer.example".into()), None)].into_iter(), Some("bank.example"));
        assert_eq!((r.status, r.domain.as_deref()), (Status::Pass, Some("mailer.example")));
        assert_eq!(Status::parse(Status::Temperror.as_str()), Status::Temperror);
    }
}
