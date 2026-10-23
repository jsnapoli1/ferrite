//! IMAP connections (TLS, STARTTLS or plain) and a small per-account pool.
//!
//! Each account gets at most [`POOL_SIZE`] command connections, reused across requests and
//! closed after [`IDLE_CLOSE`] without use; IDLE (see `sync.rs`) holds one more while the
//! owner is online. Every network operation has a timeout, so a dead server never wedges a
//! request.

use super::ServerSettings;
use crate::mailer::SmtpSecurity;
use crate::{Error, Result};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use parking_lot::Mutex;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_rustls::rustls;

/// Command connections per account.
pub const POOL_SIZE: usize = 2;
/// Pooled connections unused this long are closed.
pub const IDLE_CLOSE: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Upper bound for any single command (large FETCHes included).
const IO_TIMEOUT: Duration = Duration::from_secs(120);
/// A pooled connection unused for longer than this is checked with NOOP before reuse.
const RECHECK_AFTER: Duration = Duration::from_secs(30);

/// The byte stream under an IMAP session.
#[derive(Debug)]
pub enum MailStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for MailStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MailStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_flush(cx),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            MailStream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            MailStream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

pub type Session = async_imap::Session<MailStream>;

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots = rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            };
            let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
            let config = rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("the default provider supports the safe protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

pub(crate) async fn tls_wrap(host: &str, tcp: TcpStream) -> Result<MailStream> {
    let name = rustls_pki_types::ServerName::try_from(host.to_string()).map_err(|_| Error::bad(format!("invalid IMAP host name {host:?}")))?;
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, tokio_rustls::TlsConnector::from(tls_config()).connect(name, tcp))
        .await
        .map_err(|_| Error::Unavailable(format!("TLS handshake with {host} timed out")))?
        .map_err(|e| Error::Unavailable(format!("TLS with {host} failed: {e}")))?;
    Ok(MailStream::Tls(Box::new(tls)))
}

fn imap_err(e: async_imap::error::Error) -> Error {
    use async_imap::error::Error as E;
    match e {
        E::No(m) => Error::bad(format!("the mail server refused: {m}")),
        E::Bad(m) => Error::bad(format!("the mail server rejected a command: {m}")),
        E::Validate(v) => Error::bad(format!("invalid value: {v}")),
        other => Error::Unavailable(format!("IMAP: {other}")),
    }
}

/// Whether an error leaves the connection unusable (as opposed to a refused command).
fn fatal(e: &async_imap::error::Error) -> bool {
    use async_imap::error::Error as E;
    !matches!(e, E::No(_) | E::Bad(_) | E::Validate(_))
}

/// Server capabilities that change how commands are issued.
#[derive(Debug, Clone, Copy, Default)]
pub struct Caps {
    pub idle: bool,
    pub mv: bool,
    pub uidplus: bool,
    pub condstore: bool,
}

/// One message as fetched, owned.
#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub uid: u32,
    pub flags: Flags,
    pub size: u32,
    pub internal_date: Option<chrono::DateTime<chrono::Utc>>,
    pub header: Vec<u8>,
    pub text_prefix: Vec<u8>,
    pub body: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub seen: bool,
    pub flagged: bool,
    pub answered: bool,
    pub draft: bool,
    pub deleted: bool,
}

impl Flags {
    fn from_fetch(f: &Fetch) -> Self {
        let mut out = Flags::default();
        for flag in f.flags() {
            match flag {
                Flag::Seen => out.seen = true,
                Flag::Flagged => out.flagged = true,
                Flag::Answered => out.answered = true,
                Flag::Draft => out.draft = true,
                Flag::Deleted => out.deleted = true,
                _ => {}
            }
        }
        out
    }
}

/// Mailbox state as reported by SELECT.
#[derive(Debug, Clone, Copy, Default)]
pub struct Selected {
    pub exists: u32,
    pub uidvalidity: u32,
    pub uidnext: Option<u32>,
    pub modseq: Option<u64>,
}

/// A LIST entry.
#[derive(Debug, Clone)]
pub struct ListedFolder {
    pub name: String,
    pub delimiter: Option<String>,
    pub role: Option<&'static str>,
    /// The role came from a special-use attribute (preferred over a name match).
    pub special_use: bool,
    pub selectable: bool,
}

