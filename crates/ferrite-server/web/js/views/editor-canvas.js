// Canvas editor (.fcanvas): an infinite board over the shared Y.Maps "items" (id → Y.Map
// {type, x, y, w, h, text, ref, color, z}) and "edges" (id → Y.Map {from, to, label}). All
// geometry is in canvas coordinates; the view is a pan/zoom transform. Items are absolutely
// positioned elements keyed by id and updated incrementally on CRDT changes; edges are SVG
// arrows clipped to item borders. Peers' pointers come from awareness (`cursor`).

import { Y } from '../collab.js';
import { api } from '../api.js';
import { h, clear, icon, menu, modal, toast, errorToast, promptDialog, spinner, emptyState } from '../ui.js';
import { readEntry, setFields, changedIds, newId, rafBatch, throttle, pickFile, openFileId, inField } from './editor-common.js';
import { createSources, renderContent, NOTE_COLORS, DEFAULT_SIZE } from './editor-canvas-cards.js';

const SVG = 'http://www.w3.org/2000/svg';
const MIN_W = 60;
const MIN_H = 36;

function svg(tag, attrs = {}) {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) el.setAttribute(k, String(v));
  return el;
}

/** Point where the segment from the center of `r` towards (tx, ty) leaves the rectangle. */
function borderPoint(r, tx, ty, pad = 4) {
  const cx = r.x + r.w / 2;
  const cy = r.y + r.h / 2;
  const dx = tx - cx;
  const dy = ty - cy;
  if (!dx && !dy) return { x: cx, y: cy };
  const sx = dx ? (r.w / 2 + pad) / Math.abs(dx) : Infinity;
  const sy = dy ? (r.h / 2 + pad) / Math.abs(dy) : Infinity;
  const s = Math.min(sx, sy);
  return { x: cx + dx * s, y: cy + dy * s };
}

