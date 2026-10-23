//! Folder management: create, rename, move, delete, subscribe; mark all read, empty, statistics
//! and cache repair; the folders chosen for each role; shared folders (RFC 2342 namespaces);
//! trash retention and junk handling.
//!
//! Every change is made on the IMAP server first and then mirrored in the cache, keeping
//! folder ids (and cached messages) across renames.

use super::Account;
use super::delegation::MailRight;
use super::imap::{self, ImapConn, MailStream, uid_set};
use super::store::{FOLDER_COLS, Folder};
use crate::util::now;
use crate::{Core, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Encodes a mailbox name in RFC 3501 modified UTF-7 (the inverse of
/// [`super::parse::decode_mailbox_name`]).
pub fn encode_mailbox_name(name: &str) -> String {
    use base64::Engine;
    let mut out = String::new();
    let mut pending: Vec<u16> = Vec::new();
    let flush = |pending: &mut Vec<u16>, out: &mut String| {
        if pending.is_empty() {
            return;
        }
        let bytes: Vec<u8> = pending.iter().flat_map(|u| u.to_be_bytes()).collect();
        let b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes).replace('/', ",");
        out.push('&');
        out.push_str(&b64);
        out.push('-');
        pending.clear();
    };
    for c in name.chars() {
        if (' '..='~').contains(&c) {
            flush(&mut pending, &mut out);
            if c == '&' {
                out.push_str("&-");
            } else {
                out.push(c);
            }
        } else {
            let mut buf = [0u16; 2];
            pending.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut pending, &mut out);
    out
}

/// The server's namespaces (RFC 2342): (prefix, delimiter) per kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Namespaces {
    pub personal: Vec<(String, Option<String>)>,
    pub other_users: Vec<(String, Option<String>)>,
    pub shared: Vec<(String, Option<String>)>,
}

impl Namespaces {
    /// Whether a mailbox lives in another user's or a shared namespace.
    pub fn is_shared(&self, name: &str) -> bool {
        self.other_users.iter().chain(&self.shared).any(|(prefix, delim)| {
            if prefix.is_empty() {
                return false;
            }
            let root = delim.as_deref().and_then(|d| prefix.strip_suffix(d)).unwrap_or(prefix);
            name.starts_with(prefix.as_str()) || name == root
        })
    }

    /// The personal namespace's prefix (e.g. "INBOX." on servers that keep every folder
    /// under the inbox).
    pub fn personal_prefix(&self) -> &str {
        self.personal.first().map(|(p, _)| p.as_str()).unwrap_or("")
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Open,
    Close,
    Nil,
    Str(String),
}

fn tokens(s: &str) -> Option<Vec<Tok>> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' => {
                chars.next();
            }
            '(' => {
                chars.next();
                out.push(Tok::Open);
            }
            ')' => {
                chars.next();
                out.push(Tok::Close);
            }
            '"' => {
                chars.next();
                let mut v = String::new();
                loop {
                    match chars.next()? {
                        '\\' => v.push(chars.next()?),
                        '"' => break,
                        x => v.push(x),
                    }
                }
                out.push(Tok::Str(v));
            }
            _ => {
                let mut v = String::new();
                while let Some(&x) = chars.peek() {
                    if x == ' ' || x == '(' || x == ')' {
                        break;
                    }
                    v.push(x);
                    chars.next();
                }
                if v.eq_ignore_ascii_case("NIL") {
                    out.push(Tok::Nil);
                } else {
                    out.push(Tok::Str(v));
                }
            }
        }
    }
    Some(out)
}

