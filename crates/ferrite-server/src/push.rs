//! Web Push sender (RFC 8030 delivery, RFC 8291 encryption, RFC 8292 VAPID).
//!
//! Drains the core's push queue: for each message it checks the user's preferences, encrypts
//! the payload to each of their devices and POSTs it to the device's push service, with
//! bounded concurrency, timeouts and a short retry with backoff for 429/5xx. Subscriptions the
//! push service reports as gone (404/410) are removed. Endpoints must be public `https` URLs
//! (checked when subscribing and again before every delivery) unless
//! `push.allow_insecure_endpoints` is set for tests.

pub mod crypto;

use anyhow::anyhow;
use ferrite_core::push::{Category, PushMessage, PushSubscription};
use ferrite_core::{Core, Error, Result};
use reqwest::{StatusCode, Url};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OnceCell, Semaphore, mpsc};

/// Deliveries in flight at once, across all users.
const CONCURRENCY: usize = 8;
/// Attempts per delivery (the first try plus retries for 429/5xx and network errors).
const ATTEMPTS: u32 = 3;
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ENDPOINT_LEN: usize = 2048;
const VAPID_KV: &str = "vapid_pkcs8";

pub struct PushService {
    core: Core,
    /// Built on the first push: servers whose users never enable push don't pay for a TLS
    /// client and its root store.
    http: std::sync::OnceLock<reqwest::Client>,
    vapid: OnceCell<Arc<crypto::Vapid>>,
    slots: Arc<Semaphore>,
}

/// What happened to one delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Delivered,
    /// The push service no longer knows the subscription; it has been removed.
    Gone,
    Failed(String),
}

impl PushService {
    /// Creates the sender and starts draining the core's push queue (once per core).
    pub fn start(core: Core) -> Arc<Self> {
        let service = Arc::new(PushService {
            core: core.clone(),
            http: std::sync::OnceLock::new(),
            vapid: OnceCell::new(),
            slots: Arc::new(Semaphore::new(CONCURRENCY)),
        });
        if let Some(rx) = core.push.take_receiver() {
            let s = service.clone();
            tokio::spawn(async move { s.run(rx).await });
        }
        service
    }