export default function mount({ body, toolbar, provider, doc, entry, me, readOnly, ctx, setHelp }) {
  const items = doc.getMap('items');
  const edges = doc.getMap('edges');
  const undo = readOnly ? null : new Y.UndoManager([items, edges], { trackedOrigins: new Set(['local']), captureTimeout: 400 });
  const sources = createSources();
  const view = { x: 40, y: 40, k: 1 };
  let fitted = false;
  let tool = 'select'; // select | connect
  let connectFrom = null;
  const selection = new Set();
  let selectedEdge = null;
  let editingId = null;
  let noteColor = NOTE_COLORS[0];
  const els = new Map(); // item id -> {el, body, sig, geom, type, unsubs}
  const edgeEls = new Map(); // edge id -> {g, path, hit, label, data}
  const cursorEls = new Map(); // clientId -> element
  const overrides = new Map(); // item id -> geometry while dragging locally

  // ---- DOM ----
  const defs = svg('defs');
  for (const [id, cls] of [['cv-arrow', 'cv-arrowhead'], ['cv-arrow-sel', 'cv-arrowhead sel']]) {
    const m = svg('marker', { id: `${id}-${entry.id}`, viewBox: '0 0 10 10', refX: 9, refY: 5, markerWidth: 8, markerHeight: 8, orient: 'auto-start-reverse', markerUnits: 'userSpaceOnUse' });
    m.appendChild(svg('path', { d: 'M0 0L10 5L0 10z', class: cls }));
    defs.appendChild(m);
  }
  const edgesG = svg('g');
  const edgeSvg = svg('svg', { class: 'cv-edges', width: 1, height: 1 });
  edgeSvg.append(defs, edgesG);
  const framesLayer = h('div.cv-frames');
  const itemsLayer = h('div.cv-items');
  const cursorsLayer = h('div.cv-cursors', { 'aria-hidden': 'true' });
  const world = h('div.cv-world', framesLayer, edgeSvg, itemsLayer, cursorsLayer);
  const dropOverlay = h('div.cv-drop', { hidden: true }, icon('upload'), `Drop files to upload them next to ${entry.name}`);
  const hint = h('div.cv-hint', { hidden: true });
  const zoomLabel = h('span.mm-zoom-label', '100%');
  const zoomBar = h('div.cv-zoom.floating-bar',
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Zoom out', title: 'Zoom out (-)', onclick: () => zoomBy(1 / 1.2) }, icon('zoomout', 'sm')),
    zoomLabel,
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Zoom in', title: 'Zoom in (+)', onclick: () => zoomBy(1.2) }, icon('zoomin', 'sm')),
    h('button.btn.ghost.sm', { type: 'button', title: 'Fit to content (0)', onclick: () => fit() }, 'Fit'));
  const empty = h('div.cv-empty', { hidden: true }, emptyState('canvas', 'An empty canvas', readOnly ? 'Nothing here yet.' : 'Add notes, files, mind maps and boards from the toolbar, or drop files here.'));
  const wrap = h('div.cv-wrap', { tabindex: 0, 'aria-label': `Canvas ${entry.name}` }, world, empty, dropOverlay, hint, zoomBar);
  body.appendChild(wrap);

  // ---- toolbar ----
  const toolBtns = {};
  if (!readOnly) {
    const toolBtn = (name, ic, label, key) => {
      toolBtns[name] = h('button.btn.ghost.sm', { type: 'button', 'aria-pressed': 'false', title: `${label} (${key})`, onclick: () => setTool(name) }, icon(ic, 'sm'), label);
      return toolBtns[name];
    };
    const noteBtn = h('button.btn.sm', { type: 'button', title: 'Add a sticky note (N)', onclick: () => addNote() }, h('span.cv-note-swatch', { style: { background: noteColor } }), 'Note');
    const noteColorBtn = h('button.btn.ghost.icon.sm', {
      type: 'button', 'aria-label': 'Note color', title: 'Note color',
      onclick: () => {
        const m = menu(noteColorBtn, NOTE_COLORS.map((c) => ({ label: c, run: () => { noteColor = c; noteBtn.firstChild.style.background = c; if (selection.size) recolor(c); } })));
        m.classList.add('cv-color-menu');
        m.querySelectorAll('button').forEach((b, i) => { b.prepend(h('span.cv-note-swatch', { style: { background: NOTE_COLORS[i] } })); b.lastChild.textContent = ''; b.setAttribute('aria-label', `Color ${NOTE_COLORS[i]}`); });
      },
    }, icon('chevron-down', 'sm'));
    const addBtn = h('button.btn.sm', { type: 'button', onclick: () => addMenu(addBtn) }, icon('plus', 'sm'), 'Add');
    toolbar.append(
      toolBtn('select', 'move', 'Select', 'V'), toolBtn('connect', 'share', 'Connect', 'C'), h('span.tb-sep'),
      h('span.btn-split', noteBtn, noteColorBtn),
      h('button.btn.sm', { type: 'button', title: 'Add a file (I)', onclick: () => addFile() }, icon('file', 'sm'), 'File'),
      h('button.btn.sm', { type: 'button', title: 'Add a mind map (M)', onclick: (e) => addMindmapMenu(e.currentTarget) }, icon('mindmap', 'sm'), 'Mind map'),
      h('button.btn.sm', { type: 'button', title: 'Add a board (B)', onclick: () => addBoard() }, icon('boards', 'sm'), 'Board'),
      addBtn, h('span.tb-sep'));
    toolBtns.front = h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Bring to front', title: 'Bring to front (])', onclick: () => bringToFront() }, icon('chevron-right', 'sm'));
    toolBtns.del = h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Delete selected', title: 'Delete (Delete)', onclick: () => deleteSelection() }, icon('trash', 'sm'));
    toolBtns.undo = h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Undo', title: 'Undo (Ctrl+Z)', onclick: () => undo.undo() }, icon('history', 'sm'));
    toolbar.append(toolBtns.front, toolBtns.del, toolBtns.undo);
  }
  toolbar.append(h('div.grow'), h('span.small.muted.cv-count'));

  function setTool(t) {
    tool = t;
    connectFrom = null;
    wrap.classList.toggle('connecting', t === 'connect');
    for (const n of ['select', 'connect']) toolBtns[n]?.setAttribute('aria-pressed', String(n === t));
    hint.hidden = t !== 'connect';
    hint.textContent = 'Click an item, then the item to connect it to. Esc to stop.';
    els.forEach((e) => e.el.classList.remove('connect-from'));
  }

  function updateToolbar() {
    if (readOnly) return;
    toolBtns.front.disabled = !selection.size;
    toolBtns.del.disabled = !selection.size && !selectedEdge;
    toolBtns.undo.disabled = !undo.undoStack.length;
    const n = items.size;
    toolbar.querySelector('.cv-count').textContent = `${n} item${n === 1 ? '' : 's'}`;
  }

  // ---- view ----
  function applyView() {
    world.style.transform = `translate(${view.x}px, ${view.y}px) scale(${view.k})`;
    const g = 24 * view.k;
    wrap.style.setProperty('--grid', `${g}px`);
    wrap.style.setProperty('--grid-x', `${view.x}px`);
    wrap.style.setProperty('--grid-y', `${view.y}px`);
    zoomLabel.textContent = `${Math.round(view.k * 100)}%`;
    cursorsLayer.style.setProperty('--inv', String(1 / view.k));
  }

  function toWorld(clientX, clientY) {
    const r = wrap.getBoundingClientRect();
    return { x: (clientX - r.left - view.x) / view.k, y: (clientY - r.top - view.y) / view.k };
  }

  function center() {
    return toWorld(wrap.getBoundingClientRect().left + wrap.clientWidth / 2, wrap.getBoundingClientRect().top + wrap.clientHeight / 2);
  }

  function zoomAt(f, sx, sy) {
    const k = Math.max(0.1, Math.min(3, view.k * f));
    const ratio = k / view.k;
    view.x = sx - (sx - view.x) * ratio;
    view.y = sy - (sy - view.y) * ratio;
    view.k = k;
    applyView();
  }
  function zoomBy(f) { zoomAt(f, wrap.clientWidth / 2, wrap.clientHeight / 2); }

  function fit() {
    const all = [...els.values()].map((e) => e.geom);
    if (!all.length) { view.x = 40; view.y = 40; view.k = 1; applyView(); return; }
    const x0 = Math.min(...all.map((g) => g.x));
    const y0 = Math.min(...all.map((g) => g.y));
    const x1 = Math.max(...all.map((g) => g.x + g.w));
    const y1 = Math.max(...all.map((g) => g.y + g.h));
    const W = wrap.clientWidth || 800;
    const H = wrap.clientHeight || 600;
    const k = Math.max(0.1, Math.min(1, (W - 120) / (x1 - x0 || 1), (H - 120) / (y1 - y0 || 1)));
    view.k = k;
    view.x = W / 2 - ((x0 + x1) / 2) * k;
    view.y = H / 2 - ((y0 + y1) / 2) * k;
    applyView();
  }

  // ---- reading items ----
  function itemData(id) {
    const o = readEntry(items, id);
    if (!o) return null;
    const type = ['note', 'file', 'mindmap', 'board', 'link', 'frame', 'text'].includes(o.type) ? o.type : 'note';
    const [dw, dh] = DEFAULT_SIZE[type];
    const num = (v, d) => (Number.isFinite(Number(v)) && v !== null && v !== '' ? Number(v) : d);
    return {
      id, type, x: num(o.x, 0), y: num(o.y, 0), w: Math.max(MIN_W, num(o.w, dw)), h: Math.max(MIN_H, num(o.h, dh)),
      text: o.text == null ? '' : String(o.text), ref: o.ref == null ? '' : String(o.ref), color: o.color || '', z: num(o.z, 0),
    };
  }

  function geomOf(id) { return overrides.get(id) || els.get(id)?.geom; }

  // ---- rendering items ----
  function placeEl(rec, g) {
    const s = rec.el.style;
    s.left = `${g.x}px`;
    s.top = `${g.y}px`;
    s.width = `${g.w}px`;
    s.height = `${g.h}px`;
  }

  function renderItem(id) {
    const d = itemData(id);
    let rec = els.get(id);
    if (!d) {
      if (rec) { rec.unsubs.forEach((f) => f()); rec.el.remove(); els.delete(id); selection.delete(id); }
      return;
    }
    if (rec && rec.type !== d.type) { rec.unsubs.forEach((f) => f()); rec.el.remove(); els.delete(id); rec = null; }
    if (!rec) {
      const bodyEl = h('div.cv-item-body');
      const el = h(`div.cv-item.type-${d.type}`, { dataset: { id }, tabindex: -1 }, bodyEl, !readOnly && h('div.cv-handle', { 'aria-hidden': 'true' }));
      rec = { el, body: bodyEl, sig: '', geom: null, type: d.type, unsubs: [] };
      els.set(id, rec);
      (d.type === 'frame' ? framesLayer : itemsLayer).appendChild(el);
      if (selection.has(id)) el.classList.add('selected');
    }
    rec.geom = { x: d.x, y: d.y, w: d.w, h: d.h };
    if (!overrides.has(id)) placeEl(rec, rec.geom);
    rec.el.style.zIndex = String(Math.round(d.z) + 1000);
    rec.el.style.setProperty('--item-color', d.color || (d.type === 'note' ? NOTE_COLORS[0] : ''));
    rec.el.classList.toggle('has-color', !!d.color);
    const sig = JSON.stringify([d.type, d.text, d.ref, d.color]);
    if (rec.sig !== sig && editingId !== id) {
      rec.sig = sig;
      fillContent(id, rec, d);
    }
    rec.data = d;
  }

  function fillContent(id, rec, d) {
    rec.unsubs.forEach((f) => f());
    rec.unsubs = [];
    const subscribe = (store, key) => rec.unsubs.push(store.on(key, () => { if (els.get(id) === rec && editingId !== id) fillContent(id, rec, rec.data || d); }));
    clear(rec.body).appendChild(renderContent(d, sources, subscribe));
    rec.el.title = d.type === 'link' ? d.ref : '';
  }

  // ---- rendering edges ----
  function renderEdge(id) {
    const o = readEntry(edges, id);
    let rec = edgeEls.get(id);
    const a = o && geomOf(String(o.from));
    const b = o && geomOf(String(o.to));
    if (!o || !a || !b) {
      if (rec) { rec.g.remove(); edgeEls.delete(id); }
      if (selectedEdge === id && !o) selectedEdge = null;
      return;
    }
    if (!rec) {
      const path = svg('path', { class: 'cv-edge' });
      const hit = svg('path', { class: 'cv-edge-hit' });
      const label = svg('text', { class: 'cv-edge-label', 'text-anchor': 'middle', 'dominant-baseline': 'central' });
      const g = svg('g', { 'data-edge': id });
      g.append(hit, path, label);
      edgesG.appendChild(g);
      rec = { g, path, hit, label };
      edgeEls.set(id, rec);
    }
    rec.data = { from: String(o.from), to: String(o.to), label: o.label ? String(o.label) : '' };
    const ca = { x: a.x + a.w / 2, y: a.y + a.h / 2 };
    const cb = { x: b.x + b.w / 2, y: b.y + b.h / 2 };
    const p1 = borderPoint(a, cb.x, cb.y, 2);
    const p2 = borderPoint(b, ca.x, ca.y, 6);
    const d = `M${p1.x} ${p1.y}L${p2.x} ${p2.y}`;
    rec.path.setAttribute('d', d);
    rec.hit.setAttribute('d', d);
    const sel = selectedEdge === id;
    rec.g.classList.toggle('selected', sel);
    rec.path.setAttribute('marker-end', `url(#${sel ? 'cv-arrow-sel' : 'cv-arrow'}-${entry.id})`);
    rec.label.textContent = rec.data.label;
    rec.label.setAttribute('x', (p1.x + p2.x) / 2);
    rec.label.setAttribute('y', (p1.y + p2.y) / 2);
  }

  function edgesOf(itemIds) {
    const out = [];
    edgeEls.forEach((rec, id) => { if (rec.data && (itemIds.has(rec.data.from) || itemIds.has(rec.data.to))) out.push(id); });
    edges.forEach((v, id) => {
      if (edgeEls.has(id)) return;
      const o = readEntry(edges, id);
      if (o && (itemIds.has(String(o.from)) || itemIds.has(String(o.to)))) out.push(id);
    });
    return out;
  }

  // ---- incremental updates from the CRDT ----
  const dirtyItems = new Set();
  const dirtyEdges = new Set();
  let fullRender = true;
  const flush = rafBatch(() => {
    if (fullRender) {
      fullRender = false;
      items.forEach((_, id) => dirtyItems.add(id));
      els.forEach((_, id) => dirtyItems.add(id));
      edges.forEach((_, id) => dirtyEdges.add(id));
      edgeEls.forEach((_, id) => dirtyEdges.add(id));
    }
    const touched = new Set(dirtyItems);
    dirtyItems.forEach((id) => renderItem(id));
    dirtyItems.clear();
    edgesOf(touched).forEach((id) => dirtyEdges.add(id));
    dirtyEdges.forEach((id) => renderEdge(id));
    dirtyEdges.clear();
    empty.hidden = items.size > 0;
    updateToolbar();
    renderCursors();
  });
  const onItems = (events) => { changedIds(events, items).forEach((id) => dirtyItems.add(id)); flush(); };
  const onEdges = (events) => { changedIds(events, edges).forEach((id) => dirtyEdges.add(id)); flush(); };
  items.observeDeep(onItems);
  edges.observeDeep(onEdges);

  // ---- writing ----
  const tx = (fn) => doc.transact(fn, 'local');
  const maxZ = () => { let m = 0; els.forEach((r) => { if (r.data) m = Math.max(m, r.data.z); }); return m; };
  const minZ = () => { let m = 0; els.forEach((r) => { if (r.data) m = Math.min(m, r.data.z); }); return m; };

  function addItem(fields, { edit = false } = {}) {
    if (readOnly) return null;
    const id = newId('i');
    const [dw, dh] = DEFAULT_SIZE[fields.type];
    const at = fields.x === undefined ? center() : null;
    const w = fields.w || dw;
    const hgt = fields.h || dh;
    const data = {
      type: fields.type, x: Math.round(at ? at.x - w / 2 + (Math.random() * 40 - 20) : fields.x), y: Math.round(at ? at.y - hgt / 2 + (Math.random() * 40 - 20) : fields.y),
      w, h: hgt, text: fields.text ?? '', z: fields.type === 'frame' ? minZ() - 1 : maxZ() + 1,
    };
    if (fields.ref) data.ref = fields.ref;
    if (fields.color) data.color = fields.color;
    tx(() => {
      const m = new Y.Map();
      items.set(id, m);
      for (const [k, v] of Object.entries(data)) m.set(k, v);
    });
    dirtyItems.add(id);
    flush();
    requestAnimationFrame(() => {
      selectOnly(id);
      if (edit) startEditing(id);
    });
    return id;
  }

  function addNote() { setTool('select'); addItem({ type: 'note', color: noteColor, text: '' }, { edit: true }); }

  async function addFile() {
    const f = await pickFile({ title: 'Add a file to the canvas', rootId: me.root_id, startId: entry.parent_id, accept: (e) => e.id !== entry.id, confirm: 'Add' });
    if (!f) return;
    const img = (f.mime || '').startsWith('image/') && !/svg/.test(f.mime);
    if (f.live_editor === 'mindmap') addItem({ type: 'mindmap', ref: f.id, text: f.name });
    else addItem({ type: 'file', ref: f.id, text: f.name, h: img ? 200 : undefined });
  }

  function addMindmapMenu(anchor) {
    menu(anchor, [
      { label: 'Existing mind map…', icon: 'mindmap', run: async () => {
        const f = await pickFile({ title: 'Add a mind map', rootId: me.root_id, startId: entry.parent_id, accept: (e) => e.live_editor === 'mindmap', confirm: 'Add' });
        if (f) addItem({ type: 'mindmap', ref: f.id, text: f.name });
      } },
      { label: 'New mind map…', icon: 'plus', run: async () => {
        const title = await promptDialog('New mind map', { label: `Created next to ${entry.name}`, placeholder: 'Mind map title', confirm: 'Create' });
        if (!title) return;
        try {
          const { node } = await api.post(`/api/files/${entry.parent_id}/documents`, { title, kind: 'mindmap' });
          addItem({ type: 'mindmap', ref: node.id, text: node.name });
        } catch (e) { errorToast(e); }
      } },
    ]);
  }

  async function addBoard() {
    const list = h('div.list.picker-list', spinner());
    let chosen = null;
    const p = modal({ title: 'Add a board', body: list, actions: [{ label: 'Cancel', value: null }, { label: 'Add', kind: 'primary', run: () => chosen }] });
    try {
      const { boards } = await api.get('/api/boards');
      clear(list);
      if (!boards.length) list.appendChild(emptyState('boards', 'No boards', 'Create one in Boards first.'));
      for (const b of boards) {
        const row = h('div.list-item.clickable', {
          tabindex: 0, role: 'option', dataset: { id: b.id },
          onclick: () => { list.querySelectorAll('.selected').forEach((x) => x.classList.remove('selected')); row.classList.add('selected'); chosen = b; },
          ondblclick: () => { chosen = b; list.closest('.modal').querySelector('.modal-foot .btn.primary').click(); },
          onkeydown: (e) => { if (e.key === 'Enter') { chosen = b; list.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
        }, h('span.board-dot', { style: { background: b.color } }), h('span.grow.ellipsis', b.title));
        list.appendChild(row);
      }
    } catch (e) { clear(list).appendChild(h('p.error-text', e.message)); }
    const b = await p;
    if (b) addItem({ type: 'board', ref: b.id, text: b.title });
  }

  async function addLink() {
    const u = await promptDialog('Add a link', { placeholder: 'https://…', type: 'url', confirm: 'Add' });
    if (!u) return;
    const url = /^https?:\/\//i.test(u) ? u : `https://${u}`;
    let host = url;
    try { host = new URL(url).hostname; } catch (_) { /* keep */ }
    addItem({ type: 'link', ref: url, text: host });
  }

  function addMenu(anchor) {
    menu(anchor, [
      { label: 'Text', icon: 'doc', run: () => addItem({ type: 'text', text: '' }, { edit: true }) },
      { label: 'Frame', icon: 'canvas', run: () => addItem({ type: 'frame', text: 'Frame' }, { edit: true }) },
      { label: 'Link…', icon: 'globe', run: addLink },
    ]);
  }

  function recolor(c) {
    tx(() => selection.forEach((id) => { if (['note', 'frame', 'text'].includes(els.get(id)?.type)) setFields(items, id, { color: c }); }));
  }

  function bringToFront() {
    let z = maxZ();
    tx(() => selection.forEach((id) => { if (els.get(id)?.type !== 'frame') setFields(items, id, { z: ++z }); }));
  }

  function deleteSelection() {
    if (readOnly) return;
    if (selectedEdge && !selection.size) {
      const id = selectedEdge;
      selectedEdge = null;
      tx(() => edges.delete(id));
      return;
    }
    if (!selection.size) return;
    const ids = new Set(selection);
    tx(() => {
      edges.forEach((v, eid) => {
        const o = readEntry(edges, eid);
        if (o && (ids.has(String(o.from)) || ids.has(String(o.to)))) edges.delete(eid);
      });
      ids.forEach((id) => items.delete(id));
    });
    selection.clear();
    updateToolbar();
  }

  // ---- selection ----
  function selectOnly(id) {
    selection.forEach((s) => els.get(s)?.el.classList.remove('selected'));
    selection.clear();
    setEdgeSelection(null);
    if (id) { selection.add(id); els.get(id)?.el.classList.add('selected'); }
    updateToolbar();
  }
  function toggleSelect(id) {
    if (selection.has(id)) { selection.delete(id); els.get(id)?.el.classList.remove('selected'); } else { selection.add(id); els.get(id)?.el.classList.add('selected'); }
    updateToolbar();
  }
  function setEdgeSelection(id) {
    const prev = selectedEdge;
    selectedEdge = id;
    if (prev) renderEdge(prev);
    if (id) renderEdge(id);
    updateToolbar();
  }

  // ---- in-place text editing (notes, text, frame titles) ----
  function startEditing(id) {
    const rec = els.get(id);
    if (readOnly || !rec || !['note', 'text', 'frame'].includes(rec.type)) return;
    editingId = id;
    rec.el.classList.add('editing');
    const ta = h(rec.type === 'frame' ? 'input.cv-edit.cv-frame-edit' : 'textarea.cv-edit', { 'aria-label': rec.type === 'frame' ? 'Frame title' : 'Note text (Markdown)', spellcheck: 'true' });
    if (rec.type === 'frame') ta.type = 'text';
    ta.value = rec.data?.text || '';
    const write = throttle((v) => { if (els.has(id)) tx(() => setFields(items, id, { text: v })); }, 250);
    ta.addEventListener('input', () => write(ta.value));
    ta.addEventListener('keydown', (e) => {
      e.stopPropagation();
      if (e.key === 'Escape' || (e.key === 'Enter' && (e.ctrlKey || e.metaKey || rec.type === 'frame'))) { e.preventDefault(); ta.blur(); }
    });
    ta.addEventListener('pointerdown', (e) => e.stopPropagation());
    ta.addEventListener('blur', () => {
      if (editingId !== id) return;
      editingId = null;
      rec.el.classList.remove('editing');
      if (els.has(id) && ta.value !== (rec.data?.text || '')) tx(() => setFields(items, id, { text: ta.value }));
      rec.sig = '';
      renderItem(id);
      wrap.focus({ preventScroll: true });
    });
    clear(rec.body).appendChild(ta);
    ta.focus();
    if (rec.type === 'frame') ta.select();
  }

  // ---- opening referenced things ----
  async function openItem(id) {
    const d = els.get(id)?.data;
    if (!d) return;
    try {
      if (d.type === 'file' || d.type === 'mindmap') { if (d.ref) await openFileId(d.ref, ctx.navigate); } else if (d.type === 'board') { if (d.ref) ctx.navigate(`/boards/${d.ref}`); } else if (d.type === 'link') { if (d.ref) window.open(d.ref, '_blank', 'noopener'); } else startEditing(id);
    } catch (e) { errorToast(e.status === 404 ? new Error('That item is no longer available.') : e); }
  }

  // ---- connect mode ----
  function connectClick(id) {
    if (!connectFrom) {
      connectFrom = id;
      els.get(id)?.el.classList.add('connect-from');
      hint.textContent = 'Now click the item to connect to. Esc to cancel.';
      return;
    }
    if (connectFrom === id) return;
    const from = connectFrom;
    els.get(from)?.el.classList.remove('connect-from');
    connectFrom = null;
    const eid = newId('e');
    tx(() => {
      const m = new Y.Map();
      edges.set(eid, m);
      m.set('from', from);
      m.set('to', id);
      m.set('label', '');
    });
    dirtyEdges.add(eid);
    flush();
    hint.textContent = 'Connected. Click another item to start a new arrow, or Esc to stop. Double-click an arrow to label it.';
  }

  async function labelEdge(id) {
    if (readOnly) return;
    const cur = readEntry(edges, id);
    if (!cur) return;
    const v = await promptDialog('Arrow label', { value: cur.label || '', placeholder: 'Optional label', confirm: 'Save' });
    if (v === null && cur.label) return;
    if (edges.has(id)) tx(() => setFields(edges, id, { label: v || '' }));
  }

  // ---- pointer interactions ----
  let drag = null;
  let spaceDown = false;
  const writeDrag = rafBatch(() => {
    if (!drag || !drag.moved) return;
    tx(() => drag.ids.forEach((id) => {
      const g = overrides.get(id);
      if (g && items.has(id)) setFields(items, id, { x: Math.round(g.x), y: Math.round(g.y), w: Math.round(g.w), h: Math.round(g.h) });
    }));
  });

  wrap.addEventListener('pointerdown', (e) => {
    if (e.button !== 0 && e.button !== 1) return;
    if (e.target.closest('.floating-bar, .cv-edit')) return;
    const itemEl = e.target.closest('.cv-item');
    const edgeG = e.target.closest('[data-edge]');
    const p = toWorld(e.clientX, e.clientY);
    if (e.button === 1 || spaceDown || (!itemEl && !edgeG)) {
      // Pan.
      if (!itemEl && !edgeG && !e.shiftKey && tool === 'select') selectOnly(null);
      drag = { kind: 'pan', sx: e.clientX, sy: e.clientY, vx: view.x, vy: view.y, moved: false };
      wrap.classList.add('panning');
      wrap.setPointerCapture(e.pointerId);
      wrap.focus({ preventScroll: true });
      e.preventDefault();
      return;
    }
    if (edgeG && !itemEl) {
      selectOnly(null);
      setEdgeSelection(edgeG.dataset.edge);
      wrap.focus({ preventScroll: true });
      return;
    }
    const id = itemEl.dataset.id;
    if (editingId === id) return;
    e.preventDefault();
    wrap.focus({ preventScroll: true });
    if (tool === 'connect' && !readOnly) { connectClick(id); return; }
    if (e.shiftKey) toggleSelect(id); else if (!selection.has(id)) selectOnly(id);
    if (readOnly) return;
    const resizing = e.target.classList.contains('cv-handle');
    let ids = resizing ? [id] : [...selection];
    if (!resizing) {
      // Dragging a frame carries the items inside it.
      for (const fid of ids.filter((x) => els.get(x)?.type === 'frame')) {
        const f = els.get(fid).geom;
        els.forEach((r, oid) => {
          const g = r.geom;
          if (oid !== fid && r.type !== 'frame' && g.x >= f.x && g.y >= f.y && g.x + g.w <= f.x + f.w && g.y + g.h <= f.y + f.h) ids.push(oid);
        });
      }
      ids = [...new Set(ids)];
    }
    drag = { kind: resizing ? 'resize' : 'move', ids, start: p, orig: new Map(ids.map((x) => [x, { ...els.get(x).geom }])), moved: false };
    wrap.setPointerCapture(e.pointerId);
  });

  wrap.addEventListener('pointermove', (e) => {
    sendCursor(e);
    if (!drag) return;
    if (drag.kind === 'pan') {
      view.x = drag.vx + e.clientX - drag.sx;
      view.y = drag.vy + e.clientY - drag.sy;
      drag.moved = true;
      applyView();
      return;
    }
    const p = toWorld(e.clientX, e.clientY);
    const dx = p.x - drag.start.x;
    const dy = p.y - drag.start.y;
    if (!drag.moved && Math.hypot(dx * view.k, dy * view.k) < 3) return;
    drag.moved = true;
    wrap.classList.add('dragging');
    for (const id of drag.ids) {
      const o = drag.orig.get(id);
      const g = drag.kind === 'resize'
        ? { x: o.x, y: o.y, w: Math.max(MIN_W, o.w + dx), h: Math.max(MIN_H, o.h + dy) }
        : { x: o.x + dx, y: o.y + dy, w: o.w, h: o.h };
      overrides.set(id, g);
      const rec = els.get(id);
      if (rec) placeEl(rec, g);
    }
    edgesOf(new Set(drag.ids)).forEach((id) => renderEdge(id));
    writeDrag();
  });

  const endDrag = () => {
    if (!drag) return;
    const d = drag;
    drag = null;
    wrap.classList.remove('panning', 'dragging');
    if (d.kind !== 'pan' && d.moved) {
      tx(() => d.ids.forEach((id) => {
        const g = overrides.get(id);
        if (g && items.has(id)) setFields(items, id, { x: Math.round(g.x), y: Math.round(g.y), w: Math.round(g.w), h: Math.round(g.h) });
      }));
      d.ids.forEach((id) => { overrides.delete(id); dirtyItems.add(id); });
      flush();
    }
  };
  wrap.addEventListener('pointerup', endDrag);
  wrap.addEventListener('pointercancel', endDrag);
  wrap.addEventListener('pointerleave', () => provider.setLocal('cursor', null));

  wrap.addEventListener('dblclick', (e) => {
    const itemEl = e.target.closest('.cv-item');
    const edgeG = e.target.closest('[data-edge]');
    if (itemEl && !e.target.closest('.cv-edit')) { e.preventDefault(); openItem(itemEl.dataset.id); } else if (edgeG) labelEdge(edgeG.dataset.edge);
    else if (!itemEl && !readOnly && tool === 'select') {
      // Double-click on the background: a note right there.
      const p = toWorld(e.clientX, e.clientY);
      const [w, hh] = DEFAULT_SIZE.note;
      addItem({ type: 'note', color: noteColor, text: '', x: p.x - w / 2, y: p.y - hh / 2 }, { edit: true });
    }
  });

  wrap.addEventListener('wheel', (e) => {
    e.preventDefault();
    const r = wrap.getBoundingClientRect();
    if (e.ctrlKey || e.metaKey || !e.shiftKey) zoomAt(Math.exp(-e.deltaY * (e.deltaMode === 1 ? 0.05 : 0.0018)), e.clientX - r.left, e.clientY - r.top);
    else { view.x -= e.deltaY; applyView(); }
  }, { passive: false });

  // ---- keyboard ----
  wrap.addEventListener('keydown', (e) => {
    if (inField(e)) return;
    const k = e.key;
    const mod = e.ctrlKey || e.metaKey;
    if (k === ' ' && !spaceDown) { spaceDown = true; wrap.classList.add('space'); e.preventDefault(); return; }
    if ((k === '+' || k === '=') && !mod) { zoomBy(1.2); return; }
    if (k === '-' && !mod) { zoomBy(1 / 1.2); return; }
    if (k === '0' && !mod) { fit(); return; }
    if (k === 'Escape') { if (tool === 'connect') setTool('select'); else selectOnly(null); return; }
    if (readOnly) return;
    if ((k === 'Delete' || k === 'Backspace')) { e.preventDefault(); deleteSelection(); return; }
    if (mod && (k === 'z' || k === 'Z' || k === 'y')) { e.preventDefault(); if (k === 'y' || e.shiftKey) undo.redo(); else undo.undo(); return; }
    if (mod) return;
    const arrows = { ArrowLeft: [-1, 0], ArrowRight: [1, 0], ArrowUp: [0, -1], ArrowDown: [0, 1] };
    if (arrows[k] && selection.size) {
      e.preventDefault();
      const step = e.shiftKey ? 20 : 2;
      tx(() => selection.forEach((id) => { const g = els.get(id)?.geom; if (g) setFields(items, id, { x: g.x + arrows[k][0] * step, y: g.y + arrows[k][1] * step }); }));
      return;
    }
    if (k === 'Enter' && selection.size === 1) { e.preventDefault(); openItem([...selection][0]); return; }
    const keys = { v: () => setTool('select'), c: () => setTool('connect'), n: addNote, t: () => addItem({ type: 'text', text: '' }, { edit: true }), f: () => addItem({ type: 'frame', text: 'Frame' }, { edit: true }), i: addFile, b: addBoard, l: addLink, m: () => addMindmapMenu(toolbar), ']': bringToFront };
    const fn = keys[k.toLowerCase()];
    if (fn && !e.altKey) { e.preventDefault(); fn(); }
  });
  wrap.addEventListener('keyup', (e) => { if (e.key === ' ') { spaceDown = false; wrap.classList.remove('space'); } });

  // ---- dropping files from the desktop ----
  let dragDepth = 0;
  const hasFiles = (e) => e.dataTransfer && [...e.dataTransfer.types].includes('Files');
  wrap.addEventListener('dragenter', (e) => { if (!hasFiles(e) || readOnly) return; e.preventDefault(); dragDepth++; dropOverlay.hidden = false; });
  wrap.addEventListener('dragover', (e) => { if (!hasFiles(e) || readOnly) return; e.preventDefault(); e.dataTransfer.dropEffect = 'copy'; });
  wrap.addEventListener('dragleave', () => { if (--dragDepth <= 0) { dragDepth = 0; dropOverlay.hidden = true; } });
  wrap.addEventListener('drop', async (e) => {
    if (!hasFiles(e) || readOnly) return;
    e.preventDefault();
    dragDepth = 0;
    dropOverlay.hidden = true;
    const p = toWorld(e.clientX, e.clientY);
    const files = [...e.dataTransfer.files];
    const note = toast(`Uploading ${files.length} file${files.length === 1 ? '' : 's'}…`, 'info', { timeout: 0 });
    let i = 0;
    for (const file of files) {
      try {
        const node = await api.upload(entry.parent_id, file);
        const img = (node.mime || file.type || '').startsWith('image/') && !/svg/.test(node.mime || file.type);
        const kind = /\.fmind$/i.test(node.name) ? 'mindmap' : 'file';
        addItem({ type: kind, ref: node.id, text: node.name, x: p.x + i * 30 - 110, y: p.y + i * 30 - 38, h: img ? 200 : undefined });
        i++;
      } catch (err) { errorToast(err); }
    }
    note.remove();
    if (i) toast(`Added ${i} file${i === 1 ? '' : 's'} to the canvas`, 'ok', { timeout: 2500 });
  });

  // ---- presence: other people's pointers ----
  const sendCursor = throttle((e) => {
    const p = toWorld(e.clientX, e.clientY);
    provider.setLocal('cursor', { x: Math.round(p.x), y: Math.round(p.y) });
  }, 50);

  function renderCursors() {
    const live = new Set();
    provider.awareness.getStates().forEach((st, clientId) => {
      if (clientId === doc.clientID || !st.user || !st.cursor || !Number.isFinite(st.cursor.x)) return;
      live.add(clientId);
      let el = cursorEls.get(clientId);
      if (!el) {
        const arrow = document.createElementNS(SVG, 'svg');
        arrow.setAttribute('viewBox', '0 0 16 16');
        arrow.setAttribute('class', 'cv-cursor-arrow');
        arrow.appendChild(svg('path', { d: 'M1 1l5.5 13 2-5.5L14 6.5z' }));
        el = h('div.cv-cursor', arrow, h('span.cv-cursor-name'));
        cursorsLayer.appendChild(el);
        cursorEls.set(clientId, el);
      }
      el.style.setProperty('--peer', st.user.color);
      el.style.left = `${st.cursor.x}px`;
      el.style.top = `${st.cursor.y}px`;
      el.lastChild.textContent = st.user.name;
    });
    for (const [id, el] of cursorEls) if (!live.has(id)) { el.remove(); cursorEls.delete(id); }
  }
  const queueCursors = rafBatch(renderCursors);
  provider.awareness.on('change', queueCursors);

  const onUndo = () => updateToolbar();
  if (undo) { undo.on('stack-item-added', onUndo); undo.on('stack-item-popped', onUndo); }

  setHelp([
    ['V / C', 'Select / connect tool'],
    ['N, T, F', 'Add a note, text, frame'],
    ['I, M, B, L', 'Add a file, mind map, board, link'],
    ['Double-click', 'Edit a note, open a file, map or board'],
    ['Double-click background', 'New note there'],
    ['Drag', 'Move (frames carry their contents)'],
    ['Corner handle', 'Resize'],
    ['Shift+click', 'Select several'],
    ['Delete', 'Delete the selection or arrow'],
    [']', 'Bring to front'],
    ['Arrows', 'Nudge (Shift: more)'],
    ['Drag background / Space+drag', 'Pan'],
    ['Wheel / + / -', 'Zoom'],
    ['0', 'Fit to content'],
    ['Ctrl+Z / Ctrl+Shift+Z', 'Undo / redo'],
  ]);

  setTool('select');
  applyView();
  flush();
  provider.synced.then(() => {
    fullRender = true;
    flush();
    requestAnimationFrame(() => { if (!fitted) { fitted = true; if (items.size) fit(); } });
  });
  setTimeout(() => wrap.focus({ preventScroll: true }), 0);

  return () => {
    items.unobserveDeep(onItems);
    edges.unobserveDeep(onEdges);
    provider.awareness.off('change', queueCursors);
    provider.setLocal('cursor', null);
    sources.destroy();
    els.forEach((r) => r.unsubs.forEach((f) => f()));
    if (undo) undo.destroy();
  };
}
