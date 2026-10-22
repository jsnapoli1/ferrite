// Talk calls. Media goes through one of two transports, chosen by the server per call:
//  - SfuTransport (when the server runs its built-in SFU): one RTCPeerConnection to the
//    server, which forwards each participant's media to the others. Upload cost stays
//    constant as the call grows.
//  - MeshTransport (fallback): one RTCPeerConnection per other participant; the server only
//    relays signaling over the event socket.
//
// Structure:
//  - A transport owns the RTCPeerConnections and reports remote media per call session. Both
//    offer the same surface: start(localStream, members), replaceTrack(kind, track), stop(),
//    `peers` (session -> {pc, stream}), and the callbacks onRemoteStream(session, stream),
//    onPeerClosed(session), onConnectionState(session, state).
//  - Call owns local media, call membership/state and the stage UI (tiles keyed by the
//    remote call session id) and talks to a transport through that surface only.

import { live } from '../ws.js';
import { h, icon, avatar, toast, errorToast } from '../ui.js';

const KINDS = ['audio', 'video'];

// Cameras and screens are captured at up to 720p30. What is actually sent adapts per path:
// through the SFU as three simulcast layers (receivers get the one their tile needs), in a
// mesh as one stream per peer, scaled down once there are several peers.
const VIDEO_CONSTRAINTS = { width: { ideal: 1280 }, height: { ideal: 720 }, frameRate: { ideal: 30, max: 30 } };
const SCREEN_CONSTRAINTS = { video: { frameRate: { ideal: 30, max: 30 } }, audio: false };
const MAX_FPS = 30;

// Simulcast layers sent to the SFU, lowest first (rids match the SFU's): every layer keeps
// the full frame rate; the encoder drops the top layers first when bandwidth or CPU is short.
const SIMULCAST = [
  { rid: 'l', scaleResolutionDownBy: 4, maxBitrate: 150_000, maxFramerate: MAX_FPS },
  { rid: 'm', scaleResolutionDownBy: 2, maxBitrate: 500_000, maxFramerate: MAX_FPS },
  { rid: 'h', scaleResolutionDownBy: 1, maxBitrate: 1_500_000, maxFramerate: MAX_FPS },
];

/**
 * Encoder hints for the track a video sender carries: cameras keep their frame rate under
 * pressure (resolution drops first); screens (contentHint "detail") balance both so text
 * stays readable. `extra(encoding, index)` adjusts each encoding. Best effort: browsers
 * without these parameters keep their defaults.
 */
async function tuneVideoSender(sender, extra) {
  if (!sender || !sender.getParameters) return;
  try {
    const params = sender.getParameters();
    if (!params.encodings || !params.encodings.length) return;
    const track = sender.track;
    const screen = !!track && (track.contentHint === 'detail' || track.contentHint === 'text');
    params.degradationPreference = screen ? 'balanced' : 'maintain-framerate';
    params.encodings.forEach((e, i) => {
      e.maxFramerate = MAX_FPS;
      if (extra) extra(e, i);
    });
    await sender.setParameters(params);
  } catch (e) {
    console.warn('video sender parameters', e);
  }
}

export class MeshTransport {
  /** @param {{roomId: string, session: string, iceServers: RTCIceServer[]}} opts */
  constructor({ roomId, session, iceServers }) {
    this.roomId = roomId;
    this.session = session;
    this.iceServers = iceServers || [];
    /** session -> peer record ({pc, stream, polite, makingOffer, ignoreOffer, chain}) */
    this.peers = new Map();
    this.tracks = { audio: null, video: null };
    this.offs = [];
    this.onRemoteStream = () => {};
    this.onPeerClosed = () => {};
    this.onConnectionState = () => {};
  }

  /**
   * Starts sending the first audio and video track of `localStream` (either may be missing:
   * receive-only) and calls the members already in the call (from call.self).
   */
  start(localStream, members = []) {
    this.tracks = { audio: localStream.getAudioTracks()[0] || null, video: localStream.getVideoTracks()[0] || null };
    this.offs.push(
      live.on('call.signal', (ev) => {
        if (ev.room_id === this.roomId) this.onSignal(ev.from, ev.payload);
      }),
      live.on('call.left', (ev) => {
        if (ev.room_id === this.roomId && ev.session !== this.session) this.closePeer(ev.session);
      }),
      // Newcomers call us (they send the offers), so nothing to do on call.joined: their
      // peer connection is created lazily when the offer arrives.
    );
    // The joiner offers to everyone already in the call.
    for (const m of members) this.peer(m.session, true);
  }