    fn http(&self) -> &reqwest::Client {
        self.http.get_or_init(|| {
            let mut builder = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("Ferrite/", env!("CARGO_PKG_VERSION")));
            if !self.core.config.push.allow_insecure_endpoints {
                builder = builder.dns_resolver(Arc::new(PublicOnlyResolver)).https_only(true);
            }
            builder.build().unwrap_or_default()
        })
    }

    /// The VAPID key, generated and stored on first use (the sender loads it at startup).
    pub async fn vapid(&self) -> Result<Arc<crypto::Vapid>> {
        self.vapid
            .get_or_try_init(|| async {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD;
                let der = match self.core.kv_get(VAPID_KV).await? {
                    Some(v) => b64.decode(v).map_err(|e| Error::internal(format!("stored VAPID key: {e}")))?,
                    None => {
                        let der = crypto::Vapid::generate_pkcs8()?;
                        self.core.kv_set(VAPID_KV, &b64.encode(&der)).await?;
                        tracing::info!("generated the Web Push (VAPID) key");
                        der
                    }
                };
                Ok(Arc::new(crypto::Vapid::from_pkcs8(&der, &vapid_subject(&self.core))?))
            })
            .await
            .cloned()
    }

    async fn run(self: Arc<Self>, mut rx: mpsc::Receiver<PushMessage>) {
        if let Err(e) = self.vapid().await {
            tracing::error!(error = %e, "Web Push key unavailable");
        }
        while let Some(msg) = rx.recv().await {
            if let Err(e) = self.dispatch(msg).await {
                tracing::warn!(error = %e, "push dispatch failed");
            }
        }
    }

    async fn dispatch(self: &Arc<Self>, msg: PushMessage) -> Result<()> {
        if !self.core.push_prefs(&msg.user_id).await?.allows(msg.category) {
            return Ok(());
        }
        let subs = self.core.push_subscriptions(&msg.user_id).await?;
        if subs.is_empty() {
            return Ok(());
        }
        let msg = Arc::new(msg);
        for sub in subs {
            // Waiting for a slot here (not in request handlers) is what bounds concurrency.
            let Ok(permit) = self.slots.clone().acquire_owned().await else {
                return Ok(());
            };
            let this = self.clone();
            let msg = msg.clone();
            tokio::spawn(async move {
                if let Outcome::Failed(e) = this.deliver(&sub, &msg, ATTEMPTS).await {
                    tracing::warn!(subscription = %sub.id, error = %e, "push delivery failed");
                }
                drop(permit);
            });
        }
        Ok(())
    }

    /// Sends one message to one device, retrying 429/5xx and network errors with backoff.
    pub async fn deliver(&self, sub: &PushSubscription, msg: &PushMessage, attempts: u32) -> Outcome {
        match self.try_deliver(sub, msg, attempts).await {
            Ok(o) => o,
            Err(e) => Outcome::Failed(e.to_string()),
        }
    }

    async fn try_deliver(&self, sub: &PushSubscription, msg: &PushMessage, attempts: u32) -> Result<Outcome> {
        let url = check_endpoint(&sub.endpoint, self.core.config.push.allow_insecure_endpoints).await?;
        let payload = serde_json::to_vec(&msg.payload()).map_err(|e| Error::internal(e.to_string()))?;
        let body = crypto::encrypt(&payload, &sub.public_key()?, &sub.auth_secret()?)?;
        let vapid = self.vapid().await?;
        let audience = url.origin().ascii_serialization();
        let mut backoff = FIRST_BACKOFF;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut req = self
                .http()
                .post(url.clone())
                .header("TTL", msg.ttl.to_string())
                .header("Urgency", msg.urgency.as_str())
                .header("Content-Encoding", "aes128gcm")
                .header("Content-Type", "application/octet-stream")
                .header("Authorization", vapid.authorization(&audience, chrono::Utc::now().timestamp())?)
                .body(body.clone());
            if let Some(t) = &msg.topic {
                req = req.header("Topic", topic(t));
            }
            let (retryable, wait, error) = match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    self.core.touch_push_subscription(&sub.id).await?;
                    return Ok(Outcome::Delivered);
                }
                Ok(resp) if matches!(resp.status(), StatusCode::NOT_FOUND | StatusCode::GONE) => {
                    self.core.remove_push_endpoint(&sub.endpoint).await?;
                    tracing::info!(subscription = %sub.id, "push subscription expired; removed");
                    return Ok(Outcome::Gone);
                }
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let detail: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
                    let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    (retryable, retry_after, format!("push service answered {status} {}", detail.trim()))
                }
                Err(e) => (true, None, format!("push service unreachable: {e}")),
            };
            if !retryable || attempt >= attempts {
                return Ok(Outcome::Failed(error));
            }
            tokio::time::sleep(wait.unwrap_or(backoff).min(MAX_BACKOFF)).await;
            backoff *= 2;
        }
    }

    /// "Send test notification": delivers right away to every device of the user and reports
    /// what happened to each.
    pub async fn send_test(&self, user_id: &str) -> Result<Vec<(PushSubscription, Outcome)>> {
        if !self.core.push_prefs(user_id).await?.allows(Category::Test) {
            return Err(Error::bad("push notifications are turned off for your account"));
        }
        let msg = PushMessage {
            user_id: user_id.to_string(),
            category: Category::Test,
            kind: "test",
            title: "Ferrite test notification".into(),
            body: "Push notifications work on this device.".into(),
            url: "/settings/notifications".into(),
            tag: "test".into(),
            actions: Vec::new(),
            urgency: ferrite_core::push::Urgency::High,
            ttl: 300,
            topic: None,
            notification_id: None,
        };
        let subs = self.core.push_subscriptions(user_id).await?;
        let results = futures::future::join_all(subs.iter().map(|s| self.deliver(s, &msg, 1))).await;
        Ok(subs.into_iter().zip(results).collect())
    }
}

/// RFC 8030 §5.4: a topic is at most 32 characters of the URL-safe base64 alphabet. Hashing
/// keeps it opaque to the push service.
fn topic(key: &str) -> String {
    use base64::Engine;
    let digest = blake3::hash(key.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest.as_bytes()[..24])
}

/// VAPID `sub`: the configured contact, else the public URL when it is https, else a mailto at
/// the public host.
pub fn vapid_subject(core: &Core) -> String {
    if let Some(c) = core.config.push.contact.as_deref().filter(|c| !c.trim().is_empty()) {
        return c.trim().to_string();
    }
    let public = &core.config.public_url;
    if public.starts_with("https://") {
        return public.clone();
    }
    let host = Url::parse(public)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "localhost".into());
    format!("mailto:webpush@{host}")
}

/// Validates a push endpoint: an absolute `https` URL without credentials whose host resolves
/// only to public addresses (SSRF guard). `allow_insecure` (tests) skips the address and
/// scheme checks.
pub async fn check_endpoint(raw: &str, allow_insecure: bool) -> Result<Url> {
    if raw.len() > MAX_ENDPOINT_LEN {
        return Err(Error::bad("push endpoint is too long"));
    }
    let url = Url::parse(raw).map_err(|_| Error::bad("push endpoint is not a URL"))?;
    let scheme_ok = url.scheme() == "https" || (allow_insecure && url.scheme() == "http");
    if !scheme_ok {
        return Err(Error::bad("push endpoint must use https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::bad("push endpoint must not contain credentials"));
    }
    let host = url.host_str().ok_or_else(|| Error::bad("push endpoint has no host"))?;
    if allow_insecure {
        return Ok(url);
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<IpAddr> = match host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((host, port)))
            .await
            .map_err(|_| Error::bad("push endpoint host did not resolve in time"))?
            .map_err(|_| Error::bad("push endpoint host does not resolve"))?
            .map(|a| a.ip())
            .collect(),
    };
    if addrs.is_empty() || !addrs.iter().all(|ip| is_public(*ip)) {
        return Err(Error::bad("push endpoint must be on a public address"));
    }
    Ok(url)
}

/// True for globally routable unicast addresses: not loopback, private, link-local, CGNAT,
/// unique-local, multicast, documentation, benchmarking or reserved ranges.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_v4(v4);
            }
            let s = v6.segments();
            // NAT64 (64:ff9b::/96) embeds an IPv4 address.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
                let [a, b] = s[6].to_be_bytes();
                let [c, d] = s[7].to_be_bytes();
                return is_public_v4(Ipv4Addr::new(a, b, c, d));
            }
            !(v6.is_unspecified()
                || v6.is_loopback()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (s[0] & 0xffc0) == 0xfe80 // link local fe80::/10
                || (s[0] & 0xffc0) == 0xfec0 // site local fec0::/10
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || (s[0] == 0x2001 && s[1] < 0x0200) // IETF protocol assignments (Teredo, ORCHID, …)
                || s[0] == 0x2002 // 6to4 tunnels to arbitrary IPv4
                || s[0..6] == [0, 0, 0, 0, 0, 0]) // IPv4-compatible (deprecated)
        }
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        || (a == 100 && (b & 0xc0) == 64) // shared address space 100.64.0.0/10
        || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
        || (a == 198 && (b & 0xfe) == 18) // benchmarking 198.18.0.0/15
        || a >= 240) // reserved
}

