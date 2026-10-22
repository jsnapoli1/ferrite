// Shared helpers for the Files view and its dialogs: permission bits, file type detection,
// downloads and the upload manager (a small panel showing per-file progress).

import { api } from '../api.js';
import { h, icon, bytes, toast } from '../ui.js';

/** Permission bits, as in ferrite-core's `perm` module. */
export const PERM = { READ: 1, UPDATE: 2, CREATE: 4, DELETE: 8, SHARE: 16, ALL: 31 };

const LIVE_EXT = { md: 'text', markdown: 'text', txt: 'text', text: 'text', csv: 'sheet', fmind: 'mindmap', fcanvas: 'canvas' };

export function extOf(name) {
  const i = name.lastIndexOf('.');
  return i > 0 ? name.slice(i + 1).toLowerCase() : '';
}

/** Live editor for a file name (mirrors DocKind::for_name), for nodes that come without it. */
export function liveKindOf(name) {
  return LIVE_EXT[extOf(name)] || null;
}

/**
 * Turns a bare Node (recent, favorites, trash) into the Entry shape the list renders.
 * These lists only contain the viewer's own files, so they carry every permission.
 */
export function asEntry(node, extra = {}) {
  return {
    permissions: PERM.ALL, shared_by: null, lock: null, craft_app: null,
    live_editor: node.kind === 'dir' ? null : liveKindOf(node.name),
    ...node, ...extra,
  };
}

const INLINE_MIME = /^(image\/(png|jpeg|gif|webp|avif)|application\/pdf|video\/(mp4|webm)|audio\/(mpeg|ogg|wav|webm))$/;

/** How a file can be previewed in the browser: image, pdf, video, audio, text or null. */
export function previewKind(entry) {
  const m = entry.mime || '';
  if (m.startsWith('text/') || m === 'application/json') return 'text';
  if (!INLINE_MIME.test(m)) return null;
  if (m.startsWith('image/')) return 'image';
  if (m.startsWith('video/')) return 'video';
  if (m.startsWith('audio/')) return 'audio';
  return 'pdf';
}

export const fileUrl = (id, inline = false) => `/api/files/${encodeURIComponent(id)}/download${inline ? '?inline=true' : ''}`;

/** Starts a browser download of `href`. */
export function download(href, name = '') {
  const a = h('a', { href, download: name || true, hidden: true });
  document.body.appendChild(a);
  a.click();
  a.remove();
}

/** The "What kind of thing is this" CSS hook used to tint file icons. */
export function iconTone(entry) {
  if (entry.kind === 'dir') return 'fl-tone-dir';
  if (entry.live_editor) return `fl-tone-${entry.live_editor}`;
  const m = entry.mime || '';
  if (m.startsWith('image/')) return 'fl-tone-image';
  if (m.startsWith('video/') || m.startsWith('audio/')) return 'fl-tone-media';
  if (m === 'application/pdf') return 'fl-tone-pdf';
  if (entry.craft_app) return 'fl-tone-craft';
  return 'fl-tone-file';
}

// ---- uploads ------------------------------------------------------------------------

let panel = null;
let active = 0;

function ensurePanel() {
  if (panel && document.body.contains(panel.el)) return panel;
  const list = h('div.fl-upload-items');
  const title = h('strong.grow', 'Uploads');
  const el = h('div.fl-upload.card', { role: 'region', 'aria-label': 'Uploads' },
    h('div.fl-upload-head', icon('upload', 'sm'), title,
      h('button.btn.ghost.icon.sm', { 'aria-label': 'Close uploads', onclick: () => { el.remove(); panel = null; } }, icon('x', 'sm'))),
    list);
  document.body.appendChild(el);
  panel = { el, list, title };
  return panel;
}

/**
 * Uploads files one after another into a folder, showing progress per file. `send` performs
 * one upload (defaults to the authenticated API) and receives (file, onProgress).
 * Resolves with the number of files that succeeded.
 */
export async function uploadFiles(files, { parentId, send, onEach } = {}) {
  const list = [...files];
  if (!list.length) return 0;
  const p = ensurePanel();
  const rows = list.map((f) => {
    const bar = h('div', { style: { width: '0%' } });
    const status = h('span.small.muted', bytes(f.size));
    const row = h('div.fl-upload-item', h('div.row', h('span.grow.ellipsis', { title: f.name }, f.name), status), h('div.progress', bar));
    p.list.appendChild(row);
    return { f, bar, status, row };
  });
  active += list.length;
  p.title.textContent = `Uploading ${active} file${active === 1 ? '' : 's'}`;
  let ok = 0;
  for (const r of rows) {
    try {
      const res = await (send ? send(r.f, (x) => { r.bar.style.width = `${Math.round(x * 100)}%`; })
        : api.upload(parentId, r.f, (x) => { r.bar.style.width = `${Math.round(x * 100)}%`; }));
      r.bar.style.width = '100%';
      r.row.classList.add('done');
      r.status.textContent = 'Done';
      ok++;
      if (onEach) onEach(res);
    } catch (e) {
      r.row.classList.add('failed');
      r.status.textContent = e.status === 507 ? 'Quota exceeded' : (e.message || 'Failed');
      r.status.title = e.message || '';
    }
    active--;
    p.title.textContent = active ? `Uploading ${active} file${active === 1 ? '' : 's'}` : 'Uploads complete';
  }
  if (ok === list.length) {
    // Everything went through: tidy the panel away after a moment.
    setTimeout(() => {
      rows.forEach((r) => r.row.remove());
      if (panel && !panel.list.children.length) { panel.el.remove(); panel = null; }
    }, 2500);
  } else {
    toast(`${list.length - ok} upload${list.length - ok === 1 ? '' : 's'} failed`, 'error');
  }
  return ok;
}

/** A hidden file input that calls `onFiles(FileList)`; click it to open the picker. */
export function filePicker(onFiles, { multiple = true } = {}) {
  const input = h('input', {
    type: 'file', multiple, hidden: true, 'aria-hidden': 'true', tabindex: '-1',
    onchange: () => { if (input.files.length) onFiles([...input.files]); input.value = ''; },
  });
  return input;
}

/**
 * Makes the whole window a drop target while the view is mounted. `accept()` decides
 * whether drops are currently allowed. Returns a cleanup function.
 */
export function windowDropZone(onFiles, accept = () => true, label = 'Drop files to upload') {
  const overlay = h('div.fl-drop', { hidden: true }, h('div.fl-drop-inner', icon('upload', 'icon-lg'), h('div', label)));
  document.body.appendChild(overlay);
  let depth = 0;
  const hasFiles = (e) => [...(e.dataTransfer?.types || [])].includes('Files');
  const enter = (e) => {
    if (!hasFiles(e) || !accept()) return;
    e.preventDefault();
    depth++;
    overlay.hidden = false;
  };
  const over = (e) => {
    if (!hasFiles(e) || !accept()) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
  };
  const leave = (e) => {
    if (!hasFiles(e)) return;
    depth = Math.max(0, depth - 1);
    if (!depth) overlay.hidden = true;
  };
  const drop = (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    depth = 0;
    overlay.hidden = true;
    if (accept() && e.dataTransfer.files.length) onFiles([...e.dataTransfer.files]);
  };
  window.addEventListener('dragenter', enter);
  window.addEventListener('dragover', over);
  window.addEventListener('dragleave', leave);
  window.addEventListener('drop', drop);
  return () => {
    window.removeEventListener('dragenter', enter);
    window.removeEventListener('dragover', over);
    window.removeEventListener('dragleave', leave);
    window.removeEventListener('drop', drop);
    overlay.remove();
  };
}
