//! Server settings for well-known providers from Thunderbird's ISPDB
//! (`https://autoconfig.thunderbird.net/v1.1/<domain>`). Lookups are best effort: any
//! failure (offline server, unknown domain, odd XML) just means "fill the form by hand".

use crate::mailer::SmtpSecurity;
use quick_xml::events::Event;
use serde::Serialize;
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

/// Looks up `email`'s domain in the ISPDB. `None` when unknown or unreachable.
pub async fn lookup(email: &str) -> Option<Autoconfig> {
    let domain = email.rsplit_once('@')?.1.trim().to_ascii_lowercase();
    if domain.is_empty() || !domain.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-') {
        return None;
    }
    let client = reqwest::Client::builder().timeout(Duration::from_secs(8)).build().ok()?;
    let resp = client.get(format!("{ISPDB}{domain}")).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse(&body, email.trim())
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
}
