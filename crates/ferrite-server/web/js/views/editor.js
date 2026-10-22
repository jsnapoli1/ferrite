// Live editor shell (/edit/:id): loads the file, shows the common header (back to the folder,
// name, presence, connection status, read-only state) and mounts the editor for the file's
// kind (editor-text.js, editor-sheet.js, editor-mindmap.js, editor-canvas.js) on a shared
// CollabProvider. When the file is replaced outside the live session the provider is rebuilt.
//
// Editor module contract: `export default function mount(env)` returning a cleanup function,
// env = {body, toolbar, provider, doc, entry, me, readOnly, ctx, setHelp(shortcuts)}.

import { api } from '../api.js';
import { CollabProvider } from '../collab.js';
import { h, clear, icon, fileIcon, avatar, toast, bytes, relTime } from '../ui.js';
import { helpButton } from './editor-common.js';

const STATUS = {
  connecting: ['Connecting…', 'warn'],
  connected: ['Syncing…', 'info'],
  synced: ['Synced', 'ok'],
  offline: ['Offline', 'danger'],
  denied: ['No access', 'danger'],
  reset: ['Reloading…', 'warn'],
};

export default async function render(root, ctx) {
  const id = ctx.params.id;
  const { entry, breadcrumbs } = await api.get(`/api/files/${encodeURIComponent(id)}`);
  ctx.setTitle(entry.name);
  const go = (href) => (e) => { if (e.metaKey || e.ctrlKey) return; e.preventDefault(); ctx.navigate(href); };
  const folderHref = entry.parent_id ? `/files/${entry.parent_id}` : '/files';
  const parentName = breadcrumbs.length > 1 ? breadcrumbs[breadcrumbs.length - 2].name : 'Files';

  if (!entry.live_editor) {
    root.appendChild(h('div.page',
      h('div.empty',
        h('div', icon(fileIcon(entry), 'icon-lg')),
        h('h3', entry.name),
        h('p', 'This file has no live editor. Open it from its folder or download it.'),
        h('div.row', { style: { justifyContent: 'center' } },
          h('a.btn', { href: `${folderHref}?select=${encodeURIComponent(entry.id)}`, onclick: go(`${folderHref}?select=${encodeURIComponent(entry.id)}`) }, icon('folder'), 'Show in folder'),
          entry.kind !== 'dir' && h('a.btn.primary', { href: `/api/files/${entry.id}/download` }, icon('download'), `Download${entry.size != null ? ` (${bytes(entry.size)})` : ''}`)))));
    return null;
  }

  const lockedByOther = entry.lock && entry.lock.user_id !== ctx.me.user.id;
  const readOnly = (entry.permissions & 2) === 0 || !!lockedByOther;

  // ---- header ----
  const statusPill = h('span.pill.editor-status', { role: 'status', 'aria-live': 'polite' });
  const peersEl = h('div.editor-peers', { 'aria-label': 'People editing' });
  const toolbar = h('div.editor-toolbar', { role: 'toolbar', 'aria-label': `${entry.live_editor} tools` });
  const helpHost = h('span.editor-help');
  const nameEl = h('h1.editor-name', { title: entry.name }, entry.name);
  const header = h('div.editor-head',
    h('a.btn.ghost.icon', { href: folderHref, onclick: go(folderHref), 'aria-label': `Back to ${parentName}`, title: `Back to ${parentName}` }, icon('arrow-left')),
    h('span.editor-kind', icon(fileIcon(entry))),
    h('div.editor-title',
      nameEl,
      h('div.editor-crumbs.small.muted', breadcrumbs.slice(0, -1).map((b, i) => [i ? ' / ' : '', h('a', { href: `/files/${b.id}`, onclick: go(`/files/${b.id}`) }, b.name)]),
        ` · edited ${relTime(entry.modified_at)}`)),
    readOnly && h('span.pill.warn', { title: lockedByOther ? 'Locked by another app' : 'You can view but not edit this file' }, icon('lock', 'sm'), lockedByOther ? 'Locked' : 'Read only'),
    h('div.grow'),
    peersEl, statusPill,
    h('a.btn.ghost.sm', { href: `${folderHref}?select=${encodeURIComponent(entry.id)}&versions=1`, onclick: go(`${folderHref}?select=${encodeURIComponent(entry.id)}&versions=1`), title: 'Versions and sharing' }, icon('history', 'sm'), h('span.hide-sm', 'Versions')),
    h('a.btn.ghost.icon.sm', { href: `/api/files/${entry.id}/download`, 'aria-label': 'Download', title: 'Download' }, icon('download', 'sm')),
    helpHost);
  const body = h('div.editor-body', { class: `editor-${entry.live_editor}` });
  const page = h('div.page-full.editor-page', header, toolbar, body);
  root.appendChild(page);

  let provider = null;
  let unmount = null;
  let help = null;
  let generation = 0;

  function setStatus(s) {
    const [label, kind] = STATUS[s] || [s, ''];
    statusPill.className = `pill editor-status ${kind}`;
    statusPill.dataset.status = s;
    clear(statusPill).append(h('span.status-dot'), label);
  }

  function renderPeers(peers) {
    clear(peersEl);
    const seen = new Set();
    for (const p of peers) {
      if (seen.has(p.id)) continue;
      seen.add(p.id);
      const a = avatar(p.name || '?', { size: 'sm', seed: p.id });
      a.style.background = p.color;
      a.title = `${p.name} is here`;
      peersEl.appendChild(a);
    }
    peersEl.hidden = !seen.size;
  }

  function setHelp(shortcuts) {
    if (help) help.cleanup();
    help = helpButton(shortcuts, 'Keyboard shortcuts');
    clear(helpHost).appendChild(help);
  }

  async function start() {
    const gen = ++generation;
    provider = new CollabProvider(id, {
      user: ctx.me.user,
      onStatus: setStatus,
      onPeers: renderPeers,
      onReset: () => {
        if (gen !== generation) return;
        toast('The file was replaced outside this editor — reloaded the latest version.', 'warn');
        restart();
      },
    });
    const mod = await import(`./editor-${entry.live_editor}.js`);
    if (gen !== generation) return;
    clear(body);
    clear(toolbar);
    unmount = mod.default({ body, toolbar, provider, doc: provider.doc, entry, me: ctx.me, readOnly, ctx, setHelp, setStatus });
  }

  function stop() {
    generation++;
    if (unmount) { try { unmount(); } catch (e) { console.error(e); } }
    unmount = null;
    if (provider) provider.destroy();
    provider = null;
  }

  function restart() {
    stop();
    start();
  }

  await start();
  return () => {
    stop();
    if (help) help.cleanup();
  };
}
