// Public link page (/s/:token): works signed out. Password gate, folder browsing inside the
// share (?path=), downloads, image previews and — when the link allows it — uploads.

import { api, ApiError, q } from '../api.js';
import { h, clear, icon, fileIcon, modal, toast, errorToast, bytes, relTime, dateTime, spinner, emptyState } from '../ui.js';
import { uploadFiles, filePicker, windowDropZone, iconTone } from './files-common.js';

const IMAGE = /^image\/(png|jpeg|gif|webp|avif)$/;

/** Uploads one file into the shared folder with a raw PUT body, reporting progress. */
function publicUpload(token, path, file, onProgress) {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('PUT', `/api/public/${encodeURIComponent(token)}/upload${q({ path, name: file.name })}`);
    xhr.setRequestHeader('X-Ferrite-Request', '1');
    xhr.upload.onprogress = (e) => e.lengthComputable && onProgress(e.loaded / e.total);
    xhr.onload = () => {
      let data = null;
      try { data = JSON.parse(xhr.responseText); } catch (_) { /* not JSON */ }
      if (xhr.status >= 200 && xhr.status < 300) resolve(data);
      else reject(new ApiError(xhr.status, xhr.status === 409 ? 'A file with this name already exists' : (data && data.error) || xhr.statusText));
    };
    xhr.onerror = () => reject(new ApiError(0, 'Network error during upload'));
    xhr.send(file);
  });
}

