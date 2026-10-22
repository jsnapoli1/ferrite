// Helpers shared by the boards view and the live editors: CRDT field access that tolerates
// both nested Y.Maps and plain objects, render batching, a file/folder picker, opening file
// references and the keyboard-shortcut help popover.

import { api } from '../api.js';
import { Y } from '../collab.js';
import { h, clear, icon, fileIcon, modal, spinner, emptyState } from '../ui.js';

/** Sets CSS custom properties on an element (style objects can't) and returns it. */
export function withVars(el, vars) {
  for (const [k, v] of Object.entries(vars)) if (v) el.style.setProperty(k, v);
  return el;
}

// ---- CRDT access -----------------------------------------------------------------------

/** Reads `field` of the entry `id` in `map`, whether the entry is a nested Y.Map or a plain object. */
export function getField(map, id, field) {
  const v = map.get(id);
  if (v instanceof Y.Map) return v.get(field);
  if (v && typeof v === 'object') return v[field];
  return undefined;
}

/** Returns the entry `id` of `map` as a plain object (fields only). */
export function readEntry(map, id) {
  const v = map.get(id);
  if (v instanceof Y.Map) return v.toJSON();
  if (v && typeof v === 'object') return { ...v };
  return null;
}

/** Iterates `[id, plainObject]` for every entry of `map`. */
export function readAll(map) {
  const out = [];
  map.forEach((v, id) => {
    if (v instanceof Y.Map) out.push([id, v.toJSON()]);
    else if (v && typeof v === 'object') out.push([id, { ...v }]);
  });
  return out;
}

/**
 * Sets fields of entry `id` (creating it as a nested Y.Map when missing). A plain-object entry
 * written by another client is upgraded to a Y.Map holding its old fields. Call inside
 * `doc.transact` to group changes.
 */
export function setFields(map, id, fields) {
  let v = map.get(id);
  if (!(v instanceof Y.Map)) {
    const old = v && typeof v === 'object' ? v : {};
    v = new Y.Map();
    map.set(id, v);
    for (const [k, val] of Object.entries(old)) if (!(k in fields)) v.set(k, val);
  }
  for (const [k, val] of Object.entries(fields)) {
    if (val === undefined || val === null) v.delete(k);
    else if (v.get(k) !== val) v.set(k, val);
  }
  return v;
}

/** Ids of the top-level entries touched by an observeDeep batch on `map`. */
export function changedIds(events, map) {
  const ids = new Set();
  for (const ev of events) {
    if (ev.target === map) ev.keysChanged.forEach((k) => ids.add(k));
    else if (ev.path.length) ids.add(String(ev.path[0]));
  }
  return ids;
}

/** Random id with a prefix, compatible with the ids the server generates (`n…`, `i…`, `e…`). */
export function newId(prefix) {
  const a = new Uint8Array(6);
  crypto.getRandomValues(a);
  return prefix + [...a].map((b) => b.toString(36).padStart(2, '0')).join('').slice(0, 10);
}

/** Coalesces calls into one per animation frame. */
export function rafBatch(fn) {
  let pending = false;
  return () => {
    if (pending) return;
    pending = true;
    requestAnimationFrame(() => { pending = false; fn(); });
  };
}

/** Calls `fn` at most every `ms`, always delivering the last value. */
export function throttle(fn, ms) {
  let last = 0;
  let timer = null;
  let lastArgs;
  return (...args) => {
    lastArgs = args;
    const now = Date.now();
    if (now - last >= ms) {
      last = now;
      fn(...args);
    } else if (!timer) {
      timer = setTimeout(() => { timer = null; last = Date.now(); fn(...lastArgs); }, ms - (now - last));
    }
  };
}

// ---- opening references ----------------------------------------------------------------

/** Where to open a file entry: the live editor, the folder, or the containing folder. */
export function hrefForEntry(entry) {
  if (entry.kind === 'dir') return `/files/${entry.id}`;
  if (entry.live_editor) return `/edit/${entry.id}`;
  return `/files/${entry.parent_id}?select=${encodeURIComponent(entry.id)}`;
}

/** Opens a file by id (looks up whether it has a live editor first). */
export async function openFileId(id, navigate) {
  const { entry } = await api.get(`/api/files/${encodeURIComponent(id)}`);
  navigate(hrefForEntry(entry));
}

/** Opens a mind-map style link: an http(s) URL in a new tab, or a file path in the app. */
export async function openLink(link, navigate) {
  const l = String(link || '').trim();
  if (!l) return;
  if (/^https?:\/\//i.test(l)) {
    window.open(l, '_blank', 'noopener');
    return;
  }
  const { entry } = await api.get(`/api/files/resolve?path=${encodeURIComponent(l.startsWith('/') ? l : `/${l}`)}`);
  navigate(hrefForEntry(entry));
}

// ---- file picker ------------------------------------------------------------------------

/**
 * Lets the user pick a file (or folder) from their files. Resolves with the chosen entry or
 * null. opts: {title, rootId, startId, accept(entry) -> bool, folders: allow choosing folders}.
 */