/// Parses the data of `* NAMESPACE (personal) (other users) (shared)`.
pub fn parse_namespace(line: &str) -> Option<Namespaces> {
    let rest = line.trim().strip_prefix("* ")?.trim_start();
    let rest = rest.get(..9).filter(|p| p.eq_ignore_ascii_case("NAMESPACE")).map(|_| &rest[9..])?;
    let toks = tokens(rest)?;
    let mut i = 0;
    let mut groups: Vec<Vec<(String, Option<String>)>> = Vec::new();
    while i < toks.len() && groups.len() < 3 {
        match &toks[i] {
            Tok::Nil => {
                groups.push(vec![]);
                i += 1;
            }
            Tok::Open => {
                i += 1;
                let mut group = Vec::new();
                while i < toks.len() && toks[i] == Tok::Open {
                    // ("prefix" "delim" [extensions...])
                    i += 1;
                    let prefix = match toks.get(i)? {
                        Tok::Str(s) => s.clone(),
                        _ => return None,
                    };
                    i += 1;
                    let delim = match toks.get(i)? {
                        Tok::Str(s) => Some(s.clone()),
                        Tok::Nil => None,
                        _ => return None,
                    };
                    i += 1;
                    // Skip extension data up to this entry's closing parenthesis.
                    let mut depth = 1;
                    while depth > 0 {
                        match toks.get(i)? {
                            Tok::Open => depth += 1,
                            Tok::Close => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                    group.push((prefix, delim));
                }
                if toks.get(i)? != &Tok::Close {
                    return None;
                }
                i += 1;
                groups.push(group);
            }
            _ => return None,
        }
    }
    let mut g = groups.into_iter();
    Some(Namespaces {
        personal: g.next().unwrap_or_default(),
        other_users: g.next().unwrap_or_default(),
        shared: g.next().unwrap_or_default(),
    })
}

const NS_TIMEOUT: Duration = Duration::from_secs(20);

/// Asks the server for its namespaces on a short connection of its own (the IMAP library
/// does not parse NAMESPACE responses), opened like every mail connection (`policy`: TLS
/// verification, local servers). Servers without NAMESPACE give empty namespaces.
pub async fn fetch_namespaces(s: &super::ServerSettings, policy: imap::ConnectPolicy) -> Result<Namespaces> {
    tokio::time::timeout(NS_TIMEOUT * 3, fetch_namespaces_inner(s, policy))
        .await
        .map_err(|_| Error::Unavailable("asking the mail server for its namespaces timed out".into()))?
}

async fn read_tagged<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R, tag: &str, untagged: &mut Vec<String>) -> Result<String> {
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(NS_TIMEOUT, r.read_line(&mut line))
            .await
            .map_err(|_| Error::Unavailable("IMAP timed out".into()))?
            .map_err(|e| Error::Unavailable(format!("IMAP: {e}")))?;
        if n == 0 {
            return Err(Error::Unavailable("the mail server closed the connection".into()));
        }
        if let Some(rest) = line.strip_prefix(&format!("{tag} ")) {
            return Ok(rest.trim().to_string());
        }
        untagged.push(line.trim_end().to_string());
    }
}

async fn fetch_namespaces_inner(s: &super::ServerSettings, policy: imap::ConnectPolicy) -> Result<Namespaces> {
    use base64::Engine;
    // Connected, secured and greeted; the rest is spoken directly.
    let stream: MailStream = ImapConn::open(s, policy).await?.into_inner();
    let mut r = BufReader::new(stream);
    let io = |e: std::io::Error| Error::Unavailable(format!("IMAP: {e}"));
    let login = if s.xoauth2 {
        let token = base64::engine::general_purpose::STANDARD.encode(super::oauth::xoauth2(&s.username, &s.password));
        format!("n1 AUTHENTICATE XOAUTH2 {token}\r\n")
    } else {
        format!("n1 LOGIN {} {}\r\n", imap::quote(&s.username), imap::quote(&s.password))
    };
    r.get_mut().write_all(login.as_bytes()).await.map_err(io)?;
    let mut ignored = Vec::new();
    let st = read_tagged(&mut r, "n1", &mut ignored).await?;
    if !st.starts_with("OK") {
        return Err(Error::bad(format!("IMAP login failed: {st}")));
    }
    r.get_mut().write_all(b"n2 NAMESPACE\r\n").await.map_err(io)?;
    let mut untagged = Vec::new();
    let st = read_tagged(&mut r, "n2", &mut untagged).await?;
    let _ = r.get_mut().write_all(b"n3 LOGOUT\r\n").await;
    if !st.starts_with("OK") {
        return Ok(Namespaces::default());
    }
    Ok(untagged.iter().find_map(|l| parse_namespace(l)).unwrap_or_default())
}

/// Folder statistics for the folder dialog.
#[derive(Debug, Clone, Serialize)]
pub struct FolderStats {
    pub name: String,
    pub total: i64,
    pub unread: i64,
    pub flagged: i64,
    pub cached: i64,
    pub cached_bytes: i64,
    pub bodies_cached: i64,
    pub subscribed: bool,
    pub shared: bool,
    pub synced_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Rejects names IMAP clients trip over (wildcards, the hierarchy delimiter, control
/// characters).
fn check_leaf(name: &str, delimiter: Option<&str>) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(Error::bad("a folder needs a name"));
    }
    if n.chars().count() > 200 {
        return Err(Error::bad("folder names are at most 200 characters"));
    }
    if n.chars().any(|c| c.is_control() || c == '*' || c == '%') {
        return Err(Error::bad("folder names can't contain * or % or control characters"));
    }
    if let Some(d) = delimiter.filter(|d| n.contains(*d)) {
        return Err(Error::bad(format!("folder names can't contain “{d}”")));
    }
    Ok(n.to_string())
}

