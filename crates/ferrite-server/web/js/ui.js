// DOM and UI helpers. Content is always inserted as text nodes — never via innerHTML — so
// user data can't inject markup.

const SVG_NS = 'http://www.w3.org/2000/svg';

/**
 * h('div.card.pad', {onclick, title}, child, 'text', [more]) — build an element.
 * Attributes starting with "on" become listeners; `class`, `style` (object) and `dataset`
 * are handled; booleans toggle the attribute.
 */
export function h(tag, attrs, ...children) {
  if (attrs === null || typeof attrs !== 'object' || attrs instanceof Node || Array.isArray(attrs)) {
    children.unshift(attrs);
    attrs = {};
  }
  const [name, ...classes] = tag.split('.');
  const el = document.createElement(name || 'div');
  if (classes.length) el.className = classes.join(' ');
  for (const [k, v] of Object.entries(attrs)) {
    if (v === undefined || v === null || v === false) continue;
    if (k.startsWith('on') && typeof v === 'function') el.addEventListener(k.slice(2).toLowerCase(), v);
    else if (k === 'class') el.className = [el.className, v].filter(Boolean).join(' ');
    else if (k === 'style' && typeof v === 'object') Object.assign(el.style, v);
    else if (k === 'dataset') Object.assign(el.dataset, v);
    else if (k === 'value') el.value = v;
    else if (k === 'checked') el.checked = !!v;
    else if (v === true) el.setAttribute(k, '');
    else el.setAttribute(k, String(v));
  }
  append(el, children);
  return el;
}

