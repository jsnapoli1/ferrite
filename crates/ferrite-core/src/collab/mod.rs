//! Live collaborative editing over the y-websocket protocol (compatible with Yjs clients).
//!
//! A session exists per file while anyone edits it. Clients exchange CRDT updates through the
//! server; the server keeps the merged document, writes it back to the file every couple of
//! seconds (and keeps a version every ten minutes), and lets MCP tools and the AI assistant edit
//! the same live document, so their changes appear in everyone's editor instantly.

pub mod codec;

use crate::files::{WriteOpts, perm};
use crate::{Core, Error, Result};
use dashmap::DashMap;
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use yrs::encoding::read::Cursor;
use yrs::sync::{Awareness, Message, MessageReader, SyncMessage};
use yrs::updates::decoder::{Decode, DecoderV1};
use yrs::updates::encoder::Encode;
use yrs::{ClientID, Doc, ReadTxn, Transact, Update};

pub const MIME_MINDMAP: &str = "application/vnd.ferrite.mindmap+json";
pub const MIME_CANVAS: &str = "application/vnd.ferrite.canvas+json";

const FLUSH_EVERY: Duration = Duration::from_secs(2);
const VERSION_EVERY: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Text,
    Sheet,
    Mindmap,
    Canvas,
}

impl DocKind {
    pub fn for_name(name: &str) -> Option<Self> {
        let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
        Some(match ext.as_str() {
            "md" | "markdown" | "txt" | "text" => DocKind::Text,
            "csv" => DocKind::Sheet,
            "fmind" => DocKind::Mindmap,
            "fcanvas" => DocKind::Canvas,
            _ => return None,
        })
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "text" => DocKind::Text,
            "sheet" => DocKind::Sheet,
            "mindmap" => DocKind::Mindmap,
            "canvas" => DocKind::Canvas,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DocKind::Text => "text",
            DocKind::Sheet => "sheet",
            DocKind::Mindmap => "mindmap",
            DocKind::Canvas => "canvas",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            DocKind::Text => "md",
            DocKind::Sheet => "csv",
            DocKind::Mindmap => "fmind",
            DocKind::Canvas => "fcanvas",
        }
    }

    pub fn empty_json(self) -> Value {
        match self {
            DocKind::Mindmap => json!({"format": codec::MINDMAP_FORMAT, "version": 1, "nodes": []}),
            DocKind::Canvas => {
                json!({"format": codec::CANVAS_FORMAT, "version": 1, "items": [], "edges": []})
            }
            _ => Value::Null,
        }
    }

    /// Content of a brand new document of this kind titled `title`.
    pub fn initial_content(self, title: &str) -> Vec<u8> {
        match self {
            DocKind::Text => format!("# {title}\n\n").into_bytes(),
            DocKind::Sheet => Vec::new(),
            DocKind::Mindmap => serde_json::to_vec_pretty(&json!({
                "format": codec::MINDMAP_FORMAT, "version": 1, "title": title, "meta": {"title": title},
                "nodes": [{"id": "root", "parent": "", "text": title, "order": 0}]
            }))
            .unwrap_or_default(),
            DocKind::Canvas => serde_json::to_vec_pretty(&json!({
                "format": codec::CANVAS_FORMAT, "version": 1, "title": title, "meta": {"title": title}, "items": [], "edges": []
            }))
            .unwrap_or_default(),
        }
    }
}

pub struct Session {
    pub node_id: String,
    pub kind: DocKind,
    owner_id: String,
    awareness: Mutex<Awareness>,
    tx: broadcast::Sender<Arc<[u8]>>,
    dirty: Arc<AtomicBool>,
    clients: AtomicUsize,
    closed: AtomicBool,
    last_version: Mutex<Option<Instant>>,
    flush_lock: tokio::sync::Mutex<()>,
}