  /** Swaps the outgoing track of one kind on every connection (no renegotiation needed). */
  replaceTrack(kind, track) {
    this.tracks[kind] = track || null;
    for (const p of this.peers.values()) {
      const t = this.transceiver(p.pc, kind);
      if (t) t.sender.replaceTrack(track || null).then(() => kind === 'video' && this.tune(p)).catch((e) => console.warn('replaceTrack', e));
    }
  }

  /**
   * Every peer gets its own encode in a mesh, so with several peers each stream is halved in
   * resolution (720p capture → 360p) to keep the frame rate on small machines and uplinks.
   */
  tune(p) {
    const t = this.transceiver(p.pc, 'video');
    const several = this.peers.size > 1;
    return tuneVideoSender(t && t.sender, (e) => {
      e.scaleResolutionDownBy = several ? 2 : 1;
      e.maxBitrate = several ? 600_000 : 1_500_000;
    });
  }

  tuneAll() {
    for (const p of this.peers.values()) this.tune(p);
  }

  stop() {
    this.offs.forEach((off) => off());
    this.offs = [];
    for (const s of [...this.peers.keys()]) this.closePeer(s, false);
  }

  // ---- internals ------------------------------------------------------------------

  send(to, payload) {
    live.send({ type: 'call.signal', room_id: this.roomId, session: this.session, to, payload });
  }

  /** The first transceiver of a media kind (we use exactly one audio and one video). */
  transceiver(pc, kind) {
    return pc.getTransceivers().find((t) => t.receiver.track && t.receiver.track.kind === kind && !t.stopped);
  }

  peer(session, initiator) {
    let p = this.peers.get(session);
    if (p) return p;
    const pc = new RTCPeerConnection({ iceServers: this.iceServers });
    p = {
      session,
      pc,
      stream: new MediaStream(),
      // Exactly one side of each pair is polite; session ids are random and unique.
      polite: this.session > session,
      makingOffer: false,
      ignoreOffer: false,
      chain: Promise.resolve(),
    };
    this.peers.set(session, p);
    pc.onicecandidate = ({ candidate }) => { if (candidate) this.send(session, { candidate: candidate.toJSON() }); };
    pc.onnegotiationneeded = async () => {
      try {
        p.makingOffer = true;
        await pc.setLocalDescription();
        this.send(session, { description: pc.localDescription.toJSON() });
      } catch (e) {
        console.warn('negotiation failed', e);
      } finally {
        p.makingOffer = false;
      }
    };
    pc.ontrack = ({ track }) => {
      if (!p.stream.getTracks().includes(track)) p.stream.addTrack(track);
      this.onRemoteStream(session, p.stream);
    };
    pc.onconnectionstatechange = () => {
      this.onConnectionState(session, pc.connectionState);
      if (pc.connectionState === 'failed') pc.restartIce();
      // Encodings exist once negotiated; the peer count may also have changed meanwhile.
      if (pc.connectionState === 'connected') this.tuneAll();
    };
    if (initiator) {
      // Always negotiate one audio and one video transceiver, even without local devices:
      // turning the camera on later or screen sharing is then just replaceTrack().
      for (const kind of KINDS) pc.addTransceiver(this.tracks[kind] || kind, { direction: 'sendrecv' });
    }
    return p;
  }

  onSignal(from, payload) {
    const p = this.peer(from, false);
    // Handle signals of one peer strictly in order (a candidate must follow its description).
    p.chain = p.chain.then(() => this.handleSignal(p, payload)).catch((e) => console.warn('signal', e));
  }

  async handleSignal(p, { description, candidate }) {
    const { pc } = p;
    if (description) {
      const collision = description.type === 'offer' && (p.makingOffer || pc.signalingState !== 'stable');
      p.ignoreOffer = !p.polite && collision;
      if (p.ignoreOffer) return;
      await pc.setRemoteDescription(description);
      if (description.type === 'offer') {
        // Answering side: send our media on the transceivers the offer created.
        for (const kind of KINDS) {
          const t = this.transceiver(pc, kind);
          if (!t) continue;
          if (t.direction !== 'sendrecv') t.direction = 'sendrecv';
          if (t.sender.track !== this.tracks[kind]) await t.sender.replaceTrack(this.tracks[kind]);
        }
        await pc.setLocalDescription();
        this.send(p.session, { description: pc.localDescription.toJSON() });
      }
    } else if (candidate) {
      try {
        await pc.addIceCandidate(candidate);
      } catch (e) {
        if (!p.ignoreOffer) console.warn('ICE candidate rejected', e);
      }
    }
  }

