//! Server settings for an email address, found like Thunderbird does:
//!
//! 1. Thunderbird's ISPDB (`https://autoconfig.thunderbird.net/v1.1/<domain>`).
//! 2. The provider's own autoconfig documents, `https://autoconfig.<domain>/mail/config-v1.1.xml`
//!    and `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml`.
//! 3. Autodiscover (POX, Exchange style) at `https://autodiscover.<domain>/autodiscover/autodiscover.xml`.
//! 4. The domain's MX records: well-known hosting providers by their mail exchangers, else
//!    the ISPDB entry of the mail exchanger's domain.
//!
//! The domain's own URLs are fetched through the guarded client (https only, no redirects,
//! public addresses only unless local servers are allowed, 256 KiB at most). Lookups are best
//! effort: any failure just means "fill the form by hand".

use super::dns::Mx;
use crate::mailer::SmtpSecurity;
use crate::{Core, Error, Result};
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const ISPDB: &str = "https://autoconfig.thunderbird.net/v1.1/";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerHint {
    pub host: String,
    pub port: u16,
    pub security: SmtpSecurity,
    pub username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Autoconfig {
    pub provider: String,
    pub imap: ServerHint,
    pub smtp: ServerHint,
}

#[derive(Default)]
struct Partial {
    host: String,
    port: u16,
    security: Option<SmtpSecurity>,
    username: String,
}

impl Partial {
    fn finish(self, email: &str) -> Option<ServerHint> {
        if self.host.is_empty() || self.port == 0 {
            return None;
        }
        let local = email.split('@').next().unwrap_or(email);
        let domain = email.split('@').nth(1).unwrap_or("");
        Some(ServerHint {
            host: self.host.replace("%EMAILDOMAIN%", domain),
            port: self.port,
            security: self.security.unwrap_or(SmtpSecurity::Starttls),
            username: self
                .username
                .replace("%EMAILADDRESS%", email)
                .replace("%EMAILLOCALPART%", local)
                .replace("%EMAILDOMAIN%", domain),
        })
    }
}

/// Parses an ISPDB `clientConfig` document: the first IMAP and the first SMTP server.
pub fn parse(xml: &str, email: &str) -> Option<Autoconfig> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut provider = String::new();
    let mut imap: Option<ServerHint> = None;
    let mut smtp: Option<ServerHint> = None;
    // Which server element we are in: "imap", "smtp" or another type to skip.
    let mut current: Option<(String, Partial)> = None;
    let mut field = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if name == "incomingServer" || name == "outgoingServer" {
                    let ty = e
                        .attributes()
                        .flatten()
                        .find(|a| a.key.as_ref() == b"type")
                        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
                        .unwrap_or_default();
                    current = Some((ty, Partial::default()));
                }
                field = name;
            }
            Ok(Event::Text(t)) => {
                let text = t.decode().map(|c| c.trim().to_string()).unwrap_or_default();
                match (&mut current, field.as_str()) {
                    (Some((_, p)), "hostname") => p.host = text,
                    (Some((_, p)), "port") => p.port = text.parse().unwrap_or(0),
                    (Some((_, p)), "socketType") => {
                        p.security = match text.as_str() {
                            "SSL" => Some(SmtpSecurity::Tls),
                            "STARTTLS" => Some(SmtpSecurity::Starttls),
                            "plain" => Some(SmtpSecurity::None),
                            _ => None,
                        }
                    }
                    (Some((_, p)), "username") => p.username = text,
                    (None, "displayName") if provider.is_empty() => provider = text,
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                if name.as_ref() == b"incomingServer" || name.as_ref() == b"outgoingServer" {
                    if let Some((ty, p)) = current.take() {
                        match ty.as_str() {
                            "imap" if imap.is_none() => imap = p.finish(email),
                            "smtp" if smtp.is_none() => smtp = p.finish(email),
                            _ => {}
                        }
                    }
                }
                field.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    Some(Autoconfig {
        provider,
        imap: imap?,
        smtp: smtp?,
    })
}

/// The domain of an email address, if it looks like a host name.
pub fn domain_of(email: &str) -> Option<String> {
    let domain = email.rsplit_once('@')?.1.trim().trim_end_matches('.').to_ascii_lowercase();
    let ok = !domain.is_empty()
        && domain.contains('.')
        && domain.len() <= 253
        && domain
            .split('.')
            .all(|l| !l.is_empty() && l.len() <= 63 && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    ok.then_some(domain)
}

/// Looks up `email`'s domain in the ISPDB. `None` when unknown or unreachable.
pub async fn lookup(email: &str) -> Option<Autoconfig> {
    let domain = domain_of(email)?;
    ispdb(&domain, email.trim()).await
}

async fn ispdb(domain: &str, email: &str) -> Option<Autoconfig> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(8)).build().ok()?;
    let resp = client.get(format!("{ISPDB}{domain}")).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse(&body, email)
}

