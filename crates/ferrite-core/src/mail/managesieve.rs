//! A ManageSieve client (RFC 5804): capabilities, STARTTLS (or implicit TLS), SASL PLAIN,
//! and the script commands (LISTSCRIPTS, GETSCRIPT, CHECKSCRIPT, PUTSCRIPT, SETACTIVE,
//! DELETESCRIPT). Every network step has a timeout, like the IMAP client.
//!
//! The protocol is line based: a response is any number of data lines followed by a status
//! line (`OK`, `NO` or `BYE`, an optional response code in parentheses and an optional
//! human-readable string). Strings are quoted (`"a \"b\""`) or literals (`{12}` CRLF and
//! twelve octets); the client always sends scripts as non-synchronizing literals (`{12+}`).

use super::ServerSettings;
use super::imap::{MailStream, tls_wrap};
use crate::mailer::SmtpSecurity;
use crate::{Error, Result};
use base64::Engine;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const IO_TIMEOUT: Duration = Duration::from_secs(60);
/// Longest response line or literal accepted (scripts are small; this bounds memory).
const MAX_LITERAL: usize = 4 * 1024 * 1024;

/// One token of a response line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tok {
    Atom(String),
    /// A quoted string or a literal.
    Str(String),
    Open,
    Close,
}

/// How a command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    No,
    Bye,
}

/// The status line of a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub outcome: Outcome,
    /// The response code, e.g. `WARNINGS`, `NONEXISTENT`, `QUOTA/MAXSIZE`.
    pub code: Option<String>,
    pub message: String,
}

/// What the server announced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub implementation: String,
    pub sasl: Vec<String>,
    /// Sieve extensions the server supports.
    pub sieve: Vec<String>,
    pub starttls: bool,
    /// Present on RFC 5804 servers (which know CHECKSCRIPT).
    pub version: Option<String>,
}

impl Capabilities {
    fn from_lines(lines: &[Vec<Tok>]) -> Capabilities {
        let mut c = Capabilities::default();
        for l in lines {
            let name = match l.first() {
                Some(Tok::Str(s)) | Some(Tok::Atom(s)) => s.to_ascii_uppercase(),
                _ => continue,
            };
            let value = match l.get(1) {
                Some(Tok::Str(s)) | Some(Tok::Atom(s)) => s.clone(),
                _ => String::new(),
            };
            match name.as_str() {
                "IMPLEMENTATION" => c.implementation = value,
                "SASL" => c.sasl = value.split_whitespace().map(str::to_ascii_uppercase).collect(),
                "SIEVE" => c.sieve = value.split_whitespace().map(str::to_ascii_lowercase).collect(),
                "STARTTLS" => c.starttls = true,
                "VERSION" => c.version = Some(value),
                _ => {}
            }
        }
        c
    }
}

/// A script on the server.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ScriptInfo {
    pub name: String,
    pub active: bool,
}

/// Quotes a string for a command (names, SASL data). Strings that a quoted string cannot
/// carry are sent as literals by [`SieveConn::put_script`]/[`SieveConn::check_script`].
pub fn quote(s: &str) -> String {
    let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n' && *c != '\0').collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Splits one response line (without its CRLF) into tokens. A trailing `{n}`/`{n+}` is a
/// literal that follows on the next lines: it is returned as `Err(n)` so the caller can read
/// it and continue the line.
fn tokenize(line: &str, out: &mut Vec<Tok>) -> std::result::Result<(), usize> {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' => i += 1,
            b'(' => {
                out.push(Tok::Open);
                i += 1;
            }
            b')' => {
                out.push(Tok::Close);
                i += 1;
            }
            b'"' => {
                let mut s = Vec::new();
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    s.push(b[i]);
                    i += 1;
                }
                i += 1;
                out.push(Tok::Str(String::from_utf8_lossy(&s).into_owned()));
            }
            b'{' => {
                let end = line[i..].find('}').map(|e| i + e).unwrap_or(b.len());
                let n = line[i + 1..end].trim_end_matches('+').trim().parse::<usize>().unwrap_or(0);
                return Err(n);
            }
            _ => {
                let start = i;
                while i < b.len() && !matches!(b[i], b' ' | b'(' | b')' | b'"') {
                    i += 1;
                }
                out.push(Tok::Atom(line[start..i].to_string()));
            }
        }
    }
    Ok(())
}