/// Counts from STATUS.
#[derive(Debug, Clone, Copy, Default)]
pub struct Status {
    pub total: u32,
    pub unread: u32,
    pub uidnext: Option<u32>,
    pub uidvalidity: Option<u32>,
}

/// Formats UIDs as a compact IMAP sequence set ("1:4,9,12:13").
pub fn uid_set(uids: &[u32]) -> String {
    let mut v: Vec<u32> = uids.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}:{end}") });
        i += 1;
    }
    parts.join(",")
}

/// Quotes a string for an IMAP command (mailbox names, search terms).
pub fn quote(s: &str) -> String {
    let clean: String = s.chars().filter(|c| *c != '\r' && *c != '\n' && *c != '\0').collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Folder role from special-use attributes, else from well-known names.
pub fn role_of(name: &str, delimiter: Option<&str>, attrs: &[NameAttribute<'_>]) -> Option<&'static str> {
    special_use(attrs).or_else(|| role_from_name(name, delimiter))
}

/// The RFC 6154 special-use role among `attrs`.
pub fn special_use(attrs: &[NameAttribute<'_>]) -> Option<&'static str> {
    attrs.iter().find_map(|a| match a {
        NameAttribute::Sent => Some("sent"),
        NameAttribute::Drafts => Some("drafts"),
        NameAttribute::Trash => Some("trash"),
        NameAttribute::Junk => Some("junk"),
        NameAttribute::Archive => Some("archive"),
        _ => None,
    })
}

pub fn role_from_name(name: &str, delimiter: Option<&str>) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("INBOX") {
        return Some("inbox");
    }
    let leaf = delimiter.and_then(|d| name.rsplit(d).next()).unwrap_or(name).to_ascii_lowercase();
    // Only top-level or INBOX children (servers that nest everything under INBOX).
    let depth_ok = match delimiter {
        Some(d) => {
            let parts: Vec<&str> = name.split(d).collect();
            parts.len() == 1 || (parts.len() == 2 && parts[0].eq_ignore_ascii_case("INBOX"))
        }
        None => true,
    };
    if !depth_ok {
        return None;
    }
    match leaf.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" => Some("sent"),
        "drafts" | "draft" => Some("drafts"),
        "trash" | "deleted items" | "deleted messages" | "bin" => Some("trash"),
        "junk" | "spam" | "junk e-mail" | "junk email" => Some("junk"),
        "archive" | "archives" => Some("archive"),
        _ => None,
    }
}

/// An authenticated IMAP session with its capabilities and selected mailbox.
pub struct ImapConn {
    session: Session,
    pub caps: Caps,
    /// The selected mailbox and its UIDVALIDITY.
    selected: Option<(String, u32)>,
    /// Set after an I/O or protocol error: the connection is dropped instead of pooled.
    broken: bool,
}

async fn timed<T>(what: &str, f: impl std::future::Future<Output = std::result::Result<T, async_imap::error::Error>>) -> std::result::Result<T, Error> {
    match tokio::time::timeout(IO_TIMEOUT, f).await {
        Ok(r) => r.map_err(imap_err),
        Err(_) => Err(Error::Unavailable(format!("IMAP {what} timed out"))),
    }
}

impl ImapConn {
    /// Connects and logs in.
    pub async fn connect(s: &ServerSettings) -> Result<ImapConn> {
        let host = s.host.trim();
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, s.port)))
            .await
            .map_err(|_| Error::Unavailable(format!("connecting to {host}:{} timed out", s.port)))?
            .map_err(|e| Error::Unavailable(format!("cannot connect to {host}:{}: {e}", s.port)))?;
        let _ = tcp.set_nodelay(true);
        let greeting_timeout = |_| Error::Unavailable(format!("{host} sent no IMAP greeting"));
        let stream = match s.security {
            SmtpSecurity::Tls => tls_wrap(host, tcp).await?,
            SmtpSecurity::None => MailStream::Plain(tcp),
            SmtpSecurity::Starttls => {
                let mut client = async_imap::Client::new(MailStream::Plain(tcp));
                tokio::time::timeout(CONNECT_TIMEOUT, client.read_response())
                    .await
                    .map_err(greeting_timeout)?
                    .map_err(|e| Error::Unavailable(format!("IMAP greeting: {e}")))?
                    .ok_or_else(|| Error::Unavailable(format!("{host} closed the connection")))?;
                timed("STARTTLS", client.run_command_and_check_ok("STARTTLS", None)).await?;
                let MailStream::Plain(tcp) = client.into_inner() else {
                    unreachable!("the stream is plain before STARTTLS")
                };
                tls_wrap(host, tcp).await?
            }
        };
        let mut client = async_imap::Client::new(stream);
        if s.security != SmtpSecurity::Starttls {
            tokio::time::timeout(CONNECT_TIMEOUT, client.read_response())
                .await
                .map_err(greeting_timeout)?
                .map_err(|e| Error::Unavailable(format!("IMAP greeting: {e}")))?
                .ok_or_else(|| Error::Unavailable(format!("{host} closed the connection")))?;
        }
        let session = match tokio::time::timeout(IO_TIMEOUT, client.login(&s.username, &s.password)).await {
            Ok(Ok(session)) => session,
            Ok(Err((e, _))) => {
                return Err(match e {
                    async_imap::error::Error::No(m) | async_imap::error::Error::Bad(m) => Error::bad(format!("IMAP login failed: {m}")),
                    other => Error::Unavailable(format!("IMAP login failed: {other}")),
                });
            }
            Err(_) => return Err(Error::Unavailable("IMAP login timed out".into())),
        };
        let mut conn = ImapConn {
            session,
            caps: Caps::default(),
            selected: None,
            broken: false,
        };
        let caps = conn.run("CAPABILITY", |s| Box::pin(s.capabilities())).await?;
        conn.caps = Caps {
            idle: caps.has_str("IDLE"),
            mv: caps.has_str("MOVE"),
            uidplus: caps.has_str("UIDPLUS"),
            condstore: caps.has_str("CONDSTORE"),
        };
        Ok(conn)
    }

    /// Runs one command with the timeout, marking the connection broken on fatal errors and
    /// discarding unsolicited responses (state is re-read on every sync instead).
    async fn run<'a, T>(
        &'a mut self,
        what: &str,
        f: impl FnOnce(&'a mut Session) -> Pin<Box<dyn std::future::Future<Output = std::result::Result<T, async_imap::error::Error>> + Send + 'a>>,
    ) -> Result<T> {
        let broken = &mut self.broken;
        let r = tokio::time::timeout(IO_TIMEOUT, f(&mut self.session)).await;
        match r {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                if fatal(&e) {
                    *broken = true;
                }
                Err(imap_err(e))
            }
            Err(_) => {
                *broken = true;
                Err(Error::Unavailable(format!("IMAP {what} timed out")))
            }
        }
    }

    fn drain(&mut self) {
        while self.session.unsolicited_responses.try_recv().is_ok() {}
    }

    pub fn is_broken(&self) -> bool {
        self.broken
    }

    pub async fn noop(&mut self) -> Result<()> {
        self.run("NOOP", |s| Box::pin(s.noop())).await?;
        self.drain();
        Ok(())
    }

    pub async fn list(&mut self) -> Result<Vec<ListedFolder>> {
        let names = self
            .run("LIST", |s| {
                Box::pin(async move {
                    let stream = s.list(Some(""), Some("*")).await?;
                    stream.try_collect::<Vec<_>>().await
                })
            })
            .await?;
        self.drain();
        Ok(names
            .iter()
            .map(|n| ListedFolder {
                name: n.name().to_string(),
                delimiter: n.delimiter().map(String::from),
                role: role_of(n.name(), n.delimiter(), n.attributes()),
                special_use: special_use(n.attributes()).is_some(),
                selectable: !n.attributes().iter().any(|a| matches!(a, NameAttribute::NoSelect)),
            })
            .collect())
    }

    pub async fn status(&mut self, name: &str) -> Result<Status> {
        let quoted = name.to_string();
        let mb = self
            .run("STATUS", |s| Box::pin(s.status(quoted, "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY)")))
            .await?;
        self.drain();
        Ok(Status {
            total: mb.exists,
            unread: mb.unseen.unwrap_or(0),
            uidnext: mb.uid_next,
            uidvalidity: mb.uid_validity,
        })
    }

    /// SELECTs `name` (with CONDSTORE where supported).
    pub async fn select(&mut self, name: &str) -> Result<Selected> {
        let n = name.to_string();
        let condstore = self.caps.condstore;
        self.selected = None;
        let mb = self
            .run("SELECT", |s| {
                Box::pin(async move { if condstore { s.select_condstore(n).await } else { s.select(n).await } })
            })
            .await?;
        self.drain();
        self.selected = Some((name.to_string(), mb.uid_validity.unwrap_or(0)));
        Ok(Selected {
            exists: mb.exists,
            uidvalidity: mb.uid_validity.unwrap_or(0),
            uidnext: mb.uid_next,
            modseq: mb.highest_modseq,
        })
    }

    /// Selects `name` unless it already is; returns its UIDVALIDITY. A mailbox that stayed
    /// selected gets a NOOP first: the server only acts on UIDs it has announced to this
    /// session, and NOOP is where it announces mail delivered since.
    pub async fn ensure_selected(&mut self, name: &str) -> Result<u32> {
        match self.selected.clone() {
            Some((n, v)) if n == name => {
                self.noop().await?;
                Ok(v)
            }
            _ => Ok(self.select(name).await?.uidvalidity),
        }
    }

    /// UID FETCH of `set` with the given items; collects owned results.
    pub async fn uid_fetch(&mut self, set: &str, items: &str) -> Result<Vec<Fetched>> {
        let (set, items) = (set.to_string(), items.to_string());
        let fetches = self
            .run("FETCH", |s| {
                Box::pin(async move {
                    let stream = s.uid_fetch(set, items).await?;
                    stream.try_collect::<Vec<_>>().await
                })
            })
            .await?;
        self.drain();
        Ok(fetches.iter().filter_map(owned_fetch).collect())
    }

    /// FETCH by sequence numbers (used for the newest N messages of a folder).
    pub async fn seq_fetch(&mut self, set: &str, items: &str) -> Result<Vec<Fetched>> {
        let (set, items) = (set.to_string(), items.to_string());
        let fetches = self
            .run("FETCH", |s| {
                Box::pin(async move {
                    let stream = s.fetch(set, items).await?;
                    stream.try_collect::<Vec<_>>().await
                })
            })
            .await?;
        self.drain();
        Ok(fetches.iter().filter_map(owned_fetch).collect())
    }

    /// The UIDs in `set` whose MIME structure has a calendar part (`text/calendar` or
    /// `application/ics`): meeting invitations and replies, found without fetching bodies.
    pub async fn uids_with_calendar(&mut self, set: &str) -> Result<Vec<u32>> {
        let set = set.to_string();
        let fetches = self
            .run("FETCH", |s| {
                Box::pin(async move {
                    let stream = s.uid_fetch(set, "(UID BODYSTRUCTURE)").await?;
                    stream.try_collect::<Vec<_>>().await
                })
            })
            .await?;
        self.drain();
        Ok(fetches
            .iter()
            .filter(|f| f.bodystructure().is_some_and(has_calendar_part))
            .filter_map(|f| f.uid)
            .collect())
    }

    pub async fn uid_search(&mut self, query: &str) -> Result<Vec<u32>> {
        let q = query.to_string();
        let found = self.run("SEARCH", |s| Box::pin(s.uid_search(q))).await?;
        self.drain();
        let mut v: Vec<u32> = found.into_iter().collect();
        v.sort_unstable();
        Ok(v)
    }

    /// Number of unseen messages in the selected mailbox.
    pub async fn count_unseen(&mut self) -> Result<u32> {
        let found = self.run("SEARCH", |s| Box::pin(s.search("UNSEEN"))).await?;
        self.drain();
        Ok(found.len() as u32)
    }

    /// `UID STORE set +FLAGS (...)` / `-FLAGS`; `op` is e.g. `+FLAGS.SILENT (\Seen)`.
    pub async fn uid_store(&mut self, set: &str, op: &str) -> Result<()> {
        let (set, op) = (set.to_string(), op.to_string());
        self.run("STORE", |s| {
            Box::pin(async move {
                let stream = s.uid_store(set, op).await?;
                stream.try_collect::<Vec<_>>().await
            })
        })
        .await?;
        self.drain();
        Ok(())
    }

    /// Moves messages to `dest`: MOVE where supported, else COPY + \Deleted + expunge.
    pub async fn uid_move(&mut self, set: &str, dest: &str) -> Result<()> {
        let (set_s, dest_s) = (set.to_string(), dest.to_string());
        if self.caps.mv {
            self.run("MOVE", |s| Box::pin(s.uid_mv(set_s, dest_s))).await?;
            self.drain();
            return Ok(());
        }
        self.run("COPY", |s| Box::pin(s.uid_copy(set_s, dest_s))).await?;
        self.uid_store(set, "+FLAGS.SILENT (\\Deleted)").await?;
        self.expunge(set).await
    }

    /// Removes \Deleted messages: only `set` with UIDPLUS, else the whole mailbox.
    pub async fn expunge(&mut self, set: &str) -> Result<()> {
        let set = set.to_string();
        if self.caps.uidplus {
            self.run("EXPUNGE", |s| {
                Box::pin(async move {
                    let stream = s.uid_expunge(set).await?;
                    stream.try_collect::<Vec<_>>().await
                })
            })
            .await?;
        } else {
            self.run("EXPUNGE", |s| {
                Box::pin(async move {
                    let stream = s.expunge().await?;
                    stream.try_collect::<Vec<_>>().await
                })
            })
            .await?;
        }
        self.drain();
        Ok(())
    }

    pub async fn append(&mut self, folder: &str, flags: &str, raw: &[u8]) -> Result<()> {
        let (folder, flags, raw) = (folder.to_string(), flags.to_string(), raw.to_vec());
        self.run("APPEND", |s| Box::pin(async move { s.append(folder, Some(flags.as_str()), None, raw).await }))
            .await?;
        self.drain();
        Ok(())
    }

    pub async fn create(&mut self, folder: &str) -> Result<()> {
        let f = folder.to_string();
        self.run("CREATE", |s| Box::pin(s.create(f))).await?;
        let f = folder.to_string();
        // Subscribing is a courtesy for other clients; servers may refuse it harmlessly.
        let _ = self.run("SUBSCRIBE", |s| Box::pin(s.subscribe(f))).await;
        self.drain();
        Ok(())
    }

    /// Deletes a mailbox (and its messages).
    pub async fn delete(&mut self, folder: &str) -> Result<()> {
        let f = folder.to_string();
        if self.selected.as_ref().is_some_and(|(n, _)| n == folder) {
            self.selected = None;
        }
        self.run("DELETE", |s| Box::pin(s.delete(f))).await?;
        self.drain();
        Ok(())
    }

    pub async fn logout(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.session.logout()).await;
    }

    /// Waits on IDLE for up to `wait`. Returns the connection afterwards and whether the
    /// server reported a change (false on timeout).
    pub async fn idle(self, wait: Duration) -> Result<(ImapConn, bool)> {
        let ImapConn { session, caps, selected, .. } = self;
        let mut handle = session.idle();
        timed("IDLE", handle.init()).await?;
        let (fut, _stop) = handle.wait_with_timeout(wait);
        let changed = match fut.await.map_err(imap_err)? {
            async_imap::extensions::idle::IdleResponse::NewData(_) => true,
            async_imap::extensions::idle::IdleResponse::Timeout | async_imap::extensions::idle::IdleResponse::ManualInterrupt => false,
        };
        let session = timed("IDLE DONE", handle.done()).await?;
        let mut conn = ImapConn {
            session,
            caps,
            selected,
            broken: false,
        };
        conn.drain();
        Ok((conn, changed))
    }
}