/// Where settings were found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Thunderbird's provider database.
    Ispdb,
    /// The provider's own autoconfig document.
    Autoconfig,
    /// Autodiscover (Exchange style).
    Autodiscover,
    /// The domain's mail exchangers (MX records).
    Mx,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discovery {
    pub config: Autoconfig,
    pub source: Source,
    /// The mail exchanger the settings were derived from (MX lookups).
    pub mx: Option<String>,
}

/// A hosting provider recognised by its mail exchangers.
struct MxProvider {
    suffix: &'static str,
    name: &'static str,
    imap: (&'static str, u16, SmtpSecurity),
    smtp: (&'static str, u16, SmtpSecurity),
}

const fn p(suffix: &'static str, name: &'static str, imap: (&'static str, u16, SmtpSecurity), smtp: (&'static str, u16, SmtpSecurity)) -> MxProvider {
    MxProvider { suffix, name, imap, smtp }
}

const TLS: SmtpSecurity = SmtpSecurity::Tls;
const STARTTLS: SmtpSecurity = SmtpSecurity::Starttls;

const MX_PROVIDERS: &[MxProvider] = &[
    p("google.com", "Google Workspace", ("imap.gmail.com", 993, TLS), ("smtp.gmail.com", 465, TLS)),
    p("googlemail.com", "Google Workspace", ("imap.gmail.com", 993, TLS), ("smtp.gmail.com", 465, TLS)),
    p(
        "outlook.com",
        "Microsoft 365",
        ("outlook.office365.com", 993, TLS),
        ("smtp.office365.com", 587, STARTTLS),
    ),
    p("yahoodns.net", "Yahoo", ("imap.mail.yahoo.com", 993, TLS), ("smtp.mail.yahoo.com", 465, TLS)),
    p("zoho.com", "Zoho Mail", ("imap.zoho.com", 993, TLS), ("smtp.zoho.com", 465, TLS)),
    p("zoho.eu", "Zoho Mail", ("imap.zoho.eu", 993, TLS), ("smtp.zoho.eu", 465, TLS)),
    p("zoho.in", "Zoho Mail", ("imap.zoho.in", 993, TLS), ("smtp.zoho.in", 465, TLS)),
    p(
        "messagingengine.com",
        "Fastmail",
        ("imap.fastmail.com", 993, TLS),
        ("smtp.fastmail.com", 465, TLS),
    ),
    p("icloud.com", "iCloud Mail", ("imap.mail.me.com", 993, TLS), ("smtp.mail.me.com", 587, STARTTLS)),
    p("mailbox.org", "mailbox.org", ("imap.mailbox.org", 993, TLS), ("smtp.mailbox.org", 465, TLS)),
    p("ovh.net", "OVHcloud", ("ssl0.ovh.net", 993, TLS), ("ssl0.ovh.net", 465, TLS)),
    p("kundenserver.de", "IONOS", ("imap.ionos.de", 993, TLS), ("smtp.ionos.de", 465, TLS)),
    p("ionos.de", "IONOS", ("imap.ionos.de", 993, TLS), ("smtp.ionos.de", 465, TLS)),
    p("ionos.com", "IONOS", ("imap.ionos.com", 993, TLS), ("smtp.ionos.com", 465, TLS)),
    p("ionos.co.uk", "IONOS", ("imap.ionos.co.uk", 993, TLS), ("smtp.ionos.co.uk", 465, TLS)),
    p("migadu.com", "Migadu", ("imap.migadu.com", 993, TLS), ("smtp.migadu.com", 465, TLS)),
    p("gandi.net", "Gandi", ("mail.gandi.net", 993, TLS), ("mail.gandi.net", 465, TLS)),
    p("yandex.net", "Yandex Mail", ("imap.yandex.com", 993, TLS), ("smtp.yandex.com", 465, TLS)),
    p("mail.ru", "Mail.ru", ("imap.mail.ru", 993, TLS), ("smtp.mail.ru", 465, TLS)),
];

/// Settings of a known hosting provider for a mail exchanger host.
pub fn from_mx(mx_host: &str, email: &str) -> Option<Autoconfig> {
    let h = mx_host.trim_end_matches('.').to_ascii_lowercase();
    let hint = |(host, port, security): (&str, u16, SmtpSecurity)| ServerHint {
        host: host.to_string(),
        port,
        security,
        username: email.to_string(),
    };
    MX_PROVIDERS
        .iter()
        .find(|m| h == m.suffix || h.ends_with(&format!(".{}", m.suffix)))
        .map(|m| Autoconfig {
            provider: m.name.to_string(),
            imap: hint(m.imap),
            smtp: hint(m.smtp),
        })
}

/// The registrable-looking base of a host (`mx01.mail.example.co.uk` → `example.co.uk`):
/// the last two labels, or three under a short second-level label of a country domain.
pub fn base_domain(host: &str) -> String {
    let labels: Vec<&str> = host.trim_end_matches('.').split('.').filter(|l| !l.is_empty()).collect();
    let n = labels.len();
    if n <= 2 {
        return labels.join(".");
    }
    let take = if ["co", "com", "net", "org", "ac", "gov", "edu"].contains(&labels[n - 2]) && labels[n - 1].len() == 2 {
        3
    } else {
        2
    };
    labels[n - take..].join(".")
}

/// The preferred MX host, ignoring "null MX" (RFC 7505) records.
pub fn best_mx(records: &[Mx]) -> Option<&str> {
    records.iter().map(|m| m.exchange.as_str()).find(|e| !e.is_empty() && *e != ".")
}

#[derive(Default)]
struct Proto {
    kind: String,
    server: String,
    port: u16,
    ssl: Option<bool>,
    encryption: String,
    login: String,
}

impl Proto {
    fn hint(&self, email: &str, default_port: u16) -> ServerHint {
        let security = match self.encryption.as_str() {
            "ssl" => SmtpSecurity::Tls,
            "tls" => SmtpSecurity::Starttls,
            "none" => SmtpSecurity::None,
            _ => match (self.ssl, self.port) {
                (Some(false), _) => SmtpSecurity::None,
                (_, 993) | (_, 465) | (_, 0) => SmtpSecurity::Tls,
                _ => SmtpSecurity::Starttls,
            },
        };
        ServerHint {
            host: self.server.clone(),
            port: if self.port == 0 { default_port } else { self.port },
            security,
            username: if self.login.is_empty() { email.to_string() } else { self.login.clone() },
        }
    }
}

/// Parses an Autodiscover (POX) response: its IMAP and SMTP protocol entries.
pub fn parse_autodiscover(xml: &str, email: &str) -> Option<Autoconfig> {
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut protos: Vec<Proto> = Vec::new();
    let mut cur: Option<Proto> = None;
    let mut field = String::new();
    let mut display = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "Protocol" {
                    cur = Some(Proto::default());
                }
                field = name;
            }
            Ok(Event::Text(t)) => {
                let text = t.decode().map(|c| c.trim().to_string()).unwrap_or_default();
                match (&mut cur, field.as_str()) {
                    (Some(p), "Type") => p.kind = text.to_ascii_uppercase(),
                    (Some(p), "Server") => p.server = text,
                    (Some(p), "Port") => p.port = text.parse().unwrap_or(0),
                    (Some(p), "SSL") => p.ssl = Some(text.eq_ignore_ascii_case("on")),
                    (Some(p), "Encryption") => p.encryption = text.to_ascii_lowercase(),
                    (Some(p), "LoginName") => p.login = text,
                    (None, "DisplayName") if display.is_empty() => display = text,
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                if e.local_name().as_ref() == b"Protocol" {
                    if let Some(p) = cur.take() {
                        protos.push(p);
                    }
                }
                field.clear();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    let find = |kind: &str| protos.iter().find(|p| p.kind == kind && !p.server.is_empty());
    Some(Autoconfig {
        provider: display,
        imap: find("IMAP")?.hint(email, 993),
        smtp: find("SMTP")?.hint(email, 465),
    })
}

fn autodiscover_request(email: &str) -> String {
    let e = email.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Autodiscover xmlns=\"http://schemas.microsoft.com/exchange/autodiscover/outlook/requestschema/2006\">\n<Request><EMailAddress>{e}</EMailAddress><AcceptableResponseSchema>http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a</AcceptableResponseSchema></Request>\n</Autodiscover>"
    )
}

/// Largest autoconfig document read.
const MAX_DOC: usize = 256 * 1024;

/// What the connection test checks.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConnectivityTest {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: SmtpSecurity,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: SmtpSecurity,
    /// With a password the logins are tested too; without, only that the servers answer.
    #[serde(default)]
    pub imap_username: String,
    #[serde(default)]
    pub smtp_username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub smtp_password: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerCheck {
    pub ok: bool,
    /// Whether the login was tested (a password was given).
    pub login: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectivityReport {
    pub imap: ServerCheck,
    pub smtp: ServerCheck,
}

impl Core {
    /// Fetches a document from a domain's own servers through the guarded client.
    async fn fetch_doc(&self, url: &str, post: Option<String>) -> Option<String> {
        let allow_local = self.mail_settings().await.allow_local_servers;
        let url = crate::net::check_url(url, false).ok()?;
        let client = self.mail_http(allow_local).ok()?;
        let req = match post {
            Some(body) => client.post(url).header("Content-Type", "text/xml; charset=utf-8").body(body),
            None => client.get(url),
        };
        let mut resp = tokio::time::timeout(Duration::from_secs(8), req.send()).await.ok()?.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_DOC {
                return None;
            }
        }
        String::from_utf8(body).ok()
    }

    /// Finds server settings for `email` (see the module documentation for the order).
    pub async fn mail_autoconfig(&self, email: &str) -> Option<Discovery> {
        let email = email.trim();
        let domain = domain_of(email)?;
        let enc = super::oauth::form(email);
        let own_url = format!("https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={enc}");
        let well_known_url = format!("https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={enc}");
        let (db, own, well_known) = tokio::join!(ispdb(&domain, email), self.fetch_doc(&own_url, None), self.fetch_doc(&well_known_url, None));
        let found = |config, source| Discovery { config, source, mx: None };
        if let Some(c) = db {
            return Some(found(c, Source::Ispdb));
        }
        if let Some(c) = own
            .as_deref()
            .and_then(|x| parse(x, email))
            .or_else(|| well_known.as_deref().and_then(|x| parse(x, email)))
        {
            return Some(found(c, Source::Autoconfig));
        }
        let ad = self
            .fetch_doc(
                &format!("https://autodiscover.{domain}/autodiscover/autodiscover.xml"),
                Some(autodiscover_request(email)),
            )
            .await;
        if let Some(c) = ad.as_deref().and_then(|x| parse_autodiscover(x, email)) {
            return Some(found(c, Source::Autodiscover));
        }
        let records = match tokio::time::timeout(Duration::from_secs(6), super::dns::lookup_mx(&domain)).await {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::debug!(domain = %domain, error = %e, "MX lookup failed");
                return None;
            }
            Err(_) => return None,
        };
        let mx = best_mx(&records)?.to_string();
        let config = match from_mx(&mx, email) {
            Some(c) => c,
            None => {
                let base = base_domain(&mx);
                if base == domain {
                    return None;
                }
                ispdb(&base, email).await?
            }
        };
        Some(Discovery {
            config,
            source: Source::Mx,
            mx: Some(mx),
        })
    }

    /// Tests IMAP and SMTP separately: with a password, the logins; without, that the servers
    /// answer (TLS or STARTTLS included). Uses the admin's connection policy.
    pub async fn mail_test_connectivity(&self, t: &ConnectivityTest) -> ConnectivityReport {
        let policy = self.mail_policy().await;
        let login = !t.password.is_empty();
        let imap = super::ServerSettings {
            host: t.imap_host.trim().to_string(),
            port: t.imap_port,
            security: t.imap_security,
            username: t.imap_username.trim().to_string(),
            password: t.password.clone(),
            xoauth2: false,
        };
        let imap_r = if imap.host.is_empty() || imap.port == 0 {
            Err(Error::bad("enter the IMAP server and port"))
        } else if login {
            self.test_imap(&imap).await
        } else {
            super::imap::ImapConn::probe(&imap, policy).await
        };
        let smtp_user = if t.smtp_username.trim().is_empty() {
            imap.username.clone()
        } else {
            t.smtp_username.trim().to_string()
        };
        let smtp_pass = if t.smtp_password.is_empty() {
            t.password.clone()
        } else {
            t.smtp_password.clone()
        };
        let smtp = crate::mailer::SmtpSettings {
            host: t.smtp_host.trim().to_string(),
            port: t.smtp_port,
            security: t.smtp_security,
            username: (login && !smtp_user.is_empty()).then_some(smtp_user),
            password: login.then_some(smtp_pass),
            from: String::new(),
        };
        let smtp_r: Result<()> = if smtp.host.is_empty() || smtp.port == 0 {
            Err(Error::bad("enter the SMTP server and port"))
        } else {
            self.test_smtp(&smtp, false).await
        };
        let check = |r: Result<()>| ServerCheck {
            ok: r.is_ok(),
            login,
            error: r.err().map(|e| e.to_string()),
        };
        ConnectivityReport {
            imap: check(imap_r),
            smtp: check(smtp_r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ispdb_documents() {
        let xml = r#"<clientConfig version="1.1"><emailProvider id="example.org">
          <domain>example.org</domain><displayName>Example Mail</displayName>
          <incomingServer type="pop3"><hostname>pop.example.org</hostname><port>995</port><socketType>SSL</socketType><username>%EMAILADDRESS%</username></incomingServer>
          <incomingServer type="imap"><hostname>imap.example.org</hostname><port>993</port><socketType>SSL</socketType><username>%EMAILLOCALPART%</username></incomingServer>
          <outgoingServer type="smtp"><hostname>smtp.%EMAILDOMAIN%</hostname><port>587</port><socketType>STARTTLS</socketType><username>%EMAILADDRESS%</username></outgoingServer>
        </emailProvider></clientConfig>"#;
        let c = parse(xml, "ada@example.org").unwrap();
        assert_eq!(c.provider, "Example Mail");
        assert_eq!(
            c.imap,
            ServerHint {
                host: "imap.example.org".into(),
                port: 993,
                security: SmtpSecurity::Tls,
                username: "ada".into()
            }
        );
        assert_eq!(c.smtp.host, "smtp.example.org");
        assert_eq!((c.smtp.port, c.smtp.security), (587, SmtpSecurity::Starttls));
        assert_eq!(c.smtp.username, "ada@example.org");
        assert!(parse("<clientConfig><emailProvider/></clientConfig>", "a@b.c").is_none());
        assert!(parse("not xml <<<", "a@b.c").is_none());
    }

    #[test]
    fn hosting_providers_from_mx_records() {
        let records = vec![
            Mx {
                preference: 5,
                exchange: "alt1.aspmx.l.google.com".into(),
            },
            Mx {
                preference: 10,
                exchange: "alt2.aspmx.l.google.com".into(),
            },
        ];
        let c = from_mx(best_mx(&records).unwrap(), "ada@corp.example").unwrap();
        assert_eq!(c.provider, "Google Workspace");
        assert_eq!((c.imap.host.as_str(), c.imap.port, c.imap.security), ("imap.gmail.com", 993, SmtpSecurity::Tls));
        assert_eq!(c.smtp.username, "ada@corp.example");
        let ms = from_mx("corp-example.mail.protection.outlook.com", "x@corp.example").unwrap();
        assert_eq!((ms.smtp.host.as_str(), ms.smtp.security), ("smtp.office365.com", SmtpSecurity::Starttls));
        assert_eq!(from_mx("mx01.kundenserver.de.", "a@b.de").unwrap().imap.host, "imap.ionos.de");
        assert_eq!(from_mx("in1-smtp.messagingengine.com", "a@b.c").unwrap().provider, "Fastmail");
        // Suffixes match whole labels only.
        assert!(from_mx("evilgoogle.com", "a@b.c").is_none());
        assert!(from_mx("mx.unknown-hoster.net", "a@b.c").is_none());
        // Null MX (RFC 7505): the domain takes no mail.
        assert_eq!(
            best_mx(&[Mx {
                preference: 0,
                exchange: String::new()
            }]),
            None
        );
        assert_eq!(base_domain("mx01.mail.example-hoster.net"), "example-hoster.net");
        assert_eq!(base_domain("mx.mail.example.co.uk"), "example.co.uk");
        assert_eq!(base_domain("example.org"), "example.org");
        assert_eq!(domain_of("Ada@Example.ORG"), Some("example.org".into()));
        assert_eq!(domain_of("ada@localhost"), None);
        assert_eq!(domain_of("ada@ex ample.org"), None);
        assert_eq!(domain_of("ada@a/b.org"), None);
    }

    #[test]
    fn parses_autodiscover_responses() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Autodiscover xmlns="http://schemas.microsoft.com/exchange/autodiscover/responseschema/2006">
  <Response xmlns="http://schemas.microsoft.com/exchange/autodiscover/outlook/responseschema/2006a">
    <User><DisplayName>Ada</DisplayName></User>
    <Account><AccountType>email</AccountType><Action>settings</Action>
      <Protocol><Type>EXCH</Type><Server>ex.corp.example</Server></Protocol>
      <Protocol><Type>IMAP</Type><Server>imap.corp.example</Server><Port>993</Port><SSL>on</SSL><LoginName>ada</LoginName></Protocol>
      <Protocol><Type>SMTP</Type><Server>smtp.corp.example</Server><Port>587</Port><Encryption>TLS</Encryption></Protocol>
    </Account>
  </Response>
</Autodiscover>"#;
        let c = parse_autodiscover(xml, "ada@corp.example").unwrap();
        assert_eq!(c.imap.host, "imap.corp.example");
        assert_eq!((c.imap.port, c.imap.security, c.imap.username.as_str()), (993, SmtpSecurity::Tls, "ada"));
        assert_eq!(
            (c.smtp.port, c.smtp.security, c.smtp.username.as_str()),
            (587, SmtpSecurity::Starttls, "ada@corp.example")
        );
        assert!(parse_autodiscover("<Autodiscover><Response><Error/></Response></Autodiscover>", "a@b.c").is_none());
        assert!(autodiscover_request("a<b>@c.d").contains("<EMailAddress>a&lt;b&gt;@c.d</EMailAddress>"));
    }
}