/// Parses a status line, or None for a data line.
fn status_of(toks: &[Tok]) -> Option<Status> {
    let outcome = match toks.first() {
        Some(Tok::Atom(a)) if a.eq_ignore_ascii_case("OK") => Outcome::Ok,
        Some(Tok::Atom(a)) if a.eq_ignore_ascii_case("NO") => Outcome::No,
        Some(Tok::Atom(a)) if a.eq_ignore_ascii_case("BYE") => Outcome::Bye,
        _ => return None,
    };
    let mut rest = &toks[1..];
    let mut code = None;
    if rest.first() == Some(&Tok::Open) {
        let close = rest.iter().position(|t| *t == Tok::Close).unwrap_or(rest.len());
        code = rest[1..close].iter().find_map(|t| match t {
            Tok::Atom(a) => Some(a.to_ascii_uppercase()),
            _ => None,
        });
        rest = rest.get(close + 1..).unwrap_or(&[]);
    }
    let message = rest
        .iter()
        .filter_map(|t| match t {
            Tok::Str(s) | Tok::Atom(s) => Some(s.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(Status { outcome, code, message })
}

async fn timed<T>(what: &str, f: impl std::future::Future<Output = std::io::Result<T>>) -> Result<T> {
    match tokio::time::timeout(IO_TIMEOUT, f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(Error::Unavailable(format!("ManageSieve {what}: {e}"))),
        Err(_) => Err(Error::Unavailable(format!("ManageSieve {what} timed out"))),
    }
}

/// An authenticated ManageSieve session.
pub struct SieveConn {
    io: BufReader<MailStream>,
    pub caps: Capabilities,
}

impl SieveConn {
    /// Connects, secures the connection as configured (STARTTLS is required, not
    /// opportunistic) and logs in with SASL PLAIN.
    pub async fn connect(s: &ServerSettings) -> Result<SieveConn> {
        let host = s.host.trim();
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, s.port)))
            .await
            .map_err(|_| Error::Unavailable(format!("connecting to {host}:{} timed out", s.port)))?
            .map_err(|e| Error::Unavailable(format!("cannot connect to {host}:{}: {e}", s.port)))?;
        let _ = tcp.set_nodelay(true);
        let stream = match s.security {
            SmtpSecurity::Tls => tls_wrap(host, tcp).await?,
            _ => MailStream::Plain(tcp),
        };
        let mut conn = SieveConn {
            io: BufReader::new(stream),
            caps: Capabilities::default(),
        };
        conn.read_capabilities().await?;
        if s.security == SmtpSecurity::Starttls {
            if !conn.caps.starttls {
                return Err(Error::bad(format!("{host} does not offer STARTTLS for ManageSieve")));
            }
            conn.send("STARTTLS\r\n").await?;
            conn.expect_ok("STARTTLS").await?;
            let SieveConn { io, .. } = conn;
            let MailStream::Plain(tcp) = io.into_inner() else {
                return Err(Error::internal("STARTTLS on a TLS connection"));
            };
            conn = SieveConn {
                io: BufReader::new(tls_wrap(host, tcp).await?),
                caps: Capabilities::default(),
            };
            // RFC 5804 §2.2: the server sends its capabilities again after the handshake.
            conn.read_capabilities().await?;
        }
        conn.authenticate(&s.username, &s.password).await?;
        Ok(conn)
    }

    async fn read_capabilities(&mut self) -> Result<()> {
        let (lines, st) = self.response("greeting").await?;
        if st.outcome != Outcome::Ok {
            return Err(Error::Unavailable(format!("the ManageSieve server refused the connection: {}", st.message)));
        }
        self.caps = Capabilities::from_lines(&lines);
        Ok(())
    }

    async fn authenticate(&mut self, user: &str, pass: &str) -> Result<()> {
        if !self.caps.sasl.iter().any(|m| m == "PLAIN") {
            return Err(Error::bad(format!(
                "the ManageSieve server does not offer SASL PLAIN (it offers: {})",
                self.caps.sasl.join(", ")
            )));
        }
        let token = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"));
        self.send(&format!("AUTHENTICATE \"PLAIN\" \"{token}\"\r\n")).await?;
        let (_, st) = self.response("AUTHENTICATE").await?;
        match st.outcome {
            Outcome::Ok => Ok(()),
            _ => Err(Error::bad(format!(
                "ManageSieve login failed: {}",
                if st.message.is_empty() { "wrong user name or password" } else { &st.message }
            ))),
        }
    }

    async fn send(&mut self, data: &str) -> Result<()> {
        let w = self.io.get_mut();
        timed("write", async {
            w.write_all(data.as_bytes()).await?;
            w.flush().await
        })
        .await
    }

    /// Reads one logical line: a physical line plus any literals it continues with.
    async fn read_line(&mut self, what: &str) -> Result<Vec<Tok>> {
        let mut toks = Vec::new();
        loop {
            let mut raw = Vec::new();
            let n = timed(what, self.io.read_until(b'\n', &mut raw)).await?;
            if n == 0 {
                return Err(Error::Unavailable("the ManageSieve server closed the connection".into()));
            }
            if raw.len() > MAX_LITERAL {
                return Err(Error::Unavailable("ManageSieve response line too long".into()));
            }
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\r', '\n']);
            match tokenize(line, &mut toks) {
                Ok(()) => return Ok(toks),
                Err(len) => {
                    if len > MAX_LITERAL {
                        return Err(Error::Unavailable("ManageSieve literal too large".into()));
                    }
                    let mut buf = vec![0u8; len];
                    timed(what, self.io.read_exact(&mut buf)).await?;
                    toks.push(Tok::Str(String::from_utf8_lossy(&buf).into_owned()));
                    // The rest of the logical line follows the literal.
                }
            }
        }
    }

    /// Data lines and the status line of one response.
    async fn response(&mut self, what: &str) -> Result<(Vec<Vec<Tok>>, Status)> {
        let mut lines = Vec::new();
        loop {
            let toks = self.read_line(what).await?;
            if let Some(st) = status_of(&toks) {
                return Ok((lines, st));
            }
            lines.push(toks);
        }
    }

    async fn expect_ok(&mut self, what: &str) -> Result<(Vec<Vec<Tok>>, Status)> {
        let (lines, st) = self.response(what).await?;
        match st.outcome {
            Outcome::Ok => Ok((lines, st)),
            _ => Err(refused(what, &st)),
        }
    }

    pub async fn list_scripts(&mut self) -> Result<Vec<ScriptInfo>> {
        self.send("LISTSCRIPTS\r\n").await?;
        let (lines, _) = self.expect_ok("LISTSCRIPTS").await?;
        Ok(lines
            .iter()
            .filter_map(|l| {
                let name = match l.first() {
                    Some(Tok::Str(s)) => s.clone(),
                    _ => return None,
                };
                let active = matches!(l.get(1), Some(Tok::Atom(a)) if a.eq_ignore_ascii_case("ACTIVE"));
                Some(ScriptInfo { name, active })
            })
            .collect())
    }

    /// The active script's name, if one is active.
    pub async fn active_script(&mut self) -> Result<Option<String>> {
        Ok(self.list_scripts().await?.into_iter().find(|s| s.active).map(|s| s.name))
    }

    pub async fn get_script(&mut self, name: &str) -> Result<String> {
        self.send(&format!("GETSCRIPT {}\r\n", quote(name))).await?;
        let (lines, _) = self.expect_ok("GETSCRIPT").await?;
        Ok(lines
            .into_iter()
            .flatten()
            .find_map(|t| match t {
                Tok::Str(s) => Some(s),
                _ => None,
            })
            .unwrap_or_default())
    }

    /// Asks the server to validate a script without storing it (CHECKSCRIPT; on servers
    /// predating RFC 5804 this is skipped and PUTSCRIPT reports errors instead). Errors carry
    /// the server's message, which names the line and problem.
    pub async fn check_script(&mut self, script: &str) -> Result<()> {
        if self.caps.version.is_none() {
            return Ok(());
        }
        self.send(&format!("CHECKSCRIPT {{{}+}}\r\n{script}\r\n", script.len())).await?;
        let (_, st) = self.response("CHECKSCRIPT").await?;
        match st.outcome {
            Outcome::Ok => Ok(()),
            _ => Err(Error::bad(script_error(&st))),
        }
    }

    pub async fn put_script(&mut self, name: &str, script: &str) -> Result<()> {
        self.send(&format!("PUTSCRIPT {} {{{}+}}\r\n{script}\r\n", quote(name), script.len())).await?;
        let (_, st) = self.response("PUTSCRIPT").await?;
        match st.outcome {
            Outcome::Ok => Ok(()),
            _ => Err(Error::bad(script_error(&st))),
        }
    }

    pub async fn set_active(&mut self, name: &str) -> Result<()> {
        self.send(&format!("SETACTIVE {}\r\n", quote(name))).await?;
        self.expect_ok("SETACTIVE").await.map(|_| ())
    }

    pub async fn delete_script(&mut self, name: &str) -> Result<()> {
        self.send(&format!("DELETESCRIPT {}\r\n", quote(name))).await?;
        self.expect_ok("DELETESCRIPT").await.map(|_| ())
    }

    pub async fn logout(mut self) {
        if self.send("LOGOUT\r\n").await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(5), self.response("LOGOUT")).await;
        }
    }
}