/// Whether a BODYSTRUCTURE contains a `text/calendar` or `application/ics` part.
fn has_calendar_part(bs: &async_imap::imap_proto::BodyStructure<'_>) -> bool {
    use async_imap::imap_proto::BodyStructure as B;
    let is_cal = |c: &async_imap::imap_proto::BodyContentCommon<'_>| {
        let (t, s) = (c.ty.ty.to_ascii_lowercase(), c.ty.subtype.to_ascii_lowercase());
        (t == "text" && s == "calendar") || (t == "application" && s == "ics")
    };
    match bs {
        B::Basic { common, .. } | B::Text { common, .. } => is_cal(common),
        B::Message { body, .. } => has_calendar_part(body),
        B::Multipart { bodies, .. } => bodies.iter().any(has_calendar_part),
    }
}

fn owned_fetch(f: &Fetch) -> Option<Fetched> {
    Some(Fetched {
        uid: f.uid?,
        flags: Flags::from_fetch(f),
        size: f.size.unwrap_or(0),
        internal_date: f.internal_date().map(|d| d.with_timezone(&chrono::Utc)),
        header: f.header().map(<[u8]>::to_vec).unwrap_or_default(),
        text_prefix: f.text().map(<[u8]>::to_vec).unwrap_or_default(),
        body: f.body().map(<[u8]>::to_vec),
    })
}

