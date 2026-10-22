// Mind map editor (.fmind): an SVG tree over the shared Y.Map "nodes" (id → Y.Map {parent,
// text, order, color?, link?, collapsed?, note?}). The central node sits in the middle with
// first-level branches split left/right; each side is laid out as a tidy tree with curved,
// per-branch colored connectors. Rendering is incremental: one <g> per node and one <path>
// per connector, keyed by node id, updated in a requestAnimationFrame batch after CRDT changes
// (from people, MCP tools or the assistant).

import { Y } from '../collab.js';
import { api } from '../api.js';
import { h, icon, menu, modal, toast, errorToast, copyText } from '../ui.js';
import { readAll, setFields, newId, rafBatch, openLink, pickFile, inField } from './editor-common.js';

const SVG = 'http://www.w3.org/2000/svg';
const PALETTE = ['#e11d48', '#ea580c', '#ca8a04', '#16a34a', '#0891b2', '#2563eb', '#7c3aed', '#db2777'];
const SWATCHES = ['#e11d48', '#ea580c', '#ca8a04', '#16a34a', '#0891b2', '#2563eb', '#7c3aed', '#db2777', '#475569'];
const STYLE = [
  { font: 600, size: 17, h: 48, padX: 22, maxW: 300 }, // central node
  { font: 600, size: 14.5, h: 38, padX: 16, maxW: 260 }, // first level
  { font: 450, size: 13.5, h: 30, padX: 12, maxW: 240 }, // deeper
];
const HGAP = [0, 64, 40, 34];
const VGAP = [0, 22, 10, 8];
const LINK_PATH = 'M10 14a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1 1M14 10a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1-1';

function svg(tag, attrs = {}, ...children) {
  const el = document.createElementNS(SVG, tag);
  for (const [k, v] of Object.entries(attrs)) if (v !== undefined && v !== null) el.setAttribute(k, String(v));
  for (const c of children) if (c) el.appendChild(c);
  return el;
}

