// Dialogs used by the Files view: share, versions, folder picker (move/copy), "Open with"
// (Craft apps), preview and "Connect a sync client".

import { api, q } from '../api.js';
import { h, append, clear, icon, modal, toast, errorToast, avatar, bytes, relTime, dateTime, spinner, copyText, debounce, fileIcon } from '../ui.js';
import { PERM, fileUrl, previewKind, download } from './files-common.js';

let uid = 0;
const nextId = (p) => `${p}-${++uid}`;

/** Read-only text field with a copy button. */
export function copyField(value, label) {
  const input = h('input.mono', { type: 'text', value, readonly: true, 'aria-label': label, onfocus: () => input.select() });
  return h('div.fl-copy', input, h('button.btn', { type: 'button', onclick: () => copyText(value, label ? `${label} copied` : 'Copied') }, icon('copy', 'sm'), 'Copy'));
}

// ---- sharing --------------------------------------------------------------------------

/** Permission presets offered for a node; links never carry the share bit. */
function permOptions(isDir, kind) {
  const strip = kind === 'link' ? ~PERM.SHARE : ~0;
  const opts = [['view', 'Can view', PERM.READ]];
  if (isDir && kind === 'link') opts.push(['upload', 'Can view and upload', PERM.READ | PERM.CREATE]);
  opts.push(['edit', 'Can edit', (isDir ? PERM.ALL : PERM.READ | PERM.UPDATE | PERM.SHARE) & strip]);
  return opts;
}

function permLevel(p) {
  if (p & PERM.UPDATE) return 'edit';
  if (p & PERM.CREATE) return 'upload';
  return 'view';
}

const SHARE_ICON = { user: 'user', group: 'users', room: 'talk', link: 'link' };