impl Session {
    fn new(node_id: String, owner_id: String, kind: DocKind, doc: Doc) -> Arc<Self> {
        let (tx, _) = broadcast::channel(1024);
        let dirty = Arc::new(AtomicBool::new(false));
        {
            let tx = tx.clone();
            let dirty = dirty.clone();
            let _ = doc.observe_update_v1("ferrite-broadcast", move |_, e| {
                dirty.store(true, Ordering::Release);
                let msg = Message::Sync(SyncMessage::Update(e.update.clone())).encode_v1();
                let _ = tx.send(Arc::from(msg));
            });
        }
        let mut awareness = Awareness::new(doc);
        {
            let tx = tx.clone();
            awareness.on_update("ferrite-broadcast", move |aw, ev, _| {
                if let Ok(u) = aw.update_with_clients(ev.all_changes()) {
                    let _ = tx.send(Arc::from(Message::Awareness(u).encode_v1()));
                }
            });
        }
        Arc::new(Self {
            node_id,
            kind,
            owner_id,
            awareness: Mutex::new(awareness),
            tx,
            dirty,
            clients: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            last_version: Mutex::new(None),
            flush_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let aw = self.awareness.lock();
        codec::save(self.kind, aw.doc())
    }

    pub fn client_count(&self) -> usize {
        self.clients.load(Ordering::Acquire)
    }
}

/// One editor connection to a session.
pub struct CollabConn {
    pub session: Arc<Session>,
    pub rx: broadcast::Receiver<Arc<[u8]>>,
    pub read_only: bool,
    client_ids: HashSet<ClientID>,
}

/// Payload sent to connections when the session is reset (e.g. the file was replaced
/// externally). Transports close the socket so the client reconnects and resyncs.
pub fn is_reset(payload: &[u8]) -> bool {
    payload.is_empty()
}

#[derive(Default)]
pub struct CollabManager {
    sessions: DashMap<String, Arc<Session>>,
}

impl CollabManager {
    pub fn active(&self) -> Vec<(String, usize)> {
        self.sessions.iter().map(|e| (e.key().clone(), e.value().client_count())).collect()
    }
}

impl Core {
    async fn collab_session(&self, node_id: &str) -> Result<Arc<Session>> {
        if let Some(s) = self.collab.sessions.get(node_id) {
            if !s.closed.load(Ordering::Acquire) {
                return Ok(s.clone());
            }
        }
        let node = self.node(node_id).await?;
        let kind = DocKind::for_name(&node.name).ok_or_else(|| Error::bad("this file type has no live editor"))?;
        let data = match &node.blob_hash {
            Some(h) => self.blobs.read(h).await?,
            None => Vec::new(),
        };
        let doc = codec::load(kind, &data)?;
        let fresh = Session::new(node.id.clone(), node.owner_id.clone(), kind, doc);
        let session = match self.collab.sessions.entry(node.id.clone()) {
            dashmap::Entry::Occupied(mut e) => {
                if e.get().closed.load(Ordering::Acquire) {
                    e.insert(fresh.clone());
                    self.spawn_flusher(fresh.clone());
                    fresh
                } else {
                    e.get().clone()
                }
            }
            dashmap::Entry::Vacant(v) => {
                v.insert(fresh.clone());
                self.spawn_flusher(fresh.clone());
                fresh
            }
        };
        Ok(session)
    }

    fn spawn_flusher(&self, s: Arc<Session>) {
        let core = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(FLUSH_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                if s.closed.load(Ordering::Acquire) {
                    break;
                }
                if let Err(e) = core.flush_session(&s, false).await {
                    tracing::warn!(node = %s.node_id, error = %e, "collab flush failed");
                }
            }
        });
    }

