//! Ferrite's built-in Selective Forwarding Unit for Talk calls.
//!
//! Peer-to-peer ("mesh") calls make every participant upload their video once per other
//! participant, which is why Nextcloud Talk needs its separate High-Performance Backend
//! (signaling server + Janus + NATS) for anything beyond a handful of people. Here the SFU
//! runs inside the Ferrite binary: each participant uploads once, the server forwards
//! packets (no decoding or transcoding, so it stays cheap on small hardware), and all calls
//! share a single UDP port.
//!
//! Built on [str0m](https://github.com/algesten/str0m) (sans-I/O WebRTC) with its pure-Rust
//! crypto backend. One thread owns the UDP socket and every connection. The browser's first
//! offer/answer travels over Ferrite's WebSocket; later renegotiations (when people join,
//! leave or add tracks) travel over a WebRTC data channel, as in str0m's reference SFU.
//! Outgoing tracks are labeled with the sender's Talk call session id (the MediaStream id),
//! so clients know whose tile a track belongs to.
//!
//! **Simulcast.** Browsers send video in three layers (rids `l`, `m`, `h`: quarter, half and
//! full resolution, all at the full frame rate). Each receiver tells the SFU over the data
//! channel which layer it wants per sender (from the size of that person's tile), and the SFU
//! forwards just that layer, so a small tile never costs a 720p decode and a weak link never
//! forces everyone's frame rate down. Switching layers waits for a keyframe of the new layer
//! (requested from the sender), and when a sender stops sending the wanted layer (its encoder
//! is short on bandwidth or CPU) the best layer still arriving is forwarded instead. Senders
//! are told the highest layer anyone currently wants from them (`{"type": "publish"}`) and
//! pause the layers above it, so nobody spends CPU and upload on a 720p encode that no tile
//! shows; that budget goes to keeping the frame rate up.
//!
//! **Recording.** A room being recorded gets every participant's audio and best video layer
//! written to one WebM file per person as the frames pass through (see `record`): no
//! decoding or encoding, so recording costs disk writes and little else.

mod record;
pub mod webm;

pub use record::RecordedFile;

use std::collections::{HashMap, VecDeque};
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::{Duration, Instant};

use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelData, ChannelId};
use str0m::media::{Direction, KeyframeRequest, KeyframeRequestKind, MediaData, MediaKind, Mid, Rid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tracing::{debug, info, warn};

/// Where the SFU listens and which addresses it advertises to browsers.
#[derive(Debug, Clone)]
pub struct SfuConfig {
    /// UDP address to bind, e.g. `0.0.0.0:10000`.
    pub bind: SocketAddr,
    /// Addresses advertised as ICE host candidates. When empty, the machine's primary IP
    /// (and loopback) are used.
    pub advertise: Vec<IpAddr>,
}

#[derive(Debug, Default)]
struct Counters {
    clients: AtomicUsize,
    packets_forwarded: AtomicU64,
    bytes_forwarded: AtomicU64,
    recordings: AtomicUsize,
}

/// Snapshot of SFU activity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SfuStats {
    pub udp_addr: String,
    pub clients: usize,
    pub packets_forwarded: u64,
    pub bytes_forwarded: u64,
    /// Rooms being recorded right now.
    pub recordings: usize,
}

enum Command {
    Add(Box<Client>),
    RemoveSession(String),
    StartRecording {
        room: String,
        dir: std::path::PathBuf,
        reply: SyncSender<Result<(), String>>,
    },
    StopRecording {
        room: String,
        reply: SyncSender<Vec<RecordedFile>>,
    },
}

/// Handle to the running SFU thread.
pub struct Sfu {
    tx: SyncSender<Command>,
    local: SocketAddr,
    candidates: Vec<SocketAddr>,
    counters: Arc<Counters>,
}

/// The IP the OS would use to reach the internet (no packet is sent).
fn primary_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_unspecified())
}