  closePeer(session, notify = true) {
    const p = this.peers.get(session);
    if (!p) return;
    this.peers.delete(session);
    p.pc.close();
    this.tuneAll();
    if (notify) this.onPeerClosed(session);
  }
}

/**
 * Sends local media to the server's SFU over one peer connection and receives everyone
 * else's media on it. The first offer/answer goes over the event socket (sfu.offer →
 * sfu.answer); later renegotiations, which the server starts whenever someone's tracks
 * appear or go away, travel as JSON session descriptions over the "sfu" data channel.
 * The server labels each forwarded track's stream with the sender's call session.
 */
export class SfuTransport {
  /** @param {{roomId: string, session: string, iceServers: RTCIceServer[]}} opts */
  constructor({ roomId, session, iceServers }) {
    this.roomId = roomId;
    this.session = session;
    this.iceServers = iceServers || [];
    this.pc = null;
    this.channel = null;
    /** remote session -> {session, pc, stream}; every entry shares the one connection. */
    this.peers = new Map();
    this.senders = { audio: null, video: null };
    this.offs = [];
    this.stopped = false;
    this.chain = Promise.resolve();
    this.onRemoteStream = () => {};
    this.onPeerClosed = () => {};
    this.onConnectionState = () => {};
  }

  start(localStream, _members = []) {
    const pc = new RTCPeerConnection({ iceServers: this.iceServers, bundlePolicy: 'max-bundle' });
    this.pc = pc;
    // Created before the first offer so the data channel is part of the initial session.
    this.channel = pc.createDataChannel('sfu', { ordered: true });
    this.channel.onmessage = (e) => this.onChannelMessage(e.data);
    // Always offer one audio and one video sender, even without local devices, so turning
    // the camera on later or screen sharing is just replaceTrack().
    for (const kind of KINDS) {
      const track = (kind === 'audio' ? localStream.getAudioTracks() : localStream.getVideoTracks())[0];
      const init = { direction: 'sendonly' };
      if (kind === 'video') init.sendEncodings = SIMULCAST.map((e) => ({ ...e }));
      this.senders[kind] = pc.addTransceiver(track || kind, init).sender;
    }
    // Tell the SFU which layer to send for each person once the channel opens.
    this.channel.onopen = () => this.sendLayers();
    pc.addEventListener('connectionstatechange', () => {
      if (pc.connectionState === 'connected') this.tuneVideo();
    });
    pc.ontrack = ({ track, streams }) => this.onTrack(track, streams[0]);
    pc.onconnectionstatechange = () => {
      for (const s of this.peers.keys()) this.onConnectionState(s, pc.connectionState);
    };
    this.offs.push(
      live.on('call.left', (ev) => {
        if (ev.room_id === this.roomId && ev.session !== this.session) this.closePeer(ev.session);
      }),
    );
    this.chain = this.offer().catch((e) => {
      if (!this.stopped) errorToast(e);
    });
  }

  replaceTrack(kind, track) {
    const sender = this.senders[kind];
    if (!sender) return;
    sender.replaceTrack(track || null).then(() => kind === 'video' && this.tuneVideo()).catch((e) => console.warn('replaceTrack', e));
  }

  /** Applies encoder hints, sending only the layers up to the one someone wants. */
  tuneVideo() {
    const top = this.publishTop ?? SIMULCAST.length - 1;
    return tuneVideoSender(this.senders.video, (e, i) => {
      e.active = i <= top;
    });
  }

  /** Which simulcast layer ('l', 'm' or 'h') to receive from each remote session. */
  setLayers(want) {
    this.layers = want;
    this.sendLayers();
  }

  sendLayers() {
    if (this.layers && this.channel && this.channel.readyState === 'open') {
      this.channel.send(JSON.stringify({ type: 'layers', want: this.layers }));
    }
  }

  stop() {
    this.stopped = true;
    this.offs.forEach((off) => off());
    this.offs = [];
    for (const s of [...this.peers.keys()]) this.closePeer(s, false);
    this.pc?.close();
  }

  // ---- internals ------------------------------------------------------------------

