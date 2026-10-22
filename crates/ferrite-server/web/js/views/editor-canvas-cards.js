// Content of canvas items: notes, text, files (with image thumbnails), mind maps (title and
// first-level branches), boards (columns with card counts), links and frames. Remote data is
// cached per reference and refreshed on live `files.changed` / `boards.changed` events.

import { api } from '../api.js';
import { live } from '../ws.js';
import { markdown } from '../markdown.js';
import { h, icon, fileIcon } from '../ui.js';
import { withVars } from './editor-common.js';

const BRANCH_COLORS = ['#e11d48', '#ea580c', '#ca8a04', '#16a34a', '#0891b2', '#2563eb', '#7c3aed', '#db2777'];

export const NOTE_COLORS = ['#fde68a', '#fecaca', '#bbf7d0', '#bfdbfe', '#ddd6fe', '#fbcfe8', '#fed7aa', '#e5e7eb'];

export const DEFAULT_SIZE = {
  note: [200, 150], text: [260, 64], file: [220, 76], mindmap: [260, 200], board: [280, 180], link: [260, 86], frame: [560, 380],
};

/** Caches fetches by key, notifies subscribers when data arrives or is invalidated. */
class Store {
  constructor(fetcher) { this.fetcher = fetcher; this.data = new Map(); this.subs = new Map(); }
  get(key) {
    let rec = this.data.get(key);
    if (!rec) {
      rec = { value: undefined, error: null, loading: true };
      this.data.set(key, rec);
      this.load(key, rec);
    }
    return rec;
  }
  async load(key, rec) {
    try {
      rec.value = await this.fetcher(key);
      rec.error = null;
    } catch (e) {
      rec.error = e;
    }
    rec.loading = false;
    // Copy: subscribers re-subscribe while being notified.
    for (const fn of [...(this.subs.get(key) || [])]) fn();
  }
  refresh(key) {
    const rec = this.data.get(key);
    if (rec) this.load(key, rec);
  }
  on(key, fn) {
    if (!this.subs.has(key)) this.subs.set(key, new Set());
    this.subs.get(key).add(fn);
    return () => this.subs.get(key)?.delete(fn);
  }
}

/** Data sources for one canvas session; call destroy() when leaving. */
export function createSources() {
  const files = new Store((id) => api.get(`/api/files/${encodeURIComponent(id)}`).then((r) => r.entry));
  const maps = new Store(async (id) => {
    const text = await api.get(`/api/files/${encodeURIComponent(id)}/content`);
    const v = typeof text === 'string' ? JSON.parse(text) : text;
    const nodes = Array.isArray(v.nodes) ? v.nodes : [];
    const root = nodes.filter((n) => !n.parent).sort((a, b) => (a.order || 0) - (b.order || 0))[0];
    const branches = root ? nodes.filter((n) => n.parent === root.id).sort((a, b) => (a.order || 0) - (b.order || 0)) : [];
    return { title: root?.text || v.title || '', branches: branches.map((b) => ({ text: b.text, color: b.color, count: nodes.filter((n) => n.parent === b.id).length })), total: nodes.length };
  });
  const boards = new Store((id) => api.get(`/api/boards/${encodeURIComponent(id)}`).then((r) => r.board));
  const timers = new Map();
  const later = (store, key) => {
    clearTimeout(timers.get(key));
    timers.set(key, setTimeout(() => store.refresh(key), 400));
  };
  const offFiles = live.on('files.changed', (ev) => {
    if (files.data.has(ev.node_id)) later(files, ev.node_id);
    if (maps.data.has(ev.node_id)) later(maps, `${ev.node_id}`);
  });
  const offBoards = live.on('boards.changed', (ev) => { if (boards.data.has(ev.board_id)) later(boards, ev.board_id); });
  return {
    files, maps, boards,
    destroy() { offFiles(); offBoards(); timers.forEach((t) => clearTimeout(t)); },
  };
}

