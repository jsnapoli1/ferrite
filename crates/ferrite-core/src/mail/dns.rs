//! A small DNS client for MX lookups (RFC 1035), used by autoconfig when the provider
//! databases know nothing about a domain. It asks the system's resolvers (`/etc/resolv.conf`)
//! over UDP, retrying over TCP when the answer is truncated.

use crate::{Error, Result};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const QTYPE_MX: u16 = 15;
const QCLASS_IN: u16 = 1;
const TIMEOUT: Duration = Duration::from_secs(3);

/// One MX record: preference (lower is preferred) and the mail exchanger's host name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mx {
    pub preference: u16,
    pub exchange: String,
}

/// A DNS query for `name`'s MX records with the given id (recursion desired).
pub fn mx_query(id: u16, name: &str) -> Result<Vec<u8>> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // RD
    q.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    q.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // AN, NS, AR
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(Error::bad(format!("{name:?} is not a domain name")));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&QTYPE_MX.to_be_bytes());
    q.extend_from_slice(&QCLASS_IN.to_be_bytes());
    Ok(q)
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]))
}

/// Reads a (possibly compressed) name at `pos`; returns it and the position after it.
fn read_name(b: &[u8], mut pos: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut end: Option<usize> = None;
    let mut jumps = 0;
    loop {
        let len = *b.get(pos)? as usize;
        if len == 0 {
            pos += 1;
            break;
        }
        if len & 0xc0 == 0xc0 {
            // A pointer to an earlier name; bounded so a loop can't hang us.
            jumps += 1;
            if jumps > 16 {
                return None;
            }
            let target = (u16_at(b, pos)? & 0x3fff) as usize;
            end.get_or_insert(pos + 2);
            pos = target;
            continue;
        }
        if len > 63 {
            return None;
        }
        let label = b.get(pos + 1..pos + 1 + len)?;
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        pos += 1 + len;
        if labels.len() > 127 {
            return None;
        }
    }
    Some((labels.join("."), end.unwrap_or(pos)))
}

/// Whether a response has the truncation bit set.
pub fn truncated(resp: &[u8]) -> bool {
    resp.get(2).is_some_and(|f| f & 0x02 != 0)
}

/// Parses the MX answers of a response to the query with `id`, sorted by preference.
/// `NXDOMAIN` and empty answers give an empty list; malformed responses an error.
pub fn parse_mx(resp: &[u8], id: u16) -> Result<Vec<Mx>> {
    let bad = || Error::Unavailable("malformed DNS response".into());
    if u16_at(resp, 0).ok_or_else(bad)? != id {
        return Err(Error::Unavailable("DNS response for another query".into()));
    }
    let flags = u16_at(resp, 2).ok_or_else(bad)?;
    if flags & 0x8000 == 0 {
        return Err(bad());
    }
    match flags & 0x000f {
        0 | 3 => {}
        rcode => return Err(Error::Unavailable(format!("DNS lookup failed (rcode {rcode})"))),
    }
    let qd = u16_at(resp, 4).ok_or_else(bad)?;
    let an = u16_at(resp, 6).ok_or_else(bad)?;
    let mut pos = 12;
    for _ in 0..qd {
        let (_, next) = read_name(resp, pos).ok_or_else(bad)?;
        pos = next + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        let (_, next) = read_name(resp, pos).ok_or_else(bad)?;
        let rtype = u16_at(resp, next).ok_or_else(bad)?;
        let rdlen = u16_at(resp, next + 8).ok_or_else(bad)? as usize;
        let rdata = next + 10;
        if resp.len() < rdata + rdlen {
            return Err(bad());
        }
        if rtype == QTYPE_MX && rdlen >= 3 {
            let preference = u16_at(resp, rdata).ok_or_else(bad)?;
            let (exchange, _) = read_name(resp, rdata + 2).ok_or_else(bad)?;
            if !exchange.is_empty() {
                out.push(Mx { preference, exchange });
            }
        }
        pos = rdata + rdlen;
    }
    out.sort_by(|a, b| a.preference.cmp(&b.preference).then_with(|| a.exchange.cmp(&b.exchange)));
    Ok(out)
}

/// The name servers of `/etc/resolv.conf` (at most three), or none.
pub fn system_resolvers(conf: &str) -> Vec<SocketAddr> {
    conf.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next()? == "nameserver").then(|| w.next())?
        })
        .filter_map(|a| a.split('%').next()?.parse::<IpAddr>().ok())
        .map(|ip| SocketAddr::new(ip, 53))
        .take(3)
        .collect()
}

async fn ask_udp(server: SocketAddr, query: &[u8]) -> Result<Vec<u8>> {
    let bind: SocketAddr = if server.is_ipv4() {
        "0.0.0.0:0".parse().expect("valid")
    } else {
        "[::]:0".parse().expect("valid")
    };
    let sock = tokio::net::UdpSocket::bind(bind).await?;
    sock.connect(server).await?;
    sock.send(query).await?;
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(TIMEOUT, sock.recv(&mut buf))
        .await
        .map_err(|_| Error::Unavailable(format!("DNS server {server} did not answer")))??;
    buf.truncate(n);
    Ok(buf)
}