  async offer() {
    const { pc } = this;
    await pc.setLocalDescription(await pc.createOffer());
    // The SFU is ICE-lite and gathers no trickled candidates: send the offer once our host
    // candidates are in it (bounded, so a slow STUN server cannot stall the call).
    await waitForIce(pc, 1500);
    const res = await live.request({ type: 'sfu.offer', room_id: this.roomId, session: this.session, sdp: pc.localDescription.toJSON() }, 'sfu.answer', 10000);
    if (this.stopped) return;
    await pc.setRemoteDescription(res.sdp);
  }

  onChannelMessage(data) {
    let desc;
    try {
      desc = JSON.parse(data);
    } catch (_) {
      return;
    }
    // The highest simulcast layer anyone watches: pause the layers above it.
    if (desc && desc.type === 'publish') {
      this.publishTop = Math.max(0, SIMULCAST.findIndex((e) => e.rid === desc.max));
      this.tuneVideo();
      return;
    }
    // Strictly in order: an offer must not overlap the previous negotiation.
    this.chain = this.chain.then(async () => {
      if (this.stopped || !desc || !desc.type) return;
      await this.pc.setRemoteDescription(desc);
      if (desc.type === 'offer') {
        await this.pc.setLocalDescription(await this.pc.createAnswer());
        this.channel.send(JSON.stringify(this.pc.localDescription.toJSON()));
      }
    }).catch((e) => console.warn('SFU renegotiation failed', e));
  }

  onTrack(track, stream) {
    const session = stream && stream.id;
    if (!session || session === this.session) return;
    let p = this.peers.get(session);
    if (!p) {
      p = { session, pc: this.pc, stream: new MediaStream() };
      this.peers.set(session, p);
    }
    if (!p.stream.getTracks().includes(track)) p.stream.addTrack(track);
    // The server stops a forwarded track when its sender disconnects; membership itself is
    // driven by call.left.
    track.addEventListener('ended', () => p.stream.removeTrack(track));
    this.onRemoteStream(session, p.stream);
    this.onConnectionState(session, this.pc.connectionState);
  }

  closePeer(session, notify = true) {
    if (!this.peers.delete(session)) return;
    if (notify) this.onPeerClosed(session);
  }
}

/** Resolves when ICE gathering completes or after `ms`, whichever comes first. */
function waitForIce(pc, ms) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => { pc.removeEventListener('icegatheringstatechange', check); clearTimeout(t); resolve(); };
    const check = () => { if (pc.iceGatheringState === 'complete') done(); };
    const t = setTimeout(done, ms);
    pc.addEventListener('icegatheringstatechange', check);
  });
}

/**
 * Picks the media transport: the server's SFU when it runs one (the server says so in
 * call.self, so every participant of a call agrees), else the peer-to-peer mesh.
 */
export function createTransport(opts) {
  return opts.sfu ? new SfuTransport(opts) : new MeshTransport(opts);
}

/**
 * Gets the best local media available: requested audio/video, else audio only, else nothing
 * (receive-only). Returns {stream, note}.
 */
async function acquireMedia(audio, video) {
  const md = navigator.mediaDevices;
  if (!md || !md.getUserMedia || (!audio && !video)) return { stream: new MediaStream(), note: md ? null : 'Media devices unavailable: joining receive-only.' };
  const attempts = [];
  attempts.push({ audio, video: video ? VIDEO_CONSTRAINTS : false });
  if (audio && video) attempts.push({ audio: true, video: false });
  if (audio && video) attempts.push({ audio: false, video: VIDEO_CONSTRAINTS });
  for (const [i, c] of attempts.entries()) {
    try {
      const stream = await md.getUserMedia(c);
      let note = null;
      if (i > 0) note = c.audio ? 'No camera available: joined with audio only.' : 'No microphone available: joined with video only.';
      return { stream, note };
    } catch (_) { /* try the next, smaller request */ }
  }
  return { stream: new MediaStream(), note: 'No camera or microphone access: joined receive-only.' };
}

/**
 * A call in one room: local media, members, the transport and the stage UI (`el`).
 * opts: {token, self: {id, name}, config, onEnd()}
 */
export class Call {
  constructor({ token, self, config, onEnd }) {
    this.token = token;
    this.self = self;
    this.config = config || {};
    this.onEnd = onEnd || (() => {});
    this.roomId = null;
    this.session = null;
    this.transport = null;
    this.stream = null;
    this.camTrack = null;
    this.screenTrack = null;
    this.state = { audio: false, video: false, screen: false, hand: false };
    /** session -> member (others only) */
    this.members = new Map();
    /** session -> tile record */
    this.tiles = new Map();
    this.offs = [];
    this.ended = false;
    this.levels = new Map();
    this.audioCtx = null;
    /** Called after a tile is painted: (session or null for local, tile record, state). */
    this.onTilePaint = () => {};
    this.buildUI();
  }

