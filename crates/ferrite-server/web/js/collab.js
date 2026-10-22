// Live collaboration: a Yjs document synced with the server over /api/collab/{fileId}
// (the y-websocket protocol), plus awareness for presence and cursors.

import { Y, syncProtocol, awarenessProtocol, encoding, decoding } from '/vendor/yjs.mjs';

export { Y };

const MSG_SYNC = 0;
const MSG_AWARENESS = 1;
const COLORS = ['#c2410c', '#0f766e', '#7c3aed', '#be185d', '#1d4ed8', '#15803d', '#b45309', '#0e7490'];

export function colorFor(seed) {
  let hash = 0;
  for (const ch of String(seed)) hash = (hash * 31 + ch.codePointAt(0)) >>> 0;
  return COLORS[hash % COLORS.length];
}

export class CollabProvider {
  /**
   * @param {string} fileId
   * @param {{user: {id: string, display_name: string}, onStatus?: (s: string) => void, onReset?: () => void, onPeers?: (peers: any[]) => void}} opts
   */
  constructor(fileId, opts) {
    this.fileId = fileId;
    this.opts = opts;
    this.doc = new Y.Doc();
    this.awareness = new awarenessProtocol.Awareness(this.doc);
    this.awareness.setLocalStateField('user', { id: opts.user.id, name: opts.user.display_name, color: colorFor(opts.user.id) });
    this.ws = null;
    this.retry = 0;
    this.destroyed = false;
    this.synced = new Promise((resolve) => { this._resolveSynced = resolve; });
    this._onDocUpdate = (update, origin) => {
      if (origin === this) return;
      const enc = encoding.createEncoder();
      encoding.writeVarUint(enc, MSG_SYNC);
      syncProtocol.writeUpdate(enc, update);
      this._send(encoding.toUint8Array(enc));
    };
    this._onAwareness = ({ added, updated, removed }, origin) => {
      const changed = added.concat(updated, removed);
      if (origin === 'local' || origin === undefined) {
        const enc = encoding.createEncoder();
        encoding.writeVarUint(enc, MSG_AWARENESS);
        encoding.writeVarUint8Array(enc, awarenessProtocol.encodeAwarenessUpdate(this.awareness, changed));
        this._send(encoding.toUint8Array(enc));
      }
      if (this.opts.onPeers) this.opts.onPeers(this.peers());
    };
    this.doc.on('update', this._onDocUpdate);
    this.awareness.on('update', this._onAwareness);
    this._beforeUnload = () => awarenessProtocol.removeAwarenessStates(this.awareness, [this.doc.clientID], 'unload');
    window.addEventListener('beforeunload', this._beforeUnload);
    this._connect();
  }

  peers() {
    const out = [];
    this.awareness.getStates().forEach((state, clientId) => {
      if (clientId !== this.doc.clientID && state.user) out.push({ clientId, ...state.user, cursor: state.cursor });
    });
    return out;
  }

  _status(s) {
    this.status = s;
    if (this.opts.onStatus) this.opts.onStatus(s);
  }