export default function mount({ body, toolbar, provider, doc, entry, me, readOnly, ctx, setHelp }) {
  const nodes = doc.getMap('nodes');
  const meta = doc.getMap('meta');
  const undo = readOnly ? null : new Y.UndoManager(nodes, { trackedOrigins: new Set(['local']), captureTimeout: 300 });
  const fontFamily = getComputedStyle(document.body).fontFamily;
  const measureCtx = document.createElement('canvas').getContext('2d');
  const fitCache = new Map();

  let model = new Map(); // id -> node fields
  let kids = new Map(); // parent id -> [child ids] (sorted)
  let rootId = null;
  let layout = new Map(); // id -> {x, y, w, h, depth, side, color, label}
  let selected = null;
  let editing = null; // {id, original}
  const view = { x: 0, y: 0, k: 1 };
  let fitted = false;
  const els = new Map(); // node id -> {g, rect, text, title, link, badge, sig}
  const edgeEls = new Map(); // child id -> path
  const peerEls = new Map(); // clientId -> rect

  // ---- DOM ----
  const edgesG = svg('g', { class: 'mm-edges' });
  const nodesG = svg('g', { class: 'mm-nodes' });
  const peersG = svg('g', { class: 'mm-peers' });
  const viewG = svg('g', { class: 'mm-view' }, edgesG, peersG, nodesG);
  const svgEl = svg('svg', { class: 'mm-svg', role: 'img', 'aria-label': `Mind map ${entry.name}` }, viewG);
  const editInput = h('input.mm-edit', { type: 'text', hidden: true, 'aria-label': 'Topic text', spellcheck: 'true' });
  const zoomLabel = h('span.mm-zoom-label', '100%');
  const zoomBar = h('div.mm-zoom.floating-bar',
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Zoom out', title: 'Zoom out (-)', onclick: () => zoomBy(1 / 1.2) }, icon('zoomout', 'sm')),
    zoomLabel,
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Zoom in', title: 'Zoom in (+)', onclick: () => zoomBy(1.2) }, icon('zoomin', 'sm')),
    h('button.btn.ghost.sm', { type: 'button', title: 'Fit to screen (0)', onclick: () => fit() }, 'Fit'));
  const status = h('div.sr-only', { 'aria-live': 'polite' });
  const wrap = h('div.mm-wrap', { tabindex: 0, 'aria-label': 'Mind map. Arrow keys move between topics, Tab adds a child, Enter a sibling, F2 edits.' }, svgEl, editInput, zoomBar, status);
  body.appendChild(wrap);

  // ---- toolbar ----
  const tb = {};
  if (!readOnly) {
    tb.child = h('button.btn.sm', { type: 'button', title: 'Add child (Tab)', onclick: () => addChild() }, icon('plus', 'sm'), 'Child');
    tb.sibling = h('button.btn.sm', { type: 'button', title: 'Add sibling (Enter)', onclick: () => addSibling() }, icon('plus', 'sm'), 'Sibling');
    tb.del = h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Delete topic', title: 'Delete topic and its children (Delete)', onclick: () => deleteSelected() }, icon('trash', 'sm'));
    const sw = h('div.mm-swatches', { role: 'group', 'aria-label': 'Topic color' },
      SWATCHES.map((c) => h('button.mm-swatch', { type: 'button', style: { background: c }, 'aria-label': `Color ${c}`, title: 'Topic color', onclick: () => setColor(c) })),
      h('button.mm-swatch.none', { type: 'button', 'aria-label': 'Default color', title: 'Default color', onclick: () => setColor(null) }, icon('x', 'sm')));
    tb.link = h('button.btn.ghost.sm', { type: 'button', title: 'Link a file or web page', onclick: () => editLink() }, icon('link', 'sm'), 'Link');
    tb.note = h('button.btn.ghost.sm', { type: 'button', title: 'Note', onclick: () => editNote() }, icon('note', 'sm'), 'Note');
    tb.collapse = h('button.btn.ghost.sm', { type: 'button', title: 'Collapse / expand (Space)', onclick: () => toggleCollapse() }, icon('chevron-down', 'sm'), 'Collapse');
    tb.undo = h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Undo', title: 'Undo (Ctrl+Z)', onclick: () => undo.undo() }, icon('history', 'sm'));
    toolbar.append(tb.child, tb.sibling, tb.del, h('span.tb-sep'), sw, h('span.tb-sep'), tb.link, tb.note, tb.collapse, tb.undo);
  }
  const exportBtn = h('button.btn.ghost.sm', { type: 'button', onclick: () => exportMenu() }, icon('download', 'sm'), 'Export');
  toolbar.append(h('div.grow'), exportBtn);

  // ---- model ----
  function readModel() {
    model = new Map();
    for (const [id, o] of readAll(nodes)) {
      model.set(id, {
        id, parent: typeof o.parent === 'string' ? o.parent : '', text: o.text == null ? '' : String(o.text),
        order: Number(o.order) || 0, color: o.color || null, link: o.link || null, collapsed: !!o.collapsed, note: o.note || null,
      });
    }
    const roots = [...model.values()].filter((n) => !n.parent).sort((a, b) => a.order - b.order || (a.id < b.id ? -1 : 1));
    rootId = roots.length ? roots[0].id : null;
    kids = new Map();
    for (const n of model.values()) {
      if (n.id === rootId) continue;
      // Extra parentless nodes and orphans hang off the central node.
      let p = n.parent && model.has(n.parent) ? n.parent : rootId;
      if (!p) continue;
      if (!kids.has(p)) kids.set(p, []);
      kids.get(p).push(n.id);
    }
    for (const list of kids.values()) list.sort((a, b) => model.get(a).order - model.get(b).order || (a < b ? -1 : 1));
  }

  function styleFor(depth) { return STYLE[Math.min(depth, 2)]; }

  /** Text fitted to the node's max width (with an ellipsis), and its width. */
  function fitText(text, depth) {
    const st = styleFor(depth);
    const key = `${depth}|${text}`;
    let r = fitCache.get(key);
    if (r) return r;
    measureCtx.font = `${st.font} ${st.size}px ${fontFamily}`;
    let label = text || ' ';
    let w = measureCtx.measureText(label).width;
    if (w > st.maxW) {
      let lo = 0;
      let hi = label.length;
      while (lo < hi) {
        const mid = (lo + hi + 1) >> 1;
        if (measureCtx.measureText(`${label.slice(0, mid)}…`).width <= st.maxW) lo = mid; else hi = mid - 1;
      }
      label = `${label.slice(0, lo)}…`;
      w = measureCtx.measureText(label).width;
    }
    r = { label, w };
    if (fitCache.size > 5000) fitCache.clear();
    fitCache.set(key, r);
    return r;
  }

  function computeLayout() {
    layout = new Map();
    if (!rootId) return;
    const size = (id, depth) => {
      const n = model.get(id);
      const st = styleFor(depth);
      const { label, w } = fitText(n.text, depth);
      const extra = (n.link ? 20 : 0) + (n.note ? 12 : 0);
      return { w: Math.max(depth === 0 ? 120 : 44, w + st.padX * 2 + extra), h: st.h, label, tw: w };
    };
    const subtree = new Map(); // id -> subtree height
    const measure = (id, depth, seen) => {
      if (seen.has(id)) return 0;
      seen.add(id);
      const s = size(id, depth);
      const ch = model.get(id).collapsed && depth > 0 ? [] : (kids.get(id) || []);
      let total = 0;
      ch.forEach((c, i) => { total += measure(c, depth + 1, seen) + (i ? VGAP[Math.min(depth + 1, 3)] : 0); });
      const sh = Math.max(s.h, total);
      subtree.set(id, { ...s, sh });
      return sh;
    };
    const place = (id, x, y, depth, side, color, seen) => {
      if (seen.has(id)) return;
      seen.add(id);
      const s = subtree.get(id);
      const n = model.get(id);
      layout.set(id, { x, y, w: s.w, h: s.h, tw: s.tw, depth, side, color, label: s.label });
      if (n.collapsed && depth > 0) return;
      const ch = kids.get(id) || [];
      const gap = VGAP[Math.min(depth + 1, 3)];
      const total = ch.reduce((a, c, i) => a + subtree.get(c).sh + (i ? gap : 0), 0);
      let cy = y - total / 2;
      for (const c of ch) {
        const cs = subtree.get(c);
        const cx = x + side * (s.w / 2 + HGAP[Math.min(depth + 1, 3)] + cs.w / 2);
        place(c, cx, cy + cs.sh / 2, depth + 1, side, model.get(c).color || color, seen);
        cy += cs.sh + gap;
      }
    };
    const seen = new Set();
    measure(rootId, 0, seen);
    const rs = subtree.get(rootId);
    layout.set(rootId, { x: 0, y: 0, w: rs.w, h: rs.h, tw: rs.tw, depth: 0, side: 0, color: null, label: rs.label });
    const first = kids.get(rootId) || [];
    const nRight = Math.ceil(first.length / 2);
    const placed = new Set([rootId]);
    for (const [list, side] of [[first.slice(0, nRight), 1], [first.slice(nRight), -1]]) {
      const gap = VGAP[1];
      const total = list.reduce((a, c, i) => a + subtree.get(c).sh + (i ? gap : 0), 0);
      let cy = -total / 2;
      for (const c of list) {
        const cs = subtree.get(c);
        const idx = first.indexOf(c);
        const color = model.get(c).color || PALETTE[idx % PALETTE.length];
        place(c, side * (rs.w / 2 + HGAP[1] + cs.w / 2), cy + cs.sh / 2, 1, side, color, placed);
        cy += cs.sh + gap;
      }
    }
  }

  // ---- rendering ----
  function applyView() {
    viewG.setAttribute('transform', `translate(${view.x} ${view.y}) scale(${view.k})`);
    zoomLabel.textContent = `${Math.round(view.k * 100)}%`;
    if (editing) positionEditor();
  }

  function descendants(id) {
    const out = [];
    const stack = [...(kids.get(id) || [])];
    const seen = new Set();
    while (stack.length) {
      const c = stack.pop();
      if (seen.has(c)) continue;
      seen.add(c);
      out.push(c);
      stack.push(...(kids.get(c) || []));
    }
    return out;
  }

  function render() {
    readModel();
    computeLayout();
    if (selected && !layout.has(selected)) {
      // The selected topic vanished (deleted or collapsed away): fall back to a visible ancestor.
      let p = model.get(selected)?.parent;
      while (p && !layout.has(p)) p = model.get(p)?.parent;
      selected = p || rootId;
    }
    if (!selected) selected = rootId;
    for (const [id, L] of layout) {
      const n = model.get(id);
      const hidden = n.collapsed && id !== rootId ? descendants(id).length : 0;
      const sig = JSON.stringify([L, n.text, n.link, n.note, n.color, hidden]);
      let e = els.get(id);
      if (!e) {
        const rect = svg('rect');
        const text = svg('text', { 'dominant-baseline': 'central', 'text-anchor': 'middle' });
        const title = svg('title');
        const g = svg('g', { class: 'mm-node', 'data-id': id }, title, rect, text);
        nodesG.appendChild(g);
        e = { g, rect, text, title, link: null, badge: null, note: null, sig: '' };
        els.set(id, e);
      }
      if (e.sig !== sig) {
        e.sig = sig;
        const st = styleFor(L.depth);
        e.g.setAttribute('transform', `translate(${L.x} ${L.y})`);
        e.g.setAttribute('class', `mm-node depth-${Math.min(L.depth, 2)}${id === selected ? ' selected' : ''}${n.collapsed ? ' collapsed' : ''}`);
        e.g.style.setProperty('--branch', L.color || 'var(--accent)');
        Object.entries({ x: -L.w / 2, y: -L.h / 2, width: L.w, height: L.h, rx: L.depth === 0 ? L.h / 2 : L.depth === 1 ? 10 : 8 }).forEach(([k, v]) => e.rect.setAttribute(k, v));
        const extra = (n.link ? 20 : 0) + (n.note ? 12 : 0);
        e.text.setAttribute('x', -extra / 2);
        e.text.setAttribute('style', `font-size:${st.size}px;font-weight:${st.font}`);
        e.text.textContent = L.label;
        e.title.textContent = [n.text, n.note, n.link && `Link: ${n.link}`].filter(Boolean).join('\n\n');
        if (n.link) {
          if (!e.link) {
            e.link = svg('g', { class: 'mm-link', role: 'link', tabindex: -1 }, svg('rect', { x: -2, y: -2, width: 20, height: 20, rx: 4, class: 'mm-link-hit' }), svg('path', { d: LINK_PATH, transform: 'scale(.66)' }));
            e.g.appendChild(e.link);
          }
          e.link.setAttribute('transform', `translate(${L.tw / 2 - extra / 2 + 4} -8)`);
        } else if (e.link) { e.link.remove(); e.link = null; }
        if (n.note) {
          if (!e.note) { e.note = svg('circle', { class: 'mm-note-dot', r: 3.5 }); e.g.appendChild(e.note); }
          e.note.setAttribute('cx', L.tw / 2 - extra / 2 + (n.link ? 28 : 8));
          e.note.setAttribute('cy', 0);
        } else if (e.note) { e.note.remove(); e.note = null; }
        if (hidden) {
          if (!e.badge) {
            e.badge = svg('g', { class: 'mm-badge' }, svg('circle', { r: 10 }), svg('text', { 'text-anchor': 'middle', 'dominant-baseline': 'central' }));
            e.g.appendChild(e.badge);
          }
          e.badge.setAttribute('transform', `translate(${(L.side || 1) * (L.w / 2 + 12)} 0)`);
          e.badge.lastChild.textContent = hidden > 99 ? '99+' : String(hidden);
        } else if (e.badge) { e.badge.remove(); e.badge = null; }
      } else {
        e.g.classList.toggle('selected', id === selected);
      }
      // Connector from the parent.
      if (id !== rootId) {
        const pid = model.get(id).parent && layout.has(model.get(id).parent) ? model.get(id).parent : rootId;
        const P = layout.get(pid);
        const side = L.side;
        const x1 = P.x + (P.depth === 0 ? side * (P.w / 2 - 14) : side * P.w / 2);
        const y1 = P.y;
        const x2 = L.x - side * L.w / 2;
        const y2 = L.y;
        const dx = Math.abs(x2 - x1) * 0.5;
        const d = `M${x1} ${y1}C${x1 + side * dx} ${y1} ${x2 - side * dx} ${y2} ${x2} ${y2}`;
        let p = edgeEls.get(id);
        if (!p) { p = svg('path', { class: 'mm-edge' }); edgesG.appendChild(p); edgeEls.set(id, p); }
        if (p.getAttribute('d') !== d) p.setAttribute('d', d);
        p.style.stroke = L.color;
        p.style.strokeWidth = L.depth === 1 ? 3 : L.depth === 2 ? 2 : 1.5;
      }
    }
    for (const [id, e] of els) if (!layout.has(id)) { e.g.remove(); els.delete(id); }
    for (const [id, p] of edgeEls) if (!layout.has(id) || id === rootId) { p.remove(); edgeEls.delete(id); }
    if (!fitted && layout.size) { fitted = true; fit(true); }
    updateToolbar();
    renderPeers();
    if (editing) {
      if (!layout.has(editing.id)) cancelEdit(); else positionEditor();
    }
  }
  const queueRender = rafBatch(render);

  function updateToolbar() {
    if (readOnly) return;
    const n = selected && model.get(selected);
    const isRoot = selected === rootId;
    tb.del.disabled = !n || isRoot;
    tb.sibling.disabled = !n;
    tb.child.disabled = !n;
    tb.collapse.disabled = !n || isRoot || !(kids.get(selected) || []).length;
    tb.collapse.lastChild.textContent = n && n.collapsed ? 'Expand' : 'Collapse';
    tb.link.classList.toggle('active', !!(n && n.link));
    tb.note.classList.toggle('active', !!(n && n.note));
    tb.undo.disabled = !undo.undoStack.length;
  }

  // ---- view ----
  function fit(initial = false) {
    if (!layout.size) return;
    let x0 = Infinity; let y0 = Infinity; let x1 = -Infinity; let y1 = -Infinity;
    for (const L of layout.values()) {
      x0 = Math.min(x0, L.x - L.w / 2); x1 = Math.max(x1, L.x + L.w / 2);
      y0 = Math.min(y0, L.y - L.h / 2); y1 = Math.max(y1, L.y + L.h / 2);
    }
    const W = wrap.clientWidth || 800;
    const H = wrap.clientHeight || 600;
    const k = Math.max(0.2, Math.min(initial ? 1.1 : 1.6, (W - 80) / (x1 - x0), (H - 80) / (y1 - y0)));
    view.k = k;
    view.x = W / 2 - ((x0 + x1) / 2) * k;
    view.y = H / 2 - ((y0 + y1) / 2) * k;
    applyView();
  }

  function zoomAt(factor, sx, sy) {
    const k = Math.max(0.2, Math.min(3, view.k * factor));
    const f = k / view.k;
    view.x = sx - (sx - view.x) * f;
    view.y = sy - (sy - view.y) * f;
    view.k = k;
    applyView();
  }
  function zoomBy(f) { zoomAt(f, wrap.clientWidth / 2, wrap.clientHeight / 2); }

  function toScreen(L) { return { x: L.x * view.k + view.x, y: L.y * view.k + view.y }; }

  function ensureVisible(id) {
    const L = layout.get(id);
    if (!L) return;
    const p = toScreen(L);
    const W = wrap.clientWidth;
    const H = wrap.clientHeight;
    const hw = (L.w * view.k) / 2 + 24;
    const hh = (L.h * view.k) / 2 + 24;
    let dx = 0; let dy = 0;
    if (p.x - hw < 0) dx = hw - p.x; else if (p.x + hw > W) dx = W - hw - p.x;
    if (p.y - hh < 0) dy = hh - p.y; else if (p.y + hh > H - 50) dy = H - 50 - hh - p.y;
    if (dx || dy) { view.x += dx; view.y += dy; applyView(); }
  }

  // ---- selection and peers ----
  function select(id, { focus = true } = {}) {
    if (!id || !model.has(id)) return;
    if (selected && els.get(selected)) els.get(selected).g.classList.remove('selected');
    selected = id;
    els.get(id)?.g.classList.add('selected');
    provider.setLocal('node', id);
    updateToolbar();
    ensureVisible(id);
    status.textContent = model.get(id).text;
    if (focus && !editing) wrap.focus({ preventScroll: true });
  }

  function renderPeers() {
    const live = new Set();
    provider.awareness.getStates().forEach((st, clientId) => {
      if (clientId === doc.clientID || !st.user || !st.node || !layout.has(st.node)) return;
      const L = layout.get(st.node);
      live.add(clientId);
      let r = peerEls.get(clientId);
      if (!r) {
        r = svg('g', { class: 'mm-peer' }, svg('rect'), svg('text'));
        peersG.appendChild(r);
        peerEls.set(clientId, r);
      }
      const [rect, text] = r.childNodes;
      Object.entries({ x: L.x - L.w / 2 - 5, y: L.y - L.h / 2 - 5, width: L.w + 10, height: L.h + 10, rx: L.depth === 0 ? (L.h + 10) / 2 : 12 }).forEach(([k, v]) => rect.setAttribute(k, v));
      rect.style.stroke = st.user.color;
      text.setAttribute('x', L.x - L.w / 2);
      text.setAttribute('y', L.y - L.h / 2 - 9);
      text.style.fill = st.user.color;
      text.textContent = st.user.name;
    });
    for (const [id, r] of peerEls) if (!live.has(id)) { r.remove(); peerEls.delete(id); }
  }
  const queuePeers = rafBatch(renderPeers);
  provider.awareness.on('change', queuePeers);

  // ---- editing the CRDT ----
  const tx = (fn) => doc.transact(fn, 'local');

  function addNode(parentId, order) {
    if (readOnly || !parentId) return null;
    const id = newId('n');
    tx(() => {
      const m = new Y.Map();
      nodes.set(id, m);
      m.set('parent', parentId);
      m.set('text', 'New topic');
      m.set('order', order);
      if (model.get(parentId)?.collapsed) setFields(nodes, parentId, { collapsed: null });
    });
    render();
    select(id);
    startEdit(id, true);
    return id;
  }

  function addChild(pid = selected) {
    if (!pid) return;
    const sib = kids.get(pid) || [];
    const max = sib.reduce((m, c) => Math.max(m, model.get(c).order), 0);
    addNode(pid, sib.length ? max + 1 : 1);
  }

  function addSibling() {
    if (!selected) return;
    if (selected === rootId) { addChild(rootId); return; }
    const n = model.get(selected);
    const pid = n.parent && model.has(n.parent) ? n.parent : rootId;
    const sib = kids.get(pid) || [];
    const i = sib.indexOf(selected);
    const next = sib[i + 1];
    addNode(pid, next ? (n.order + model.get(next).order) / 2 : n.order + 1);
  }

  function deleteSelected() {
    if (readOnly || !selected || selected === rootId) return;
    const doomed = [selected, ...descendants(selected)];
    const n = model.get(selected);
    const sib = kids.get(n.parent) || [];
    const i = sib.indexOf(selected);
    const next = sib[i + 1] || sib[i - 1] || n.parent || rootId;
    tx(() => doomed.forEach((d) => nodes.delete(d)));
    selected = model.has(next) ? next : rootId;
    if (doomed.length > 1) toast(`Deleted ${doomed.length} topics`, 'info', { timeout: 4000, action: { label: 'Undo', run: () => undo.undo() } });
    render();
    select(selected);
  }

  function setColor(c) {
    if (readOnly || !selected) return;
    tx(() => setFields(nodes, selected, { color: c }));
  }

  function toggleCollapse(id = selected) {
    if (readOnly || !id || id === rootId || !(kids.get(id) || []).length) return;
    tx(() => setFields(nodes, id, { collapsed: model.get(id).collapsed ? null : true }));
  }

  function reparent(id, target) {
    if (readOnly || id === rootId || id === target || descendants(id).includes(target)) return false;
    const sib = kids.get(target) || [];
    const max = sib.reduce((m, c) => Math.max(m, model.get(c).order), 0);
    tx(() => setFields(nodes, id, { parent: target, order: max + 1 }));
    if (model.get(target).collapsed) tx(() => setFields(nodes, target, { collapsed: null }));
    return true;
  }

  async function editLink() {
    if (!selected) return;
    const n = model.get(selected);
    const input = h('input', { type: 'text', value: n.link || '', placeholder: 'https://… or /Folder/file.md', 'aria-label': 'Link' });
    const v = await modal({
      title: 'Link',
      body: h('form', { onsubmit: (e) => { e.preventDefault(); e.target.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
        h('label', 'Web address or file path'), input,
        h('div.row', { style: { marginTop: '10px' } }, h('button.btn.sm', {
          type: 'button',
          onclick: async () => {
            const f = await pickFile({ title: 'Link a file', rootId: me.root_id, startId: entry.parent_id });
            if (!f) return;
            try { input.value = (await api.get(`/api/files/${f.id}`)).path; } catch (e) { errorToast(e); }
          },
        }, icon('file', 'sm'), 'Choose a file…'))),
      actions: [
        n.link && { label: 'Remove link', kind: 'danger', run: () => '' },
        { label: 'Cancel', value: null },
        { label: 'Save', kind: 'primary', run: () => input.value.trim() },
      ].filter(Boolean),
    });
    if (v === null || !model.has(n.id)) return;
    tx(() => setFields(nodes, n.id, { link: v || null }));
  }

  async function editNote() {
    if (!selected) return;
    const n = model.get(selected);
    const ta = h('textarea', { rows: 6, 'aria-label': 'Note' });
    ta.value = n.note || '';
    const v = await modal({ title: `Note on “${n.text}”`, body: ta, actions: [{ label: 'Cancel', value: null }, { label: 'Save', kind: 'primary', run: () => ta.value }] });
    if (v === null || !model.has(n.id)) return;
    tx(() => setFields(nodes, n.id, { note: v.trim() ? v : null }));
  }

  function outline() {
    const lines = [`# ${model.get(rootId)?.text || entry.name}`, ''];
    const walk = (id, depth, seen) => {
      for (const c of kids.get(id) || []) {
        if (seen.has(c)) continue;
        seen.add(c);
        const n = model.get(c);
        const text = n.link && /^https?:/i.test(n.link) ? `[${n.text}](${n.link})` : n.text;
        lines.push(`${'  '.repeat(depth)}- ${text}`);
        if (n.note) n.note.split('\n').forEach((l) => lines.push(`${'  '.repeat(depth + 1)}${l}`));
        walk(c, depth + 1, seen);
      }
    };
    if (rootId) walk(rootId, 0, new Set([rootId]));
    return `${lines.join('\n')}\n`;
  }

  function exportMenu() {
    menu(exportBtn, [
      { label: 'Copy outline (Markdown)', icon: 'copy', run: () => copyText(outline(), 'Outline copied') },
      { label: 'Download outline (.md)', icon: 'download', run: () => {
        const url = URL.createObjectURL(new Blob([outline()], { type: 'text/markdown' }));
        const a = h('a', { href: url, download: entry.name.replace(/\.fmind$/i, '') + '.md' });
        document.body.appendChild(a);
        a.click();
        a.remove();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
      } },
    ]);
  }

  // ---- inline text editing ----
  function positionEditor() {
    const L = layout.get(editing.id);
    if (!L) return;
    const p = toScreen(L);
    const st = styleFor(L.depth);
    const w = Math.max(L.w, 160) * view.k;
    Object.assign(editInput.style, {
      left: `${p.x - w / 2}px`, top: `${p.y - (L.h * view.k) / 2}px`, width: `${w}px`, height: `${L.h * view.k}px`,
      fontSize: `${st.size * view.k}px`, fontWeight: st.font,
    });
  }

  function startEdit(id = selected, selectAll = false) {
    if (readOnly || !id || !layout.has(id)) return;
    editing = { id, original: model.get(id).text };
    editInput.value = model.get(id).text;
    editInput.hidden = false;
    positionEditor();
    editInput.focus();
    if (selectAll) editInput.select(); else editInput.setSelectionRange(editInput.value.length, editInput.value.length);
  }

  function commitEdit() {
    if (!editing) return;
    const { id } = editing;
    editing = null;
    editInput.hidden = true;
    const t = editInput.value.trim();
    if (model.has(id) && t && t !== model.get(id).text) tx(() => setFields(nodes, id, { text: t }));
    wrap.focus({ preventScroll: true });
  }

  function cancelEdit() {
    editing = null;
    editInput.hidden = true;
    wrap.focus({ preventScroll: true });
  }

  editInput.addEventListener('keydown', (e) => {
    e.stopPropagation();
    if (e.key === 'Enter') { e.preventDefault(); commitEdit(); } else if (e.key === 'Escape') { e.preventDefault(); cancelEdit(); } else if (e.key === 'Tab') {
      e.preventDefault();
      commitEdit();
      if (e.shiftKey) return;
      addChild();
    }
  });
  editInput.addEventListener('blur', () => { if (editing) commitEdit(); });

  // ---- keyboard ----
  function navigate(key) {
    if (!selected || !layout.has(selected)) { select(rootId); return; }
    const L = layout.get(selected);
    const visibleKids = (id) => (kids.get(id) || []).filter((c) => layout.has(c));
    if (key === 'ArrowLeft' || key === 'ArrowRight') {
      const dir = key === 'ArrowRight' ? 1 : -1;
      if (selected === rootId) {
        const side = visibleKids(rootId).filter((c) => layout.get(c).side === dir);
        if (side.length) select(nearestY(side, L.y));
      } else if (L.side === dir) {
        const ch = visibleKids(selected);
        if (ch.length) select(nearestY(ch, L.y));
      } else {
        const p = model.get(selected).parent;
        select(p && layout.has(p) ? p : rootId);
      }
      return;
    }
    // Up/down: nearest topic above/below on the same side and depth.
    const dir = key === 'ArrowDown' ? 1 : -1;
    let best = null;
    let bestD = Infinity;
    for (const [id, M] of layout) {
      if (id === selected || M.depth !== L.depth || M.side !== L.side) continue;
      const dy = (M.y - L.y) * dir;
      if (dy <= 0) continue;
      const d = dy + Math.abs(M.x - L.x) * 0.2;
      if (d < bestD) { bestD = d; best = id; }
    }
    if (best) select(best);
  }
  function nearestY(list, y) {
    return list.reduce((a, b) => (Math.abs(layout.get(b).y - y) < Math.abs(layout.get(a).y - y) ? b : a));
  }

  wrap.addEventListener('keydown', (e) => {
    if (inField(e)) return;
    const mod = e.ctrlKey || e.metaKey;
    const k = e.key;
    if (k.startsWith('Arrow')) { e.preventDefault(); navigate(k); return; }
    if ((k === '+' || k === '=') && !mod) { e.preventDefault(); zoomBy(1.2); return; }
    if (k === '-' && !mod) { e.preventDefault(); zoomBy(1 / 1.2); return; }
    if (k === '0' && !mod) { e.preventDefault(); fit(); return; }
    if (readOnly) return;
    if (k === 'Tab') { e.preventDefault(); addChild(); } else if (k === 'Enter') { e.preventDefault(); addSibling(); } else if (k === 'F2') { e.preventDefault(); startEdit(); } else if (k === 'Delete' || k === 'Backspace') { e.preventDefault(); deleteSelected(); } else if (k === ' ') { e.preventDefault(); toggleCollapse(); } else if (mod && (k === 'z' || k === 'Z' || k === 'y')) {
      e.preventDefault();
      if (k === 'y' || e.shiftKey) undo.redo(); else undo.undo();
    } else if (k.length === 1 && !mod && !e.altKey && selected) {
      // Typing on a selected topic starts replacing its text.
      e.preventDefault();
      startEdit(selected, true);
      editInput.value = k;
    }
  });

  // ---- pointer: pan, select, drag to re-parent, wheel zoom ----
  let pointer = null;
  function nodeAt(clientX, clientY, exclude) {
    const r = wrap.getBoundingClientRect();
    const wx = (clientX - r.left - view.x) / view.k;
    const wy = (clientY - r.top - view.y) / view.k;
    for (const [id, L] of layout) {
      if (exclude && exclude.has(id)) continue;
      if (Math.abs(wx - L.x) <= L.w / 2 + 4 && Math.abs(wy - L.y) <= L.h / 2 + 4) return id;
    }
    return null;
  }

  svgEl.addEventListener('pointerdown', (e) => {
    if (e.button !== 0) return;
    if (editing) commitEdit();
    const g = e.target.closest('.mm-node');
    if (e.target.closest('.mm-link')) return;
    if (e.target.closest('.mm-badge') && g) { toggleCollapse(g.dataset.id); return; }
    svgEl.setPointerCapture(e.pointerId);
    if (g) {
      const id = g.dataset.id;
      select(id);
      pointer = { kind: 'node', id, sx: e.clientX, sy: e.clientY, moved: false };
    } else {
      pointer = { kind: 'pan', sx: e.clientX, sy: e.clientY, vx: view.x, vy: view.y };
      wrap.classList.add('panning');
      wrap.focus({ preventScroll: true });
    }
  });
  svgEl.addEventListener('pointermove', (e) => {
    if (!pointer) return;
    const dx = e.clientX - pointer.sx;
    const dy = e.clientY - pointer.sy;
    if (pointer.kind === 'pan') {
      view.x = pointer.vx + dx;
      view.y = pointer.vy + dy;
      applyView();
      return;
    }
    if (readOnly || pointer.id === rootId) return;
    if (!pointer.moved && Math.hypot(dx, dy) < 5) return;
    if (!pointer.moved) {
      pointer.moved = true;
      pointer.exclude = new Set([pointer.id, ...descendants(pointer.id)]);
      for (const id of pointer.exclude) els.get(id)?.g.classList.add('dragging');
    }
    for (const id of pointer.exclude) {
      const L = layout.get(id);
      const e2 = els.get(id);
      if (L && e2) e2.g.setAttribute('transform', `translate(${L.x + dx / view.k} ${L.y + dy / view.k})`);
    }
    const target = nodeAt(e.clientX, e.clientY, pointer.exclude);
    if (target !== pointer.target) {
      if (pointer.target) els.get(pointer.target)?.g.classList.remove('drop-target');
      pointer.target = target;
      if (target) els.get(target)?.g.classList.add('drop-target');
    }
  });
  const endPointer = () => {
    if (!pointer) return;
    const p = pointer;
    pointer = null;
    wrap.classList.remove('panning');
    if (p.kind === 'node' && p.moved) {
      for (const id of p.exclude) {
        const e2 = els.get(id);
        if (!e2) continue;
        e2.g.classList.remove('dragging');
        e2.sig = ''; // force the position back on the next render
      }
      if (p.target) els.get(p.target)?.g.classList.remove('drop-target');
      if (!(p.target && reparent(p.id, p.target))) render();
    }
  };
  svgEl.addEventListener('pointerup', endPointer);
  svgEl.addEventListener('pointercancel', endPointer);
  svgEl.addEventListener('dblclick', (e) => {
    const g = e.target.closest('.mm-node');
    if (g && !e.target.closest('.mm-link')) startEdit(g.dataset.id);
    else if (!g && !readOnly && rootId) { select(rootId); }
  });
  svgEl.addEventListener('click', async (e) => {
    const l = e.target.closest('.mm-link');
    if (!l) return;
    const id = l.closest('.mm-node').dataset.id;
    try { await openLink(model.get(id).link, ctx.navigate); } catch (err) { errorToast(err.status === 404 ? new Error(`Can’t find “${model.get(id).link}”`) : err); }
  });
  wrap.addEventListener('wheel', (e) => {
    e.preventDefault();
    const r = wrap.getBoundingClientRect();
    zoomAt(Math.exp(-e.deltaY * (e.deltaMode === 1 ? 0.05 : 0.0018)), e.clientX - r.left, e.clientY - r.top);
  }, { passive: false });
  const ro = new ResizeObserver(() => { if (!fitted) return; applyView(); });
  ro.observe(wrap);

  // ---- CRDT observation ----
  const onDeep = () => queueRender();
  nodes.observeDeep(onDeep);
  const onUndoStack = () => updateToolbar();
  if (undo) { undo.on('stack-item-added', onUndoStack); undo.on('stack-item-popped', onUndoStack); }

  setHelp([
    ['Tab', 'Add a child topic'],
    ['Enter', 'Add a sibling topic'],
    ['F2 / double-click / type', 'Edit the topic text'],
    ['Delete', 'Delete the topic and its children'],
    ['↑ ↓ ← →', 'Move between topics'],
    ['Space', 'Collapse / expand'],
    ['Drag a topic', 'Move it under another topic'],
    ['Drag the background', 'Pan'],
    ['Wheel / + / -', 'Zoom'],
    ['0', 'Fit to screen'],
    ['Ctrl+Z / Ctrl+Shift+Z', 'Undo / redo'],
  ]);

  render();
  applyView();
  provider.synced.then(() => {
    if (!nodes.size && !readOnly) {
      const title = String(meta.get('title') || entry.name.replace(/\.fmind$/i, ''));
      doc.transact(() => {
        const m = new Y.Map();
        nodes.set('root', m);
        m.set('parent', '');
        m.set('text', title);
        m.set('order', 0);
      }, 'init');
    }
    render();
    if (!fitted) fit(true);
    select(selected || rootId, { focus: !document.activeElement || document.activeElement === document.body });
  });

  return () => {
    nodes.unobserveDeep(onDeep);
    provider.awareness.off('change', queuePeers);
    ro.disconnect();
    if (undo) undo.destroy();
  };
}