  /** Peer connections (exposed for diagnostics and tests). */
  get peers() {
    return this.transport ? this.transport.peers : new Map();
  }

  // ---- lifecycle --------------------------------------------------------------------

  async join({ audio = true, video = false } = {}) {
    const { stream, note } = await acquireMedia(audio, video);
    if (this.ended) { stream.getTracks().forEach((t) => t.stop()); return; }
    this.stream = stream;
    this.camTrack = stream.getVideoTracks()[0] || null;
    if (this.camTrack) this.camTrack.contentHint = 'motion';
    this.state.audio = !!stream.getAudioTracks()[0];
    this.state.video = !!this.camTrack;
    if (note) toast(note, 'warn');
    this.offs.push(
      live.on('call.joined', (ev) => this.onJoined(ev)),
      live.on('call.left', (ev) => this.onLeft(ev)),
      live.on('call.state', (ev) => this.onState(ev)),
      live.on('call.recording', (ev) => {
        if (ev.room_id !== this.roomId || this.recording === ev.active) return;
        this.recording = ev.active;
        this.paintControls();
        if (ev.by && ev.by !== this.self.name) toast(ev.active ? `${ev.by} started recording this call` : `${ev.by} stopped the recording`);
      }),
      // The server drops our session when the socket closes; rejoin after a reconnect.
      live.on('hello', () => { if (this.session) this.rejoin(); }),
    );
    await this.connect();
    this.tile(null);
    this.paintControls();
    if (this.stream.getAudioTracks()[0]) this.watchLevel(null, this.stream);
    window.__ferriteCall = this;
    this.levelTimer = setInterval(() => this.measureLevels(), 250);
    // Ask the SFU for the simulcast layer each tile needs, now and whenever tiles resize or
    // the tab is hidden (then the smallest layer is enough).
    this.layerObserver = new ResizeObserver(() => this.scheduleLayers());
    for (const [key, t] of this.tiles) if (key !== 'local') this.layerObserver.observe(t.el);
    this.onVisibility = () => this.scheduleLayers();
    document.addEventListener('visibilitychange', this.onVisibility);
    this.scheduleLayers();
  }

  async connect() {
    const res = await live.request({ type: 'call.join', room: this.token, audio: this.state.audio, video: this.state.video }, 'call.self', 10000);
    if (this.ended) { live.send({ type: 'call.leave', room_id: res.room_id, session: res.session }); return; }
    this.roomId = res.room_id;
    this.session = res.session;
    this.recording = !!res.recording;
    this.canRecord = !!res.can_record;
    this.paintControls();
    const iceServers = (res.ice_servers && res.ice_servers.length ? res.ice_servers : this.config.ice_servers) || [];
    this.transport = createTransport({ roomId: this.roomId, session: this.session, iceServers, sfu: !!res.sfu });
    this.transport.onRemoteStream = (s, st) => this.attachRemote(s, st);
    this.transport.onPeerClosed = (s) => this.removeMember(s);
    this.transport.onConnectionState = (s, st) => {
      const t = this.tiles.get(s);
      if (t) t.el.dataset.conn = st;
    };
    for (const m of res.members) this.addMember(m);
    const outgoing = [this.stream.getAudioTracks()[0], this.currentVideoTrack()].filter(Boolean);
    this.transport.start(new MediaStream(outgoing), res.members);
    if (this.layerObserver) this.scheduleLayers();
    // Report a hand or screen state that survived a rejoin.
    if (this.state.hand || this.state.screen) this.sendState();
  }

  async rejoin() {
    if (this.ended) return;
    this.transport?.stop();
    for (const s of [...this.members.keys()]) this.removeMember(s);
    this.session = null;
    try {
      await this.connect();
    } catch (e) {
      errorToast(e);
      this.leave();
    }
  }

  leave() {
    if (this.ended) return;
    this.ended = true;
    if (this.session) live.send({ type: 'call.leave', room_id: this.roomId, session: this.session });
    this.transport?.stop();
    this.offs.forEach((off) => off());
    clearInterval(this.levelTimer);
    this.stream?.getTracks().forEach((t) => t.stop());
    this.camTrack?.stop();
    this.screenTrack?.stop();
    this.audioCtx?.close().catch(() => {});
    this.layerObserver?.disconnect();
    clearTimeout(this.layerTimer);
    if (this.onVisibility) document.removeEventListener('visibilitychange', this.onVisibility);
    this.el.remove();
    if (window.__ferriteCall === this) window.__ferriteCall = null;
    this.onEnd();
  }

