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

use std::collections::VecDeque;
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
}

/// Snapshot of SFU activity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SfuStats {
    pub udp_addr: String,
    pub clients: usize,
    pub packets_forwarded: u64,
    pub bytes_forwarded: u64,
}

enum Command {
    Add(Box<Client>),
    RemoveSession(String),
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

impl Sfu {
    pub fn start(cfg: SfuConfig) -> std::io::Result<Sfu> {
        // Install the pure-Rust crypto provider once per process.
        static INSTALL: std::sync::Once = std::sync::Once::new();
        INSTALL.call_once(|| str0m::crypto::from_feature_flags().install_process_default());

        let socket = UdpSocket::bind(cfg.bind)?;
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
        let mut rtc = Rtc::builder().build(Instant::now());
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

    pub fn stats(&self) -> SfuStats {
        SfuStats {
            udp_addr: self.local.to_string(),
            clients: self.counters.clients.load(Ordering::Relaxed),
            packets_forwarded: self.counters.packets_forwarded.load(Ordering::Relaxed),
            bytes_forwarded: self.counters.bytes_forwarded.load(Ordering::Relaxed),
        }
    }
}

/// The SFU loop: reads UDP, drives every connection and forwards media within rooms.
fn run(socket: UdpSocket, rx: Receiver<Command>, counters: Arc<Counters>) {
    let mut clients: Vec<Client> = Vec::new();
    let mut queue: VecDeque<Propagated> = VecDeque::new();
    let mut buf = vec![0u8; 2000];
    loop {
        clients.retain(|c| c.rtc.is_alive());
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
                }
                Ok(Command::RemoveSession(s)) => {
                    for c in clients.iter_mut().filter(|c| c.session == s) {
                        c.rtc.disconnect();
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let mut timeout = Instant::now() + Duration::from_millis(100);
        for c in clients.iter_mut() {
            timeout = timeout.min(c.poll_until_timeout(&mut queue, &socket));
        }

        if let Some(p) = queue.pop_front() {
            propagate(&p, &mut clients, &counters);
            continue;
        }

        let wait = timeout.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        let _ = socket.set_read_timeout(Some(wait));
        buf.resize(2000, 0);
        match socket.recv_from(&mut buf) {
            Ok((n, source)) => {
                buf.truncate(n);
                if let (Ok(contents), Ok(destination)) = (buf.as_slice().try_into(), socket.local_addr()) {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination,
                            contents,
                        },
                    );
                    match clients.iter_mut().find(|c| c.rtc.accepts(&input)) {
                        Some(c) => c.handle_input(input),
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

        let now = Instant::now();
        for c in clients.iter_mut() {
            c.handle_input(Input::Timeout(now));
        }
    }
}

fn propagate(p: &Propagated, clients: &mut [Client], counters: &Counters) {
    let Some((origin, room)) = p.origin() else { return };
    let room = room.to_string();
    for c in clients.iter_mut().filter(|c| c.id != origin && c.room == room) {
        match p {
            Propagated::TrackOpen(_, _, track) => c.track_open(track.clone()),
            Propagated::MediaData(_, _, data) => {
                if c.media_out(origin, data) {
                    counters.packets_forwarded.fetch_add(1, Ordering::Relaxed);
                    counters.bytes_forwarded.fetch_add(data.data.len() as u64, Ordering::Relaxed);
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

#[derive(Debug)]
struct TrackOut {
    track_in: Weak<TrackIn>,
    state: OutState,
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
    chosen_rid: Option<Rid>,
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
            chosen_rid: None,
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
                req.rid = self.chosen_rid;
                Some(Propagated::KeyframeRequest(self.id, self.room.clone(), req, track_in.origin, track_in.mid))
            }
            Event::ChannelOpen(cid, _) => {
                self.channel = Some(cid);
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

    fn track_open(&mut self, track_in: Weak<TrackIn>) {
        self.tracks_out.push(TrackOut {
            track_in,
            state: OutState::ToOpen,
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

    /// Forwards media from `origin` if this client subscribes to it. Returns true if sent.
    fn media_out(&mut self, origin: ClientId, data: &MediaData) -> bool {
        let Some(mid) = self
            .tracks_out
            .iter()
            .find(|o| o.track_in.upgrade().is_some_and(|i| i.origin == origin && i.mid == data.mid))
            .and_then(|o| match o.state {
                OutState::Open(m) => Some(m),
                _ => None,
            })
        else {
            return false;
        };
        // Simulcast: forward the high layer (or the only layer).
        if data.rid.is_some() && data.rid != Some("h".into()) {
            return false;
        }
        self.chosen_rid = data.rid;
        let Some(writer) = self.rtc.writer(mid) else { return false };
        let Some(pt) = writer.match_params(data.params) else { return false };
        if let Err(e) = writer.write(pt, data.network_time, data.time, data.data.clone()) {
            debug!(session = %self.session, error = ?e, "SFU write failed");
            self.rtc.disconnect();
            return false;
        }
        true
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