export function pickFile({ title = 'Choose a file', rootId, startId, accept = () => true, folders = false, confirm = 'Choose' } = {}) {
  let trail = [{ id: rootId, name: 'Home' }];
  let selected = null;
  const list = h('div.list.picker-list', { role: 'listbox', 'aria-label': 'Files' });
  const crumbs = h('div.picker-crumbs');
  let chooseBtn;

  async function load() {
    clear(list).appendChild(spinner());
    selected = null;
    if (chooseBtn) chooseBtn.disabled = !folders;
    clear(crumbs);
    trail.forEach((c, i) => {
      if (i) crumbs.appendChild(icon('chevron-right', 'sm'));
      crumbs.appendChild(h('button.btn.ghost.sm', { type: 'button', onclick: () => { trail = trail.slice(0, i + 1); load(); } }, c.name));
    });
    const cur = trail[trail.length - 1];
    let entries;
    try {
      ({ entries } = await api.get(`/api/files/${encodeURIComponent(cur.id)}/children`));
    } catch (e) {
      clear(list).appendChild(h('p.error-text', e.message));
      return;
    }
    entries.sort((a, b) => (a.kind === 'dir') === (b.kind === 'dir') ? a.name.localeCompare(b.name) : a.kind === 'dir' ? -1 : 1);
    const shown = entries.filter((e) => e.kind === 'dir' || (!folders && accept(e)));
    clear(list);
    if (!shown.length) list.appendChild(emptyState('folder', 'Nothing here', folders ? 'Choose this folder or go back.' : 'No matching files in this folder.'));
    for (const e of shown) {
      const row = h('div.list-item.clickable', {
        role: 'option', tabindex: 0, dataset: { id: e.id, name: e.name },
        onclick: () => {
          if (e.kind === 'dir') { trail.push({ id: e.id, name: e.name }); load(); return; }
          list.querySelectorAll('.selected').forEach((x) => x.classList.remove('selected'));
          row.classList.add('selected');
          selected = e;
          chooseBtn.disabled = false;
        },
        ondblclick: () => { if (e.kind !== 'dir') chooseBtn.click(); },
        onkeydown: (ev) => { if (ev.key === 'Enter') row.click(); },
      }, icon(fileIcon(e)), h('span.grow.ellipsis', e.name), e.kind === 'dir' && icon('chevron-right', 'sm'));
      list.appendChild(row);
    }
  }

  (async () => {
    if (startId && startId !== rootId) {
      // Start in a given folder: build the trail from its breadcrumbs.
      try {
        const { entry, breadcrumbs } = await api.get(`/api/files/${encodeURIComponent(startId)}`);
        if (entry.kind === 'dir') trail = [{ id: rootId, name: 'Home' }, ...breadcrumbs.filter((b) => b.id !== rootId).map((b) => ({ id: b.id, name: b.name }))];
      } catch (_) { /* fall back to home */ }
    }
    load();
  })();

  const p = modal({
    title,
    body: h('div.picker', crumbs, list),
    actions: [
      { label: 'Cancel', value: null },
      { label: folders ? 'Choose this folder' : confirm, kind: 'primary', run: () => (folders ? { id: trail[trail.length - 1].id, name: trail[trail.length - 1].name, kind: 'dir' } : selected) },
    ],
  });
  const backdrops = document.querySelectorAll('.modal-backdrop');
  chooseBtn = backdrops[backdrops.length - 1]?.querySelector('.modal-foot .btn.primary');
  if (chooseBtn) chooseBtn.disabled = !folders;
  return p;
}

// ---- help popover ----------------------------------------------------------------------

/** A "?" button that toggles a popover listing keyboard shortcuts: [[keys, description], ...]. */
export function helpButton(shortcuts, title = 'Keyboard shortcuts') {
  let pop = null;
  const btn = h('button.btn.ghost.icon.sm.help-btn', { type: 'button', 'aria-label': title, title, 'aria-expanded': 'false' }, '?');
  const close = () => {
    if (!pop) return;
    pop.remove();
    pop = null;
    btn.setAttribute('aria-expanded', 'false');
    document.removeEventListener('mousedown', away, true);
    document.removeEventListener('keydown', esc, true);
  };
  const away = (e) => { if (pop && !pop.contains(e.target) && !btn.contains(e.target)) close(); };
  const esc = (e) => { if (e.key === 'Escape') { e.stopPropagation(); close(); btn.focus(); } };
  btn.addEventListener('click', () => {
    if (pop) return close();
    pop = h('div.popover.help-pop', { role: 'dialog', 'aria-label': title },
      h('h3', title),
      h('table', h('tbody', shortcuts.map(([k, d]) => h('tr', h('td', k.split(' / ').map((x, i) => [i ? ' / ' : '', ...x.split('+').map((p, j) => [j ? '+' : '', h('kbd', p)])])), h('td', d))))));
    document.body.appendChild(pop);
    const r = btn.getBoundingClientRect();
    const pr = pop.getBoundingClientRect();
    pop.style.position = 'fixed';
    pop.style.top = `${Math.min(r.bottom + 6, window.innerHeight - pr.height - 8)}px`;
    pop.style.left = `${Math.max(8, Math.min(r.right - pr.width, window.innerWidth - pr.width - 8))}px`;
    btn.setAttribute('aria-expanded', 'true');
    setTimeout(() => {
      document.addEventListener('mousedown', away, true);
      document.addEventListener('keydown', esc, true);
    }, 0);
  });
  btn.cleanup = close;
  return btn;
}

/** True when a keyboard event happens in a text field (shortcuts should not fire). */
export function inField(e) {
  const t = e.target;
  return t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.tagName === 'SELECT' || t.isContentEditable);
}
