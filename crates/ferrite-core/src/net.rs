//! Outbound connection guards (SSRF): which addresses count as public, a DNS resolver for
//! HTTP clients that drops non-public answers, and host checks for mail servers.

use crate::{Error, Result};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

const DNS_TIMEOUT: Duration = Duration::from_secs(5);

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

/// DNS for HTTP clients: drops non-public answers so a name can't be re-pointed at an
/// internal service between a check and the connection (DNS rebinding).
pub struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.filter(|a| is_public(a.ip())).collect();
            if addrs.is_empty() {
                return Err(anyhow::anyhow!("{host} has no public address").into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Resolves `host:port` for an outgoing connection. Unless `allow_local`, every address must
/// be public (one private answer refuses the host). The caller connects to these addresses,
/// not to the name again, so the check holds for the connection.
pub async fn resolve(host: &str, port: u16, allow_local: bool) -> Result<Vec<SocketAddr>> {
    let bare = host.trim().trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<SocketAddr> = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => tokio::time::timeout(DNS_TIMEOUT, tokio::net::lookup_host((bare, port)))
            .await
            .map_err(|_| Error::Unavailable(format!("looking up {bare} timed out")))?
            .map_err(|e| Error::Unavailable(format!("cannot resolve {bare}: {e}")))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(Error::Unavailable(format!("{bare} has no address")));
    }
    if !allow_local && addrs.iter().any(|a| !is_public(a.ip())) {
        return Err(Error::bad(format!(
            "{bare} is on a local or private network address; an administrator can allow local mail servers in Settings → Administration → Mail"
        )));
    }
    Ok(addrs)
}

/// Checks a URL for an outgoing HTTP request: `https` (plain `http` only when local servers
/// are allowed, e.g. a test provider), no credentials, and an IP-literal host must be public
/// unless `allow_local` (names are checked when they resolve, by [`PublicOnlyResolver`]).
pub fn check_url(raw: &str, allow_local: bool) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| Error::bad(format!("{raw:?} is not a URL")))?;
    if !(url.scheme() == "https" || (allow_local && url.scheme() == "http")) {
        return Err(Error::bad(format!("{raw:?} must use https")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::bad("URLs must not contain credentials"));
    }
    let ip = match url.host() {
        Some(url::Host::Ipv4(v4)) => Some(IpAddr::V4(v4)),
        Some(url::Host::Ipv6(v6)) => Some(IpAddr::V6(v6)),
        Some(url::Host::Domain(_)) => None,
        None => return Err(Error::bad(format!("{raw:?} has no host"))),
    };
    if ip.is_some_and(|ip| !allow_local && !is_public(ip)) {
        return Err(Error::bad(format!("{raw:?} is on a local or private network address")));
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_for_outgoing_requests() {
        assert!(check_url("https://oauth2.googleapis.com/token", false).is_ok());
        assert!(check_url("http://oauth2.googleapis.com/token", false).is_err(), "plain http");
        assert!(check_url("https://127.0.0.1/token", false).is_err());
        assert!(check_url("https://[::1]/token", false).is_err());
        assert!(check_url("https://user:pw@example.org/", false).is_err());
        assert!(check_url("ftp://example.org/", true).is_err());
        assert!(check_url("http://127.0.0.1:9/token", true).is_ok(), "allowed for local test providers");
    }

    #[test]
    fn public_and_local_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private} must not count as public");
        }
        for public in ["8.8.8.8", "142.250.1.1", "2a00:1450:4001::1"] {
            assert!(is_public(public.parse().unwrap()), "{public} is public");
        }
    }

    #[tokio::test]
    async fn resolving_with_and_without_local_servers() {
        assert!(resolve("127.0.0.1", 993, false).await.is_err());
        assert!(resolve("[::1]", 993, false).await.is_err());
        assert_eq!(resolve("127.0.0.1", 993, true).await.unwrap(), vec!["127.0.0.1:993".parse().unwrap()]);
        assert!(resolve("localhost", 25, false).await.is_err(), "names resolving to loopback are refused too");
    }
}