  // ---- membership & state --------------------------------------------------------------

  onJoined(ev) {
    if (ev.room_id !== this.roomId || !this.session || ev.member.session === this.session) return;
    this.addMember(ev.member);
  }

  onLeft(ev) {
    if (ev.room_id !== this.roomId) return;
    if (ev.session === this.session) {
      // Dropped by the server (e.g. another tab took over): end locally too.
      this.session = null;
      this.leave();
      return;
    }
    this.removeMember(ev.session);
  }

  onState(ev) {
    if (ev.room_id !== this.roomId || ev.member.session === this.session) return;
    if (!this.members.has(ev.member.session)) return;
    this.members.set(ev.member.session, ev.member);
    this.paintTile(ev.member.session);
  }

  addMember(m) {
    this.members.set(m.session, m);
    this.tile(m.session);
    this.paintTile(m.session);
    this.layout();
  }

  removeMember(session) {
    this.members.delete(session);
    const t = this.tiles.get(session);
    if (t) this.layerObserver?.unobserve(t.el);
    if (t) {
      t.el.remove();
      this.tiles.delete(session);
    }
    this.levels.delete(session);
    this.transport?.peers.get(session) && this.transport.closePeer?.(session, false);
    this.layout();
  }

  sendState() {
    if (!this.session) return;
    live.send({ type: 'call.state', room_id: this.roomId, session: this.session, ...this.state });
    this.paintTile(null);
    this.paintControls();
  }

  currentVideoTrack() {
    return this.state.screen ? this.screenTrack : (this.state.video ? this.camTrack : null);
  }

  // ---- controls ---------------------------------------------------------------------------

  async toggleMic() {
    let track = this.stream.getAudioTracks()[0];
    if (!track) {
      try {
        const s = await navigator.mediaDevices.getUserMedia({ audio: true });
        track = s.getAudioTracks()[0];
        this.stream.addTrack(track);
        this.transport.replaceTrack('audio', track);
        this.watchLevel(null, this.stream);
        this.state.audio = true;
      } catch (e) {
        toast('Microphone unavailable', 'error');
        return;
      }
    } else {
      this.state.audio = !this.state.audio;
      track.enabled = this.state.audio;
    }
    this.sendState();
  }

  async toggleCam() {
    if (this.state.video) {
      // Release the camera entirely (turns the camera light off).
      this.camTrack?.stop();
      this.camTrack = null;
      this.state.video = false;
    } else {
      try {
        const s = await navigator.mediaDevices.getUserMedia({ video: VIDEO_CONSTRAINTS });
        this.camTrack = s.getVideoTracks()[0];
        this.camTrack.contentHint = 'motion';
        this.state.video = true;
      } catch (e) {
        toast('Camera unavailable', 'error');
        return;
      }
    }
    if (!this.state.screen) this.transport.replaceTrack('video', this.camTrack);
    this.paintLocalVideo();
    this.sendState();
  }

  async toggleScreen() {
    if (this.state.screen) return this.stopScreen();
    try {
      const s = await navigator.mediaDevices.getDisplayMedia(SCREEN_CONSTRAINTS);
      this.screenTrack = s.getVideoTracks()[0];
      this.screenTrack.contentHint = 'detail';
    } catch (e) {
      if (e.name !== 'NotAllowedError') toast(`Screen sharing failed: ${e.message}`, 'error');
      return;
    }
    // The browser's own "Stop sharing" button ends the track.
    this.screenTrack.addEventListener('ended', () => this.stopScreen());
    this.state.screen = true;
    this.transport.replaceTrack('video', this.screenTrack);
    this.paintLocalVideo();
    this.sendState();
  }

  stopScreen() {
    if (!this.state.screen) return;
    this.screenTrack?.stop();
    this.screenTrack = null;
    this.state.screen = false;
    if (!this.ended) {
      this.transport?.replaceTrack('video', this.camTrack);
      this.paintLocalVideo();
      this.sendState();
    }
  }

  async toggleRecording() {
    const action = this.recording ? 'stop' : 'start';
    this.btns.record.disabled = true;
    try {
      await live.request({ type: 'call.record', room_id: this.roomId, action }, 'call.recording', 15000);
    } catch (e) {
      errorToast(e);
    } finally {
      this.btns.record.disabled = false;
    }
  }

  toggleHand() {
    this.state.hand = !this.state.hand;
    this.sendState();
  }