/// Pooled connections of one account.
pub struct Pool {
    slots: Arc<Semaphore>,
    idle: Mutex<Vec<(ImapConn, Instant)>>,
}

impl Default for Pool {
    fn default() -> Self {
        Pool {
            slots: Arc::new(Semaphore::new(POOL_SIZE)),
            idle: Mutex::new(Vec::new()),
        }
    }
}

/// A connection borrowed from a [`Pool`]; returned to it on drop unless broken.
pub struct PooledConn {
    conn: Option<ImapConn>,
    pool: Arc<Pool>,
    _permit: OwnedSemaphorePermit,
}

impl std::ops::Deref for PooledConn {
    type Target = ImapConn;
    fn deref(&self) -> &ImapConn {
        self.conn.as_ref().expect("present until drop")
    }
}

impl std::ops::DerefMut for PooledConn {
    fn deref_mut(&mut self) -> &mut ImapConn {
        self.conn.as_mut().expect("present until drop")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            if !c.broken {
                self.pool.idle.lock().push((c, Instant::now()));
            }
        }
    }
}

impl Pool {
    /// Borrows a connection, reusing an idle one (checked with NOOP when it sat for a while)
    /// or connecting a new one. Waits while all [`POOL_SIZE`] are busy.
    pub async fn get(self: &Arc<Self>, settings: &ServerSettings) -> Result<PooledConn> {
        let permit = tokio::time::timeout(IO_TIMEOUT, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| Error::Unavailable("all connections to the mail server are busy".into()))?
            .map_err(|_| Error::internal("mail pool closed"))?;
        loop {
            let reused = self.idle.lock().pop();
            let Some((mut conn, since)) = reused else { break };
            if since.elapsed() < RECHECK_AFTER || conn.noop().await.is_ok() {
                return Ok(PooledConn {
                    conn: Some(conn),
                    pool: self.clone(),
                    _permit: permit,
                });
            }
        }
        let conn = ImapConn::connect(settings).await?;
        Ok(PooledConn {
            conn: Some(conn),
            pool: self.clone(),
            _permit: permit,
        })
    }

