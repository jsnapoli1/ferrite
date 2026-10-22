// Files: folder browsing with breadcrumbs and a sortable list, uploads (button and drag and
// drop anywhere), new folders and live documents, per-entry actions (context menu and row
// menu), and the side sections Recent, Favorites, Shared with me, Shared by me and Trash.
//
// Routes: /files (your root), /files/:id (a folder), ?select=<id> highlights an entry,
// ?view=recent|favorites|shared|outgoing|trash picks a section, ?node=<id> (links from
// notifications) jumps to a node.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, append, clear, icon, fileIcon, menu, toast, errorToast, confirmDialog, promptDialog, bytes, relTime, dateTime, spinner, emptyState, debounce, copyText } from '../ui.js';
import { PERM, asEntry, previewKind, fileUrl, download, iconTone, uploadFiles, filePicker, windowDropZone } from './files-common.js';
import { shareDialog, versionsDialog, pickFolder, openWithDialog, previewDialog, syncDialog } from './files-dialogs.js';

const SECTIONS = [
  ['', 'All files', 'files'],
  ['recent', 'Recent', 'history'],
  ['favorites', 'Favorites', 'star'],
  ['shared', 'Shared with me', 'users'],
  ['outgoing', 'Shared by me', 'share'],
  ['trash', 'Trash', 'trash'],
];

const DOC_KINDS = [
  ['text', 'Text document', 'doc', 'Untitled document'],
  ['sheet', 'Spreadsheet', 'sheet', 'Untitled sheet'],
  ['mindmap', 'Mind map', 'mindmap', 'Untitled mind map'],
  ['canvas', 'Canvas', 'canvas', 'Untitled canvas'],
];

const SORT_KEY = 'ferrite-files-sort';