  _send(buf) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) this.ws.send(buf);
  }

  _connect() {
    if (this.destroyed) return;
    this._status('connecting');
    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const ws = new WebSocket(`${proto}//${location.host}/api/collab/${encodeURIComponent(this.fileId)}`);
    ws.binaryType = 'arraybuffer';
    this.ws = ws;
    ws.onopen = () => {
      this.retry = 0;
      this._status('connected');
      const enc = encoding.createEncoder();
      encoding.writeVarUint(enc, MSG_SYNC);
      syncProtocol.writeSyncStep1(enc, this.doc);
      ws.send(encoding.toUint8Array(enc));
      if (this.awareness.getLocalState() !== null) {
        const a = encoding.createEncoder();
        encoding.writeVarUint(a, MSG_AWARENESS);
        encoding.writeVarUint8Array(a, awarenessProtocol.encodeAwarenessUpdate(this.awareness, [this.doc.clientID]));
        ws.send(encoding.toUint8Array(a));
      }
    };
    ws.onmessage = (e) => {
      const decoder = decoding.createDecoder(new Uint8Array(e.data));
      // A frame may hold several messages back to back.
      while (decoding.hasContent(decoder)) {
        const type = decoding.readVarUint(decoder);
        if (type === MSG_SYNC) {
          const enc = encoding.createEncoder();
          encoding.writeVarUint(enc, MSG_SYNC);
          const kind = syncProtocol.readSyncMessage(decoder, enc, this.doc, this);
          if (encoding.length(enc) > 1) this._send(encoding.toUint8Array(enc));
          if (kind === syncProtocol.messageYjsSyncStep2) {
            this._status('synced');
            this._resolveSynced();
          }
        } else if (type === MSG_AWARENESS) {
          awarenessProtocol.applyAwarenessUpdate(this.awareness, decoding.readVarUint8Array(decoder), this);
        } else {
          break;
        }
      }
    };
    ws.onclose = (e) => {
      if (this.destroyed) return;
      if (e.code === 4000) {
        // The file was replaced outside the live session: start over from the file.
        this._status('reset');
        if (this.opts.onReset) this.opts.onReset();
        return;
      }
      if (e.code === 1008 || e.code === 4403) {
        this._status('denied');
        return;
      }
      this._status('offline');
      const delay = Math.min(10000, 400 * 2 ** this.retry++);
      setTimeout(() => this._connect(), delay);
    };
  }

  /** Sets a field of this user's awareness state (e.g. cursor or selection). */
  setLocal(field, value) {
    this.awareness.setLocalStateField(field, value);
  }

  destroy() {
    this.destroyed = true;
    window.removeEventListener('beforeunload', this._beforeUnload);
    try { awarenessProtocol.removeAwarenessStates(this.awareness, [this.doc.clientID], 'local'); } catch (_) { /* closing */ }
    this.doc.off('update', this._onDocUpdate);
    this.awareness.off('update', this._onAwareness);
    if (this.ws) this.ws.close();
    this.awareness.destroy();
    this.doc.destroy();
  }
}

/**
 * Binds a <textarea> to a Y.Text, preserving the local caret across remote edits.
 * Returns an unbind function.
 */
export function bindTextarea(ytext, textarea, onChange) {
  let applying = false;
  textarea.value = ytext.toString();
  const onInput = () => {
    if (applying) return;
    const next = textarea.value;
    const prev = ytext.toString();
    if (next === prev) return;
    // Minimal single-range diff: common prefix and suffix.
    let start = 0;
    while (start < prev.length && start < next.length && prev[start] === next[start]) start++;
    let endPrev = prev.length;
    let endNext = next.length;
    while (endPrev > start && endNext > start && prev[endPrev - 1] === next[endNext - 1]) { endPrev--; endNext--; }
    ytext.doc.transact(() => {
      if (endPrev > start) ytext.delete(start, endPrev - start);
      if (endNext > start) ytext.insert(start, next.slice(start, endNext));
    }, 'local-input');
    if (onChange) onChange();
  };
  const observer = (event, tr) => {
    if (tr.origin === 'local-input') return;
    // Map the caret through the remote change.
    let { selectionStart: s, selectionEnd: e } = textarea;
    let index = 0;
    for (const d of event.changes.delta) {
      if (d.retain) index += d.retain;
      else if (d.insert) {
        const n = d.insert.length;
        if (index <= s) s += n;
        if (index <= e) e += n;
        index += n;
      } else if (d.delete) {
        if (index < s) s -= Math.min(d.delete, s - index);
        if (index < e) e -= Math.min(d.delete, e - index);
      }
    }
    applying = true;
    textarea.value = ytext.toString();
    applying = false;
    if (document.activeElement === textarea) textarea.setSelectionRange(s, e);
    if (onChange) onChange();
  };
  textarea.addEventListener('input', onInput);
  ytext.observe(observer);
  return () => {
    textarea.removeEventListener('input', onInput);
    ytext.unobserve(observer);
  };
}