async fn ask_tcp(server: SocketAddr, query: &[u8]) -> Result<Vec<u8>> {
    let io = async {
        let mut s = tokio::net::TcpStream::connect(server).await?;
        s.write_all(&(query.len() as u16).to_be_bytes()).await?;
        s.write_all(query).await?;
        let len = s.read_u16().await? as usize;
        let mut buf = vec![0u8; len];
        s.read_exact(&mut buf).await?;
        Ok::<_, std::io::Error>(buf)
    };
    Ok(tokio::time::timeout(TIMEOUT, io)
        .await
        .map_err(|_| Error::Unavailable(format!("DNS server {server} did not answer")))??)
}

/// The MX records of `domain` from the system's resolvers. Empty when there are none.
pub async fn lookup_mx(domain: &str) -> Result<Vec<Mx>> {
    let conf = tokio::fs::read_to_string("/etc/resolv.conf").await.unwrap_or_default();
    let mut servers = system_resolvers(&conf);
    if servers.is_empty() {
        servers.push("127.0.0.1:53".parse().expect("valid"));
    }
    let mut last = Error::Unavailable("no DNS server answered".into());
    for server in servers {
        let id: u16 = rand::random();
        let query = mx_query(id, domain)?;
        let r = async {
            let mut resp = ask_udp(server, &query).await?;
            if truncated(&resp) {
                resp = ask_tcp(server, &query).await?;
            }
            parse_mx(&resp, id)
        }
        .await;
        match r {
            Ok(v) => return Ok(v),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response for `example.org MX` with two answers, the second name compressed.
    fn response(id: u16) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&id.to_be_bytes());
        r.extend_from_slice(&0x8180u16.to_be_bytes());
        r.extend_from_slice(&[0, 1, 0, 2, 0, 0, 0, 0]);
        // Question at offset 12: example.org MX IN.
        r.extend_from_slice(b"\x07example\x03org\x00");
        r.extend_from_slice(&[0, 15, 0, 1]);
        // Answer 1: name pointer to 12, MX, IN, TTL, rdata: pref 20, "alt.mx.example.org"
        let rdata1 = {
            let mut d = vec![0, 20];
            d.extend_from_slice(b"\x03alt\x02mx\xc0\x0c");
            d
        };
        r.extend_from_slice(&[0xc0, 0x0c, 0, 15, 0, 1, 0, 0, 0x0e, 0x10]);
        r.extend_from_slice(&(rdata1.len() as u16).to_be_bytes());
        r.extend_from_slice(&rdata1);
        // Answer 2: pref 10, "ASPMX.L.GOOGLE.COM" (uppercase, uncompressed).
        let rdata2 = {
            let mut d = vec![0, 10];
            d.extend_from_slice(b"\x05ASPMX\x01L\x06GOOGLE\x03COM\x00");
            d
        };
        r.extend_from_slice(&[0xc0, 0x0c, 0, 15, 0, 1, 0, 0, 0x0e, 0x10]);
        r.extend_from_slice(&(rdata2.len() as u16).to_be_bytes());
        r.extend_from_slice(&rdata2);
        r
    }

    #[test]
    fn encodes_queries() {
        let q = mx_query(0xabcd, "example.org.").unwrap();
        assert_eq!(&q[..4], &[0xab, 0xcd, 0x01, 0x00]);
        assert_eq!(&q[12..], b"\x07example\x03org\x00\x00\x0f\x00\x01");
        assert!(mx_query(1, "bad..name").is_err());
        assert!(mx_query(1, &"x".repeat(64)).is_err());
    }

    #[test]
    fn parses_answers_with_compression() {
        let mx = parse_mx(&response(7), 7).unwrap();
        assert_eq!(
            mx,
            vec![
                Mx {
                    preference: 10,
                    exchange: "aspmx.l.google.com".into()
                },
                Mx {
                    preference: 20,
                    exchange: "alt.mx.example.org".into()
                },
            ]
        );
        assert!(parse_mx(&response(7), 8).is_err(), "another query's id");
        let mut cut = response(7);
        cut.truncate(cut.len() - 5);
        assert!(parse_mx(&cut, 7).is_err());
        // NXDOMAIN: no records.
        let mut nx = response(7);
        nx[3] = 0x83;
        nx[7] = 0;
        assert_eq!(parse_mx(&nx[..29], 7).unwrap(), vec![]);
        // SERVFAIL is an error.
        let mut fail = response(7);
        fail[3] = 0x82;
        assert!(parse_mx(&fail, 7).is_err());
        // A pointer loop is refused rather than followed forever.
        let mut looped = response(7);
        looped[12] = 0xc0;
        looped[13] = 0x0c;
        assert!(parse_mx(&looped, 7).is_err());
        let mut tc = response(7);
        tc[2] |= 0x02;
        assert!(truncated(&tc) && !truncated(&response(7)));
    }

    #[test]
    fn reads_resolv_conf() {
        let conf = "# comment\nnameserver 8.8.8.8\nsearch example.org\nnameserver fe80::1%eth0\nnameserver 1.1.1.1\nnameserver 9.9.9.9\n";
        let s = system_resolvers(conf);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0], "8.8.8.8:53".parse().unwrap());
        assert_eq!(s[1].ip(), "fe80::1".parse::<IpAddr>().unwrap());
    }
}