/// Socket buffer size asked of the kernel. High-resolution calls send bursts of packets per
/// frame (a 1080p60 keyframe is ~100 packets, 4K several hundred); the Linux default of about
/// 200 KiB drops some when the SFU thread is briefly busy. The kernel caps the request at
/// `net.core.rmem_max` / `wmem_max`.
const SOCKET_BUFFER: usize = 4 << 20;

fn grow_buffers(socket: &UdpSocket) {
    let s = socket2::SockRef::from(socket);
    let _ = s.set_recv_buffer_size(SOCKET_BUFFER);
    let _ = s.set_send_buffer_size(SOCKET_BUFFER);
    let (recv, send) = (s.recv_buffer_size().unwrap_or(0), s.send_buffer_size().unwrap_or(0));
    // Linux reports double the usable size (it counts bookkeeping).
    if recv < SOCKET_BUFFER {
        tracing::warn!(
            recv,
            send,
            "SFU socket buffers are smaller than {} MiB; for 1080p and above raise net.core.rmem_max and net.core.wmem_max",
            SOCKET_BUFFER >> 20
        );
    } else {
        tracing::debug!(recv, send, "SFU socket buffers");
    }
}

impl Sfu {
    pub fn start(cfg: SfuConfig) -> std::io::Result<Sfu> {
        // Install the pure-Rust crypto provider once per process.
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

        let socket = UdpSocket::bind(cfg.bind)?;
        grow_buffers(&socket);
        let local = socket.local_addr()?;
        let mut ips = cfg.advertise.clone();
        if ips.is_empty() {
            if !local.ip().is_unspecified() {
                ips.push(local.ip());
            } else {
                ips.extend(primary_ip());
                ips.push(IpAddr::V4(Ipv4Addr::LOCALHOST));
            }
        }
        ips.dedup();
        let candidates: Vec<SocketAddr> = ips.into_iter().map(|ip| SocketAddr::new(ip, local.port())).collect();
        let (tx, rx) = mpsc::sync_channel(64);
        let counters = Arc::new(Counters::default());
        let c2 = counters.clone();
        thread::Builder::new().name("ferrite-sfu".into()).spawn(move || run(socket, rx, c2))?;
        info!(udp = %local, candidates = ?candidates, "SFU listening");
        Ok(Sfu {
            tx,
            local,
            candidates,
            counters,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Accepts a browser's offer for `session` in `room`; returns the SDP answer (JSON
    /// `{"type": "answer", "sdp": "…"}`). Any previous connection of the session is replaced.
    pub fn connect(&self, room: &str, session: &str, offer: serde_json::Value) -> Result<serde_json::Value, String> {
        let offer: SdpOffer = serde_json::from_value(offer).map_err(|e| format!("invalid offer: {e}"))?;
        // VP8 + Opus only: every browser has them, VP8 simulcast is the best supported, and
        // recordings can go straight into WebM.
        let mut rtc = Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false).build(Instant::now());
        for addr in &self.candidates {
            let c = Candidate::host(*addr, "udp").map_err(|e| format!("bad candidate {addr}: {e}"))?;
            rtc.add_local_candidate(c);
        }
        let answer = rtc.sdp_api().accept_offer(offer).map_err(|e| format!("offer rejected: {e}"))?;
        let _ = self.tx.send(Command::RemoveSession(session.to_string()));
        let client = Client::new(rtc, room.to_string(), session.to_string());
        self.tx.send(Command::Add(Box::new(client))).map_err(|_| "SFU stopped".to_string())?;
        serde_json::to_value(&answer).map_err(|e| e.to_string())
    }

    /// Drops the connection of a call session (left the call or closed the tab).
    pub fn disconnect(&self, session: &str) {
        let _ = self.tx.send(Command::RemoveSession(session.to_string()));
    }

    /// Starts writing everyone's media in `room` into `dir` (one WebM file per call session).
    /// Blocks briefly; call from a blocking context.
    pub fn start_recording(&self, room: &str, dir: std::path::PathBuf) -> Result<(), String> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.tx
            .send(Command::StartRecording {
                room: room.to_string(),
                dir,
                reply,
            })
            .map_err(|_| "SFU stopped".to_string())?;
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| "SFU did not answer".to_string())?
    }