  // ---- UI -------------------------------------------------------------------------------

  buildUI() {
    const btn = (key, label, run, cls = '') => {
      const b = h(`button.call-btn${cls}`, { type: 'button', 'aria-label': label, title: label, onclick: run });
      this.btns[key] = b;
      return b;
    };
    this.btns = {};
    this.grid = h('div.call-grid');
    const screenOk = !!(navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia);
    this.el = h('section.call-stage', { 'aria-label': 'Call' },
      this.grid,
      h('div.call-controls',
        btn('mic', 'Mute microphone', () => this.toggleMic()),
        btn('cam', 'Turn camera off', () => this.toggleCam()),
        screenOk && btn('screen', 'Share screen', () => this.toggleScreen()),
        btn('hand', 'Raise hand', () => this.toggleHand()),
        btn('record', 'Start recording', () => this.toggleRecording()),
        btn('leave', 'Leave call', () => this.leave(), '.danger')));
    // Everyone sees when a call is recorded.
    this.recBadge = h('div.call-rec', { role: 'status', hidden: true }, h('span.call-rec-dot'), 'Recording');
    this.el.prepend(this.recBadge);
    this.paintControls();
  }

  paintControls() {
    const set = (key, on, onIcon, offIcon, onLabel, offLabel, warnWhenOff = false) => {
      const b = this.btns[key];
      if (!b) return;
      b.replaceChildren(icon(on ? onIcon : offIcon));
      b.setAttribute('aria-label', on ? onLabel : offLabel);
      b.title = on ? onLabel : offLabel;
      b.setAttribute('aria-pressed', String(on));
      b.classList.toggle('off', warnWhenOff && !on);
      b.classList.toggle('on', !warnWhenOff && on);
    };
    set('mic', this.state.audio, 'mic', 'mic-off', 'Mute microphone', 'Unmute microphone', true);
    set('cam', this.state.video, 'video', 'video-off', 'Turn camera off', 'Turn camera on', true);
    set('screen', this.state.screen, 'screen', 'screen', 'Stop sharing screen', 'Share screen');
    set('hand', this.state.hand, 'hand', 'hand', 'Lower hand', 'Raise hand');
    set('record', !!this.recording, 'record', 'record', 'Stop recording', 'Start recording');
    // Only moderators can record, and only through the server's SFU.
    this.btns.record.hidden = !this.canRecord;
    this.btns.record.classList.toggle('recording', !!this.recording);
    if (this.recBadge) this.recBadge.hidden = !this.recording;
    this.btns.leave.replaceChildren(icon('phone-off'));
  }

  /** Tile for a remote session, or the local tile when session is null. */
  tile(session) {
    const key = session || 'local';
    let t = this.tiles.get(key);
    if (t) return t;
    const local = !session;
    const name = local ? `${this.self.name} (you)` : this.members.get(session)?.actor.name || 'Guest';
    const seed = local ? this.self.id : this.members.get(session)?.actor.id;
    const video = h('video', { autoplay: true, playsinline: true });
    video.muted = local;
    const label = h('div.tile-label', h('span.ellipsis', name));
    const flags = h('div.tile-flags');
    const el = h(`div.call-tile${local ? '.local' : ''}`, { dataset: { session: key } },
      video,
      h('div.tile-placeholder', avatar(name.replace(/ \((you|guest)\)$/, ''), { size: 'lg', seed })),
      h('div.tile-foot', label, flags));
    t = { el, video, flags, local };
    this.tiles.set(key, t);
    if (local) this.grid.prepend(el);
    else {
      this.grid.appendChild(el);
      this.layerObserver?.observe(el);
    }
    if (local) this.paintLocalVideo();
    this.layout();
    return t;
  }

  scheduleLayers() {
    clearTimeout(this.layerTimer);
    this.layerTimer = setTimeout(() => this.updateLayers(), 250);
  }

  /** Picks a simulcast layer per remote tile from its size on screen (in device pixels). */
  updateLayers() {
    if (!this.transport || !this.transport.setLayers) return;
    const want = {};
    for (const [session, t] of this.tiles) {
      if (session === 'local') continue;
      const px = t.el.clientHeight * (window.devicePixelRatio || 1);
      want[session] = document.hidden ? 'l' : px >= 480 ? 'h' : px >= 200 ? 'm' : 'l';
    }
    this.transport.setLayers(want);
  }