function isImage(entry, name) {
  if (entry) return (entry.mime || '').startsWith('image/') && !/svg/.test(entry.mime);
  return /\.(png|jpe?g|gif|webp|avif)$/i.test(name || '');
}

/** Builds the content element of an item; `subscribe(store, key)` re-renders on data change. */
export function renderContent(item, sources, subscribe) {
  const t = item.type;
  if (t === 'note') {
    return h('div.cv-note-body', item.text ? markdown(item.text) : h('p.cv-placeholder', 'Double-click to write'));
  }
  if (t === 'text') {
    return h('div.cv-text-body', item.text ? markdown(item.text) : h('p.cv-placeholder', 'Text'));
  }
  if (t === 'frame') {
    return h('div.cv-frame-title', item.text || 'Frame');
  }
  if (t === 'link') {
    let host = '';
    try { host = new URL(item.ref).hostname; } catch (_) { host = item.ref || ''; }
    return h('div.cv-card-body.cv-link-body',
      h('div.cv-card-head', icon('globe', 'sm'), h('span.ellipsis', item.text || host || 'Link')),
      h('div.small.muted.ellipsis', item.ref || 'No address'));
  }
  if (t === 'file') {
    const rec = item.ref ? sources.files.get(item.ref) : null;
    if (item.ref) subscribe(sources.files, item.ref);
    const entry = rec && rec.value;
    const name = entry?.name || item.text || 'File';
    const gone = rec && rec.error;
    const img = !gone && item.ref && isImage(entry, name);
    return h(`div.cv-card-body.cv-file-body${img ? '.has-thumb' : ''}`,
      img && h('div.cv-thumb', h('img', { src: `/api/files/${encodeURIComponent(item.ref)}/download?inline=true`, alt: name, loading: 'lazy', draggable: 'false' })),
      h('div.cv-card-head', icon(entry ? fileIcon(entry) : 'file', 'sm'), h('span.ellipsis', name)),
      gone ? h('div.small.cv-error', 'Not available') : entry && h('div.small.muted.ellipsis', entry.live_editor ? 'Double-click to open' : (entry.mime || '')));
  }
  if (t === 'mindmap') {
    const rec = item.ref ? sources.maps.get(item.ref) : null;
    if (item.ref) subscribe(sources.maps, item.ref);
    const v = rec && rec.value;
    return h('div.cv-card-body.cv-map-body',
      h('div.cv-card-head', icon('mindmap', 'sm'), h('span.ellipsis', v?.title || item.text || 'Mind map')),
      rec?.error ? h('div.small.cv-error', 'Not available')
        : !v ? h('div.small.muted', 'Loading…')
          : v.branches.length ? h('ul.cv-branches', v.branches.slice(0, 8).map((b, i) => withVars(h('li', h('span.ellipsis', b.text), b.count ? h('span.small.muted', String(b.count)) : null), { '--branch': b.color || BRANCH_COLORS[i % BRANCH_COLORS.length] })),
            v.branches.length > 8 && h('li.muted.small', `+${v.branches.length - 8} more`))
            : h('div.small.muted', 'No branches yet'));
  }
  if (t === 'board') {
    const rec = item.ref ? sources.boards.get(item.ref) : null;
    if (item.ref) subscribe(sources.boards, item.ref);
    const b = rec && rec.value;
    return withVars(h('div.cv-card-body.cv-board-body',
      h('div.cv-card-head', icon('boards', 'sm'), h('span.ellipsis', b?.title || item.text || 'Board')),
      rec?.error ? h('div.small.cv-error', 'Not available')
        : !b ? h('div.small.muted', 'Loading…')
          : h('div.cv-board-cols', b.stacks.slice(0, 6).map((s) => h('div.cv-board-col', h('span.ellipsis', s.title), h('strong', String(s.cards.length)))))), { '--board-color': b?.color });
  }
  return h('div.cv-card-body', h('div.cv-card-head', icon('file', 'sm'), h('span.ellipsis', item.text || t || 'Item')));
}