    /// Stops recording `room` and returns the finished files (empty if it wasn't recording).
    /// Blocks while files are finalized; call from a blocking context.
    pub fn stop_recording(&self, room: &str) -> Vec<RecordedFile> {
        let (reply, rx) = mpsc::sync_channel(1);
        if self.tx.send(Command::StopRecording { room: room.to_string(), reply }).is_err() {
            return Vec::new();
        }
        rx.recv_timeout(Duration::from_secs(120)).unwrap_or_default()
    }

    pub fn stats(&self) -> SfuStats {
        SfuStats {
            udp_addr: self.local.to_string(),
            clients: self.counters.clients.load(Ordering::Relaxed),
            packets_forwarded: self.counters.packets_forwarded.load(Ordering::Relaxed),
            recordings: self.counters.recordings.load(Ordering::Relaxed),
            bytes_forwarded: self.counters.bytes_forwarded.load(Ordering::Relaxed),
        }
    }
}

/// The SFU loop: reads UDP, drives every connection and forwards media within rooms.
fn run(socket: UdpSocket, rx: Receiver<Command>, counters: Arc<Counters>) {
    let mut clients: Vec<Client> = Vec::new();
    let mut recordings: HashMap<String, record::RoomRecording> = HashMap::new();
    let mut queue: VecDeque<Propagated> = VecDeque::new();
    let mut buf = vec![0u8; 2000];
    let Ok(local) = socket.local_addr() else { return };
    let mut read_timeout: Option<Duration> = None;
    loop {
        let before = clients.len();
        clients.retain(|c| c.rtc.is_alive());
        let mut layers_changed = clients.len() != before;
        counters.clients.store(clients.len(), Ordering::Relaxed);

        loop {
            match rx.try_recv() {
                Ok(Command::Add(mut client)) => {
                    // Subscribe the newcomer to every track already published in its room.
                    let existing: Vec<Weak<TrackIn>> = clients
                        .iter()
                        .filter(|c| c.room == client.room)
                        .flat_map(|c| c.tracks_in.iter().map(|t| Arc::downgrade(&t.id)))
                        .collect();
                    for t in existing {
                        client.track_open(t);
                    }
                    clients.push(*client);
                    layers_changed = true;
                }
                Ok(Command::RemoveSession(s)) => {
                    for c in clients.iter_mut().filter(|c| c.session == s) {
                        c.rtc.disconnect();
                        c.dirty = true;
                    }
                }
                Ok(Command::StartRecording { room, dir, reply }) => {
                    let result = match recordings.entry(room) {
                        std::collections::hash_map::Entry::Occupied(_) => Err("this call is already being recorded".to_string()),
                        std::collections::hash_map::Entry::Vacant(slot) => record::RoomRecording::new(dir)
                            .map_err(|e| format!("cannot create the recording folder: {e}"))
                            .map(|r| {
                                slot.insert(r);
                            }),
                    };
                    let _ = reply.send(result);
                    layers_changed = true;
                    counters.recordings.store(recordings.len(), Ordering::Relaxed);
                }
                Ok(Command::StopRecording { room, reply }) => {
                    let files = recordings.remove(&room).map(record::RoomRecording::finish).unwrap_or_default();
                    let _ = reply.send(files);
                    layers_changed = true;
                    counters.recordings.store(recordings.len(), Ordering::Relaxed);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        for c in clients.iter_mut() {
            layers_changed |= std::mem::take(&mut c.layers_dirty);
        }
        if layers_changed {
            update_published_layers(&mut clients, &recordings);
        }

        // Only clients that were given something, or whose timer is due, can have output:
        // polling just those keeps the cost of a packet independent of the call's size.
        let now = Instant::now();
        let mut timeout = now + Duration::from_millis(100);
        for c in clients.iter_mut() {
            if c.next_timeout <= now {
                c.handle_input(Input::Timeout(now));
                c.dirty = true;
            }
            if std::mem::take(&mut c.dirty) {
                c.next_timeout = c.poll_until_timeout(&mut queue, &socket);
            }
            timeout = timeout.min(c.next_timeout);
        }

        if let Some(p) = queue.pop_front() {
            propagate(&p, &mut clients, &counters, &mut queue, &mut recordings);
            continue;
        }

        let wait = timeout.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        // Waking early is harmless, late is not: only shorten the wait, or lengthen it when it
        // is well off, to save a system call per packet.
        if read_timeout.is_none_or(|t| wait < t || wait > t + Duration::from_millis(5)) {
            let _ = socket.set_read_timeout(Some(wait));
            read_timeout = Some(wait);
        }
        buf.resize(2000, 0);
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                buf.truncate(n);
                if let Ok(contents) = buf.as_slice().try_into() {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: local,
                            contents,
                        },
                    );
                    match clients.iter_mut().find(|c| c.rtc.accepts(&input)) {
                        Some(c) => {
                            c.handle_input(input);
                            c.dirty = true;
                        }
                        None => debug!(%source, "no SFU client accepts this packet"),
                    }
                }
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => {
                // ICMP port unreachable and similar transient errors: keep serving.
                debug!(error = %e, "SFU socket read error");
            }
        }
    }
}