  paintLocalVideo() {
    const t = this.tiles.get('local');
    if (!t) return;
    const track = this.currentVideoTrack();
    const cur = t.video.srcObject && t.video.srcObject.getVideoTracks()[0];
    if (cur !== track) {
      t.video.srcObject = track ? new MediaStream([track]) : null;
      if (track) t.video.play().catch(() => {});
    }
    t.el.classList.toggle('mirror', !!track && !this.state.screen);
    this.paintTile(null);
  }

  paintTile(session) {
    const t = this.tiles.get(session || 'local');
    if (!t) return;
    const st = session ? this.members.get(session) : this.state;
    if (!st) return;
    const showVideo = !!(st.video || st.screen) && (session ? !!t.hasVideo : !!this.currentVideoTrack());
    t.el.classList.toggle('no-video', !showVideo);
    t.el.classList.toggle('hand', !!st.hand);
    t.el.classList.toggle('screen', !!st.screen);
    const f = [];
    if (!st.audio) f.push(h('span.tile-flag', { title: 'Muted', dataset: { flag: 'muted' } }, icon('mic-off', 'sm')));
    if (st.screen) f.push(h('span.tile-flag', { title: 'Sharing screen', dataset: { flag: 'screen' } }, icon('screen', 'sm')));
    if (st.hand) f.push(h('span.tile-flag.hand', { title: 'Hand raised', dataset: { flag: 'hand' } }, icon('hand', 'sm')));
    t.flags.replaceChildren(...f);
    this.onTilePaint(session, t, st);
  }

  attachRemote(session, stream) {
    const t = this.tile(session);
    if (t.video.srcObject !== stream) {
      t.video.srcObject = stream;
      t.video.play().catch(() => this.audioBlocked(t));
    }
    const vt = stream.getVideoTracks()[0];
    if (vt && !t.watching) {
      // A remote video track exists from the start but only produces frames while the
      // sender sends something; follow mute/unmute to switch to and from the avatar.
      t.watching = true;
      const upd = () => { t.hasVideo = !vt.muted; this.paintTile(session); };
      vt.addEventListener('mute', upd);
      vt.addEventListener('unmute', upd);
      upd();
    }
    if (stream.getAudioTracks()[0]) this.watchLevel(session, stream);
  }

  /** Autoplay with sound was refused (no user gesture yet): offer a button. */
  audioBlocked(t) {
    if (t.el.querySelector('.tile-unblock')) return;
    t.video.muted = true;
    t.video.play().catch(() => {});
    const b = h('button.btn.sm.tile-unblock', { onclick: () => { t.video.muted = false; t.video.play().catch(() => {}); b.remove(); } }, icon('mic', 'sm'), 'Enable sound');
    t.el.appendChild(b);
  }

  layout() {
    const n = this.grid.children.length || 1;
    const cols = n <= 1 ? 1 : n <= 4 ? 2 : n <= 9 ? 3 : 4;
    this.grid.style.setProperty('--cols', cols);
    this.grid.dataset.count = n;
  }

  // ---- active speaker -----------------------------------------------------------------------

  watchLevel(session, stream) {
    const key = session || 'local';
    const track = stream.getAudioTracks()[0];
    if (!track || (this.levels.get(key) && this.levels.get(key).track === track)) return;
    try {
      if (!this.audioCtx) this.audioCtx = new AudioContext();
      const src = this.audioCtx.createMediaStreamSource(new MediaStream([track]));
      const an = this.audioCtx.createAnalyser();
      an.fftSize = 512;
      src.connect(an);
      this.levels.set(key, { track, an, buf: new Uint8Array(an.fftSize), smooth: 0 });
    } catch (_) { /* WebAudio unavailable: no speaker highlight */ }
  }

  measureLevels() {
    if (!this.levels.size) return;
    if (this.audioCtx && this.audioCtx.state === 'suspended') this.audioCtx.resume().catch(() => {});
    let best = null;
    let bestLevel = 0.02;
    for (const [key, l] of this.levels) {
      const muted = key === 'local' ? !this.state.audio : this.members.get(key) && !this.members.get(key).audio;
      l.an.getByteTimeDomainData(l.buf);
      let sum = 0;
      for (const v of l.buf) sum += ((v - 128) / 128) ** 2;
      l.smooth = l.smooth * 0.6 + Math.sqrt(sum / l.buf.length) * 0.4;
      if (!muted && l.smooth > bestLevel) { best = key; bestLevel = l.smooth; }
    }
    for (const [key, t] of this.tiles) t.el.classList.toggle('speaking', key === best);
  }
}