fn refused(what: &str, st: &Status) -> Error {
    let msg = if st.message.is_empty() {
        "no reason given".to_string()
    } else {
        st.message.clone()
    };
    match st.outcome {
        Outcome::Bye => Error::Unavailable(format!("the ManageSieve server closed the session ({what}): {msg}")),
        _ => Error::bad(format!("the ManageSieve server refused {what}: {msg}")),
    }
}

fn script_error(st: &Status) -> String {
    let msg = st.message.trim();
    match st.code.as_deref() {
        Some(c) if c.starts_with("QUOTA") => format!("the script is too large for the server's quota: {msg}"),
        _ if msg.is_empty() => "the server rejected the script".to_string(),
        _ => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn tokenizes_lines() {
        let mut t = Vec::new();
        tokenize(r#""SIEVE" "fileinto vacation""#, &mut t).unwrap();
        assert_eq!(t, vec![Tok::Str("SIEVE".into()), Tok::Str("fileinto vacation".into())]);
        let mut t = Vec::new();
        tokenize(r#"NO (QUOTA/MAXSIZE) "Too \"big\"""#, &mut t).unwrap();
        let st = status_of(&t).unwrap();
        assert_eq!(
            st,
            Status {
                outcome: Outcome::No,
                code: Some("QUOTA/MAXSIZE".into()),
                message: "Too \"big\"".into()
            }
        );
        let mut t = Vec::new();
        assert_eq!(tokenize("\"x\" {12}", &mut t), Err(12));
        assert_eq!(tokenize("{3+}", &mut Vec::new()), Err(3));
        let mut t = Vec::new();
        tokenize(r#""a" ACTIVE"#, &mut t).unwrap();
        assert!(status_of(&t).is_none());
        assert_eq!(quote(r#"my "x" \y"#), r#""my \"x\" \\y""#);
    }

    /// A scripted ManageSieve server: answers each command it receives with the next reply
    /// (after the greeting) and records what it was sent.
    async fn fake(greeting: &'static str, replies: Vec<&'static str>) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        let h = tokio::spawn(async move {
            let (sock, _) = l.accept().await.unwrap();
            let (r, mut w) = sock.into_split();
            let mut r = tokio::io::BufReader::new(r);
            w.write_all(greeting.as_bytes()).await.unwrap();
            let mut got = Vec::new();
            for reply in replies {
                let mut line = String::new();
                if r.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                // Read a non-synchronizing literal that ends the command.
                if let Some(n) = line
                    .trim_end()
                    .strip_suffix("+}")
                    .and_then(|x| x.rsplit_once('{'))
                    .and_then(|(_, n)| n.parse::<usize>().ok())
                {
                    let mut buf = vec![0u8; n + 2];
                    r.read_exact(&mut buf).await.unwrap();
                    line.push_str(&String::from_utf8_lossy(&buf));
                }
                got.push(line);
                w.write_all(reply.as_bytes()).await.unwrap();
            }
            got
        });
        (port, h)
    }

    const GREETING: &str = "\"IMPLEMENTATION\" \"Fake Sieve\"\r\n\"SIEVE\" \"fileinto vacation imap4flags\"\r\n\"SASL\" \"PLAIN LOGIN\"\r\n\"VERSION\" \"1.0\"\r\nOK \"Ready.\"\r\n";

    fn settings(port: u16, security: SmtpSecurity) -> ServerSettings {
        ServerSettings {
            host: "127.0.0.1".into(),
            port,
            security,
            username: "alice@example.test".into(),
            password: "pw".into(),
        }
    }

    #[tokio::test]
    async fn speaks_the_protocol() {
        let script = "require \"fileinto\";\r\nfileinto \"Ünïcode\";\r\n";
        let (port, server) = fake(
            GREETING,
            vec![
                "OK \"Logged in.\"\r\n",
                "\"old\"\r\n\"main\" ACTIVE\r\nOK \"Listscripts completed.\"\r\n",
                "{28}\r\nrequire \"fileinto\";\r\nkeep;\r\n\r\nOK \"Getscript completed.\"\r\n",
                "NO \"line 2: error: unknown command 'fileintoo'.\"\r\n",
                "OK (WARNINGS) \"line 1: warning: deprecated\"\r\n",
                "OK\r\n",
                "OK\r\n",
                "NO (NONEXISTENT) \"There is no script by that name.\"\r\n",
                "OK \"Logout completed.\"\r\n",
            ],
        )
        .await;
        let mut c = SieveConn::connect(&settings(port, SmtpSecurity::None)).await.unwrap();
        assert_eq!(c.caps.implementation, "Fake Sieve");
        assert!(c.caps.sieve.contains(&"vacation".to_string()) && c.caps.version.is_some() && !c.caps.starttls);
        assert_eq!(
            c.list_scripts().await.unwrap(),
            vec![
                ScriptInfo {
                    name: "old".into(),
                    active: false
                },
                ScriptInfo {
                    name: "main".into(),
                    active: true
                }
            ]
        );
        assert_eq!(c.get_script("main").await.unwrap(), "require \"fileinto\";\r\nkeep;\r\n");
        let err = c.check_script("fileintoo \"x\";").await.unwrap_err();
        assert!(err.to_string().contains("unknown command 'fileintoo'"), "{err}");
        c.check_script(script).await.unwrap();
        c.put_script("main", script).await.unwrap();
        c.set_active("main").await.unwrap();
        assert!(c.delete_script("nope").await.unwrap_err().to_string().contains("no script by that name"));
        c.logout().await;
        let sent = server.await.unwrap();
        assert_eq!(
            sent[0],
            format!(
                "AUTHENTICATE \"PLAIN\" \"{}\"\r\n",
                base64::engine::general_purpose::STANDARD.encode("\0alice@example.test\0pw")
            )
        );
        assert_eq!(sent[1], "LISTSCRIPTS\r\n");
        assert_eq!(sent[2], "GETSCRIPT \"main\"\r\n");
        assert_eq!(sent[4], format!("CHECKSCRIPT {{{}+}}\r\n{script}\r\n", script.len()));
        assert_eq!(sent[5], format!("PUTSCRIPT \"main\" {{{}+}}\r\n{script}\r\n", script.len()));
        assert_eq!(sent[6], "SETACTIVE \"main\"\r\n");
        assert_eq!(sent[8], "LOGOUT\r\n");
    }

    #[tokio::test]
    async fn login_failures_and_missing_mechanisms() {
        let (port, _s) = fake(GREETING, vec!["NO \"Authentication failed.\"\r\n"]).await;
        let e = SieveConn::connect(&settings(port, SmtpSecurity::None)).await.err().unwrap();
        assert!(e.to_string().contains("ManageSieve login failed: Authentication failed."), "{e}");
        let (port, _s) = fake("\"SASL\" \"LOGIN\"\r\nOK\r\n", vec![]).await;
        let e = SieveConn::connect(&settings(port, SmtpSecurity::None)).await.err().unwrap();
        assert!(e.to_string().contains("does not offer SASL PLAIN"), "{e}");
    }

    #[tokio::test]
    async fn starttls_is_required_and_negotiated() {
        // No STARTTLS offered: refuse rather than send the password in the clear.
        let (port, _s) = fake(GREETING, vec![]).await;
        let e = SieveConn::connect(&settings(port, SmtpSecurity::Starttls)).await.err().unwrap();
        assert!(e.to_string().contains("does not offer STARTTLS"), "{e}");
        // Offered: the client sends STARTTLS and starts a TLS handshake (which this plain
        // fake cannot complete), never the credentials.
        let (port, server) = fake(
            "\"SASL\" \"PLAIN\"\r\n\"STARTTLS\"\r\n\"VERSION\" \"1.0\"\r\nOK\r\n",
            vec!["OK \"Begin TLS negotiation now.\"\r\n"],
        )
        .await;
        let e = SieveConn::connect(&settings(port, SmtpSecurity::Starttls)).await.err().unwrap();
        assert!(e.to_string().contains("TLS"), "{e}");
        let sent = server.await.unwrap();
        assert_eq!(sent[0], "STARTTLS\r\n");
        assert!(!sent.iter().any(|l| l.contains("AUTHENTICATE")));
    }
}