export function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.appendChild(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

export function clear(el) {
  while (el.firstChild) el.removeChild(el.firstChild);
  return el;
}

// ---- icons (24x24 stroke paths) ------------------------------------------------
const ICONS = {
  files: 'M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z',
  folder: 'M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z',
  file: 'M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5',
  doc: 'M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5M9 13h6M9 17h6M9 9h2',
  sheet: 'M4 5a1 1 0 0 1 1-1h14a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1zM4 10h16M4 15h16M10 4v16',
  mindmap: 'M12 12m-2.5 0a2.5 2.5 0 1 0 5 0a2.5 2.5 0 1 0-5 0M5 5m-2 0a2 2 0 1 0 4 0a2 2 0 1 0-4 0M19 5m-2 0a2 2 0 1 0 4 0a2 2 0 1 0-4 0M5 19m-2 0a2 2 0 1 0 4 0a2 2 0 1 0-4 0M19 19m-2 0a2 2 0 1 0 4 0a2 2 0 1 0-4 0M6.5 6.5l3.7 3.7M17.5 6.5l-3.7 3.7M6.5 17.5l3.7-3.7M17.5 17.5l-3.7-3.7',
  canvas: 'M3 3h7v7H3zM14 3h7v5h-7zM14 12h7v9h-7zM3 14h7v7H3z',
  image: 'M4 5h16v14H4zM8.5 10a1.5 1.5 0 1 0 0-3 1.5 1.5 0 0 0 0 3zM20 15l-5-5L5 19',
  talk: 'M21 12a8 8 0 0 1-11.6 7.1L4 20l1.1-4.4A8 8 0 1 1 21 12z',
  boards: 'M4 4h5v16H4zM10.5 4h5v10h-5zM17 4h3v13h-3z',
  assistant: 'M12 3l1.8 4.7L18.5 9.5l-4.7 1.8L12 16l-1.8-4.7L5.5 9.5l4.7-1.8zM19 15l.9 2.1L22 18l-2.1.9L19 21l-.9-2.1L16 18l2.1-.9zM5 15l.6 1.4L7 17l-1.4.6L5 19l-.6-1.4L3 17l1.4-.6z',
  search: 'M11 11m-7 0a7 7 0 1 0 14 0a7 7 0 1 0-14 0M20 20l-3.5-3.5',
  settings: 'M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z',
  bell: 'M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9M13.7 21a2 2 0 0 1-3.4 0',
  plus: 'M12 5v14M5 12h14',
  upload: 'M12 16V4M7 9l5-5 5 5M4 20h16',
  download: 'M12 4v12M7 11l5 5 5-5M4 20h16',
  share: 'M18 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM6 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM18 22a3 3 0 1 0 0-6 3 3 0 0 0 0 6zM8.6 13.5l6.8 4M15.4 6.5l-6.8 4',
  trash: 'M4 7h16M10 11v6M14 11v6M5 7l1 12a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2l1-12M9 7V4h6v3',
  edit: 'M4 20h4L19 9l-4-4L4 16zM13.5 6.5l4 4',
  more: 'M12 6h.01M12 12h.01M12 18h.01',
  x: 'M6 6l12 12M18 6L6 18',
  check: 'M5 12l5 5L20 7',
  user: 'M12 12a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM4 21a8 8 0 0 1 16 0',
  users: 'M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM2 21a7 7 0 0 1 14 0M16 3.1a4 4 0 0 1 0 7.8M22 21a7 7 0 0 0-4-6.3',
  lock: 'M5 11h14v10H5zM8 11V7a4 4 0 0 1 8 0v4',
  link: 'M10 14a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1 1M14 10a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1-1',
  phone: 'M5 4h4l2 5-2.5 1.5a11 11 0 0 0 5 5L15 13l5 2v4a2 2 0 0 1-2 2A16 16 0 0 1 3 6a2 2 0 0 1 2-2',
  'phone-off': 'M10.7 13.3a11 11 0 0 1-2.2-2.8L11 9 9 4H5a2 2 0 0 0-2 2 16 16 0 0 0 4.7 11.3M13 15.5l2-2.5 5 2v4a2 2 0 0 1-2 2 16 16 0 0 1-6-1.5M3 3l18 18',
  video: 'M3 7h12v10H3zM15 10l6-3v10l-6-3',
  'video-off': 'M3 7h3m4 0h5v5m0 5H3V7M15 10l6-3v10l-3-1.5M3 3l18 18',
  mic: 'M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3zM19 11a7 7 0 0 1-14 0M12 18v3',
  'mic-off': 'M15 9.3V6a3 3 0 0 0-5.7-1.3M9 9v3a3 3 0 0 0 4.7 2.5M19 11a7 7 0 0 1-1.2 3.9M5 11a7 7 0 0 0 10.6 6M12 18v3M3 3l18 18',
  screen: 'M3 4h18v12H3zM8 20h8M12 16v4',
  hand: 'M8 13V5a1.5 1.5 0 0 1 3 0v6M11 10V3.5a1.5 1.5 0 0 1 3 0V10M14 10V5a1.5 1.5 0 0 1 3 0v8a7 7 0 0 1-7 7h-.5a7 7 0 0 1-5.2-2.4L3 15.5a1.6 1.6 0 0 1 2.4-2L8 16',
  logout: 'M15 4h3a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2h-3M10 17l5-5-5-5M15 12H4',
  sun: 'M12 16a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
  moon: 'M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z',
  menu: 'M4 6h16M4 12h16M4 18h16',
  'arrow-left': 'M19 12H5M12 19l-7-7 7-7',
  'chevron-right': 'M9 6l6 6-6 6',
  'chevron-down': 'M6 9l6 6 6-6',
  star: 'M12 3l2.8 5.7 6.2.9-4.5 4.4 1.1 6.2L12 17.3 6.4 20.2l1.1-6.2L3 9.6l6.2-.9z',
  history: 'M3 12a9 9 0 1 0 3-6.7L3 8M3 3v5h5M12 7v5l3 2',
  refresh: 'M20 11a8 8 0 0 0-14.9-3.9L3 9M4 13a8 8 0 0 0 14.9 3.9L21 15M3 4v5h5M21 20v-5h-5',
  send: 'M22 2L11 13M22 2l-7 20-4-9-9-4z',
  smile: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM8 14s1.5 2 4 2 4-2 4-2M9 9h.01M15 9h.01',
  paperclip: 'M21 11l-8.5 8.5a5.5 5.5 0 0 1-7.8-7.8L13 3.5a3.7 3.7 0 0 1 5.2 5.2l-8.5 8.5a1.8 1.8 0 0 1-2.6-2.6L15 6.8',
  reply: 'M9 17l-5-5 5-5M4 12h11a5 5 0 0 1 5 5v2',
  poll: 'M4 20V10M10 20V4M16 20v-7M22 20H2',
  copy: 'M9 9h11v11H9zM5 15H4V4h11v1',
  move: 'M5 9l-3 3 3 3M9 5l3-3 3 3M15 19l-3 3-3-3M19 9l3 3-3 3M2 12h20M12 2v20',
  eye: 'M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12zM12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z',
  key: 'M15 7a4 4 0 1 1-3.9 5H9v3H6v3H3v-3l7.1-7.1A4 4 0 0 1 15 7zM16 8h.01',
  cpu: 'M6 6h12v12H6zM9 9h6v6H9zM9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4',
  plug: 'M9 2v6M15 2v6M6 8h12v3a6 6 0 0 1-12 0zM12 17v5',
  external: 'M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
  grip: 'M9 6h.01M15 6h.01M9 12h.01M15 12h.01M9 18h.01M15 18h.01',
  calendar: 'M4 6h16v14H4zM4 10h16M8 3v4M16 3v4',
  tag: 'M3 12V4h8l10 10-8 8zM7.5 7.5h.01',
  comment: 'M4 5h16v11H8l-4 4z',
  activity: 'M22 12h-4l-3 9L9 3l-3 9H2',
  zoomin: 'M11 11m-7 0a7 7 0 1 0 14 0a7 7 0 1 0-14 0M20 20l-3.5-3.5M11 8v6M8 11h6',
  zoomout: 'M11 11m-7 0a7 7 0 1 0 14 0a7 7 0 1 0-14 0M20 20l-3.5-3.5M8 11h6',
  note: 'M5 4h14v12l-4 4H5zM15 16v4M15 16h4',
  craft: 'M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.6 2.6-2.4-.6-.6-2.4z',
  play: 'M7 4v16l13-8z',
  globe: 'M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18zM3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18',
};

export function icon(name, cls = '') {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', `icon ${cls}`.trim());
  svg.setAttribute('aria-hidden', 'true');
  const path = document.createElementNS(SVG_NS, 'path');
  path.setAttribute('d', ICONS[name] || ICONS.file);
  svg.appendChild(path);
  return svg;
}

/** Icon for a file entry from the API. */
export function fileIcon(entry) {
  if (entry.kind === 'dir') return 'folder';
  const ed = entry.live_editor;
  if (ed === 'text') return 'doc';
  if (ed === 'sheet') return 'sheet';
  if (ed === 'mindmap') return 'mindmap';
  if (ed === 'canvas') return 'canvas';
  if ((entry.mime || '').startsWith('image/')) return 'image';
  if ((entry.mime || '').startsWith('video/')) return 'video';
  if ((entry.mime || '').startsWith('audio/')) return 'mic';
  if (entry.craft_app) return 'craft';
  return 'file';
}

// ---- formatting ------------------------------------------------------------------
export function bytes(n) {
  if (n === null || n === undefined) return '';
  if (n < 1024) return `${n} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
}

export function relTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  const s = (Date.now() - d.getTime()) / 1000;
  if (s < 45) return 'just now';
  if (s < 3600) return `${Math.round(s / 60)} min ago`;
  if (s < 86400) return `${Math.round(s / 3600)} h ago`;
  if (s < 86400 * 7) return `${Math.round(s / 86400)} d ago`;
  return d.toLocaleDateString();
}

export function dateTime(ts) {
  if (!ts) return '';
  return new Date(ts).toLocaleString([], { dateStyle: 'medium', timeStyle: 'short' });
}

export function timeOnly(ts) {
  return new Date(ts).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
}

const AVATAR_COLORS = ['#c2410c', '#0f766e', '#7c3aed', '#be185d', '#1d4ed8', '#15803d', '#b45309', '#475569', '#0e7490', '#9333ea'];

export function avatar(name = '?', { size = '', online = false, seed } = {}) {
  const key = seed || name;
  let hash = 0;
  for (const ch of key) hash = (hash * 31 + ch.codePointAt(0)) >>> 0;
  const initials = name.split(/\s+/).filter(Boolean).slice(0, 2).map((p) => [...p][0].toUpperCase()).join('') || '?';
  return h(`span.avatar${size ? '.' + size : ''}`, { style: { background: AVATAR_COLORS[hash % AVATAR_COLORS.length] }, title: name }, initials, online && h('span.dot'));
}

// ---- feedback ----------------------------------------------------------------------
let toastHost = null;

export function toast(message, kind = 'info', { timeout = 4500, action } = {}) {
  if (!toastHost) toastHost = document.body.appendChild(h('div.toasts', { role: 'status', 'aria-live': 'polite' }));
  const el = h(`div.toast.${kind}`, h('div.grow', message));
  if (action) el.appendChild(h('button.btn.sm', { onclick: () => { action.run(); el.remove(); } }, action.label));
  el.appendChild(h('button.btn.ghost.icon.sm', { 'aria-label': 'Dismiss', onclick: () => el.remove() }, icon('x', 'sm')));
  toastHost.appendChild(el);
  if (timeout) setTimeout(() => el.remove(), timeout);
  return el;
}

export function errorToast(e) {
  toast(e && e.message ? e.message : String(e), 'error');
}

/**
 * Opens a modal. `body` is a node or a function(close) returning one; `actions` is a list
 * of {label, kind, run(close) -> value|Promise}. Resolves with the value of the action that
 * closed it (or null when dismissed).
 */
export function modal({ title, body, actions = [], wide = false, onClose } = {}) {
  return new Promise((resolve) => {
    const prevFocus = document.activeElement;
    const close = (value = null) => {
      backdrop.remove();
      document.removeEventListener('keydown', onKey);
      if (onClose) onClose();
      if (prevFocus && prevFocus.focus) prevFocus.focus();
      resolve(value);
    };
    const onKey = (e) => { if (e.key === 'Escape') close(null); };
    const content = typeof body === 'function' ? body(close) : body;
    const foot = actions.length
      ? h('div.modal-foot', actions.map((a) => {
        const btn = h(`button.btn${a.kind ? '.' + a.kind : ''}`, {
          type: a.submit ? 'submit' : 'button',
          onclick: async (e) => {
            e.preventDefault();
            if (!a.run) return close(a.value ?? null);
            btn.disabled = true;
            try {
              const v = await a.run(close);
              if (v !== undefined) close(v);
            } catch (err) {
              errorToast(err);
            } finally {
              btn.disabled = false;
            }
          },
        }, a.label);
        return btn;
      }))
      : null;
    const dlg = h(`div.modal${wide ? '.wide' : ''}`, { role: 'dialog', 'aria-modal': 'true', 'aria-label': title },
      h('div.modal-head', h('h2', title), h('button.btn.ghost.icon.sm', { 'aria-label': 'Close', onclick: () => close(null) }, icon('x'))),
      h('div.modal-body', content), foot);
    const backdrop = h('div.modal-backdrop', { onmousedown: (e) => { if (e.target === backdrop) close(null); } }, dlg);
    document.body.appendChild(backdrop);
    document.addEventListener('keydown', onKey);
    const first = dlg.querySelector('input, textarea, select, [autofocus]');
    (first || dlg.querySelector('.modal-foot .btn.primary') || dlg).focus?.();
  });
}

export async function confirmDialog(title, message, { confirm = 'OK', danger = false } = {}) {
  const v = await modal({
    title,
    body: h('p', message),
    actions: [{ label: 'Cancel', value: false }, { label: confirm, kind: danger ? 'danger.solid' : 'primary', value: true }],
  });
  return v === true;
}

export async function promptDialog(title, { label = '', value = '', placeholder = '', confirm = 'OK', type = 'text' } = {}) {
  const input = h('input', { type, value, placeholder, 'aria-label': label || title });
  const form = h('form', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
    label && h('label', label), input);
  setTimeout(() => { input.focus(); input.select(); }, 0);
  const v = await modal({
    title,
    body: form,
    actions: [{ label: 'Cancel', value: null }, { label: confirm, kind: 'primary', run: () => input.value.trim() || null }],
  });
  return v;
}

/** Context menu at the pointer (or below an element). items: {label, icon, run, danger} | 'sep' */
export function menu(anchor, items) {
  document.querySelectorAll('.menu').forEach((m) => m.remove());
  const el = h('div.menu', { role: 'menu' }, items.filter(Boolean).map((it) => (it === 'sep'
    ? h('hr')
    : h(`button${it.danger ? '.danger' : ''}`, { role: 'menuitem', onclick: () => { el.remove(); it.run(); } }, it.icon && icon(it.icon, 'sm'), it.label))));
  document.body.appendChild(el);
  let x;
  let y;
  if (anchor instanceof MouseEvent) {
    x = anchor.clientX; y = anchor.clientY;
  } else {
    const r = anchor.getBoundingClientRect();
    x = r.left; y = r.bottom + 4;
  }
  const mr = el.getBoundingClientRect();
  el.style.left = `${Math.min(x, window.innerWidth - mr.width - 8)}px`;
  el.style.top = `${Math.min(y, window.innerHeight - mr.height - 8)}px`;
  const away = (e) => {
    if (!el.contains(e.target)) { el.remove(); document.removeEventListener('mousedown', away, true); }
  };
  setTimeout(() => document.addEventListener('mousedown', away, true), 0);
  return el;
}

export function debounce(fn, ms) {
  let t;
  return (...args) => { clearTimeout(t); t = setTimeout(() => fn(...args), ms); };
}

export function spinner() {
  return h('div.loading', h('span.spinner'));
}

export function emptyState(iconName, title, hint, action) {
  return h('div.empty', h('div', icon(iconName, 'icon-lg')), h('h3', title), hint && h('p', hint), action);
}

/** Copies text and confirms with a toast. */
export async function copyText(text, what = 'Copied') {
  try {
    await navigator.clipboard.writeText(text);
    toast(`${what} to the clipboard`, 'ok', { timeout: 2000 });
  } catch (_) {
    await modal({ title: 'Copy', body: h('input', { type: 'text', value: text, readonly: true }) });
  }
}