/// Whether `name` is `ancestor` or below it.
fn within(name: &str, ancestor: &str, delimiter: Option<&str>) -> bool {
    name == ancestor || delimiter.is_some_and(|d| name.starts_with(&format!("{ancestor}{d}")))
}

impl Core {
    async fn account_folders(&self, account_id: &str) -> Result<Vec<Folder>> {
        Ok(
            sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {FOLDER_COLS} FROM mail_folders WHERE account_id = ?")))
                .bind(account_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    async fn namespaces_of(&self, account_id: &str) -> Result<Option<Namespaces>> {
        Ok(self.account_prefs_row(account_id).await?.namespaces.and_then(|s| serde_json::from_str(&s).ok()))
    }

    /// After a folder list sync: subscriptions (LSUB), the user's role choices, the Snoozed
    /// folder and shared-folder markers.
    pub(crate) async fn apply_folder_meta(&self, conn: &mut ImapConn, a: &Account) -> Result<()> {
        let subscribed = match conn.lsub().await {
            Ok(v) => Some(v),
            Err(e) if conn.is_broken() => return Err(e),
            Err(e) => {
                tracing::debug!(error = %e, "LSUB failed");
                None
            }
        };
        let folders = self.account_folders(&a.id).await?;
        let overrides = self.role_overrides(&a.id).await?;
        let mut tx = self.mail_tx().await?;
        if let Some(subs) = subscribed.filter(|s| !s.is_empty()) {
            for f in &folders {
                // Containers (\Noselect) follow their children.
                let on = !f.selectable || f.role.as_deref() == Some("inbox") || subs.iter().any(|s| *s == f.name);
                if on != f.subscribed {
                    sqlx::query("UPDATE mail_folders SET subscribed = ? WHERE id = ?")
                        .bind(on)
                        .bind(&f.id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }
        for (role, name) in &overrides {
            if folders.iter().any(|f| f.name == *name) {
                sqlx::query("UPDATE mail_folders SET role = NULL WHERE account_id = ? AND role = ? AND name <> ?")
                    .bind(&a.id)
                    .bind(role)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query("UPDATE mail_folders SET role = ? WHERE account_id = ? AND name = ?")
                    .bind(role)
                    .bind(&a.id)
                    .bind(name)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if !overrides.contains_key("snoozed") {
            // A folder named Snoozed (at the top or under the inbox) is the Snoozed folder.
            if let Some(f) = folders.iter().find(|f| {
                let leaf = f.delimiter.as_deref().and_then(|d| f.name.rsplit(d).next()).unwrap_or(&f.name);
                let top = f.delimiter.as_deref().is_none_or(|d| {
                    let parts: Vec<&str> = f.name.split(d).collect();
                    parts.len() == 1 || (parts.len() == 2 && parts[0].eq_ignore_ascii_case("INBOX"))
                });
                leaf.eq_ignore_ascii_case("snoozed") && top && f.role.is_none() && f.selectable
            }) {
                sqlx::query("UPDATE mail_folders SET role = 'snoozed' WHERE id = ?")
                    .bind(&f.id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        match self.namespaces_of(&a.id).await? {
            Some(ns) => self.mark_shared(a, &ns, Some(conn)).await?,
            None => self.load_namespaces_soon(a),
        }
        Ok(())
    }

    /// Marks folders of other users' and shared namespaces. A folder seen shared for the
    /// first time is subscribed, so what others share shows up (unsubscribing it later sticks).
    async fn mark_shared(&self, a: &Account, ns: &Namespaces, conn: Option<&mut ImapConn>) -> Result<()> {
        let mut newly: Vec<Folder> = Vec::new();
        for f in self.account_folders(&a.id).await? {
            let shared = ns.is_shared(&f.name);
            if shared != f.shared {
                sqlx::query("UPDATE mail_folders SET shared = ? WHERE id = ?")
                    .bind(shared)
                    .bind(&f.id)
                    .execute(&self.db)
                    .await?;
                if shared && !f.subscribed && f.selectable {
                    newly.push(f);
                }
            }
        }
        if newly.is_empty() {
            return Ok(());
        }
        let mut pooled = None;
        let conn = match conn {
            Some(c) => c,
            None => &mut **pooled.insert(self.imap_conn(a).await?),
        };
        for f in newly {
            match conn.subscribe(&f.name, true).await {
                Ok(()) => {
                    sqlx::query("UPDATE mail_folders SET subscribed = 1 WHERE id = ?")
                        .bind(&f.id)
                        .execute(&self.db)
                        .await?;
                }
                Err(e) if conn.is_broken() => return Err(e),
                Err(e) => tracing::debug!(folder = %f.name, error = %e, "subscribing a shared folder failed"),
            }
        }
        Ok(())
    }

    /// Asks for the namespaces in the background (once per account, and again on repair).
    fn load_namespaces_soon(&self, a: &Account) {
        let key = format!("namespaces:{}", a.id);
        if self.mail.last_sync.get(&key).is_some_and(|t| t.elapsed() < Duration::from_secs(600)) {
            return;
        }
        self.mail.last_sync.insert(key, std::time::Instant::now());
        let (core, a) = (self.clone(), a.clone());
        tokio::spawn(async move {
            if let Err(e) = core.load_namespaces(&a).await {
                tracing::info!(account = %a.id, error = %e, "reading the mail server's namespaces failed");
            }
        });
    }

    pub async fn load_namespaces(&self, a: &Account) -> Result<Namespaces> {
        let fresh = self.mail_fresh_credentials(a).await?;
        let ns = fetch_namespaces(&self.imap_settings(&fresh)?, self.mail_policy().await).await?;
        self.account_prefs_row(&a.id).await?;
        sqlx::query("UPDATE mail_account_prefs SET namespaces = ? WHERE account_id = ?")
            .bind(serde_json::to_string(&ns)?)
            .bind(&a.id)
            .execute(&self.db)
            .await?;
        self.mark_shared(a, &ns, None).await?;
        self.mail_publish(a, &json!({"type": "mail.changed", "account_id": a.id, "what": "folders"}));
        Ok(ns)
    }

    fn folders_changed(&self, a: &Account) {
        self.mail_publish(a, &json!({"type": "mail.changed", "account_id": a.id, "what": "folders"}));
    }

    /// The delimiter to use under `parent` (or at the top).
    async fn delimiter_for(&self, account_id: &str, parent: Option<&Folder>) -> Result<Option<String>> {
        if let Some(p) = parent {
            return Ok(p.delimiter.clone());
        }
        Ok(
            sqlx::query_scalar("SELECT delimiter FROM mail_folders WHERE account_id = ? AND delimiter IS NOT NULL ORDER BY role = 'inbox' DESC LIMIT 1")
                .bind(account_id)
                .fetch_optional(&self.db)
                .await?
                .flatten(),
        )
    }

    /// The IMAP name for a new folder `leaf` under `parent` (or at the top of the personal
    /// namespace).
    async fn child_name(&self, a: &Account, parent: Option<&Folder>, leaf: &str) -> Result<String> {
        let delim = self.delimiter_for(&a.id, parent).await?;
        let leaf = check_leaf(leaf, delim.as_deref())?;
        let encoded = encode_mailbox_name(&leaf);
        Ok(match parent {
            Some(p) => format!("{}{}{encoded}", p.name, delim.as_deref().unwrap_or("/")),
            None => {
                let prefix = self.namespaces_of(&a.id).await?.map(|n| n.personal_prefix().to_string()).unwrap_or_default();
                format!("{prefix}{encoded}")
            }
        })
    }

    /// Creates a folder (subscribed), at the top or under `parent_id`.
    pub async fn create_mail_folder(&self, user_id: &str, account_id: &str, name: &str, parent_id: Option<&str>) -> Result<Folder> {
        let a = self.mail_access(user_id, account_id, MailRight::Modify).await?;
        let parent = match parent_id.filter(|p| !p.is_empty()) {
            Some(pid) => {
                let (pa, p) = self.mail_folder(user_id, pid).await?;
                if pa.id != a.id {
                    return Err(Error::bad("the parent folder belongs to another account"));
                }
                Some(p)
            }
            None => None,
        };
        let full = self.child_name(&a, parent.as_ref(), name).await?;
        if self.account_folders(&a.id).await?.iter().any(|f| f.name.eq_ignore_ascii_case(&full)) {
            return Err(Error::conflict("a folder with that name exists"));
        }
        let mut conn = self.imap_conn(&a).await?;
        conn.create(&full).await?;
        self.sync_folders_on(&mut conn, &a).await?;
        drop(conn);
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {FOLDER_COLS} FROM mail_folders WHERE account_id = ? AND name = ?"
        )))
        .bind(&a.id)
        .bind(&full)
        .fetch_optional(&self.db)
        .await?
        .ok_or_else(|| Error::Unavailable("the server did not list the new folder".into()))
    }

    /// Renames a folder and/or moves it under another parent ("" for the top). Its
    /// subfolders move along; cached messages stay.
    pub async fn rename_mail_folder(&self, user_id: &str, folder_id: &str, name: Option<&str>, parent_id: Option<&str>) -> Result<Folder> {
        let (a, f) = self.mail_folder_access(user_id, folder_id, MailRight::Modify).await?;
        if f.role.as_deref() == Some("inbox") {
            return Err(Error::bad("the inbox can't be renamed or moved"));
        }
        if f.shared {
            return Err(Error::bad("shared folders are managed by their owner"));
        }
        let delim = f.delimiter.clone();
        let current_leaf_raw = delim.as_deref().and_then(|d| f.name.rsplit(d).next()).unwrap_or(&f.name).to_string();
        let current_parent = delim.as_deref().and_then(|d| f.name.rsplit_once(d).map(|(p, _)| p.to_string()));
        let all = self.account_folders(&a.id).await?;
        let new_parent: Option<Folder> = match parent_id {
            None => current_parent.as_ref().and_then(|p| all.iter().find(|x| x.name == *p).cloned()),
            Some("") => None,
            Some(pid) => {
                let p = all.iter().find(|x| x.id == pid).cloned().ok_or(Error::NotFound)?;
                if within(&p.name, &f.name, delim.as_deref()) {
                    return Err(Error::bad("a folder can't move into itself"));
                }
                if p.shared {
                    return Err(Error::bad("folders can't move into a shared folder"));
                }
                Some(p)
            }
        };
        let new_name = match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(leaf) => self.child_name(&a, new_parent.as_ref(), leaf).await?,
            None => match &new_parent {
                Some(p) => format!("{}{}{current_leaf_raw}", p.name, p.delimiter.as_deref().or(delim.as_deref()).unwrap_or("/")),
                None if parent_id == Some("") => {
                    let prefix = self.namespaces_of(&a.id).await?.map(|n| n.personal_prefix().to_string()).unwrap_or_default();
                    format!("{prefix}{current_leaf_raw}")
                }
                None => f.name.clone(),
            },
        };
        if new_name == f.name {
            return Ok(f);
        }
        if all.iter().any(|x| x.name.eq_ignore_ascii_case(&new_name) && x.id != f.id) {
            return Err(Error::conflict("a folder with that name exists"));
        }
        let mut conn = self.imap_conn(&a).await?;
        conn.rename(&f.name, &new_name).await?;
        // Keep ids and cached messages: rename the folder and its subfolders in the cache.
        let mut tx = self.mail_tx().await?;
        for x in all.iter().filter(|x| within(&x.name, &f.name, delim.as_deref())) {
            let renamed = format!("{new_name}{}", &x.name[f.name.len()..]);
            let leaf = x.delimiter.as_deref().and_then(|d| renamed.rsplit(d).next()).unwrap_or(&renamed).to_string();
            sqlx::query("UPDATE mail_folders SET name = ?, display_name = ? WHERE id = ?")
                .bind(&renamed)
                .bind(super::parse::decode_mailbox_name(&leaf))
                .bind(&x.id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE mail_snoozed SET src_folder = ? WHERE account_id = ? AND src_folder = ?")
                .bind(&renamed)
                .bind(&a.id)
                .bind(&x.name)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        // RENAME leaves subscriptions alone (RFC 3501): carry them over to the new names.
        for x in all.iter().filter(|x| within(&x.name, &f.name, delim.as_deref()) && x.subscribed) {
            let renamed = format!("{new_name}{}", &x.name[f.name.len()..]);
            for (name, on) in [(renamed.as_str(), true), (x.name.as_str(), false)] {
                if let Err(e) = conn.subscribe(name, on).await {
                    if conn.is_broken() {
                        return Err(e);
                    }
                    tracing::debug!(error = %e, "moving a subscription failed");
                }
            }
        }
        let mut overrides = self.role_overrides(&a.id).await?;
        let mut touched = false;
        for v in overrides.values_mut() {
            if within(v, &f.name, delim.as_deref()) {
                *v = format!("{new_name}{}", &v[f.name.len()..]);
                touched = true;
            }
        }
        if touched {
            sqlx::query("UPDATE mail_account_prefs SET roles = ? WHERE account_id = ?")
                .bind(serde_json::to_string(&overrides)?)
                .bind(&a.id)
                .execute(&self.db)
                .await?;
        }
        conn.deselect();
        self.sync_folders_on(&mut conn, &a).await?;
        drop(conn);
        self.drop_stale_connections(&a.id);
        self.folder_row(&f.id).await
    }

    /// Servers end sessions that had a renamed or deleted mailbox selected (Dovecot sends
    /// BYE): close the account's idle pooled connections rather than hand out dead ones.
    fn drop_stale_connections(&self, account_id: &str) {
        self.mail.pool(account_id).close_all();
    }

    /// Deletes a folder, its subfolders and their messages on the server.
    pub async fn delete_mail_folder(&self, user_id: &str, folder_id: &str) -> Result<()> {
        let (a, f) = self.mail_folder_access(user_id, folder_id, MailRight::Modify).await?;
        if f.role.as_deref() == Some("inbox") {
            return Err(Error::bad("the inbox can't be deleted"));
        }
        if f.shared {
            return Err(Error::bad("shared folders are managed by their owner"));
        }
        let mut doomed: Vec<Folder> = self
            .account_folders(&a.id)
            .await?
            .into_iter()
            .filter(|x| within(&x.name, &f.name, f.delimiter.as_deref()))
            .collect();
        // Deepest first, so each is empty of children when deleted.
        doomed.sort_by_key(|x| std::cmp::Reverse(x.name.len()));
        let mut conn = self.imap_conn(&a).await?;
        for x in &doomed {
            match conn.delete(&x.name).await {
                Ok(()) => {}
                // A \Noselect parent that disappeared with its last child.
                Err(e) if !conn.is_broken() && x.id != f.id && !x.selectable => tracing::debug!(error = %e, "deleting a placeholder folder failed"),
                Err(e) => return Err(e),
            }
            // Servers keep subscriptions of deleted mailboxes; drop them too.
            if x.subscribed {
                if let Err(e) = conn.subscribe(&x.name, false).await {
                    if conn.is_broken() {
                        return Err(e);
                    }
                    tracing::debug!(error = %e, "dropping a deleted folder's subscription failed");
                }
            }
            sqlx::query("DELETE FROM mail_folders WHERE id = ?").bind(&x.id).execute(&self.db).await?;
        }
        let mut overrides = self.role_overrides(&a.id).await?;
        let before = overrides.len();
        overrides.retain(|_, v| !within(v, &f.name, f.delimiter.as_deref()));
        if overrides.len() != before {
            sqlx::query("UPDATE mail_account_prefs SET roles = ? WHERE account_id = ?")
                .bind(serde_json::to_string(&overrides)?)
                .bind(&a.id)
                .execute(&self.db)
                .await?;
        }
        self.sync_folders_on(&mut conn, &a).await?;
        drop(conn);
        self.drop_stale_connections(&a.id);
        Ok(())
    }

    pub async fn subscribe_mail_folder(&self, user_id: &str, folder_id: &str, on: bool) -> Result<Folder> {
        let (a, f) = self.mail_folder_access(user_id, folder_id, MailRight::Modify).await?;
        if !on && f.role.as_deref() == Some("inbox") {
            return Err(Error::bad("the inbox is always shown"));
        }
        let mut conn = self.imap_conn(&a).await?;
        conn.subscribe(&f.name, on).await?;
        drop(conn);
        sqlx::query("UPDATE mail_folders SET subscribed = ? WHERE id = ?")
            .bind(on)
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        self.folders_changed(&a);
        self.folder_row(&f.id).await
    }

    /// Marks every message of a folder read (also those not cached).
    pub async fn mark_folder_read(&self, user_id: &str, folder_id: &str) -> Result<()> {
        let (a, f) = self.mail_folder_access(user_id, folder_id, MailRight::Modify).await?;
        if !f.selectable {
            return Err(Error::bad("that folder holds no messages"));
        }
        let mut conn = self.imap_conn(&a).await?;
        let sel = conn.select(&f.name).await?;
        if sel.exists > 0 {
            conn.uid_store("1:*", "+FLAGS.SILENT (\\Seen)").await?;
        }
        drop(conn);
        sqlx::query("UPDATE mail_messages SET seen = 1 WHERE folder_id = ?")
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        sqlx::query("UPDATE mail_folders SET unread = 0 WHERE id = ?")
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        self.mail_changed(&a, &[&f.id]);
        self.folders_changed(&a);
        Ok(())
    }

    /// Deletes every message of a folder for good.
    pub async fn empty_mail_folder(&self, user_id: &str, folder_id: &str) -> Result<u32> {
        let (a, f) = self.mail_folder_access(user_id, folder_id, MailRight::Modify).await?;
        if !f.selectable {
            return Err(Error::bad("that folder holds no messages"));
        }
        let mut conn = self.imap_conn(&a).await?;
        let sel = conn.select(&f.name).await?;
        if sel.exists > 0 {
            conn.uid_store("1:*", "+FLAGS.SILENT (\\Deleted)").await?;
            conn.expunge("1:*").await?;
        }
        drop(conn);
        sqlx::query("DELETE FROM mail_messages WHERE folder_id = ?")
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        sqlx::query("UPDATE mail_folders SET total = 0, unread = 0, complete = 1 WHERE id = ?")
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM mail_trash_seen WHERE account_id = ? AND ? = 'trash'")
            .bind(&a.id)
            .bind(f.role.as_deref().unwrap_or(""))
            .execute(&self.db)
            .await?;
        self.mail_changed(&a, &[&f.id]);
        self.folders_changed(&a);
        Ok(sel.exists)
    }

    pub async fn mail_folder_stats(&self, user_id: &str, folder_id: &str) -> Result<FolderStats> {
        let (a, mut f) = self.mail_folder(user_id, folder_id).await?;
        if f.selectable {
            let mut conn = self.imap_conn(&a).await?;
            if let Ok(st) = conn.status(&f.name).await {
                f.total = st.total as i64;
                f.unread = st.unread as i64;
                sqlx::query("UPDATE mail_folders SET total = ?, unread = ? WHERE id = ?")
                    .bind(f.total)
                    .bind(f.unread)
                    .bind(&f.id)
                    .execute(&self.db)
                    .await?;
            }
        }
        let (cached, cached_bytes, flagged): (i64, i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(size), 0), COALESCE(SUM(flagged), 0) FROM mail_messages WHERE folder_id = ?")
                .bind(&f.id)
                .fetch_one(&self.db)
                .await?;
        let bodies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mail_bodies b JOIN mail_messages m ON m.id = b.message_id WHERE m.folder_id = ?")
            .bind(&f.id)
            .fetch_one(&self.db)
            .await?;
        Ok(FolderStats {
            name: f.display_name,
            total: f.total,
            unread: f.unread,
            flagged,
            cached,
            cached_bytes,
            bodies_cached: bodies,
            subscribed: f.subscribed,
            shared: f.shared,
            synced_at: f.synced_at,
        })
    }

    /// Drops a folder's cache and syncs it again from scratch (also re-reads namespaces).
    pub async fn repair_mail_folder(&self, user_id: &str, folder_id: &str) -> Result<()> {
        let (a, f) = self.mail_folder_access(user_id, folder_id, MailRight::Modify).await?;
        sqlx::query("DELETE FROM mail_messages WHERE folder_id = ?")
            .bind(&f.id)
            .execute(&self.db)
            .await?;
        sqlx::query(
            "UPDATE mail_folders SET uidvalidity = NULL, uidnext = NULL, modseq = NULL, window_uid = NULL, complete = 0, synced_at = NULL WHERE id = ?",
        )
        .bind(&f.id)
        .execute(&self.db)
        .await?;
        if let Err(e) = self.load_namespaces(&a).await {
            tracing::info!(error = %e, "reading namespaces during repair failed");
        }
        self.sync_mail_folders(&a).await?;
        if f.selectable {
            self.sync_mail_folder(&a, &f.id).await?;
        }
        self.mail_changed(&a, &[&f.id]);
        Ok(())
    }

    /// Deletes messages that have been in an account's Trash longer than its retention
    /// period. Returns how many were deleted.
    pub async fn run_trash_retention(&self, a: &Account) -> Result<usize> {
        let days = self.account_prefs_row(&a.id).await?.trash_retention_days;
        if days <= 0 {
            return Ok(0);
        }
        let Some(trash) = self.folder_by_role(&a.id, "trash").await? else {
            return Ok(0);
        };
        let mut conn = self.imap_conn(a).await?;
        let sel = conn.select(&trash.name).await?;
        let uids = if sel.exists == 0 { vec![] } else { conn.uid_search("ALL").await? };
        let validity = sel.uidvalidity as i64;
        let mut tx = self.mail_tx().await?;
        sqlx::query("DELETE FROM mail_trash_seen WHERE account_id = ? AND (uidvalidity <> ? OR uid NOT IN (SELECT value FROM json_each(?)))")
            .bind(&a.id)
            .bind(validity)
            .bind(serde_json::to_string(&uids)?)
            .execute(&mut *tx)
            .await?;
        for chunk in uids.chunks(500) {
            sqlx::query("INSERT OR IGNORE INTO mail_trash_seen (account_id, uidvalidity, uid, seen_at) SELECT ?, ?, value, ? FROM json_each(?)")
                .bind(&a.id)
                .bind(validity)
                .bind(now())
                .bind(serde_json::to_string(chunk)?)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        let cutoff = now() - chrono::Duration::days(days);
        let expired: Vec<i64> = sqlx::query_scalar("SELECT uid FROM mail_trash_seen WHERE account_id = ? AND uidvalidity = ? AND seen_at < ?")
            .bind(&a.id)
            .bind(validity)
            .bind(cutoff)
            .fetch_all(&self.db)
            .await?;
        if expired.is_empty() {
            return Ok(0);
        }
        let set_uids: Vec<u32> = expired.iter().map(|u| *u as u32).collect();
        for chunk in set_uids.chunks(500) {
            let set = uid_set(chunk);
            conn.uid_store(&set, "+FLAGS.SILENT (\\Deleted)").await?;
            conn.expunge(&set).await?;
        }
        drop(conn);
        sqlx::query("DELETE FROM mail_trash_seen WHERE account_id = ? AND uidvalidity = ? AND seen_at < ?")
            .bind(&a.id)
            .bind(validity)
            .bind(cutoff)
            .execute(&self.db)
            .await?;
        sqlx::query("DELETE FROM mail_messages WHERE folder_id = ? AND uid IN (SELECT value FROM json_each(?))")
            .bind(&trash.id)
            .bind(serde_json::to_string(&expired)?)
            .execute(&self.db)
            .await?;
        self.sync_mail_folder(a, &trash.id).await?;
        Ok(expired.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::parse::decode_mailbox_name;

    #[test]
    fn modified_utf7_round_trip() {
        assert_eq!(encode_mailbox_name("Projects"), "Projects");
        assert_eq!(encode_mailbox_name("Entwürfe"), "Entw&APw-rfe");
        assert_eq!(encode_mailbox_name("A&B"), "A&-B");
        assert_eq!(encode_mailbox_name("日本語"), "&ZeVnLIqe-");
        for s in ["Entwürfe", "A&B", "日本語", "Émoji 🎉 box", "plain", "x/ÿ,z"] {
            assert_eq!(decode_mailbox_name(&encode_mailbox_name(s)), s, "{s}");
        }
    }

    #[test]
    fn namespaces_parse() {
        let ns = parse_namespace(r#"* NAMESPACE (("" "/")) (("shared/" "/")) NIL"#).unwrap();
        assert_eq!(ns.personal, vec![("".to_string(), Some("/".to_string()))]);
        assert_eq!(ns.other_users, vec![("shared/".to_string(), Some("/".to_string()))]);
        assert!(ns.shared.is_empty());
        assert!(ns.is_shared("shared/carol@example.test/INBOX"));
        assert!(ns.is_shared("shared"));
        assert!(!ns.is_shared("Sharedocs"));
        assert!(!ns.is_shared("INBOX"));
        // Cyrus-style, with extension data and several entries.
        let ns =
            parse_namespace(r#"* NAMESPACE (("INBOX." "." "X-PARAM" ("FLAG1" "FLAG2"))) (("Other Users." ".")) (("Shared Folders." ".") ("Public." "."))"#)
                .unwrap();
        assert_eq!(ns.personal_prefix(), "INBOX.");
        assert_eq!(ns.shared.len(), 2);
        assert!(ns.is_shared("Public.News"));
        assert!(ns.is_shared("Other Users.bob.Sent"));
        assert_eq!(parse_namespace("* NAMESPACE NIL NIL NIL").unwrap(), Namespaces::default());
        assert!(parse_namespace("* OK hello").is_none());
        assert!(parse_namespace(r#"* NAMESPACE (("unterminated"#).is_none());
    }

    #[test]
    fn folder_names() {
        assert_eq!(check_leaf("  Receipts ", Some("/")).unwrap(), "Receipts");
        assert!(check_leaf("a/b", Some("/")).is_err());
        assert!(check_leaf("a*", Some(".")).is_err());
        assert!(check_leaf("", None).is_err());
        assert!(within("Work/2026", "Work", Some("/")));
        assert!(within("Work", "Work", Some("/")));
        assert!(!within("Workshop", "Work", Some("/")));
    }
}