    /// Closes connections idle for longer than `max_idle`; returns how many remain open.
    pub fn reap(&self, max_idle: Duration) -> usize {
        let mut idle = self.idle.lock();
        let (keep, close): (Vec<_>, Vec<_>) = std::mem::take(&mut *idle).into_iter().partition(|(_, t)| t.elapsed() < max_idle);
        *idle = keep;
        let open = idle.len();
        drop(idle);
        for (c, _) in close {
            tokio::spawn(c.logout());
        }
        open
    }

    /// Closes every idle connection (account changed or removed).
    pub fn close_all(&self) {
        for (c, _) in std::mem::take(&mut *self.idle.lock()) {
            tokio::spawn(c.logout());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_sets_are_compact() {
        assert_eq!(uid_set(&[5, 1, 2, 3, 9, 11, 12, 3]), "1:3,5,9,11:12");
        assert_eq!(uid_set(&[7]), "7");
        assert_eq!(uid_set(&[]), "");
    }

    #[test]
    fn quoting_escapes() {
        assert_eq!(quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(quote("x\r\ny"), "\"xy\"");
    }

    #[test]
    fn roles_from_attributes_and_names() {
        assert_eq!(role_of("Gesendet", Some("/"), &[NameAttribute::Sent]), Some("sent"));
        assert_eq!(role_of("INBOX", Some("/"), &[]), Some("inbox"));
        assert_eq!(role_of("INBOX.Trash", Some("."), &[]), Some("trash"));
        assert_eq!(role_of("Sent Items", Some("/"), &[]), Some("sent"));
        assert_eq!(role_of("Projects/Archive", Some("/"), &[]), None);
        assert_eq!(role_of("Spam", None, &[]), Some("junk"));
        assert_eq!(role_of("Receipts", Some("/"), &[]), None);
    }
}