    /// Writes the live document back to its file when it changed.
    async fn flush_session(&self, s: &Arc<Session>, force_version: bool) -> Result<()> {
        let _guard = s.flush_lock.lock().await;
        if !s.dirty.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        let data = s.snapshot()?;
        let keep_version = {
            let mut last = s.last_version.lock();
            let due = force_version || last.is_none_or(|t| t.elapsed() >= VERSION_EVERY);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        let node = self.node(&s.node_id).await?;
        let parent = node.parent_id.clone().ok_or(Error::NotFound)?;
        let opts = WriteOpts {
            keep_version,
            from_collab: true,
            ..Default::default()
        };
        // The owner is always allowed to write; locks held by others make sessions read-only,
        // so no edits (and no flushes) can happen while a Craft app holds the file.
        let lock_token = self.lock_of(&node.id).await?.map(|l| l.token);
        let opts = WriteOpts { lock_token, ..opts };
        if let Err(e) = self.write_bytes(&s.owner_id, &parent, &node.name, &data, opts).await {
            s.dirty.store(true, Ordering::Release);
            return Err(e);
        }
        Ok(())
    }

    async fn close_session_if_idle(&self, s: &Arc<Session>) {
        if s.clients.load(Ordering::Acquire) > 0 {
            return;
        }
        if let Err(e) = self.flush_session(s, true).await {
            tracing::warn!(node = %s.node_id, error = %e, "final collab flush failed");
        }
        if s.clients.load(Ordering::Acquire) == 0 {
            s.closed.store(true, Ordering::Release);
            self.collab.sessions.remove_if(&s.node_id, |_, v| Arc::ptr_eq(v, s));
        }
    }

    /// Opens an editor connection. Returns the connection and the initial protocol messages.
    pub async fn collab_open(&self, user_id: &str, node_id: &str) -> Result<(CollabConn, Vec<u8>)> {
        let (node, p) = self.node_for(user_id, node_id, perm::READ).await?;
        let locked_by_other = self.lock_of(&node.id).await?.is_some_and(|l| l.user_id != user_id || l.owner_app != "ferrite");
        let read_only = p & perm::UPDATE == 0 || locked_by_other;
        let session = self.collab_session(&node.id).await?;
        session.clients.fetch_add(1, Ordering::AcqRel);
        let rx = session.tx.subscribe();
        let initial = {
            let aw = session.awareness.lock();
            let sv = aw.doc().transact().state_vector();
            let mut out = Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1();
            if let Ok(update) = aw.update() {
                out.extend(Message::Awareness(update).encode_v1());
            }
            out
        };
        Ok((
            CollabConn {
                session,
                rx,
                read_only,
                client_ids: HashSet::new(),
            },
            initial,
        ))
    }

    /// Handles one binary frame from a client; returns frames to send back to it.
    pub fn collab_receive(&self, conn: &mut CollabConn, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut replies = Vec::new();
        let mut decoder = DecoderV1::new(Cursor::new(data));
        let reader = MessageReader::new(&mut decoder);
        let mut aw = conn.session.awareness.lock();
        for msg in reader {
            let msg = msg.map_err(|e| Error::bad(format!("bad collab frame: {e}")))?;
            match msg {
                Message::Sync(SyncMessage::SyncStep1(sv)) => {
                    let update = aw.doc().transact().encode_state_as_update_v1(&sv);
                    replies.push(Message::Sync(SyncMessage::SyncStep2(update)).encode_v1());
                }
                Message::Sync(SyncMessage::SyncStep2(u)) | Message::Sync(SyncMessage::Update(u)) => {
                    if conn.read_only {
                        continue;
                    }
                    let update = Update::decode_v1(&u).map_err(|e| Error::bad(format!("bad update: {e}")))?;
                    aw.doc()
                        .transact_mut()
                        .apply_update(update)
                        .map_err(|e| Error::bad(format!("cannot apply update: {e}")))?;
                }
                Message::Awareness(update) => {
                    conn.client_ids.extend(update.clients.keys().copied());
                    aw.apply_update(update).map_err(|e| Error::bad(format!("bad awareness: {e}")))?;
                }
                Message::AwarenessQuery => {
                    if let Ok(u) = aw.update() {
                        replies.push(Message::Awareness(u).encode_v1());
                    }
                }
                Message::Auth(_) | Message::Custom(..) => {}
            }
        }
        Ok(replies)
    }

    pub async fn collab_close(&self, conn: CollabConn) {
        {
            let mut aw = conn.session.awareness.lock();
            for id in &conn.client_ids {
                aw.remove_state(*id);
            }
        }
        conn.session.clients.fetch_sub(1, Ordering::AcqRel);
        self.close_session_if_idle(&conn.session).await;
    }

    /// Called after a write that did not come from a live session: live editors would otherwise
    /// overwrite the new content, so the session is reset and clients reload from the file.
    pub(crate) fn collab_external_write(&self, node_id: &str) {
        if let Some((_, s)) = self.collab.sessions.remove(node_id) {
            s.closed.store(true, Ordering::Release);
            let _ = s.tx.send(Arc::from(Vec::<u8>::new()));
        }
    }

    /// Runs `f` against the live document of a file, as `user_id`. Changes are broadcast to
    /// connected editors immediately and written back to the file.
    pub async fn collab_edit<R>(&self, user_id: &str, node_id: &str, f: impl FnOnce(&Doc) -> Result<R>) -> Result<R> {
        let (node, _) = self.node_for(user_id, node_id, perm::UPDATE).await?;
        if let Some(l) = self.lock_of(&node.id).await? {
            if l.user_id != user_id {
                return Err(Error::Locked(l.owner_app));
            }
        }
        let session = self.collab_session(&node.id).await?;
        session.clients.fetch_add(1, Ordering::AcqRel);
        let result = {
            let aw = session.awareness.lock();
            f(aw.doc())
        };
        session.clients.fetch_sub(1, Ordering::AcqRel);
        if session.client_count() == 0 {
            self.close_session_if_idle(&session).await;
        } else {
            self.flush_session(&session, false).await?;
        }
        result
    }

    /// Current content of a live-editable file, including unsaved live edits.
    pub async fn collab_content(&self, user_id: &str, node_id: &str) -> Result<Vec<u8>> {
        let (node, _) = self.node_for(user_id, node_id, perm::READ).await?;
        if let Some(s) = self.collab.sessions.get(&node.id) {
            if !s.closed.load(Ordering::Acquire) {
                return s.snapshot();
            }
        }
        match &node.blob_hash {
            Some(h) => self.blobs.read(h).await,
            None => Ok(Vec::new()),
        }
    }

    /// Creates a new live document file (`text`, `sheet`, `mindmap` or `canvas`).
    pub async fn create_document(&self, user_id: &str, parent_id: &str, title: &str, kind: DocKind) -> Result<crate::files::Node> {
        let title = title.trim();
        if title.is_empty() {
            return Err(Error::bad("title required"));
        }
        let ext = kind.extension();
        let name = if title.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
            title.to_string()
        } else {
            format!("{title}.{ext}")
        };
        let stem = name.trim_end_matches(&format!(".{ext}")).to_string();
        let opts = WriteOpts {
            create_only: true,
            ..WriteOpts::user()
        };
        self.write_bytes(user_id, parent_id, &name, &kind.initial_content(&stem), opts).await
    }

    pub async fn flush_all_sessions(&self) {
        let sessions: Vec<Arc<Session>> = self.collab.sessions.iter().map(|e| e.value().clone()).collect();
        for s in sessions {
            if let Err(e) = self.flush_session(&s, true).await {
                tracing::warn!(node = %s.node_id, error = %e, "flush on shutdown failed");
            }
        }
    }
}