/// Tells each sender the highest simulcast layer any receiver in its room wants from it (a
/// recording wants the top layer).
fn update_published_layers(clients: &mut [Client], recordings: &HashMap<String, record::RoomRecording>) {
    for i in 0..clients.len() {
        let (id, room, session) = (clients[i].id, clients[i].room.clone(), clients[i].session.clone());
        let top = if recordings.contains_key(&room) {
            LAYERS.len() - 1
        } else {
            clients
                .iter()
                .filter(|c| c.id != id && c.room == room)
                .map(|c| c.layer_prefs.get(&session).copied().unwrap_or(LAYERS.len() - 1))
                .max()
                // Nobody to send to: the smallest layer keeps the track alive at almost no cost.
                .unwrap_or(0)
        };
        let sender = &mut clients[i];
        if sender.published_top != Some(top) && sender.send_channel(&serde_json::json!({"type": "publish", "max": LAYERS[top]}).to_string()) {
            sender.published_top = Some(top);
            sender.dirty = true;
        }
    }
}

fn propagate(
    p: &Propagated,
    clients: &mut [Client],
    counters: &Counters,
    queue: &mut VecDeque<Propagated>,
    recordings: &mut HashMap<String, record::RoomRecording>,
) {
    let Some((origin, room)) = p.origin() else { return };
    let room = room.to_string();
    if let (Propagated::MediaData(_, _, data), Some(rec)) = (p, recordings.get_mut(&room)) {
        if let Some(session) = clients.iter().find(|c| c.id == origin).map(|c| c.session.clone()) {
            // The recording waits for a keyframe of the layer it records.
            if let Some(rid) = rec.write(&session, data) {
                let req = KeyframeRequest {
                    mid: data.mid,
                    rid: Some(rid),
                    kind: KeyframeRequestKind::Pli,
                };
                queue.push_back(Propagated::KeyframeRequest(RECORDER, room.clone(), req, origin, data.mid));
            }
        }
    }
    for c in clients.iter_mut().filter(|c| c.id != origin && c.room == room) {
        c.dirty = true;
        match p {
            Propagated::TrackOpen(_, _, track) => c.track_open(track.clone()),
            Propagated::MediaData(_, _, data) => {
                let (sent, keyframe) = c.media_out(origin, data);
                if sent {
                    counters.packets_forwarded.fetch_add(1, Ordering::Relaxed);
                    counters.bytes_forwarded.fetch_add(data.data.len() as u64, Ordering::Relaxed);
                }
                // The receiver needs a keyframe of another layer to switch to it.
                if let Some(rid) = keyframe {
                    let req = KeyframeRequest {
                        mid: data.mid,
                        rid: Some(rid),
                        kind: KeyframeRequestKind::Pli,
                    };
                    queue.push_back(Propagated::KeyframeRequest(c.id, room.clone(), req, origin, data.mid));
                }
            }
            Propagated::KeyframeRequest(_, _, req, source, mid) => {
                if *source == c.id {
                    c.keyframe_request(*req, *mid);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClientId(u64);

/// The origin of keyframe requests made on behalf of a recording (no client has id 0).
const RECORDER: ClientId = ClientId(0);

#[derive(Debug)]
struct TrackIn {
    origin: ClientId,
    /// Call session of the sender; becomes the stream id of forwarded tracks.
    session: String,
    mid: Mid,
    kind: MediaKind,
}

#[derive(Debug)]
struct TrackInEntry {
    id: Arc<TrackIn>,
    last_keyframe_request: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutState {
    ToOpen,
    Negotiating(Mid),
    Open(Mid),
    ToStop(Mid),
    NegotiatingStop(Mid),
}

/// Simulcast layers by rank, lowest first.
const LAYERS: [&str; 3] = ["l", "m", "h"];
/// A layer counts as being sent if a frame of it arrived this recently.
const LAYER_ALIVE: Duration = Duration::from_millis(1000);
/// Minimum spacing of the SFU's own keyframe requests for one forwarded track.
const LAYER_KEYFRAME_SPACING: Duration = Duration::from_millis(300);

fn layer_rank(rid: &str) -> usize {
    LAYERS.iter().position(|l| *l == rid).unwrap_or(LAYERS.len() - 1)
}

/// Chooses which simulcast layer of one incoming video track to pass on (to a receiver or to
/// a recording): the wanted layer when it is arriving, else the best one below it that is,
/// else any above. It switches only on a keyframe of the new layer and asks for one.
#[derive(Debug, Default)]
struct LayerPicker {
    /// Layer currently passed on (None until the first keyframe).
    current: Option<Rid>,
    /// When each layer (by rank) last arrived from the sender.
    seen: [Option<Instant>; 3],
    last_keyframe_request: Option<Instant>,
}

enum Pick {
    Pass,
    /// Drop this frame; the layer given (if any) needs a keyframe before the switch.
    Skip(Option<Rid>),
}

impl LayerPicker {
    fn pick(&mut self, data: &MediaData, want: usize) -> Pick {
        let Some(rid) = data.rid else { return Pick::Pass };
        let now = data.network_time;
        let rank = layer_rank(&rid);
        self.seen[rank] = Some(now);
        let seen = self.seen;
        let alive = |r: usize| seen[r].is_some_and(|t| now.saturating_duration_since(t) < LAYER_ALIVE);
        let target = (0..=want).rev().find(|&r| alive(r)).or_else(|| (want + 1..LAYERS.len()).find(|&r| alive(r)));
        if self.current.map(|c| layer_rank(&c)) == Some(rank) {
            return Pick::Pass;
        }
        if target == Some(rank) && data.is_keyframe() {
            self.current = Some(rid);
            return Pick::Pass;
        }
        if target == Some(rank)
            && self
                .last_keyframe_request
                .is_none_or(|t| now.saturating_duration_since(t) >= LAYER_KEYFRAME_SPACING)
        {
            self.last_keyframe_request = Some(now);
            return Pick::Skip(Some(rid));
        }
        Pick::Skip(None)
    }
}

#[derive(Debug)]
struct TrackOut {
    track_in: Weak<TrackIn>,
    state: OutState,
    layers: LayerPicker,
}

impl TrackOut {
    fn mid(&self) -> Option<Mid> {
        match self.state {
            OutState::ToOpen => None,
            OutState::Negotiating(m) | OutState::Open(m) | OutState::ToStop(m) | OutState::NegotiatingStop(m) => Some(m),
        }
    }
}

enum Propagated {
    TrackOpen(ClientId, String, Weak<TrackIn>),
    MediaData(ClientId, String, Box<MediaData>),
    KeyframeRequest(ClientId, String, KeyframeRequest, ClientId, Mid),
}

impl Propagated {
    fn origin(&self) -> Option<(ClientId, &str)> {
        Some(match self {
            Propagated::TrackOpen(c, r, _) | Propagated::MediaData(c, r, _) | Propagated::KeyframeRequest(c, r, _, _, _) => (*c, r.as_str()),
        })
    }
}

struct Client {
    id: ClientId,
    room: String,
    session: String,
    rtc: Rtc,
    pending: Option<SdpPendingOffer>,
    channel: Option<ChannelId>,
    tracks_in: Vec<TrackInEntry>,
    tracks_out: Vec<TrackOut>,
    /// Wanted simulcast layer (rank) per sender session; unlisted senders get the top layer.
    layer_prefs: HashMap<String, usize>,
    /// Highest layer this client was last told to publish.
    published_top: Option<usize>,
    /// Preferences changed or the data channel opened: recompute what senders publish.
    layers_dirty: bool,
    /// Something was handed to this client (a packet, media to forward, a command) since it
    /// was last polled, so it may have output.
    dirty: bool,
    /// When str0m next wants time to pass for this client (its last `Output::Timeout`).
    next_timeout: Instant,
}

impl Client {
    fn new(rtc: Rtc, room: String, session: String) -> Client {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Client {
            id: ClientId(NEXT.fetch_add(1, Ordering::Relaxed)),
            room,
            session,
            rtc,
            pending: None,
            channel: None,
            tracks_in: Vec::new(),
            tracks_out: Vec::new(),
            layer_prefs: HashMap::new(),
            published_top: None,
            layers_dirty: false,
            dirty: true,
            next_timeout: Instant::now(),
        }
    }

    fn handle_input(&mut self, input: Input) {
        if !self.rtc.is_alive() {
            return;
        }
        if let Err(e) = self.rtc.handle_input(input) {
            debug!(session = %self.session, error = ?e, "SFU client input failed");
            self.rtc.disconnect();
        }
    }

    fn poll_until_timeout(&mut self, queue: &mut VecDeque<Propagated>, socket: &UdpSocket) -> Instant {
        loop {
            if !self.rtc.is_alive() {
                return Instant::now();
            }
            if self.negotiate_if_needed() {
                continue;
            }
            let output = match self.rtc.poll_output() {
                Ok(o) => o,
                Err(e) => {
                    debug!(session = %self.session, error = ?e, "SFU client poll failed");
                    self.rtc.disconnect();
                    return Instant::now();
                }
            };
            match output {
                Output::Timeout(t) => return t,
                Output::Transmit(t) => {
                    if let Err(e) = socket.send_to(&t.contents, t.destination) {
                        debug!(error = %e, "SFU send failed");
                    }
                }
                Output::Event(e) => {
                    if let Some(p) = self.handle_event(e) {
                        queue.push_back(p);
                    }
                }
            }
        }
    }

    fn handle_event(&mut self, e: Event) -> Option<Propagated> {
        match e {
            Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                self.rtc.disconnect();
                None
            }
            Event::MediaAdded(m) => {
                let entry = TrackInEntry {
                    id: Arc::new(TrackIn {
                        origin: self.id,
                        session: self.session.clone(),
                        mid: m.mid,
                        kind: m.kind,
                    }),
                    last_keyframe_request: None,
                };
                let weak = Arc::downgrade(&entry.id);
                self.tracks_in.push(entry);
                Some(Propagated::TrackOpen(self.id, self.room.clone(), weak))
            }
            Event::MediaData(data) => {
                if !data.contiguous {
                    self.request_keyframe_throttled(data.mid, data.rid, KeyframeRequestKind::Fir);
                }
                Some(Propagated::MediaData(self.id, self.room.clone(), Box::new(data)))
            }
            Event::KeyframeRequest(mut req) => {
                let track_out = self.tracks_out.iter().find(|t| t.mid() == Some(req.mid))?;
                let track_in = track_out.track_in.upgrade()?;
                // The receiver's decoder wants a keyframe of whatever layer it is being sent.
                req.rid = track_out.layers.current;
                Some(Propagated::KeyframeRequest(self.id, self.room.clone(), req, track_in.origin, track_in.mid))
            }
            Event::ChannelOpen(cid, _) => {
                self.channel = Some(cid);
                self.layers_dirty = true;
                None
            }
            Event::ChannelData(d) => {
                self.channel_data(d);
                None
            }
            _ => None,
        }
    }

    fn request_keyframe_throttled(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        let Some(entry) = self.tracks_in.iter_mut().find(|t| t.id.mid == mid) else {
            return;
        };
        if entry.last_keyframe_request.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        if let Some(mut w) = self.rtc.writer(mid) {
            let _ = w.request_keyframe(rid, kind);
            entry.last_keyframe_request = Some(Instant::now());
        }
    }

    /// Sends a JSON control message on the data channel; false if it isn't open.
    fn send_channel(&mut self, json: &str) -> bool {
        match self.channel.and_then(|id| self.rtc.channel(id)) {
            Some(mut ch) => ch.write(false, json.as_bytes()).is_ok(),
            None => false,
        }
    }

    fn track_open(&mut self, track_in: Weak<TrackIn>) {
        self.tracks_out.push(TrackOut {
            track_in,
            state: OutState::ToOpen,
            layers: LayerPicker::default(),
        });
    }

    /// Sends a server-side offer over the data channel when tracks must be added/removed.
    fn negotiate_if_needed(&mut self) -> bool {
        if self.channel.is_none() || self.pending.is_some() {
            return false;
        }
        for t in &mut self.tracks_out {
            if let OutState::Open(m) = t.state {
                if t.track_in.upgrade().is_none() {
                    t.state = OutState::ToStop(m);
                }
            }
        }
        let mut change = self.rtc.sdp_api();
        for t in &mut self.tracks_out {
            match t.state {
                OutState::ToOpen => {
                    if let Some(track_in) = t.track_in.upgrade() {
                        let mid = change.add_media(track_in.kind, Direction::SendOnly, Some(track_in.session.clone()), None, None);
                        t.state = OutState::Negotiating(mid);
                    }
                }
                OutState::ToStop(mid) => {
                    change.stop_media(mid);
                    t.state = OutState::NegotiatingStop(mid);
                }
                _ => {}
            }
        }
        // Tracks whose sender left before they were ever negotiated are simply dropped.
        self.tracks_out.retain(|t| !(t.state == OutState::ToOpen && t.track_in.upgrade().is_none()));
        if !change.has_changes() {
            return false;
        }
        let Some((offer, pending)) = change.apply() else { return false };
        let Some(mut ch) = self.channel.and_then(|id| self.rtc.channel(id)) else {
            return false;
        };
        let Ok(json) = serde_json::to_string(&offer) else { return false };
        if ch.write(false, json.as_bytes()).is_err() {
            warn!(session = %self.session, "could not send renegotiation offer");
            self.rtc.disconnect();
            return false;
        }
        self.pending = Some(pending);
        true
    }

    fn channel_data(&mut self, d: ChannelData) {
        // `{"type": "layers", "want": {"<sender session>": "l" | "m" | "h"}}` replaces the
        // receiver's simulcast preferences.
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&d.data) {
            if v["type"] == "layers" {
                if let Some(want) = v["want"].as_object() {
                    self.layer_prefs = want
                        .iter()
                        .filter_map(|(session, layer)| layer.as_str().map(|l| (session.clone(), layer_rank(l))))
                        .collect();
                    self.layers_dirty = true;
                }
                return;
            }
        }
        if let Ok(offer) = serde_json::from_slice::<SdpOffer>(&d.data) {
            // Client-initiated renegotiation (e.g. a new track).
            let answer = match self.rtc.sdp_api().accept_offer(offer) {
                Ok(a) => a,
                Err(e) => {
                    warn!(session = %self.session, error = ?e, "bad renegotiation offer");
                    return;
                }
            };
            for t in &mut self.tracks_out {
                match t.state {
                    OutState::Negotiating(_) => t.state = OutState::ToOpen,
                    OutState::NegotiatingStop(m) => t.state = OutState::ToStop(m),
                    _ => {}
                }
            }
            self.pending = None;
            if let (Some(mut ch), Ok(json)) = (self.channel.and_then(|id| self.rtc.channel(id)), serde_json::to_string(&answer)) {
                let _ = ch.write(false, json.as_bytes());
            }
        } else if let Ok(answer) = serde_json::from_slice::<SdpAnswer>(&d.data) {
            if let Some(pending) = self.pending.take() {
                if let Err(e) = self.rtc.sdp_api().accept_answer(pending, answer) {
                    warn!(session = %self.session, error = ?e, "bad renegotiation answer");
                    self.rtc.disconnect();
                    return;
                }
                for t in &mut self.tracks_out {
                    if let OutState::Negotiating(m) = t.state {
                        t.state = OutState::Open(m);
                    }
                }
                self.tracks_out.retain(|t| !matches!(t.state, OutState::NegotiatingStop(_)));
            }
        }
    }

    /// Forwards media from `origin` if this client subscribes to it. Returns whether it was
    /// sent, and a simulcast layer whose keyframe this receiver is waiting for.
    fn media_out(&mut self, origin: ClientId, data: &MediaData) -> (bool, Option<Rid>) {
        let Some(out) = self
            .tracks_out
            .iter_mut()
            .find(|o| o.track_in.upgrade().is_some_and(|i| i.origin == origin && i.mid == data.mid))
        else {
            return (false, None);
        };
        let OutState::Open(mid) = out.state else { return (false, None) };
        let want = out
            .track_in
            .upgrade()
            .and_then(|i| self.layer_prefs.get(&i.session).copied())
            .unwrap_or(LAYERS.len() - 1);
        let keyframe = match out.layers.pick(data, want) {
            Pick::Pass => None,
            Pick::Skip(keyframe) => return (false, keyframe),
        };
        let Some(writer) = self.rtc.writer(mid) else { return (false, keyframe) };
        let Some(pt) = writer.match_params(data.params) else {
            return (false, keyframe);
        };
        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            debug!(session = %self.session, error = ?e, "SFU write failed");
            self.rtc.disconnect();
            return (false, keyframe);
        }
        (true, keyframe)
    }

    fn keyframe_request(&mut self, req: KeyframeRequest, mid_in: Mid) {
        if !self.tracks_in.iter().any(|i| i.id.mid == mid_in) {
            return;
        }
        if let Some(mut w) = self.rtc.writer(mid_in) {
            let _ = w.request_keyframe(req.rid, req.kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_and_rejects_garbage_offers() {
        let sfu = Sfu::start(SfuConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertise: vec![],
        })
        .unwrap();
        assert_eq!(sfu.local_addr().ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(sfu.connect("room", "s1", serde_json::json!({"type": "offer", "sdp": "garbage"})).is_err());
        assert_eq!(sfu.stats().clients, 0);
    }
}