function loadSort() {
  try {
    const s = JSON.parse(localStorage.getItem(SORT_KEY));
    if (s && ['name', 'size', 'modified'].includes(s.key)) return s;
  } catch (_) { /* default */ }
  return { key: 'name', dir: 1 };
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

function sortEntries(list, { key, dir }) {
  return [...list].sort((a, b) => {
    // Folders stay on top whichever way the list is sorted.
    if ((a.kind === 'dir') !== (b.kind === 'dir')) return a.kind === 'dir' ? -1 : 1;
    let c = 0;
    if (key === 'size') c = (a.size || 0) - (b.size || 0);
    else if (key === 'modified') c = new Date(a.modified_at) - new Date(b.modified_at);
    if (!c) c = collator.compare(a.name, b.name);
    return c * dir;
  });
}

export default async function render(root, ctx) {
  const me = ctx.me;
  const section = ctx.query.view || '';

  // A notification links to /files?node=<id>: show the node in its folder.
  if (ctx.query.node && !ctx.params.id) {
    try {
      const { entry } = await api.get(`/api/files/${encodeURIComponent(ctx.query.node)}`);
      const target = entry.kind === 'dir' ? `/files/${entry.id}` : `/files/${entry.parent_id}?select=${entry.id}`;
      if (entry.kind !== 'dir' && entry.shared_by) ctx.navigate('/files?view=shared', { replace: true });
      else ctx.navigate(target, { replace: true });
      return null;
    } catch (_) { /* fall through to the root */ }
  }

  const state = {
    folderId: ctx.params.id || me.root_id,
    folder: null,
    crumbs: [],
    entries: [],
    sort: loadSort(),
    selected: ctx.query.select || null,
    quota: me.quota,
  };
  const cleanups = [];

  // ---- layout ----
  const content = h('section.files-main', { 'aria-live': 'polite' });
  const quotaBox = h('div.files-quota');
  const sideNav = h('nav.files-sections', { 'aria-label': 'File sections' }, SECTIONS.map(([key, label, ic]) => {
    const href = key ? `/files?view=${key}` : '/files';
    return h(`a${key === section ? '.active' : ''}`, {
      href, 'aria-current': key === section ? 'page' : null,
      onclick: (e) => { e.preventDefault(); ctx.navigate(href); },
    }, icon(ic), h('span', label));
  }));
  const side = h('aside.files-side',
    sideNav,
    h('div.files-side-foot',
      quotaBox,
      h('button.btn.sm.ghost.fl-sync-btn', { type: 'button', onclick: () => syncDialog(ctx) }, icon('globe', 'sm'), 'Connect a sync client')));
  root.appendChild(h('div.files-page', side, content));

  function renderQuota() {
    const { used, limit } = state.quota || { used: 0, limit: null };
    append(clear(quotaBox), [
      h('div.small', h('strong', bytes(used)), limit ? ` of ${bytes(limit)} used` : ' used'),
      limit ? h('div.progress', { role: 'progressbar', 'aria-label': 'Storage used', 'aria-valuenow': String(Math.round((used / limit) * 100)), 'aria-valuemin': '0', 'aria-valuemax': '100' },
        h('div', { style: { width: `${Math.min(100, (used / limit) * 100)}%`, background: used / limit > 0.9 ? 'var(--danger)' : null } })) : null]);
  }
  async function refreshQuota() {
    try { state.quota = (await api.get('/api/files/quota')).quota; renderQuota(); } catch (_) { /* keep old value */ }
  }
  renderQuota();
  refreshQuota();

  // ---- actions ----
  const reload = () => load().catch(errorToast);

  function openEntry(e) {
    if (e.kind === 'dir') return ctx.navigate(`/files/${e.id}`);
    if (e.live_editor) return ctx.navigate(`/edit/${e.id}`);
    if (previewKind(e)) return previewDialog(e);
    return download(fileUrl(e.id), e.name);
  }

  async function rename(e) {
    const name = await promptDialog(`Rename ${e.kind === 'dir' ? 'folder' : 'file'}`, { label: 'New name', value: e.name, confirm: 'Rename' });
    if (!name || name === e.name) return;
    try {
      await api.patch(`/api/files/${e.id}`, { name });
      state.selected = e.id;
      reload();
    } catch (ex) { errorToast(ex); }
  }

  async function moveOrCopy(e, copy) {
    const dest = await pickFolder(ctx, {
      title: `${copy ? 'Copy' : 'Move'} “${e.name}” to…`,
      confirm: copy ? 'Copy here' : 'Move here',
      start: e.parent_id || me.root_id,
      exclude: e.kind === 'dir' ? e.id : null,
    });
    if (!dest) return;
    try {
      if (copy) await api.post(`/api/files/${e.id}/copy`, { parent_id: dest });
      else await api.patch(`/api/files/${e.id}`, { parent_id: dest });
      toast(copy ? 'Copied' : 'Moved', 'ok', { timeout: 2500, action: { label: 'Show', run: () => ctx.navigate(`/files/${dest}`) } });
      reload();
      refreshQuota();
    } catch (ex) { errorToast(ex); }
  }

  async function toggleFavorite(e) {
    try {
      await api.post(`/api/files/${e.id}/favorite`, { favorite: !e.favorite });
      e.favorite = !e.favorite;
      reload();
    } catch (ex) { errorToast(ex); }
  }

  async function remove(e) {
    try {
      await api.del(`/api/files/${e.id}`);
      toast(`Moved “${e.name}” to the trash`, 'info', {
        action: e.shared_by ? null : {
          label: 'Undo',
          run: async () => { try { await api.post(`/api/trash/${e.id}/restore`); reload(); } catch (ex) { errorToast(ex); } },
        },
      });
      reload();
      refreshQuota();
    } catch (ex) { errorToast(ex); }
  }

  async function toggleLock(e) {
    try {
      if (e.lock) await api.del(`/api/files/${e.id}/lock`);
      else await api.post(`/api/files/${e.id}/lock`, { app: 'manual', ttl_secs: 24 * 3600 });
      toast(e.lock ? 'Unlocked' : 'Locked for 24 hours: others can read but not change it', 'ok', { timeout: 2500 });
      reload();
    } catch (ex) { errorToast(ex); }
  }

  function actionsFor(e, { inFolder = true } = {}) {
    const p = e.permissions ?? PERM.ALL;
    const file = e.kind !== 'dir';
    return [
      { label: file && !e.live_editor && !previewKind(e) ? 'Download' : 'Open', icon: file ? 'eye' : 'folder', run: () => openEntry(e) },
      file && { label: 'Open with…', icon: 'craft', run: () => openWithDialog(e, ctx) },
      file && { label: 'Download', icon: 'download', run: () => download(fileUrl(e.id), e.name) },
      !inFolder && !e.shared_by && e.parent_id && { label: 'Show in folder', icon: 'files', run: () => ctx.navigate(`/files/${e.parent_id}?select=${e.id}`) },
      'sep',
      (p & PERM.UPDATE) && { label: 'Rename', icon: 'edit', run: () => rename(e) },
      (p & PERM.DELETE) && { label: 'Move', icon: 'move', run: () => moveOrCopy(e, false) },
      { label: 'Copy', icon: 'copy', run: () => moveOrCopy(e, true) },
      { label: e.favorite ? 'Remove from favorites' : 'Add to favorites', icon: 'star', run: () => toggleFavorite(e) },
      (p & PERM.SHARE) && { label: 'Share', icon: 'share', run: () => shareDialog(e, ctx).then(reload) },
      file && { label: 'Versions', icon: 'history', run: () => versionsDialog(e, ctx, reload) },
      file && (p & PERM.UPDATE) && { label: e.lock ? 'Unlock' : 'Lock', icon: 'lock', run: () => toggleLock(e) },
      (p & PERM.DELETE) && 'sep',
      (p & PERM.DELETE) && { label: 'Delete', icon: 'trash', danger: true, run: () => remove(e) },
    ].filter(Boolean).filter((it, i, arr) => it !== 'sep' || (i > 0 && arr[i - 1] !== 'sep'));
  }

  // ---- list rendering ----
  function badges(e) {
    const out = [];
    if (e.favorite) out.push(h('span.fl-fav', { title: 'Favorite', 'aria-label': 'Favorite' }, icon('star', 'sm')));
    if (e.shared_by) out.push(h('span.pill.info', { title: `Shared with you by ${e.shared_by}` }, icon('users', 'sm'), e.shared_by));
    if (e.lock) {
      const who = e.lock.owner_app === 'manual' ? 'Locked' : `Open in ${e.lock.owner_app}`;
      out.push(h('span.pill.warn', { title: `${who} until ${dateTime(e.lock.expires_at)}` }, icon('lock', 'sm'), who));
    }
    if (e.craft_app) out.push(h('span.pill', { title: 'Opens in a Craft app' }, icon('craft', 'sm'), e.craft_app.replace(/craft$/, 'Craft').replace(/^./, (c) => c.toUpperCase())));
    return out;
  }

  function sortHeader(key, label, cls = '') {
    const active = state.sort.key === key;
    return h(`th${cls}`, { 'aria-sort': active ? (state.sort.dir > 0 ? 'ascending' : 'descending') : 'none' },
      h('button.fl-sort', {
        type: 'button',
        onclick: () => {
          state.sort = active ? { key, dir: -state.sort.dir } : { key, dir: key === 'name' ? 1 : -1 };
          try { localStorage.setItem(SORT_KEY, JSON.stringify(state.sort)); } catch (_) { /* private mode */ }
          renderContent();
        },
      }, label, active ? icon('chevron-down', state.sort.dir > 0 ? 'sm flip' : 'sm') : null));
  }

  /** The file table. `opts.path(e)` adds a secondary line, `opts.inFolder` tunes actions. */
  function fileTable(entries, opts = {}) {
    const rows = sortEntries(entries, state.sort).map((e) => {
      const menuBtn = h('button.btn.ghost.icon.sm', {
        type: 'button', 'aria-label': `Actions for ${e.name}`, 'aria-haspopup': 'menu',
        onclick: (ev) => { ev.stopPropagation(); menu(menuBtn, actionsFor(e, opts)); },
      }, icon('more'));
      const tr = h(`tr.fl-row${e.id === state.selected ? '.selected' : ''}`, {
        tabindex: '0', dataset: { id: e.id },
        onclick: (ev) => { if (!ev.target.closest('button, a')) openEntry(e); },
        oncontextmenu: (ev) => { ev.preventDefault(); menu(ev, actionsFor(e, opts)); },
        onkeydown: (ev) => {
          if (ev.target !== tr) return;
          if (ev.key === 'Enter') openEntry(e);
          else if (ev.key === 'ContextMenu' || (ev.shiftKey && ev.key === 'F10')) { ev.preventDefault(); menu(menuBtn, actionsFor(e, opts)); }
          else if (ev.key === 'Delete' && (e.permissions & PERM.DELETE)) remove(e);
          else if (ev.key === 'F2' && (e.permissions & PERM.UPDATE)) rename(e);
          else if (ev.key === 'ArrowDown') { ev.preventDefault(); tr.nextElementSibling?.focus(); }
          else if (ev.key === 'ArrowUp') { ev.preventDefault(); tr.previousElementSibling?.focus(); }
        },
      },
      h('td.col-name',
        h('div.fl-name',
          h(`span.fl-icon.${iconTone(e)}`, icon(fileIcon(e))),
          h('div.grow',
            h('div.row.fl-title', h('span.ellipsis', { title: e.name }, e.name), badges(e)),
            opts.path ? h('div.small.muted.ellipsis', opts.path(e)) : null))),
      h('td.col-size.muted', e.kind === 'dir' ? '—' : bytes(e.size)),
      h('td.col-mod.muted', { title: dateTime(e.modified_at) }, relTime(e.modified_at)),
      h('td.col-act', menuBtn));
      return tr;
    });
    return h('div.panel.files-table-wrap', h('table.table.files-table',
      h('thead', h('tr', sortHeader('name', 'Name', '.col-name'), sortHeader('size', 'Size', '.col-size'), sortHeader('modified', 'Modified', '.col-mod'), h('th.col-act', h('span.sr-only', 'Actions')))),
      h('tbody', rows)));
  }

  // ---- toolbar for folders ----
  const picker = filePicker((files) => doUpload(files));
  root.appendChild(picker);

  function canCreate() {
    return !section && state.folder && (state.folder.permissions & PERM.CREATE);
  }

  async function doUpload(files) {
    if (!canCreate()) { toast('You can’t upload here', 'warn'); return; }
    const target = state.folderId;
    const n = await uploadFiles(files, { parentId: target, onEach: (node) => { if (node) state.selected = node.id; } });
    if (n) { if (target === state.folderId) reload(); refreshQuota(); }
  }
  cleanups.push(windowDropZone(doUpload, () => !!canCreate()));

  async function newFolder() {
    const name = await promptDialog('New folder', { label: 'Folder name', value: 'New folder', confirm: 'Create' });
    if (!name) return;
    try {
      const { node } = await api.post(`/api/files/${state.folderId}/mkdir`, { name });
      state.selected = node.id;
      reload();
    } catch (ex) { errorToast(ex); }
  }

  async function newDocument(kind, label, placeholder) {
    const title = await promptDialog(`New ${label.toLowerCase()}`, { label: 'Title', value: placeholder, confirm: 'Create' });
    if (!title) return;
    try {
      const { node } = await api.post(`/api/files/${state.folderId}/documents`, { title, kind });
      ctx.navigate(`/edit/${node.id}`);
    } catch (ex) { errorToast(ex); }
  }

  function crumbsNav() {
    const items = [];
    if (state.folder?.shared_by) items.push({ label: 'Shared with me', href: '/files?view=shared' });
    else items.push({ label: 'All files', href: '/files' });
    state.crumbs.forEach((c) => items.push({ label: c.name, href: `/files/${c.id}` }));
    return h('nav.fl-crumbs', { 'aria-label': 'Breadcrumbs' }, h('ol', items.map((it, i) => h('li',
      i ? icon('chevron-right', 'sm') : null,
      i === items.length - 1
        ? h('span', { 'aria-current': 'page' }, it.label)
        : h('a', { href: it.href, onclick: (e) => { e.preventDefault(); ctx.navigate(it.href); } }, it.label)))));
  }

  function folderToolbar() {
    const writable = canCreate();
    const newBtn = h('button.btn', {
      type: 'button', 'aria-haspopup': 'menu', disabled: !writable,
      onclick: () => menu(newBtn, DOC_KINDS.map(([kind, label, ic, ph]) => ({ label, icon: ic, run: () => newDocument(kind, label, ph) }))),
    }, icon('doc', 'sm'), h('span.fl-hide-sm', 'New document'), icon('chevron-down', 'sm'));
    return h('div.files-toolbar',
      crumbsNav(),
      h('div.actions',
        h('button.btn', { type: 'button', onclick: newFolder, disabled: !writable }, icon('folder', 'sm'), h('span.fl-hide-sm', 'New folder')),
        newBtn,
        h('button.btn.primary', { type: 'button', onclick: () => picker.click(), disabled: !writable }, icon('upload', 'sm'), 'Upload')));
  }

  // ---- sections ----
  function head(title, actions) {
    return h('div.files-toolbar', h('h1', title), actions ? h('div.actions', actions) : null);
  }

  async function loadFolder() {
    const [info, { entries }] = await Promise.all([
      api.get(`/api/files/${state.folderId}`),
      api.get(`/api/files/${state.folderId}/children`),
    ]);
    if (info.entry.kind !== 'dir') {
      ctx.navigate(`/files/${info.entry.parent_id}?select=${info.entry.id}`, { replace: true });
      return null;
    }
    state.folder = info.entry;
    state.crumbs = info.breadcrumbs;
    state.entries = entries;
    ctx.setTitle(state.crumbs.length ? state.crumbs[state.crumbs.length - 1].name : 'Files');
    const body = entries.length ? fileTable(entries) : h('div.panel.files-empty', emptyState('folder', 'This folder is empty',
      canCreate() ? 'Drop files anywhere on this page, or use Upload.' : 'Nothing has been added here yet.',
      canCreate() ? h('button.btn.primary', { type: 'button', onclick: () => picker.click() }, icon('upload', 'sm'), 'Upload files') : null));
    return [folderToolbar(), body];
  }

  async function loadRecent() {
    ctx.setTitle('Recent files');
    const { files } = await api.get('/api/files/recent');
    const paths = new Map(files.map((f) => [f.node.id, f.path]));
    const entries = files.map((f) => asEntry(f.node));
    return [head('Recent'), entries.length
      ? fileTable(entries, { inFolder: false, path: (e) => paths.get(e.id) })
      : h('div.panel', emptyState('history', 'No recent files', 'Files you change show up here.'))];
  }

  async function loadFavorites() {
    ctx.setTitle('Favorites');
    const { files } = await api.get('/api/files/favorites');
    const entries = files.map((n) => asEntry(n));
    return [head('Favorites'), entries.length
      ? fileTable(entries, { inFolder: false })
      : h('div.panel', emptyState('star', 'No favorites yet', 'Use “Add to favorites” on any file or folder to pin it here.'))];
  }

  async function loadShared() {
    ctx.setTitle('Shared with me');
    const { shares } = await api.get('/api/shares/incoming');
    const ids = [...new Set(shares.map((s) => s.node_id))];
    // Share views lack the sharer and live permissions; the node info has both.
    const infos = await Promise.all(ids.map((id) => api.get(`/api/files/${id}`).catch(() => null)));
    const entries = infos.filter(Boolean).map((i) => i.entry);
    return [head('Shared with me'), entries.length
      ? fileTable(entries, { inFolder: false, path: (e) => `Shared by ${e.shared_by || 'someone'}` })
      : h('div.panel', emptyState('users', 'Nothing shared with you yet', 'Files and folders people share with you, your groups or your conversations appear here.'))];
  }

  async function loadOutgoing() {
    ctx.setTitle('Shared by me');
    const { shares } = await api.get('/api/shares/outgoing');
    const KIND = { user: ['user', 'Person'], group: ['users', 'Group'], room: ['talk', 'Conversation'], link: ['link', 'Public link'] };
    const level = (p) => (p & PERM.UPDATE ? 'Can edit' : p & PERM.CREATE ? 'View and upload' : 'Can view');
    const rows = shares.map((s) => {
      const [ic, kindLabel] = KIND[s.kind] || ['share', s.kind];
      const node = { id: s.node_id, name: s.node_name, kind: s.node_kind, permissions: PERM.ALL };
      const manage = async () => { await shareDialog(node, ctx); reload(); };
      const reveal = async () => {
        try {
          const { entry } = await api.get(`/api/files/${s.node_id}`);
          ctx.navigate(entry.kind === 'dir' ? `/files/${entry.id}` : `/files/${entry.parent_id}?select=${entry.id}`);
        } catch (ex) { errorToast(ex); }
      };
      return h('tr.fl-row', { tabindex: '0', onclick: (e) => { if (!e.target.closest('button, a')) reveal(); }, onkeydown: (e) => { if (e.key === 'Enter' && e.target === e.currentTarget) reveal(); } },
        h('td.col-name', h('div.fl-name',
          h(`span.fl-icon.${s.node_kind === 'dir' ? 'fl-tone-dir' : 'fl-tone-file'}`, icon(s.node_kind === 'dir' ? 'folder' : 'file')),
          h('div.grow', h('div.ellipsis', s.node_name),
            h('div.small.muted.row', { style: { gap: '6px' } }, icon(ic, 'sm'), s.kind === 'link' ? kindLabel : `${kindLabel}: ${s.target_name || '—'}`,
              s.has_password ? h('span.pill', icon('lock', 'sm'), 'Password') : null,
              s.expires_at ? h('span.pill.warn', `Expires ${dateTime(s.expires_at)}`) : null)))),
        h('td.col-size.muted', level(s.permissions)),
        h('td.col-mod.muted', { title: dateTime(s.created_at) }, relTime(s.created_at)),
        h('td.col-act', h('div.row', { style: { gap: '2px', justifyContent: 'flex-end' } },
          s.url ? h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Copy link to ${s.node_name}`, title: 'Copy link', onclick: () => copyText(s.url, 'Link') }, icon('copy', 'sm')) : null,
          h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Manage sharing of ${s.node_name}`, title: 'Manage', onclick: manage }, icon('settings', 'sm')),
          h('button.btn.ghost.icon.sm', {
            type: 'button', 'aria-label': `Stop sharing ${s.node_name}`, title: 'Stop sharing',
            onclick: async () => { try { await api.del(`/api/shares/${s.id}`); reload(); } catch (ex) { errorToast(ex); } },
          }, icon('x', 'sm')))));
    });
    return [head('Shared by me'), rows.length
      ? h('div.panel.files-table-wrap', h('table.table.files-table',
        h('thead', h('tr', h('th.col-name', 'Shared item'), h('th.col-size', 'Access'), h('th.col-mod', 'Since'), h('th.col-act', h('span.sr-only', 'Actions')))),
        h('tbody', rows)))
      : h('div.panel', emptyState('share', 'You haven’t shared anything', 'Use Share on a file or folder to give people access or create a public link.'))];
  }

  async function loadTrash() {
    ctx.setTitle('Trash');
    const { items } = await api.get('/api/trash');
    const restore = async (n) => {
      try {
        const { node } = await api.post(`/api/trash/${n.id}/restore`);
        toast(`Restored “${n.name}”`, 'ok', { action: { label: 'Show', run: () => ctx.navigate(`/files/${node.parent_id}?select=${node.id}`) } });
        reload();
        refreshQuota();
      } catch (ex) { errorToast(ex); }
    };
    const purge = async (n) => {
      if (!(await confirmDialog('Delete forever?', `“${n.name}” will be permanently deleted. This can’t be undone.`, { confirm: 'Delete forever', danger: true }))) return;
      try { await api.del(`/api/trash/${n.id}`); reload(); refreshQuota(); } catch (ex) { errorToast(ex); }
    };
    const empty = async () => {
      if (!(await confirmDialog('Empty the trash?', `All ${items.length} item${items.length === 1 ? '' : 's'} will be permanently deleted.`, { confirm: 'Empty trash', danger: true }))) return;
      try { const r = await api.del('/api/trash'); toast(`Deleted ${r.purged} item${r.purged === 1 ? '' : 's'} forever`, 'ok'); reload(); refreshQuota(); } catch (ex) { errorToast(ex); }
    };
    const rows = items.map((n) => h('tr.fl-row',
      h('td.col-name', h('div.fl-name', h(`span.fl-icon.${iconTone(asEntry(n))}`, icon(fileIcon(asEntry(n)))), h('div.grow.ellipsis', { title: n.name }, n.name))),
      h('td.col-size.muted', n.kind === 'dir' ? '—' : bytes(n.size)),
      h('td.col-mod.muted', { title: dateTime(n.trashed_at) }, relTime(n.trashed_at)),
      h('td.col-act', h('div.row', { style: { gap: '4px', justifyContent: 'flex-end' } },
        h('button.btn.sm', { type: 'button', onclick: () => restore(n), 'aria-label': `Restore ${n.name}` }, icon('refresh', 'sm'), h('span.fl-hide-sm', 'Restore')),
        h('button.btn.sm.ghost.danger', { type: 'button', onclick: () => purge(n), 'aria-label': `Delete ${n.name} forever`, title: 'Delete forever' }, icon('trash', 'sm'))))));
    return [
      head('Trash', items.length ? h('button.btn.danger', { type: 'button', onclick: empty }, icon('trash', 'sm'), 'Empty trash') : null),
      h('p.small.muted.files-note', 'Deleted items stay here for 30 days before they are removed for good.'),
      rows.length
        ? h('div.panel.files-table-wrap', h('table.table.files-table.trash-table',
          h('thead', h('tr', h('th.col-name', 'Name'), h('th.col-size', 'Size'), h('th.col-mod', 'Deleted'), h('th.col-act', h('span.sr-only', 'Actions')))),
          h('tbody', rows)))
        : h('div.panel', emptyState('trash', 'Trash is empty', 'Deleted files and folders can be restored from here.')),
    ];
  }

  const LOADERS = { '': loadFolder, recent: loadRecent, favorites: loadFavorites, shared: loadShared, outgoing: loadOutgoing, trash: loadTrash };
  let lastNodes = null;

  function renderContent() {
    if (!lastNodes) return;
    // Re-render folder tables (sort changes) from the cached state without refetching.
    if (!section) {
      const scroll = root.closest('.main')?.scrollTop;
      clear(content).append(folderToolbar(), state.entries.length ? fileTable(state.entries) : lastNodes[1]);
      if (scroll !== undefined) root.closest('.main').scrollTop = scroll;
    } else {
      load();
    }
  }

  let loadSeq = 0;
  async function load() {
    const seq = ++loadSeq;
    const nodes = await (LOADERS[section] || loadFolder)();
    if (seq !== loadSeq || !nodes) return;
    lastNodes = nodes;
    clear(content).append(...nodes);
    if (state.selected) {
      const row = content.querySelector(`tr[data-id="${CSS.escape(state.selected)}"]`);
      if (row) {
        row.classList.add('selected');
        row.scrollIntoView({ block: 'nearest' });
      }
    }
  }

  content.appendChild(spinner());
  await load();

  // Live refresh: changes in the open folder (or anywhere, for the other sections).
  const liveReload = debounce(reload, 300);
  cleanups.push(live.on('files.changed', (ev) => {
    if (section ? true : (ev.parent_id === state.folderId || ev.node_id === state.folderId)) liveReload();
  }));

  return () => cleanups.forEach((fn) => fn());
}