/** Opens the share dialog for a file or folder. */
export async function shareDialog(entry, ctx) {
  const isDir = entry.kind === 'dir';
  const existing = h('div.fl-share-list', spinner());
  const formHost = h('div');
  let kind = 'user';

  async function refresh() {
    try {
      const { shares } = await api.get(`/api/files/${entry.id}/shares`);
      clear(existing);
      if (!shares.length) {
        existing.appendChild(h('p.muted.small.fl-share-none', 'Not shared yet. Add people, a group, a conversation or a public link below.'));
        return;
      }
      for (const s of shares) existing.appendChild(shareRow(s));
    } catch (e) {
      clear(existing).appendChild(h('p.error-text', e.message));
    }
  }

  function shareRow(s) {
    const level = permLevel(s.permissions);
    const sel = h('select.sm', {
      'aria-label': `Permission for ${s.target_name || 'public link'}`,
      onchange: async () => {
        const opt = permOptions(isDir, s.kind).find(([k]) => k === sel.value);
        try {
          await api.patch(`/api/shares/${s.id}`, { permissions: opt[2] });
          toast('Permission updated', 'ok', { timeout: 2000 });
        } catch (e) { errorToast(e); refresh(); }
      },
    }, permOptions(isDir, s.kind).map(([k, label]) => h('option', { value: k, selected: k === level }, label)));
    const title = s.kind === 'link' ? (s.label || 'Public link') : (s.target_name || 'Unknown');
    const sub = [];
    if (s.kind === 'group') sub.push('Group');
    if (s.kind === 'room') sub.push('Conversation');
    if (s.has_password) sub.push(h('span.pill', icon('lock', 'sm'), 'Password'));
    if (s.expires_at) sub.push(h('span.pill.warn', icon('calendar', 'sm'), `Expires ${dateTime(s.expires_at)}`));
    return h('div.fl-share-row',
      s.kind === 'user' ? avatar(s.target_name || '?', { size: 'sm', seed: s.target_user }) : h('span.fl-share-kind', icon(SHARE_ICON[s.kind] || 'share', 'sm')),
      h('div.grow',
        h('div.ellipsis', title),
        s.url && h('div.small.muted.mono.ellipsis', s.url),
        sub.length ? h('div.row.wrap.small.muted', { style: { gap: '6px', marginTop: '2px' } }, sub) : null),
      sel,
      s.url && h('button.btn.ghost.icon.sm', { type: 'button', onclick: () => copyText(s.url, 'Link'), 'aria-label': 'Copy link', title: 'Copy link' }, icon('copy', 'sm')),
      h('button.btn.ghost.icon.sm', {
        type: 'button', 'aria-label': `Stop sharing with ${title}`, title: 'Remove',
        onclick: async () => {
          try { await api.del(`/api/shares/${s.id}`); refresh(); } catch (e) { errorToast(e); }
        },
      }, icon('x', 'sm')));
  }

  // The "add" form changes with the kind of recipient.
  const seg = h('div.fl-seg', { role: 'tablist', 'aria-label': 'Share with' },
    [['user', 'Person', 'user'], ['group', 'Group', 'users'], ['room', 'Conversation', 'talk'], ['link', 'Public link', 'link']].map(([k, label, ic]) =>
      h('button', { type: 'button', role: 'tab', dataset: { kind: k }, 'aria-selected': String(k === kind), onclick: () => { kind = k; renderForm(); } }, icon(ic, 'sm'), label)));

  async function renderForm() {
    seg.querySelectorAll('button').forEach((b) => b.setAttribute('aria-selected', String(b.dataset.kind === kind)));
    clear(formHost);
    const err = h('div.error-text');
    const perm = h('select', { 'aria-label': 'Permission' }, permOptions(isDir, kind).map(([k, label]) => h('option', { value: k }, label)));
    let target = null;
    let extra = null;
    if (kind === 'user') {
      const listId = nextId('share-users');
      const dl = h('datalist', { id: listId });
      target = h('input', { type: 'text', placeholder: 'Name or username', 'aria-label': 'User to share with', list: listId, autocomplete: 'off' });
      const lookup = debounce(async () => {
        const v = target.value.trim();
        try {
          const { users } = await api.get(`/api/users${q({ q: v })}`);
          clear(dl);
          users.filter((u) => u.id !== ctx.me.user.id).forEach((u) => dl.appendChild(h('option', { value: u.username }, `${u.display_name} (@${u.username})`)));
        } catch (_) { /* suggestions are best effort */ }
      }, 200);
      target.addEventListener('input', lookup);
      lookup();
      extra = dl;
    } else if (kind === 'group') {
      target = h('select', { 'aria-label': 'Group' }, h('option', { value: '' }, 'Loading…'));
      api.get('/api/admin/groups').then(({ groups }) => {
        clear(target);
        if (!groups.length) target.appendChild(h('option', { value: '' }, 'No groups exist yet'));
        groups.forEach((g) => target.appendChild(h('option', { value: g.name }, g.name)));
      }).catch((e) => { err.textContent = e.message; });
    } else if (kind === 'room') {
      target = h('select', { 'aria-label': 'Conversation' }, h('option', { value: '' }, 'Loading…'));
      api.get('/api/talk/rooms').then(({ rooms }) => {
        clear(target);
        if (!rooms.length) target.appendChild(h('option', { value: '' }, 'You are not in any conversation'));
        rooms.forEach((r) => target.appendChild(h('option', { value: r.token }, r.display_name)));
      }).catch((e) => { err.textContent = e.message; });
    }
    const password = h('input', { type: 'password', placeholder: 'Optional', autocomplete: 'new-password', id: nextId('link-pw') });
    const tomorrow = new Date(Date.now() + 86400000).toISOString().slice(0, 10);
    const expires = h('input', { type: 'date', min: tomorrow, id: nextId('link-exp') });
    const submit = h('button.btn.primary', { type: 'submit' }, kind === 'link' ? 'Create link' : 'Share');
    const form = h('form.fl-share-form', {
      onsubmit: async (e) => {
        e.preventDefault();
        err.textContent = '';
        const opt = permOptions(isDir, kind).find(([k]) => k === perm.value);
        const body = { kind, permissions: opt[2] };
        if (kind !== 'link') {
          body.target = target.value.trim();
          if (!body.target) { err.textContent = 'Choose who to share with.'; return; }
        } else {
          if (password.value) body.password = password.value;
          if (expires.value) body.expires_at = new Date(`${expires.value}T23:59:59`).toISOString();
        }
        submit.disabled = true;
        try {
          await api.post(`/api/files/${entry.id}/shares`, body);
          toast(kind === 'link' ? 'Public link created' : 'Shared', 'ok', { timeout: 2000 });
          renderForm();
          refresh();
        } catch (ex) {
          err.textContent = ex.message;
        } finally {
          submit.disabled = false;
        }
      },
    },
    kind === 'link'
      ? [
        h('p.small.muted', 'Anyone with the link can open it, no account needed.', isDir ? ' “Can view and upload” turns the folder into a file drop.' : ''),
        h('div.fl-share-grid',
          h('div', h('label', { for: password.id }, 'Password'), password),
          h('div', h('label', { for: expires.id }, 'Expires'), expires)),
        h('div.row', { style: { marginTop: '10px' } }, perm, submit),
      ]
      : h('div.row.fl-share-add', target, extra, perm, submit),
    err);
    formHost.appendChild(form);
  }

  refresh();
  renderForm();
  await modal({
    title: `Share “${entry.name}”`,
    body: h('div.fl-share-dialog', existing, h('h3.fl-share-sub', 'Add'), seg, formHost),
    actions: [{ label: 'Done', kind: 'primary', value: true }],
  });
}