/// DNS for the push client: drops non-public answers so a name can't be re-pointed at an
/// internal service between the endpoint check and the connection (DNS rebinding).
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| is_public(a.ip())).collect();
            if addrs.is_empty() {
                return Err(anyhow!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// "Firefox on Android", "Safari on iOS", … from a User-Agent header.
pub fn device_label(user_agent: &str) -> String {
    let ua = user_agent;
    let browser = if ua.contains("Edg/") || ua.contains("EdgA/") || ua.contains("EdgiOS/") {
        "Edge"
    } else if ua.contains("OPR/") || ua.contains("Opera") {
        "Opera"
    } else if ua.contains("SamsungBrowser/") {
        "Samsung Internet"
    } else if ua.contains("Firefox/") || ua.contains("FxiOS/") {
        "Firefox"
    } else if ua.contains("CriOS/") || ua.contains("Chrome/") || ua.contains("Chromium/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "Browser"
    };
    let os = if ua.contains("iPhone") || ua.contains("iPod") {
        Some("iOS")
    } else if ua.contains("iPad") {
        Some("iPadOS")
    } else if ua.contains("Android") {
        Some("Android")
    } else if ua.contains("CrOS") {
        Some("ChromeOS")
    } else if ua.contains("Windows") {
        Some("Windows")
    } else if ua.contains("Mac OS X") || ua.contains("Macintosh") {
        Some("macOS")
    } else if ua.contains("Linux") {
        Some("Linux")
    } else {
        None
    };
    match os {
        Some(os) => format!("{browser} on {os}"),
        None => browser.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_address_classification() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::a00:1",
            "2002:7f00:1::",
            "::127.0.0.1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private} must not count as public");
        }
        for public in [
            "8.8.8.8",
            "142.250.1.1",
            "17.0.0.1",
            "2a00:1450:4001::1",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
        ] {
            assert!(is_public(public.parse().unwrap()), "{public} is public");
        }
    }

    #[tokio::test]
    async fn endpoint_checks() {
        let bad = [
            "http://fcm.googleapis.com/fcm/send/x",
            "https://user:pw@fcm.googleapis.com/x",
            "https://127.0.0.1/push",
            "https://[::1]/push",
            "https://10.0.0.8/push",
            "https://169.254.169.254/latest/meta-data",
            "https://localhost/push",
            "not a url",
            "ftp://example.org/x",
        ];
        for b in bad {
            assert!(check_endpoint(b, false).await.is_err(), "{b} must be refused");
        }
        assert!(check_endpoint("https://93.184.215.14/push/abc", false).await.is_ok());
        assert!(check_endpoint("http://127.0.0.1:9/push", true).await.is_ok());
        assert!(check_endpoint("ftp://127.0.0.1/push", true).await.is_err());
        let long = format!("https://93.184.215.14/{}", "a".repeat(MAX_ENDPOINT_LEN));
        assert!(check_endpoint(&long, false).await.is_err());
    }

    #[test]
    fn topics_are_short_and_url_safe() {
        let t = topic("chat:/talk/abc");
        assert_eq!(t.len(), 32);
        assert!(t.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_eq!(t, topic("chat:/talk/abc"));
        assert_ne!(t, topic("chat:/talk/abd"));
    }

    #[test]
    fn labels_from_user_agents() {
        let cases = [
            (
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36",
                "Chrome on Linux",
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
                "Safari on iOS",
            ),
            ("Mozilla/5.0 (Android 14; Mobile; rv:131.0) Gecko/131.0 Firefox/131.0", "Firefox on Android"),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Edg/130.0.0.0",
                "Edge on Windows",
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
                "Safari on macOS",
            ),
            ("curl/8.0", "Browser"),
        ];
        for (ua, label) in cases {
            assert_eq!(device_label(ua), label);
        }
    }
}