export default async function render(root, ctx) {
  const token = ctx.params.token;
  const path = (ctx.query.path || '').replace(/^\/+|\/+$/g, '');
  const base = `/s/${encodeURIComponent(token)}`;
  const cleanups = [];

  const main = h('main.pub-main', spinner());
  root.appendChild(h('div.pub',
    h('header.pub-bar',
      h('a.pub-brand', { href: ctx.me ? '/' : base, 'data-route': ctx.me ? '' : null }, h('span.brand-mark', 'Fe'), 'Ferrite'),
      h('div.spacer'),
      ctx.me ? h('a.btn.sm', { href: '/files', 'data-route': '' }, icon('files', 'sm'), 'Your files') : null),
    main,
    h('footer.pub-foot.small.muted', 'Shared with Ferrite, a self-hosted collaboration server.')));

  const go = (p) => ctx.navigate(p ? `${base}?path=${encodeURIComponent(p)}` : base);
  const dl = (p) => `/api/public/${encodeURIComponent(token)}/download${q({ path: p })}`;

  function notFound() {
    ctx.setTitle('Link not found');
    clear(main).appendChild(h('div.pub-card.card.pub-404',
      h('div.pub-404-code', '404'),
      h('h1', 'This link doesn’t work anymore'),
      h('p.muted', 'The share may have expired, been removed, or the address is mistyped. Ask the person who sent it for a new link.'),
      ctx.me ? h('a.btn.primary', { href: '/files', 'data-route': '' }, 'Go to your files') : null));
  }

  function passwordForm(share) {
    ctx.setTitle(share.name);
    const input = h('input', { type: 'password', id: 'pub-pw', autocomplete: 'current-password', required: true });
    const err = h('div.error-text', { role: 'alert' });
    const btn = h('button.btn.primary', { type: 'submit', style: { width: '100%', marginTop: '6px' } }, 'Open');
    clear(main).appendChild(h('div.pub-card.card.pub-lock',
      h('div.pub-lock-icon', icon('lock', 'icon-lg')),
      h('h1', share.name),
      h('p.muted', `${share.owner} protected this ${share.kind === 'dir' ? 'folder' : 'file'} with a password.`),
      h('form', {
        onsubmit: async (e) => {
          e.preventDefault();
          err.textContent = '';
          btn.disabled = true;
          try {
            await api.post(`/api/public/${encodeURIComponent(token)}/unlock`, { password: input.value });
            load();
          } catch (ex) {
            err.textContent = ex.status === 401 ? 'That password is not right.' : ex.message;
            input.select();
          } finally {
            btn.disabled = false;
          }
        },
      }, h('label', { for: 'pub-pw' }, 'Password'), input, err, btn)));
    input.focus();
  }

  function preview(name, p) {
    modal({
      title: name,
      wide: true,
      body: h('div.fl-preview', h('img.fl-preview-img', { src: dl(p), alt: name })),
      actions: [{ label: 'Close', kind: 'primary', value: true }],
    });
  }

  function shareHead(share, entry) {
    return h('div.pub-head',
      h(`span.fl-icon.pub-icon.${share.kind === 'dir' ? 'fl-tone-dir' : iconTone(entry)}`, icon(share.kind === 'dir' ? 'folder' : fileIcon(entry))),
      h('div.grow',
        h('h1', share.label || share.name),
        h('div.small.muted.row.wrap', { style: { gap: '8px' } },
          h('span', `Shared by ${share.owner}`),
          share.expires_at ? h('span.pill.warn', icon('calendar', 'sm'), `Available until ${dateTime(share.expires_at)}`) : null,
          share.can_upload ? h('span.pill.ok', icon('upload', 'sm'), 'You can upload here') : null)));
  }

  function fileView(share, entry) {
    const isImage = IMAGE.test(entry.mime);
    return h('div.pub-card.card',
      shareHead(share, entry),
      isImage ? h('button.pub-image', { type: 'button', onclick: () => preview(entry.name, ''), 'aria-label': `Enlarge ${entry.name}` }, h('img', { src: dl(''), alt: entry.name })) : null,
      h('div.pub-file-meta',
        h('div.grow.small.muted', `${bytes(entry.size)} · updated ${relTime(entry.modified)}`),
        h('a.btn.primary', { href: dl(''), download: entry.name }, icon('download', 'sm'), 'Download')));
  }

  function folderView(share, entry, entries) {
    const parts = path ? path.split('/') : [];
    // At the top of the share the title already names the folder; show a count instead.
    const crumbs = !parts.length ? h('div.small.muted.grow', `${entries.length} item${entries.length === 1 ? '' : 's'}`) : h('nav.fl-crumbs.pub-crumbs', { 'aria-label': 'Folder path' }, h('ol',
      [share.name, ...parts].map((name, i, all) => h('li',
        i ? icon('chevron-right', 'sm') : null,
        i === all.length - 1
          ? h('span', { 'aria-current': 'page' }, name)
          : h('a', { href: `${base}${i ? `?path=${encodeURIComponent(parts.slice(0, i).join('/'))}` : ''}`, onclick: (e) => { e.preventDefault(); go(parts.slice(0, i).join('/')); } }, name)))));

    const picker = filePicker(async (files) => upload(files));
    async function upload(files) {
      const n = await uploadFiles(files, { send: (f, onP) => publicUpload(token, path, f, onP) });
      if (n) { toast(`Uploaded ${n} file${n === 1 ? '' : 's'}`, 'ok'); load(); }
    }
    if (share.can_upload) cleanups.push(windowDropZone(upload));

    const rows = entries.map((e) => {
      const p = path ? `${path}/${e.name}` : e.name;
      const isDir = e.kind === 'dir';
      const open = () => (isDir ? go(p) : IMAGE.test(e.mime) ? preview(e.name, p) : null);
      const clickable = isDir || IMAGE.test(e.mime);
      return h(`div.list-item.pub-row${clickable ? '.clickable' : ''}`, clickable ? {
        tabindex: '0', role: 'button', 'aria-label': isDir ? `Open folder ${e.name}` : `Preview ${e.name}`,
        onclick: (ev) => { if (!ev.target.closest('a')) open(); },
        onkeydown: (ev) => { if (ev.key === 'Enter' && ev.target === ev.currentTarget) open(); },
      } : {},
      IMAGE.test(e.mime) ? h('img.pub-thumb', { src: dl(p), alt: '', loading: 'lazy' }) : h(`span.fl-icon.${iconTone(e)}`, icon(fileIcon(e))),
      h('div.grow', h('div.ellipsis', e.name), h('div.small.muted', isDir ? 'Folder' : `${bytes(e.size)} · ${relTime(e.modified)}`)),
      isDir ? icon('chevron-right', 'sm') : h('a.btn.sm.ghost', { href: dl(p), download: e.name, 'aria-label': `Download ${e.name}` }, icon('download', 'sm'), h('span.fl-hide-sm', 'Download')));
    });

    return h('div.pub-card.card',
      shareHead(share, entry),
      h('div.pub-toolbar', crumbs,
        share.can_upload ? h('button.btn.primary', { type: 'button', onclick: () => picker.click() }, icon('upload', 'sm'), 'Upload') : null,
        picker),
      rows.length ? h('div.list.panel.pub-list', rows)
        : h('div.panel', emptyState('folder', 'This folder is empty', share.can_upload ? 'Drop files here or use Upload to add some.' : null)),
      share.can_upload ? h('p.small.muted.pub-drop-hint', icon('upload', 'sm'), ' Drag files anywhere on this page to upload them.') : null);
  }

  async function load() {
    cleanups.splice(0).forEach((fn) => fn());
    let data;
    try {
      data = await api.get(`/api/public/${encodeURIComponent(token)}${q({ path })}`);
    } catch (e) {
      if (e.status === 404) return notFound();
      clear(main).appendChild(h('div.pub-card.card', h('h1', 'Something went wrong'), h('p.muted', e.message)));
      return null;
    }
    if (data.locked) return passwordForm(data.share);
    ctx.setTitle(data.share.name);
    clear(main).appendChild(data.entry.kind === 'dir' ? folderView(data.share, data.entry, data.entries) : fileView(data.share, data.entry));
    return null;
  }

  await load().catch(errorToast);
  return () => cleanups.forEach((fn) => fn());
}