// ---- versions -------------------------------------------------------------------------

export async function versionsDialog(entry, ctx, onChange) {
  const body = h('div.versions', spinner());
  async function load() {
    try {
      const [{ versions }, info] = await Promise.all([api.get(`/api/files/${entry.id}/versions`), api.get(`/api/files/${entry.id}`)]);
      const cur = info.entry;
      clear(body).appendChild(h('div.list.panel',
        h('div.list-item', icon('check', 'sm'),
          h('div.grow', h('div', 'Current version'), h('div.small.muted', `${dateTime(cur.modified_at)} · ${bytes(cur.size)}`)),
          h('a.btn.sm', { href: fileUrl(entry.id), download: entry.name }, icon('download', 'sm'), 'Download')),
        versions.length
          ? versions.map((v) => h('div.list-item', icon('history', 'sm'),
            h('div.grow',
              h('div', { title: dateTime(v.created_at) }, relTime(v.created_at), v.label ? ` · ${v.label}` : ''),
              h('div.small.muted', `${dateTime(v.created_at)} · ${bytes(v.size)}${v.author_id === ctx.me.user.id ? ' · by you' : ''}`)),
            h('a.btn.sm.ghost', { href: `/api/files/${entry.id}/versions/${v.id}`, download: entry.name, 'aria-label': `Download version from ${dateTime(v.created_at)}` }, icon('download', 'sm')),
            (entry.permissions & PERM.UPDATE) ? h('button.btn.sm', {
              type: 'button',
              onclick: async (e) => {
                e.currentTarget.disabled = true;
                try {
                  await api.post(`/api/files/${entry.id}/versions/${v.id}/restore`);
                  toast('Version restored; the previous content was kept as a version', 'ok');
                  if (onChange) onChange();
                  load();
                } catch (ex) { errorToast(ex); }
              },
            }, 'Restore') : null))
          : h('div.list-item.muted.small', 'No earlier versions yet. Every save keeps the previous content here.')));
    } catch (e) {
      clear(body).appendChild(h('p.error-text', e.message));
    }
  }
  load();
  await modal({ title: `Versions of “${entry.name}”`, body, actions: [{ label: 'Close', value: true }] });
}

// ---- folder picker --------------------------------------------------------------------

