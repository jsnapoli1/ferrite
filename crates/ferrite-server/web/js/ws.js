// The live event socket: one WebSocket per tab carries chat, notifications, file changes,
// board changes, AI progress and call signaling. Reconnects with backoff.

const handlers = new Map();
let socket = null;
let retry = 0;
let queue = [];
let stopped = false;
let guestSecret = null;

function emit(type, ev) {
  for (const fn of handlers.get(type) || []) {
    try { fn(ev); } catch (e) { console.error(e); }
  }
  for (const fn of handlers.get('*') || []) {
    try { fn(ev); } catch (e) { console.error(e); }
  }
}

function connect() {
  if (stopped) return;
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const qs = guestSecret ? `?guest=${encodeURIComponent(guestSecret)}` : '';
  socket = new WebSocket(`${proto}//${location.host}/api/ws${qs}`);
  socket.onopen = () => {
    retry = 0;
    const pending = queue;
    queue = [];
    for (const m of pending) socket.send(m);
  };
  socket.onmessage = (e) => {
    let ev;
    try { ev = JSON.parse(e.data); } catch (_) { return; }
    if (ev.type === 'hello') live.conn = ev.conn;
    emit(ev.type, ev);
  };
  socket.onclose = () => {
    emit('disconnected', {});
    if (stopped) return;
    const delay = Math.min(15000, 500 * 2 ** retry++) + Math.random() * 300;
    setTimeout(() => { emit('reconnecting', {}); connect(); }, delay);
  };
}

export const live = {
  conn: null,
  start(opts = {}) {
    guestSecret = opts.guest || null;
    stopped = false;
    if (!socket) connect();
    setInterval(() => live.send({ type: 'ping' }), 25000);
  },
  stop() {
    stopped = true;
    if (socket) socket.close();
    socket = null;
  },
  /** Subscribe to an event type ('*' for all). Returns an unsubscribe function. */
  on(type, fn) {
    if (!handlers.has(type)) handlers.set(type, new Set());
    handlers.get(type).add(fn);
    return () => handlers.get(type).delete(fn);
  },
  send(obj) {
    const m = JSON.stringify(obj);
    if (socket && socket.readyState === WebSocket.OPEN) socket.send(m);
    else queue.push(m);
  },
  /** Sends and resolves with the first event of `type` (e.g. call.self after call.join). */
  request(obj, type, timeoutMs = 8000) {
    return new Promise((resolve, reject) => {
      const off = live.on(type, (ev) => { off(); offErr(); clearTimeout(t); resolve(ev); });
      const offErr = live.on('error', (ev) => {
        if (ev.for === obj.type) { off(); offErr(); clearTimeout(t); reject(new Error(ev.error)); }
      });
      const t = setTimeout(() => { off(); offErr(); reject(new Error('timed out')); }, timeoutMs);
      live.send(obj);
    });
  },
};