/** Lets the user choose a folder. Resolves with its id, or null. `exclude` can't be entered. */
export function pickFolder(ctx, { title = 'Choose a folder', confirm = 'Select', start, exclude } = {}) {
  let cur = start || ctx.me.root_id;
  const host = h('div.fl-picker', spinner());
  const label = h('span');
  async function show(id) {
    cur = id;
    clear(host).appendChild(spinner());
    try {
      const [info, { entries }] = await Promise.all([api.get(`/api/files/${id}`), api.get(`/api/files/${id}/children`)]);
      const crumbs = [{ id: ctx.me.root_id, name: 'All files' }, ...info.breadcrumbs];
      const dirs = entries.filter((e) => e.kind === 'dir');
      label.textContent = info.breadcrumbs.length ? info.breadcrumbs[info.breadcrumbs.length - 1].name : 'All files';
      clear(host).append(
        h('nav.fl-crumbs.small', { 'aria-label': 'Folder path' }, crumbs.map((c, i) => [
          i ? icon('chevron-right', 'sm') : null,
          i === crumbs.length - 1 ? h('strong', c.name) : h('button.fl-link', { type: 'button', onclick: () => show(c.id) }, c.name),
        ])),
        h('div.list.panel.fl-picker-list', dirs.length
          ? dirs.map((d) => h(`button.list-item.clickable${d.id === exclude ? '.disabled' : ''}`, {
            type: 'button', disabled: d.id === exclude, onclick: () => show(d.id),
          }, icon('folder', 'fl-tone-dir'), h('span.grow.ellipsis', d.name), d.shared_by && h('span.pill.info', 'Shared'), icon('chevron-right', 'sm')))
          : h('div.list-item.muted.small', 'No folders here')));
    } catch (e) {
      clear(host).appendChild(h('p.error-text', e.message));
    }
  }
  show(cur);
  return modal({
    title,
    body: host,
    actions: [{ label: 'Cancel', value: null }, { label: confirm, kind: 'primary', run: () => cur }],
  });
}

// ---- Open with (Craft apps) -----------------------------------------------------------

export async function openWithDialog(entry, ctx) {
  const body = h('div.openwith', spinner());
  const go = (href) => (e) => { e.preventDefault(); closeFn(); ctx.navigate(href); };
  let closeFn = () => {};
  const promise = modal({
    title: `Open “${entry.name}” with`,
    wide: true,
    body: (close) => { closeFn = close; return body; },
    actions: [{ label: 'Close', value: true }],
  });
  try {
    const [ow, info] = await Promise.all([api.get(`/api/files/${entry.id}/open-with`), api.get(`/api/files/${entry.id}`)]);
    const dav = `${ctx.config.public_url}/remote.php/dav/files/${encodeURIComponent(ctx.me.user.username)}/`;
    append(clear(body), [
      ow.live_editor ? h('div.openwith-app.card.pad',
        h('div.row', icon(fileIcon({ kind: 'file', live_editor: ow.live_editor }), 'fl-tone-text'),
          h('div.grow', h('strong', 'Ferrite live editor'), h('div.small.muted', 'Edit together in the browser, in real time.')),
          h('a.btn.primary', { href: `/edit/${entry.id}`, onclick: go(`/edit/${entry.id}`) }, 'Open'))) : null,
      ow.apps.length ? h('div.openwith-apps', ow.apps.map((a) => h('div.openwith-app.card.pad',
        h('div.row.wrap',
          h('span.fl-craft-mark', icon('craft')),
          h('div.grow', h('div.row.wrap', { style: { gap: '6px' } }, h('strong', a.name), a.default && h('span.pill.info', 'Default')),
            h('div.small.muted', `Replaces ${a.replaces}`)),
          a.installed_on_server ? h('span.pill.ok', icon('check', 'sm'), 'Installed on server') : h('span.pill', 'Not on server'),
          /^https:\/\//.test(a.repo) && h('a.btn.sm.ghost', { href: a.repo, target: '_blank', rel: 'noopener noreferrer' }, icon('external', 'sm'), 'Get the app')))))
        : h('p.muted', 'No Craft app opens this file type.'),
      ow.apps.length ? h('div.openwith-ways',
        h('div.card.pad',
          h('h3', h('span.fl-step', '1'), 'On your computer'),
          h('p.small', 'Install the desktop app, then sync your Ferrite files with any WebDAV client (or mount them). Open ',
            h('code', info.path), ' from the synced folder: saves come back to Ferrite as new versions, and the file is locked while the app edits it.'),
          copyField(dav, 'WebDAV URL')),
        h('div.card.pad',
          h('h3', h('span.fl-step', '2'), 'On the server, with the assistant'),
          h('p.small', 'When the app is installed on the server, the assistant (or any MCP client connected to Ferrite) can open the file with the ',
            h('code', 'craft_open'), ' tool and work inside it using the app’s own tools. For example: ',
            h('em', `“Open ${info.path} in ${ow.apps.find((a) => a.default)?.name || ow.apps[0].name} and …”`)),
          h('a.btn.sm', { href: '/assistant', onclick: go('/assistant') }, icon('assistant', 'sm'), 'Ask the assistant'))) : null]);
  } catch (e) {
    clear(body).appendChild(h('p.error-text', e.message));
  }
  return promise;
}

// ---- preview --------------------------------------------------------------------------

export async function previewDialog(entry) {
  const kind = previewKind(entry);
  const src = fileUrl(entry.id, true);
  let content;
  if (kind === 'image') content = h('img.fl-preview-img', { src, alt: entry.name });
  else if (kind === 'video') content = h('video.fl-preview-media', { src, controls: true, preload: 'metadata' });
  else if (kind === 'audio') content = h('audio', { src, controls: true, preload: 'metadata', style: { width: '100%' } });
  else if (kind === 'pdf') content = h('iframe.fl-preview-pdf', { src, title: entry.name });
  else {
    content = h('pre.fl-preview-text', 'Loading…');
    api.text(`/api/files/${entry.id}/content`)
      .then((t) => { content.textContent = t.length > 400000 ? `${t.slice(0, 400000)}\n…` : t; })
      .catch((e) => { content.textContent = e.message; });
  }
  await modal({
    title: entry.name,
    wide: true,
    body: h('div.fl-preview', content, h('div.small.muted', { style: { marginTop: '8px' } }, `${bytes(entry.size)} · modified ${dateTime(entry.modified_at)}`)),
    actions: [
      { label: 'Download', run: () => { download(fileUrl(entry.id), entry.name); } },
      { label: 'Close', kind: 'primary', value: true },
    ],
  });
}

// ---- sync client ----------------------------------------------------------------------

export async function syncDialog(ctx) {
  const dav = `${ctx.config.public_url}/remote.php/dav/files/${encodeURIComponent(ctx.me.user.username)}/`;
  await modal({
    title: 'Connect a sync client',
    body: (close) => h('div.fl-sync-help',
      h('p', 'Ferrite speaks WebDAV, so desktop sync clients, file managers and mobile apps can work with your files directly.'),
      h('label', 'WebDAV URL'), copyField(dav, 'WebDAV URL'),
      h('ol.small',
        h('li', 'In your client (the Nextcloud desktop or mobile app, rclone, macOS Finder “Connect to Server”, Windows “Map network drive”, GNOME Files…) add a WebDAV account with the URL above.'),
        h('li', 'Sign in with your username ', h('code', ctx.me.user.username), ' and an app token as the password.'),
        h('li', 'Edits sync back as new versions; files open in a desktop Craft app are locked for others while you work.')),
      h('a.btn', { href: '/settings/tokens', onclick: (e) => { e.preventDefault(); close(); ctx.navigate('/settings/tokens'); } }, icon('key', 'sm'), 'Create an app token')),
    actions: [{ label: 'Done', kind: 'primary', value: true }],
  });
}
