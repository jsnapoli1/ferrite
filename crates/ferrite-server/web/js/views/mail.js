// Mail: the user's IMAP/SMTP accounts in three panes (folders, messages, conversation),
// one pane at a time on phones. New mail and changes made elsewhere arrive live over the
// `mail.changed` event (the server watches the inbox with IMAP IDLE while you are online).
//
// The list follows the user's preferences (stored on the server): side-by-side, stacked or
// list-only layout, compact rows, conversations or single messages, newest or oldest first,
// favourites on top. Besides folders it shows views across accounts (all inboxes, the
// priority inbox with Important and Others, favourites, one tag). Rows offer one-click
// actions on hover or focus; tags are IMAP keywords; snoozed mail waits in a Snoozed folder.
//
// Route: /mail?account=<id>&folder=<id>&view=<all|priority|starred|tag:kw>&m=<message id>
// &mid=<Message-ID> (finds a message again after it moved), /mail?mailto=<uri> (composer).
// Keyboard: j/k next/previous, Enter open, r reply, a reply all, f forward, e archive,
// # delete, u unread, s flag, i important, b snooze, t tags, v move, c compose, / search,
// Escape back.

import { api, q } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, menu, toast, errorToast, confirmDialog, promptDialog, bytes, dateTime, spinner, emptyState, debounce, copyText } from '../ui.js';
import { pickFolder } from './files-dialogs.js';
import { accountDialog } from './mail-setup.js';
import { shareDialog, quotaBar, startOAuth, PROVIDER_LABEL } from './mail-accounts.js';
import { openComposer } from './mail-compose.js';
import {
  ROLE_ICONS, depth, folderPath, tagChip, moveDialog, snoozeDialog, tagsDialog, quickActionsDialog, folderStatsDialog,
  advancedSearchDialog, filterSummary, inboxSettingsDialog, stepIcon,
} from './mail-dialogs.js';
import { mailSettingsDialog } from './mail-settings.js';
import { decorateMessage, decorateThread, messageMenuItems, accountMenuItems, navExtras } from './mail-integrations.js';

const PAGE = 50;
const IMPORTANT = '$label1';
const VIEW_TITLES = { all: 'All inboxes', priority: 'Priority inbox', starred: 'Favourites' };
const DEFAULT_PREFS = { layout: 'vertical', compact: false, threaded: false, sort: 'newest', favorites_top: true, show_unsubscribed: false, body_search: true, priority_inbox: true, all_inboxes: true };

/** "14:05" today, "Oct 19" this year, else a short date. */
export function listDate(ts) {
  const d = new Date(ts);
  const now = new Date();
  if (d.toDateString() === now.toDateString()) return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (d.getFullYear() === now.getFullYear()) return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
  return d.toLocaleDateString([], { year: 'numeric', month: 'short', day: 'numeric' });
}

const person = (name, addr) => name || addr || '(unknown sender)';
const addrList = (list) => (list || []).map((a) => (a.name ? `${a.name} <${a.address}>` : a.address)).join(', ');

/** Plain text with links, and quoted lines (">") dimmed. Built from text nodes only. */
function textBody(text) {
  const box = h('div.ml-text');
  const urlRe = /(https?:\/\/[^\s<>"')\]]+)/g;
  for (const line of (text || '').replace(/\r\n/g, '\n').replace(/\s+$/, '').split('\n')) {
    const el = h(line.startsWith('>') ? 'div.ml-quoted' : 'div');
    if (!line) el.appendChild(document.createElement('br'));
    line.split(urlRe).forEach((part, i) => {
      if (i % 2) el.appendChild(h('a', { href: part, target: '_blank', rel: 'noopener noreferrer' }, part));
      else if (part) el.appendChild(document.createTextNode(part));
    });
    box.appendChild(el);
  }
  return box;
}

/** Link to a message that still works after it moved (by Message-ID). */
export function messageLink(m) {
  return `${location.origin}/mail${q({ m: m.id, mid: m.message_id })}`;
}

export default async function render(root, ctx) {
  ctx.setTitle('Mail');
  const state = {
    accounts: [],
    folders: {}, // account id -> folders
    accountId: ctx.query.account || null,
    folderId: ctx.query.folder || null,
    view: ctx.query.view || null, // all | priority | starred | tag:<keyword>
    messageId: ctx.query.m || null,
    mid: ctx.query.mid || null,
    messages: [],
    important: [], // the priority inbox's Important section
    hasMore: false,
    loadingMore: false,
    selected: new Set(),
    unreadOnly: false,
    search: '',
    filter: null, // advanced search criteria
    lastClicked: null,
    thread: [],
    pane: ctx.query.m ? 'reader' : 'list',
    openSeq: 0,
    prefs: { ...DEFAULT_PREFS },
    tags: [],
    quickActions: [],
    snoozed: new Map(), // Message-ID -> until
    canAdd: true,
    dragAccount: null,
  };

  const nav = h('aside.ml-nav', { 'aria-label': 'Mail folders' });
  const listHead = h('div.ml-list-head');
  const listBody = h('div.ml-list-body', { role: 'listbox', 'aria-label': 'Messages', 'aria-multiselectable': 'true', tabindex: '-1' });
  const listPane = h('section.ml-list', listHead, listBody);
  const reader = h('section.ml-reader', { 'aria-live': 'polite' });
  const page = h('div.page-full.ml', nav, listPane, reader, h('div.ml-scrim', { onclick: () => setPane('list') }));
  root.appendChild(page);

  const account = () => state.accounts.find((a) => a.id === state.accountId);
  const folders = () => state.folders[state.accountId] || [];
  const folder = () => (state.view ? null : folders().find((f) => f.id === state.folderId));
  const folderByRole = (role, accountId = state.accountId) => (state.folders[accountId] || []).find((f) => f.role === role);
  const folderById = (id) => Object.values(state.folders).flat().find((f) => f.id === id);
  const tagOf = (kw) => state.tags.find((t) => t.keyword === kw);
  const isImportant = (m) => (m.keywords || []).includes(IMPORTANT);
  const listing = () => !state.search && !state.filter;

  function setUrl() {
    history.replaceState({}, '', `/mail${q({ account: state.view ? null : state.accountId, folder: state.view ? null : state.folderId, view: state.view, m: state.messageId })}`);
  }

  function setPane(p) {
    state.pane = p;
    page.classList.toggle('pane-folders', p === 'folders');
    page.classList.toggle('pane-reader', p === 'reader');
    page.classList.toggle('pane-list', p === 'list');
  }

  function applyLayout() {
    for (const l of ['list', 'vertical', 'horizontal']) page.classList.toggle(`layout-${l}`, state.prefs.layout === l);
    page.classList.toggle('compact', !!state.prefs.compact);
  }
  const singlePane = () => state.prefs.layout === 'list' || matchMedia('(max-width: 760px)').matches;

  function viewTitle(view = state.view) {
    if (!view) return null;
    if (view.startsWith('tag:')) return tagOf(view.slice(4))?.name || view.slice(4);
    return VIEW_TITLES[view] || view;
  }

  // ---- folders pane ---------------------------------------------------------------------
  function dropTarget(f) {
    return {
      ondragover: (e) => { if (f.selectable && e.dataTransfer.types.includes('application/x-ferrite-mail')) { e.preventDefault(); e.currentTarget.classList.add('drop'); } },
      ondragleave: (e) => e.currentTarget.classList.remove('drop'),
      ondrop: (e) => {
        e.preventDefault();
        e.currentTarget.classList.remove('drop');
        const ids = JSON.parse(e.dataTransfer.getData('application/x-ferrite-mail') || '[]');
        if (ids.length) act('move', ids, { folder_id: f.id });
      },
    };
  }

  function viewButton(view, label, iconName, badge, extra = null) {
    const active = state.view === view && listing();
    return h(`button.ml-folder.ml-view${active ? '.active' : ''}`, { type: 'button', dataset: { view }, onclick: () => openView(view) },
      extra || icon(iconName, 'sm'), h('span.grow.ellipsis', label), badge > 0 && h('span.badge.ml-unread', String(badge)));
  }

  function drawNav() {
    clear(nav);
    nav.appendChild(h('div.ml-nav-head',
      h('h1', 'Mail'),
      state.accounts.length > 0 && h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Mail settings', title: 'Mail settings', onclick: openSettings }, icon('settings', 'sm')),
      state.accounts.length > 0 && h('button.btn.primary.sm.ml-compose-btn', { type: 'button', onclick: () => compose(), title: 'Compose (c)' }, icon('edit', 'sm'), h('span', 'Compose'))));
    const tree = h('div.ml-folders');
    if (state.accounts.length) {
      const inboxes = state.accounts.map((a) => folderByRole('inbox', a.id)).filter(Boolean);
      const unread = inboxes.reduce((n, f) => n + (f.unread || 0), 0);
      const views = h('div.ml-views', { role: 'group', 'aria-label': 'Views' });
      if (state.prefs.all_inboxes && state.accounts.length > 1) views.appendChild(viewButton('all', 'All inboxes', 'mail', unread));
      if (state.prefs.priority_inbox) views.appendChild(viewButton('priority', 'Priority inbox', 'important', 0));
      views.appendChild(viewButton('starred', 'Favourites', 'star', 0));
      tree.appendChild(views);
      if (state.tags.length) {
        tree.appendChild(h('div.ml-account', h('span.ml-account-name.ellipsis', 'Tags')));
        for (const t of state.tags) {
          tree.appendChild(viewButton(`tag:${t.keyword}`, t.name, 'tag', 0, h('span.ml-tag-dot.ml-nav-dot', { style: `--tag: ${t.color}` })));
        }
      }
    }
    const many = state.accounts.length > 1;
    state.accounts.forEach((a, index) => {
      const p = a.permissions || {};
      const items = [
        { label: 'Refresh', icon: 'refresh', run: () => refreshAccount(a.id) },
        { label: 'New folder…', icon: 'plus', run: () => newFolder(a, null) },
        p.configure && { label: 'Inbox settings…', icon: 'important', run: () => inboxSettings(a) },
        { label: 'Quick actions…', icon: 'zap', run: () => manageQuickActions(a) },
        p.configure && { label: 'Edit account', icon: 'settings', run: () => editAccount(a) },
        p.share && { label: 'Share…', icon: 'share', run: () => shareDialog(a).then(loadAccounts).catch(errorToast) },
        ...accountMenuItems(a, ctx),
        a.auth_method === 'xoauth2' && p.authenticate && { label: `Reconnect with ${PROVIDER_LABEL[a.oauth_provider] || 'the provider'}`, icon: 'key', run: () => startOAuth(a.oauth_provider, { email: a.email, accountId: a.id }).catch(errorToast) },
        many && index > 0 && { label: 'Move up', icon: 'chevron-left', run: () => moveAccount(a.id, -1) },
        many && index < state.accounts.length - 1 && { label: 'Move down', icon: 'chevron-down', run: () => moveAccount(a.id, 1) },
        p.remove && 'sep',
        p.remove && { label: 'Remove account', icon: 'trash', danger: true, run: () => removeAccount(a) },
      ];
      const head = h(`div.ml-account${a.delegated ? '.shared' : ''}`, {
        dataset: { account: a.id }, draggable: many ? 'true' : null,
        ondragstart: (e) => { state.dragAccount = a.id; e.dataTransfer.setData('application/x-ferrite-account', a.id); e.dataTransfer.effectAllowed = 'move'; },
        ondragend: () => { state.dragAccount = null; },
        ondragover: (e) => { if (state.dragAccount && state.dragAccount !== a.id) { e.preventDefault(); e.currentTarget.classList.add('drop'); } },
        ondragleave: (e) => e.currentTarget.classList.remove('drop'),
        ondrop: (e) => {
          e.preventDefault();
          e.currentTarget.classList.remove('drop');
          const from = e.dataTransfer.getData('application/x-ferrite-account');
          if (from && from !== a.id) reorderAccounts(from, a.id);
        },
      },
      many && h('span.ml-account-grip', { 'aria-hidden': 'true', title: 'Drag to reorder' }, icon('grip', 'sm')),
      h('span.ml-account-name.ellipsis', { title: a.delegated ? `${a.email}, shared by ${a.owner_name}` : a.email }, a.email),
      a.delegated && h('span.ml-account-badge', { title: `Shared with you by ${a.owner_name}` }, icon('users', 'sm'), h('span.sr-only', `Shared by ${a.owner_name}`)),
      a.provisioning_id && h('span.ml-account-badge', { title: 'Set up by your administrator' }, icon('lock', 'sm'), h('span.sr-only', 'Managed by your administrator')),
      a.last_error && h('span.ml-account-error', { title: a.last_error }, icon('x', 'sm')),
      h('button.btn.ghost.icon.sm', {
        type: 'button', 'aria-label': `Account menu for ${a.email}`,
        onclick: (e) => menu(e.currentTarget, items),
      }, icon('more', 'sm')));
      tree.appendChild(head);
      if (a.delegated) tree.appendChild(h('div.ml-account-owner.small.muted', `Shared by ${a.owner_name}`));
      if (a.last_error) tree.appendChild(h('div.ml-account-warning.small', a.last_error));
      const fs = state.folders[a.id] || [];
      for (const f of fs) {
        if (!f.subscribed && !state.prefs.show_unsubscribed && f.id !== state.folderId) continue;
        const active = f.id === state.folderId && !state.view && listing();
        const menuFor = (anchor) => folderMenu(anchor, a, f);
        tree.appendChild(h(`div.ml-folder-row${f.subscribed ? '' : '.unsubscribed'}`,
          h(`button.ml-folder${active ? '.active' : ''}${f.selectable ? '' : '.disabled'}`, {
            type: 'button', disabled: !f.selectable, dataset: { id: f.id, role: f.role || '', name: f.name },
            style: { paddingLeft: `${10 + Math.min(depth(f), 4) * 14}px` },
            title: f.shared ? `${folderPath(f, fs)} (shared with you)` : (f.subscribed ? undefined : `${f.display_name} (not subscribed)`),
            onclick: () => openFolder(a.id, f.id),
            oncontextmenu: (e) => { e.preventDefault(); menuFor(e); },
            ...dropTarget(f),
          }, icon(ROLE_ICONS[f.role] || (f.shared ? 'users' : 'folder'), 'sm'), h('span.grow.ellipsis', f.display_name),
          f.shared && h('span.ml-shared', { title: 'Shared with you', 'aria-label': 'shared' }, icon('share', 'sm')),
          f.unread > 0 && f.role !== 'sent' && f.role !== 'drafts' && f.role !== 'trash' && h('span.badge.ml-unread', String(f.unread))),
          h('button.btn.ghost.icon.sm.ml-folder-more', { type: 'button', 'aria-label': `Folder menu for ${f.display_name}`, onclick: (e) => menuFor(e.currentTarget) }, icon('more', 'sm'))));
      }
      const quota = quotaBar(a);
      if (quota) tree.appendChild(quota);
    });
    navExtras(tree, ctx, { openMessage, compose });
    nav.appendChild(tree);
    if (state.canAdd) nav.appendChild(h('div.ml-nav-foot', h('button.btn.ghost.sm', { type: 'button', onclick: addAccount }, icon('plus', 'sm'), 'Add account')));
  }

  async function saveOrder(ids) {
    const byId = new Map(state.accounts.map((a) => [a.id, a]));
    state.accounts = ids.map((id) => byId.get(id));
    drawNav();
    try { await api.put('/api/mail/accounts/order', { ids }); } catch (e) { errorToast(e); loadAccounts(); }
  }

  function reorderAccounts(from, before) {
    const ids = state.accounts.map((a) => a.id).filter((id) => id !== from);
    ids.splice(ids.indexOf(before), 0, from);
    saveOrder(ids);
  }

  function moveAccount(id, delta) {
    const ids = state.accounts.map((a) => a.id);
    const i = ids.indexOf(id);
    const j = i + delta;
    if (j < 0 || j >= ids.length) return;
    [ids[i], ids[j]] = [ids[j], ids[i]];
    saveOrder(ids);
  }

  function folderMenu(anchor, a, f) {
    const own = !f.shared;
    const fixed = f.role === 'inbox';
    menu(anchor, [
      own && { label: 'New subfolder…', icon: 'plus', run: () => newFolder(a, f) },
      own && !fixed && { label: 'Rename…', icon: 'edit', run: () => renameFolder(f) },
      own && !fixed && { label: 'Move to…', icon: 'move', run: () => moveFolder(a, f) },
      f.selectable && { label: 'Mark all as read', icon: 'eye', run: () => folderAction(f, 'read-all', 'Marked as read') },
      f.selectable && own && { label: 'Empty folder…', icon: 'trash', run: () => emptyFolder(f) },
      'sep',
      { label: 'Statistics', icon: 'poll', run: () => folderStatsDialog(f) },
      { label: 'Repair (clear cache)', icon: 'refresh', run: () => folderAction(f, 'repair', 'Folder synced again') },
      !fixed && { label: f.subscribed ? 'Unsubscribe' : 'Subscribe', icon: 'bell', run: () => setSubscribed(f, !f.subscribed) },
      own && !fixed && 'sep',
      own && !fixed && { label: 'Delete folder…', icon: 'trash', danger: true, run: () => deleteFolder(f) },
    ].filter((x, i, arr) => x && !(x === 'sep' && (i === 0 || arr[i - 1] === 'sep'))));
  }

  async function newFolder(a, parent) {
    const name = await promptDialog(parent ? `New folder in “${parent.display_name}”` : `New folder in ${a.email}`, { label: 'Name', confirm: 'Create' });
    if (!name) return;
    try {
      const { folder: f } = await api.post(`/api/mail/accounts/${encodeURIComponent(a.id)}/mailboxes`, { name, parent_id: parent?.id });
      await reloadFolders();
      toast(`Created “${f.display_name}”`, 'ok', { timeout: 2000 });
    } catch (e) { errorToast(e); }
  }

  async function renameFolder(f) {
    const name = await promptDialog(`Rename “${f.display_name}”`, { label: 'New name', value: f.display_name, confirm: 'Rename' });
    if (!name || name === f.display_name) return;
    try {
      await api.patch(`/api/mail/folders/${encodeURIComponent(f.id)}`, { name });
      await reloadFolders();
    } catch (e) { errorToast(e); }
  }

  async function moveFolder(a, f) {
    const all = state.folders[a.id] || [];
    const top = { id: '', name: '', display_name: '(top level)', selectable: true, role: null, delimiter: null };
    const inside = (x) => x.name === f.name || (f.delimiter && x.name.startsWith(`${f.name}${f.delimiter}`));
    const candidates = [top, ...all.filter((x) => !inside(x) && !x.shared)].map((x) => ({ ...x, selectable: true }));
    const dest = await moveDialog(candidates, { title: `Move “${f.display_name}” into` });
    if (dest === null || dest === undefined) return;
    try {
      await api.patch(`/api/mail/folders/${encodeURIComponent(f.id)}`, { parent_id: dest });
      await reloadFolders();
    } catch (e) { errorToast(e); }
  }

  async function folderAction(f, what, done) {
    try {
      await api.post(`/api/mail/folders/${encodeURIComponent(f.id)}/${what}`, {});
      toast(done, 'ok', { timeout: 2000 });
      await reloadFolders();
      if (f.id === state.folderId) loadMessages();
    } catch (e) { errorToast(e); }
  }

  async function emptyFolder(f) {
    if (!await confirmDialog('Empty folder', `Delete all ${f.total} messages in “${f.display_name}” for good? This cannot be undone.`, { confirm: 'Empty', danger: true })) return;
    folderAction(f, 'empty', `Emptied “${f.display_name}”`);
  }

  async function setSubscribed(f, on) {
    try {
      await api.patch(`/api/mail/folders/${encodeURIComponent(f.id)}`, { subscribed: on });
      await reloadFolders();
      if (!on && !state.prefs.show_unsubscribed) toast(`“${f.display_name}” is hidden; show it again from Mail settings › Show folders I am not subscribed to.`, 'info');
    } catch (e) { errorToast(e); }
  }

  async function deleteFolder(f) {
    const subs = (state.folders[f.account_id] || []).filter((x) => f.delimiter && x.name.startsWith(`${f.name}${f.delimiter}`)).length;
    const what = subs ? ` with its ${subs} subfolder${subs === 1 ? '' : 's'}` : '';
    if (!await confirmDialog('Delete folder', `Delete “${f.display_name}”${what} and all messages in it from the mail server? This cannot be undone.`, { confirm: 'Delete', danger: true })) return;
    try {
      await api.del(`/api/mail/folders/${encodeURIComponent(f.id)}`);
      if (state.folderId === f.id) { state.folderId = folderByRole('inbox', f.account_id)?.id || null; }
      await reloadFolders();
      loadMessages();
    } catch (e) { errorToast(e); }
  }

  async function inboxSettings(a) {
    const saved = await inboxSettingsDialog(a, state.folders[a.id] || [], { onQuickActions: () => manageQuickActions(a) });
    if (saved) await reloadFolders();
  }

  async function manageQuickActions(a) {
    await quickActionsDialog(a, state.folders[a.id] || [], state.tags);
    await loadQuickActions();
    drawList();
  }

  async function openSettings() {
    const saved = await mailSettingsDialog(ctx, { prefs: state.prefs, tags: state.tags });
    await loadTags();
    if (saved) {
      const relist = saved.sort !== state.prefs.sort || saved.threaded !== state.prefs.threaded;
      state.prefs = saved;
      applyLayout();
      if (relist) await loadMessages();
    }
    drawNav();
    drawList();
  }

  // ---- message list -----------------------------------------------------------------------
  const selectedMessages = () => state.messages.concat(state.important).filter((m) => state.selected.has(m.id));
  const sameAccount = (msgs) => msgs.length > 0 && msgs.every((m) => m.account_id === msgs[0].account_id);
  const quickActionsFor = (msgs) => (sameAccount(msgs) ? state.quickActions.filter((qa) => qa.account_id === msgs[0].account_id) : []);
  const inRole = (m, role) => folderById(m.folder_id)?.role === role;

  function selectionToolbar() {
    const ids = [...state.selected];
    const msgs = selectedMessages();
    const anyUnread = msgs.some((m) => !m.seen);
    const anyUnflagged = msgs.some((m) => !m.flagged);
    const anyUnimportant = msgs.some((m) => !isImportant(m));
    const allSnoozed = msgs.length > 0 && msgs.every((m) => inRole(m, 'snoozed'));
    const allJunk = msgs.length > 0 && msgs.every((m) => inRole(m, 'junk'));
    const qas = quickActionsFor(msgs);
    const btn = (label, iconName, run, extra = '', title = label) => h(`button.btn.ghost.icon.sm${extra}`, { type: 'button', title, 'aria-label': label, onclick: run }, icon(iconName, 'sm'));
    return h('div.ml-toolbar.ml-selection',
      btn('Clear selection', 'x', () => { state.selected.clear(); drawList(); }),
      h('strong.grow', `${ids.length} selected`),
      btn(anyUnread ? 'Mark read' : 'Mark unread', anyUnread ? 'eye' : 'mail', () => act(anyUnread ? 'read' : 'unread', ids)),
      btn(anyUnflagged ? 'Flag' : 'Unflag', 'star', () => act(anyUnflagged ? 'flag' : 'unflag', ids)),
      btn(anyUnimportant ? 'Mark important' : 'Mark unimportant', 'important', () => setKeyword(msgs, IMPORTANT, anyUnimportant)),
      btn('Tags…', 'tag', () => editTags(msgs)),
      btn(allSnoozed ? 'Unsnooze' : 'Snooze…', 'clock', () => (allSnoozed ? unsnooze(msgs) : snooze(msgs))),
      btn('Move to…', 'move', () => moveTo(msgs)),
      folder()?.role !== 'archive' && btn('Archive', 'archive', () => act('archive', ids), '', 'Archive (e)'),
      btn(allJunk ? 'Not spam' : 'Report spam', allJunk ? 'mail' : 'junk', () => markJunk(msgs, !allJunk)),
      qas.length > 0 && btn('Quick actions', 'zap', (e) => quickActionMenu(e.currentTarget, msgs)),
      btn('Delete', 'trash', () => remove(ids), '.danger', 'Delete (#)'));
  }

  function drawListHead() {
    clear(listHead);
    const f = folder();
    if (state.selected.size) {
      listHead.appendChild(selectionToolbar());
      return;
    }
    const search = h('input', {
      type: 'search', value: state.search, placeholder: state.view === 'all' ? 'Search all inboxes' : (account() && !state.view ? `Search ${account().email}` : 'Search mail'), 'aria-label': 'Search mail',
      onkeydown: (e) => {
        if (e.key === 'Enter') { e.preventDefault(); runSearch(search.value.trim()); }
        if (e.key === 'Escape' && search.value) { e.stopPropagation(); search.value = ''; runSearch(''); }
      },
      onsearch: () => { if (!search.value) runSearch(''); },
    });
    const title = state.filter ? 'Search results' : state.search ? `Results for “${state.search}”` : (viewTitle() || (f ? f.display_name : 'Mail'));
    listHead.append(h('div.ml-toolbar',
      h('button.btn.ghost.icon.sm.ml-show-folders', { type: 'button', 'aria-label': 'Folders', onclick: () => setPane('folders') }, icon('menu', 'sm')),
      h('h2.grow.ellipsis', title),
      listing() && h(`button.btn.ghost.sm${state.unreadOnly ? '.active' : ''}`, { type: 'button', 'aria-pressed': String(state.unreadOnly), onclick: () => { state.unreadOnly = !state.unreadOnly; loadMessages(); } }, 'Unread'),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Advanced search', title: 'Advanced search', onclick: advancedSearch }, icon('filter', 'sm')),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Refresh', title: 'Refresh', onclick: () => refreshAll() }, icon('refresh', 'sm'))),
    state.filter
      ? h('div.ml-filter-bar.small', icon('filter', 'sm'), h('span.grow.ellipsis', filterSummary(state.filter, state.tags)),
        h('button.btn.ghost.sm', { type: 'button', onclick: advancedSearch }, 'Edit'),
        h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Clear search', onclick: () => { state.filter = null; loadMessages(); } }, icon('x', 'sm')))
      : h('div.ml-search', icon('search', 'sm'), search));
  }

  /** Who a row shows: recipients in Sent and Drafts, else the sender. */
  function who(m) {
    const sent = inRole(m, 'sent') || inRole(m, 'drafts');
    return sent ? (m.to.length ? m.to.map((a) => a.name || a.address).join(', ') : '(no recipients)') : person(m.from_name, m.from_addr);
  }

  function rowActions(m) {
    const btn = (label, iconName, run, cls = '') => h(`button.ml-act${cls}`, {
      type: 'button', title: label, 'aria-label': label,
      onclick: (e) => { e.stopPropagation(); run(e); },
    }, icon(iconName, 'sm'));
    const snoozedHere = inRole(m, 'snoozed');
    const qas = quickActionsFor([m]);
    return h('div.ml-row-actions', { role: 'toolbar', 'aria-label': 'Message actions' },
      btn(m.seen ? 'Mark unread' : 'Mark read', m.seen ? 'mail' : 'eye', () => act(m.seen ? 'unread' : 'read', [m.id])),
      btn(m.flagged ? 'Unflag' : 'Flag', 'star', () => act(m.flagged ? 'unflag' : 'flag', [m.id]), m.flagged ? '.on.star' : ''),
      btn(isImportant(m) ? 'Mark unimportant' : 'Mark important', 'important', () => setKeyword([m], IMPORTANT, !isImportant(m)), isImportant(m) ? '.on.important' : ''),
      !inRole(m, 'archive') && btn('Archive', 'archive', () => act('archive', [m.id])),
      btn(snoozedHere ? 'Unsnooze' : 'Snooze…', 'clock', () => (snoozedHere ? unsnooze([m]) : snooze([m]))),
      btn('Delete', 'trash', () => remove([m.id]), '.danger'),
      btn('More actions', 'more', (e) => menu(e.currentTarget, [
        { label: 'Tags…', icon: 'tag', run: () => editTags([m]) },
        { label: 'Move to…', icon: 'move', run: () => moveTo([m]) },
        inRole(m, 'junk') ? { label: 'Not spam', icon: 'mail', run: () => markJunk([m], false) } : { label: 'Report spam', icon: 'junk', run: () => markJunk([m], true) },
        { label: 'Copy link', icon: 'link', run: () => copyText(messageLink(m), 'Link copied') },
        ...(qas.length ? ['sep', ...qas.map((qa) => ({ label: qa.name, icon: stepIcon(qa.steps[0] || {}), run: () => runQuickAction(qa, [m]) }))] : []),
      ])));
  }

  function row(m) {
    const sel = state.selected.has(m.id);
    const open = m.id === state.messageId;
    const name = who(m);
    // Important shows as its own marker, not as a chip.
    const tags = (m.keywords || []).filter((k) => k !== IMPORTANT).map(tagOf).filter(Boolean);
    const until = inRole(m, 'snoozed') && m.message_id && state.snoozed.get(m.message_id);
    const where = state.view && (state.view === 'all' || state.view === 'priority'
      ? (state.accounts.length > 1 ? state.accounts.find((a) => a.id === m.account_id)?.email : null)
      : folderById(m.folder_id)?.display_name);
    const el = h(`div.ml-row${m.seen ? '' : '.unread'}${sel ? '.selected' : ''}${open ? '.open' : ''}`, {
      role: 'option', tabindex: '-1', 'aria-selected': String(open || sel), dataset: { id: m.id }, draggable: 'true',
      onclick: (e) => {
        if (e.target.closest('.ml-check, .ml-row-actions')) return;
        if (e.shiftKey || e.metaKey || e.ctrlKey) { toggleSelect(m.id, e.shiftKey); return; }
        openMessage(m.id);
      },
      ondragstart: (e) => {
        const ids = state.selected.has(m.id) ? [...state.selected] : [m.id];
        e.dataTransfer.setData('application/x-ferrite-mail', JSON.stringify(ids));
        e.dataTransfer.effectAllowed = 'move';
      },
    },
    h('label.ml-check', { title: 'Select', onclick: (e) => e.stopPropagation() },
      h('input', { type: 'checkbox', checked: sel, 'aria-label': `Select message from ${name}`, onclick: (e) => toggleSelect(m.id, e.shiftKey) })),
    avatar(name, { seed: inRole(m, 'sent') ? (m.to[0]?.address || name) : m.from_addr }),
    h('div.ml-row-main',
      h('div.ml-row-top',
        isImportant(m) && h('span.ml-important', { title: 'Important' }, icon('important', 'sm')),
        h('span.ml-from.ellipsis', name),
        m.thread_size > 1 && h('span.ml-count', String(m.thread_size)),
        h('span.ml-date', { title: dateTime(m.date) }, listDate(m.date))),
      h('div.ml-row-mid',
        h('span.ml-subject.ellipsis', m.subject || '(no subject)'),
        m.has_attachments && h('span.ml-icon', { title: 'Has attachments' }, icon('paperclip', 'sm')),
        h('button.ml-flag', {
          type: 'button', 'aria-label': m.flagged ? 'Unflag' : 'Flag', 'aria-pressed': String(m.flagged), class: m.flagged ? 'on' : '',
          onclick: (e) => { e.stopPropagation(); act(m.flagged ? 'unflag' : 'flag', [m.id]); },
        }, icon('star', 'sm'))),
      h('div.ml-preview.ellipsis', m.draft ? h('span.ml-draft', 'Draft ') : null, m.preview),
      (tags.length || until || where) && h('div.ml-row-tags',
        until && h('span.ml-until.small', { title: `Comes back ${dateTime(until)}` }, icon('clock', 'sm'), dateTime(until)),
        where && h('span.ml-where.small.muted.ellipsis', where),
        tags.map(tagChip))),
    rowActions(m));
    return el;
  }

  /** One row per conversation (the first, i.e. newest or oldest, of each thread). */
  function rows(list) {
    if (!state.prefs.threaded) return list;
    const seen = new Set();
    return list.filter((m) => {
      const key = `${m.account_id}:${m.thread_id}`;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
  }

  const section = (title, count) => h('div.ml-section', { role: 'presentation' }, h('span', title), count != null && h('span.ml-section-count', String(count)));

  function drawList() {
    drawListHead();
    clear(listBody);
    if (!state.accounts.length) return;
    const msgs = rows(state.messages);
    const important = rows(state.important);
    if (!msgs.length && !important.length) {
      const empty = state.search || state.filter ? 'No messages match.'
        : state.unreadOnly ? 'No unread messages.'
          : state.view === 'starred' ? 'No favourites. Star messages to find them here.'
            : state.view?.startsWith('tag:') ? 'No messages with this tag.'
              : 'This folder is empty.';
      listBody.appendChild(h('div.ml-empty.small.muted', empty));
      return;
    }
    const role = folder()?.role;
    const favouritesFirst = state.prefs.favorites_top && listing() && (role === 'inbox' || state.view === 'all' || state.view === 'priority');
    if (state.view === 'priority') {
      const favs = favouritesFirst ? msgs.filter((m) => m.flagged) : [];
      const others = msgs.filter((m) => !favs.includes(m));
      if (important.length) { listBody.appendChild(section('Important', important.length)); important.forEach((m) => listBody.appendChild(row(m))); }
      if (favs.length) { listBody.appendChild(section('Favourites', favs.length)); favs.forEach((m) => listBody.appendChild(row(m))); }
      listBody.appendChild(section('Others'));
      others.forEach((m) => listBody.appendChild(row(m)));
      if (!others.length) listBody.appendChild(h('div.ml-empty.small.muted', 'Nothing else.'));
    } else if (favouritesFirst && msgs.some((m) => m.flagged) && msgs.some((m) => !m.flagged)) {
      const favs = msgs.filter((m) => m.flagged);
      listBody.appendChild(section('Favourites', favs.length));
      favs.forEach((m) => listBody.appendChild(row(m)));
      listBody.appendChild(section('Other'));
      msgs.filter((m) => !m.flagged).forEach((m) => listBody.appendChild(row(m)));
    } else {
      for (const m of msgs) listBody.appendChild(row(m));
    }
    if (state.hasMore && listing()) {
      listBody.appendChild(h('div.ml-more', h('button.btn.ghost.sm', { type: 'button', onclick: loadMore }, state.loadingMore ? spinner() : 'Load older messages')));
    }
  }

  /** Message ids in the order the list shows them. */
  const shownIds = () => [...listBody.querySelectorAll('.ml-row')].map((el) => el.dataset.id);

  function toggleSelect(id, range) {
    if (range && state.lastClicked) {
      const ids = shownIds();
      const [a, b] = [ids.indexOf(state.lastClicked), ids.indexOf(id)].sort((x, y) => x - y);
      if (a >= 0 && b >= 0) ids.slice(a, b + 1).forEach((x) => state.selected.add(x));
    } else if (state.selected.has(id)) state.selected.delete(id);
    else state.selected.add(id);
    state.lastClicked = id;
    drawList();
  }

  // ---- reader -------------------------------------------------------------------------------
  function drawReaderEmpty() {
    clear(reader);
    reader.appendChild(state.accounts.length
      ? emptyState('mail', 'No conversation selected', 'Pick a message from the list.')
      : emptyState('mail', 'Connect your mail', 'Read and send mail from your existing accounts (IMAP and SMTP) right here.',
        h('button.btn.primary', { type: 'button', onclick: addAccount }, icon('plus'), 'Add mail account')));
  }

  async function openMessage(id, { focusList = true } = {}) {
    state.messageId = id;
    state.selected.clear();
    setUrl();
    setPane('reader');
    drawList();
    const seq = ++state.openSeq;
    clear(reader).appendChild(spinner());
    try {
      if (state.mid) {
        // A link with the Message-ID: the message may have moved (and got a new id) since.
        const mid = state.mid;
        state.mid = null;
        ({ id } = await api.get(`/api/mail/resolve${q({ m: id, mid })}`));
        if (seq !== state.openSeq) return;
        state.messageId = id;
        setUrl();
      }
      const { messages } = await api.get(`/api/mail/messages/${encodeURIComponent(id)}/thread`);
      if (seq !== state.openSeq) return;
      state.thread = messages;
      // Opened from a link (search, notification): show the folder the message lives in.
      const opened = messages.find((m) => m.id === id);
      if (opened && opened.folder_id !== state.folderId && !state.search && !state.filter && !state.view) {
        state.accountId = opened.account_id;
        state.folderId = opened.folder_id;
        setUrl();
        drawNav();
        loadMessages();
      }
      await drawThread(id);
      const unseen = state.thread.filter((m) => !m.seen).map((m) => m.id);
      if (unseen.length) act('read', unseen, { quiet: true });
    } catch (e) {
      if (seq !== state.openSeq) return;
      clear(reader).appendChild(emptyState('mail', e.status === 404 ? 'This message is gone' : 'Could not open the message', e.status === 404 ? 'It was moved or deleted, maybe in another app.' : e.message));
    }
    if (focusList) listBody.querySelector('.ml-row.open')?.scrollIntoView({ block: 'nearest' });
  }

  async function drawThread(openId) {
    const thread = state.thread;
    const current = thread.find((m) => m.id === openId) || thread[thread.length - 1];
    const subject = current?.subject || '(no subject)';
    const isDraftFolder = folder()?.role === 'drafts';
    const box = h('article.ml-thread');
    box.appendChild(h('div.ml-thread-head',
      h('button.btn.ghost.icon.sm.ml-back', { type: 'button', 'aria-label': 'Back to the list', onclick: closeReader }, icon('arrow-left', 'sm')),
      h('h2.grow', subject),
      thread.length > 1 && h('span.pill', `${thread.length} messages`)));
    const currentTags = (current?.keywords || []).map(tagOf).filter(Boolean);
    const until = current?.message_id && state.snoozed.get(current.message_id);
    if (currentTags.length || until) {
      box.appendChild(h('div.ml-thread-tags', until && h('span.ml-until.small', icon('clock', 'sm'), `Snoozed until ${dateTime(until)}`), currentTags.map(tagChip)));
    }
    box.appendChild(h('div.ml-actions',
      isDraftFolder
        ? h('button.btn.primary.sm', { type: 'button', onclick: () => compose({ fromMessage: current.id, mode: 'edit' }) }, icon('edit', 'sm'), 'Edit draft')
        : [
          h('button.btn.sm', { type: 'button', title: 'Reply (r)', 'aria-label': 'Reply', onclick: () => compose({ fromMessage: current.id, mode: 'reply' }) }, icon('reply', 'sm'), h('span', 'Reply')),
          h('button.btn.sm', { type: 'button', title: 'Reply all (a)', 'aria-label': 'Reply all', onclick: () => compose({ fromMessage: current.id, mode: 'reply_all' }) }, icon('users', 'sm'), h('span', 'Reply all')),
          h('button.btn.sm', { type: 'button', title: 'Forward (f)', 'aria-label': 'Forward', onclick: () => compose({ fromMessage: current.id, mode: 'forward' }) }, icon('share', 'sm'), h('span', 'Forward')),
        ],
      h('div.grow'),
      folder()?.role !== 'archive' && h('button.btn.ghost.icon.sm', { type: 'button', title: 'Archive (e)', 'aria-label': 'Archive', onclick: () => act('archive', [current.id]) }, icon('archive', 'sm')),
      h('button.btn.ghost.icon.sm', { type: 'button', title: 'Snooze… (b)', 'aria-label': inRole(current, 'snoozed') ? 'Unsnooze' : 'Snooze', onclick: () => (inRole(current, 'snoozed') ? unsnooze(threadIn(current)) : snooze(threadIn(current))) }, icon('clock', 'sm')),
      h('button.btn.ghost.icon.sm', { type: 'button', title: 'Move to…', 'aria-label': 'Move to folder', onclick: (e) => moveMenu(e.currentTarget, [current.id]) }, icon('move', 'sm')),
      h('button.btn.ghost.icon.sm.danger', { type: 'button', title: 'Delete (#)', 'aria-label': 'Delete', onclick: () => remove([current.id]) }, icon('trash', 'sm')),
      h('button.btn.ghost.icon.sm', {
        type: 'button', 'aria-label': 'More actions',
        onclick: (e) => menu(e.currentTarget, [
          { label: current.seen ? 'Mark unread' : 'Mark read', icon: 'mail', run: () => act(current.seen ? 'unread' : 'read', [current.id]) },
          { label: current.flagged ? 'Unflag' : 'Flag', icon: 'star', run: () => act(current.flagged ? 'unflag' : 'flag', [current.id]) },
          folder()?.role === 'junk'
            ? { label: 'Not spam', icon: 'mail', run: () => act('not_spam', [current.id]) }
            : { label: 'Report spam', icon: 'junk', run: () => act('spam', [current.id]) },
          ...threadMenuItems(current),
          { label: 'Download (.eml)', icon: 'download', run: () => { location.href = `/api/mail/messages/${encodeURIComponent(current.id)}/raw`; } },
          ...messageMenuItems(current, ctx, { accounts: state.accounts }),
        ]),
      }, icon('more', 'sm'))));
    const cards = h('div.ml-cards');
    box.appendChild(cards);
    for (const m of thread) {
      // The opened message and unread ones start expanded; the rest show one line.
      const expanded = m.id === current.id || !m.seen;
      cards.appendChild(messageCard(m, expanded));
    }
    decorateThread(box, cards, thread, current, ctx, { compose, accounts: state.accounts });
    clear(reader).appendChild(box);
    reader.scrollTop = 0;
    const openCard = cards.querySelector(`[data-id="${CSS.escape(current.id)}"]`);
    if (openCard && thread.length > 1) openCard.scrollIntoView({ block: 'start' });
  }

  function messageCard(m, expanded) {
    const card = h(`section.ml-card${expanded ? '.expanded' : ''}`, { dataset: { id: m.id } });
    const head = h('header.ml-card-head', {
      tabindex: '0', role: 'button', 'aria-expanded': String(expanded),
      onclick: () => toggleCard(),
      onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggleCard(); } },
    },
    avatar(person(m.from_name, m.from_addr), { seed: m.from_addr }),
    h('div.ml-card-who',
      h('div.ml-card-from', h('strong', person(m.from_name, m.from_addr)), m.from_name && h('span.small.muted', ` <${m.from_addr}>`)),
      h('div.ml-card-sub.small.muted.ellipsis', expanded ? `to ${addrList(m.to) || '—'}${m.cc.length ? `, cc ${addrList(m.cc)}` : ''}` : m.preview)),
    h('span.ml-card-date.small.muted', { title: dateTime(m.date) }, listDate(m.date)));
    const body = h('div.ml-card-body');
    card.append(head, body);
    let loaded = false;
    async function load() {
      if (loaded) return;
      loaded = true;
      body.appendChild(spinner());
      try {
        const { message } = await api.get(`/api/mail/messages/${encodeURIComponent(m.id)}`);
        clear(body);
        drawBody(body, message);
      } catch (e) {
        loaded = false;
        clear(body).appendChild(h('p.error-text', e.message));
      }
    }
    function toggleCard() {
      const open = !card.classList.contains('expanded');
      card.classList.toggle('expanded', open);
      head.setAttribute('aria-expanded', String(open));
      head.querySelector('.ml-card-sub').textContent = open ? `to ${addrList(m.to) || '—'}${m.cc.length ? `, cc ${addrList(m.cc)}` : ''}` : m.preview;
      if (open) load();
    }
    if (expanded) load();
    return card;
  }

  function drawBody(body, message) {
    if (message.has_html) {
      let remote = false;
      const banner = message.remote_content && h('div.ml-remote.small',
        icon('image', 'sm'), h('span.grow', 'Remote images are hidden to protect your privacy.'),
        h('button.btn.sm', { type: 'button', onclick: () => { remote = true; banner.remove(); frame.src = src(); } }, 'Show images'));
      const src = () => `/api/mail/messages/${encodeURIComponent(message.id)}/html${remote ? '?remote=true' : ''}`;
      const frame = h('iframe.ml-frame', {
        title: `Message from ${person(message.from_name, message.from_addr)}`,
        sandbox: 'allow-same-origin allow-popups allow-popups-to-escape-sandbox',
        referrerpolicy: 'no-referrer',
        src: src(),
      });
      // Same-origin (but script-free) frame: size it to its content, and again whenever
      // images load or the width changes.
      let observer = null;
      frame.addEventListener('load', () => {
        const doc = frame.contentDocument;
        if (!doc) return;
        const fit = () => { frame.style.height = `${Math.max(60, doc.documentElement.scrollHeight)}px`; };
        fit();
        observer?.disconnect();
        observer = new ResizeObserver(fit);
        observer.observe(doc.body);
        doc.querySelectorAll('img').forEach((img) => img.addEventListener('load', fit));
      });
      if (banner) body.appendChild(banner);
      body.appendChild(frame);
    } else {
      body.appendChild(textBody(message.text));
    }
    const files = message.attachments.filter((a) => !a.inline);
    if (files.length) {
      body.appendChild(h('div.ml-attachments', files.map((a) => {
        const href = `/api/mail/messages/${encodeURIComponent(message.id)}/attachments/${a.index}`;
        return h('div.ml-attachment',
          icon('paperclip', 'sm'),
          h('a.ellipsis', { href, download: a.name, title: `Download ${a.name}` }, a.name),
          h('span.small.muted', bytes(a.size)),
          h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Save ${a.name} to Files`, title: 'Save to Files', onclick: () => saveToFiles(message.id, a) }, icon('files', 'sm')));
      })));
    }
    decorateMessage(body, message, ctx, { accounts: state.accounts });
  }

  async function saveToFiles(messageId, a) {
    const folderId = await pickFolder(ctx, { title: `Save “${a.name}” to…`, confirm: 'Save here' });
    if (!folderId) return;
    try {
      const { node } = await api.post(`/api/mail/messages/${encodeURIComponent(messageId)}/attachments/${a.index}/save`, { folder_id: folderId });
      toast(`Saved as ${node.name}`, 'ok', { action: { label: 'Open folder', run: () => ctx.navigate(`/files/${node.parent_id}?select=${node.id}`) } });
    } catch (e) {
      errorToast(e);
    }
  }

  function closeReader() {
    state.messageId = null;
    state.thread = [];
    setUrl();
    setPane('list');
    drawList();
    drawReaderEmpty();
    listBody.focus();
  }

  // ---- actions ------------------------------------------------------------------------------

  // ---- actions ------------------------------------------------------------------------------
  async function act(action, ids, { folder_id, quiet = false } = {}) {
    if (!ids.length) return;
    const leaving = ['archive', 'delete', 'move', 'spam', 'not_spam'].includes(action);
    // Optimistic update of the list; the server event refreshes it for real.
    const all = state.messages.concat(state.important);
    const shown = shownIds();
    const idx = shown.findIndex((id) => ids.includes(id));
    for (const m of all) {
      if (!ids.includes(m.id)) continue;
      if (action === 'read') m.seen = true;
      if (action === 'unread') m.seen = false;
      if (action === 'flag') m.flagged = true;
      if (action === 'unflag') m.flagged = false;
    }
    for (const m of state.thread) {
      if (ids.includes(m.id) && (action === 'read' || action === 'unread')) m.seen = action === 'read';
      if (ids.includes(m.id) && (action === 'flag' || action === 'unflag')) m.flagged = action === 'flag';
    }
    const deleteForGood = ids.every((id) => ['trash', 'junk'].includes(folderById(all.find((m) => m.id === id)?.folder_id)?.role));
    let next = null;
    let closedOpen = false;
    if (leaving) {
      const remaining = shown.filter((id) => !ids.includes(id));
      if (state.messageId && ids.includes(state.messageId)) {
        next = remaining[Math.min(Math.max(idx, 0), remaining.length - 1)] || null;
        // The open message is leaving this folder: stop showing (and loading) it now.
        closedOpen = true;
        state.messageId = null;
        state.thread = [];
        state.openSeq++;
        setUrl();
        drawReaderEmpty();
      }
      state.messages = state.messages.filter((m) => !ids.includes(m.id));
      state.important = state.important.filter((m) => !ids.includes(m.id));
      state.selected.clear();
    }
    drawList();
    try {
      await api.post('/api/mail/actions', { ids, action, folder_id });
      if (!quiet && leaving) {
        const what = { archive: 'Archived', delete: deleteForGood ? 'Deleted' : 'Moved to Trash', move: 'Moved', spam: 'Reported as spam:', not_spam: 'Moved to Inbox:' }[action];
        toast(`${what} ${ids.length === 1 ? '1 message' : `${ids.length} messages`}`, 'ok', { timeout: 2500 });
      }
      if (closedOpen) {
        if (next && !singlePane()) openMessage(next);
        else closeReader();
      } else if (!leaving && state.messageId && state.thread.some((m) => ids.includes(m.id)) && (action === 'flag' || action === 'unflag' || action === 'unread')) {
        drawThread(state.messageId);
      }
      reloadFolders();
    } catch (e) {
      errorToast(e);
      loadMessages();
    }
  }

  async function remove(ids) {
    const all = state.messages.concat(state.important, state.thread);
    const inBin = ids.length > 0 && ids.every((id) => ['trash', 'junk'].includes(folderById(all.find((m) => m.id === id)?.folder_id)?.role || folder()?.role));
    if (inBin) {
      const ok = await confirmDialog('Delete for good', `Permanently delete ${ids.length === 1 ? 'this message' : `${ids.length} messages`}? This cannot be undone.`, { confirm: 'Delete', danger: true });
      if (!ok) return;
    }
    act('delete', ids);
  }

  /** Messages that leave their list after an action done elsewhere: drop them now. */
  function dropFromList(ids) {
    state.messages = state.messages.filter((m) => !ids.includes(m.id));
    state.important = state.important.filter((m) => !ids.includes(m.id));
    state.selected.clear();
    if (state.messageId && ids.includes(state.messageId)) {
      state.messageId = null;
      state.thread = [];
      state.openSeq++;
      setUrl();
      setPane('list');
      drawReaderEmpty();
    }
    drawList();
  }

  /** The thread's messages in the same folder as `m` (what thread actions apply to). */
  const threadIn = (m) => {
    const same = state.thread.filter((x) => x.folder_id === m.folder_id);
    return same.length ? same : [m];
  };

  async function moveTo(msgs) {
    if (!sameAccount(msgs)) { toast('Messages can only be moved within their account.', 'error'); return; }
    const fs = state.folders[msgs[0].account_id] || [];
    const exclude = msgs.every((m) => m.folder_id === msgs[0].folder_id) ? msgs[0].folder_id : null;
    const dest = await moveDialog(fs, { exclude });
    if (dest) act('move', msgs.map((m) => m.id), { folder_id: dest });
  }

  // The reader's Move button (one message).
  function moveMenu(_anchor, ids) {
    const msgs = state.thread.concat(state.messages, state.important).filter((m, i, arr) => ids.includes(m.id) && arr.findIndex((x) => x.id === m.id) === i);
    moveTo(msgs.length ? msgs : ids.map((id) => ({ id, account_id: state.accountId, folder_id: state.folderId })));
  }

  async function setKeyword(msgs, keyword, on) {
    const ids = msgs.map((m) => m.id);
    for (const m of state.messages.concat(state.important, state.thread)) {
      if (!ids.includes(m.id)) continue;
      m.keywords = (m.keywords || []).filter((k) => k !== keyword);
      if (on) m.keywords.push(keyword);
    }
    if (keyword === IMPORTANT && state.view === 'priority') {
      // Move rows between Important and Others right away.
      const moved = state.messages.concat(state.important).filter((m, i, arr) => arr.findIndex((x) => x.id === m.id) === i);
      state.important = moved.filter(isImportant);
      state.messages = moved.filter((m) => !isImportant(m));
    }
    drawList();
    if (state.messageId && state.thread.some((m) => ids.includes(m.id))) drawThread(state.messageId);
    try {
      await api.post('/api/mail/keywords', { ids, keyword, on });
      if (keyword === IMPORTANT) toast(on ? 'Marked important' : 'Marked not important', 'ok', { timeout: 2000 });
    } catch (e) {
      errorToast(e);
      loadMessages();
    }
  }

  async function editTags(msgs) {
    await tagsDialog(state.tags, msgs, async (keyword, on) => {
      await api.post('/api/mail/keywords', { ids: msgs.map((m) => m.id), keyword, on });
    }, { onCreated: () => drawNav() });
    drawList();
    if (state.messageId) drawThread(state.messageId);
  }

  async function snooze(msgs) {
    const until = await snoozeDialog();
    if (!until) return;
    const ids = msgs.map((m) => m.id);
    try {
      await api.post('/api/mail/snooze', { ids, until });
      toast(`Snoozed until ${dateTime(until)}`, 'ok', { timeout: 3000 });
      dropFromList(ids);
      loadSnoozed();
      reloadFolders();
    } catch (e) { errorToast(e); }
  }

  async function unsnooze(msgs) {
    const ids = msgs.map((m) => m.id);
    try {
      await api.post('/api/mail/unsnooze', { ids });
      toast('Back where it was', 'ok', { timeout: 2000 });
      dropFromList(ids);
      loadSnoozed();
      reloadFolders();
    } catch (e) { errorToast(e); }
  }

  /** Reports spam (or not spam): keywords for the server's filter, a copy to the admin's
   * report address, and a move to Junk (or back) when the account's settings say so. */
  function markJunk(msgs, junk) {
    act(junk ? 'spam' : 'not_spam', msgs.map((m) => m.id));
  }

  function quickActionMenu(anchor, msgs) {
    menu(anchor, quickActionsFor(msgs).map((qa) => ({ label: qa.name, icon: stepIcon(qa.steps[0] || {}), run: () => runQuickAction(qa, msgs) })));
  }

  async function runQuickAction(qa, msgs) {
    const ids = msgs.map((m) => m.id);
    try {
      await api.post(`/api/mail/quick-actions/${encodeURIComponent(qa.id)}/run`, { ids });
      toast(`“${qa.name}” done`, 'ok', { timeout: 2000 });
      if (qa.steps.some((s) => ['move', 'delete', 'spam'].includes(s.type))) dropFromList(ids);
      loadMessages();
      reloadFolders();
    } catch (e) { errorToast(e); }
  }

  /** Inbox-management entries of the reader's More menu (for the open conversation). */
  function threadMenuItems(current) {
    const msgs = threadIn(current);
    const qas = quickActionsFor([current]);
    return [
      { label: isImportant(current) ? 'Mark not important' : 'Mark important', icon: 'important', run: () => setKeyword(msgs, IMPORTANT, !isImportant(current)) },
      { label: 'Tags…', icon: 'tag', run: () => editTags(msgs) },
      { label: 'Copy link', icon: 'link', run: () => copyText(messageLink(current), 'Link copied') },
      ...qas.map((qa) => ({ label: qa.name, icon: stepIcon(qa.steps[0] || {}), run: () => runQuickAction(qa, msgs) })),
    ];
  }

  async function advancedSearch() {
    const f = await advancedSearchDialog(state.filter || (state.search ? { q: state.search } : (state.view === 'all' ? { unified: true } : null)), {
      tags: state.tags, accounts: state.accounts, folders: state.folders, bodySearch: state.prefs.body_search,
    });
    if (!f) return;
    if (state.search && !f.q) f.q = state.search;
    state.search = '';
    state.filter = f;
    state.selected.clear();
    await loadMessages();
  }

  // ---- composing ------------------------------------------------------------------------------
  function compose(opts = {}) {
    if (!state.accounts.length) { addAccount(); return; }
    openComposer(ctx, { accounts: state.accounts, accountId: state.view ? state.accounts[0].id : state.accountId, ...opts });
  }

  /** A mailto: link handed over by the browser: the composer, filled in (RFC 6068). */
  async function openMailto(uri) {
    history.replaceState({}, '', '/mail');
    try {
      const { mailto } = await api.get(`/api/mail/mailto${q({ uri })}`);
      const a = account() || state.accounts[0];
      const signature = a?.signature ? `\n\n-- \n${a.signature}` : '';
      compose({ prefill: { to: mailto.to, cc: mailto.cc, bcc: mailto.bcc, subject: mailto.subject, text: `${mailto.body}${signature}` } });
    } catch (e) {
      errorToast(e);
    }
  }

  // ---- accounts -------------------------------------------------------------------------------
  async function addAccount() {
    const a = await accountDialog(ctx, null);
    if (!a) return;
    state.accountId = a.id;
    state.folderId = null;
    state.view = null;
    await loadAccounts();
  }

  async function editAccount(a) {
    const saved = await accountDialog(ctx, a);
    if (saved) await loadAccounts();
  }

  async function removeAccount(a) {
    if (!await confirmDialog('Remove account', `Remove ${a.email} from Ferrite? Your mail stays on the mail server.`, { confirm: 'Remove', danger: true })) return;
    try {
      await api.del(`/api/mail/accounts/${encodeURIComponent(a.id)}`);
      if (state.accountId === a.id) { state.accountId = null; state.folderId = null; state.messageId = null; }
      await loadAccounts();
    } catch (e) {
      errorToast(e);
    }
  }

  /** Syncs an account's folder list (and the folder shown) from the server now. */
  async function refreshAccount(accountId, withFolder = false, redraw = true) {
    if (!accountId) return;
    try {
      const { folders: fs } = await api.post(`/api/mail/accounts/${encodeURIComponent(accountId)}/sync`, withFolder && state.folderId ? { folder_id: state.folderId } : {});
      state.folders[accountId] = fs;
      if (redraw) {
        drawNav();
        if (accountId === state.accountId || state.view) await loadMessages();
      }
    } catch (e) {
      errorToast(e);
    }
  }

  // ---- loading ------------------------------------------------------------------------------
  async function loadPrefs() {
    try {
      ({ prefs: state.prefs } = await api.get('/api/mail/preferences'));
    } catch (e) {
      errorToast(e);
    }
    applyLayout();
  }

  async function loadTags() {
    try { ({ tags: state.tags } = await api.get('/api/mail/tags')); } catch (_) { /* keep the last list */ }
  }

  async function loadQuickActions() {
    try { ({ quick_actions: state.quickActions } = await api.get('/api/mail/quick-actions')); } catch (_) { /* keep the last list */ }
  }

  async function loadSnoozed() {
    try {
      const { snoozed } = await api.get('/api/mail/snoozed');
      state.snoozed = new Map(snoozed.map((s) => [s.message_id, s.until]));
    } catch (_) { /* keep the last list */ }
  }

  async function loadAccounts() {
    const { accounts, can_add: canAdd } = await api.get('/api/mail/accounts');
    state.accounts = accounts;
    state.canAdd = canAdd !== false;
    if (!accounts.some((a) => a.id === state.accountId)) state.accountId = accounts[0]?.id || null;
    await Promise.all(accounts.map(async (a) => {
      try {
        state.folders[a.id] = (await api.get(`/api/mail/accounts/${encodeURIComponent(a.id)}/folders`)).folders;
      } catch (e) {
        state.folders[a.id] = [];
        a.last_error = a.last_error || e.message;
      }
    }));
    if (state.folderId && !Object.values(state.folders).flat().some((f) => f.id === state.folderId)) state.folderId = null;
    if (state.folderId) state.accountId = Object.entries(state.folders).find(([, fs]) => fs.some((f) => f.id === state.folderId))?.[0] || state.accountId;
    if (!state.folderId) state.folderId = folderByRole('inbox')?.id || folders().find((f) => f.selectable)?.id || null;
    if (state.view === 'all' && accounts.length < 2) state.view = null;
    drawNav();
    if (accounts.length && !state.messageId) drawReaderEmpty();
    setUrl();
    page.classList.toggle('no-accounts', !accounts.length);
    if (!accounts.length) {
      setPane('reader');
      drawList();
      drawReaderEmpty();
      return;
    }
    if (!state.messageId) drawReaderEmpty();
    await loadMessages();
  }

  async function reloadFolders() {
    await Promise.all([
      ...state.accounts.map(async (a) => {
        try { state.folders[a.id] = (await api.get(`/api/mail/accounts/${encodeURIComponent(a.id)}/folders`)).folders; } catch (_) { /* keep the last list */ }
      }),
      // Quota and errors change with syncs.
      api.get('/api/mail/accounts').then(({ accounts }) => {
        for (const fresh of accounts) {
          const a = state.accounts.find((x) => x.id === fresh.id);
          if (a) Object.assign(a, { quota_used: fresh.quota_used, quota_limit: fresh.quota_limit, last_error: fresh.last_error });
        }
      }).catch(() => { /* keep the last state */ }),
    ]);
    drawNav();
  }

  /** Refreshes the folder lists of every account and the list shown. */
  async function refreshAll() {
    const ids = state.view ? state.accounts.map((a) => a.id) : [state.accountId];
    await Promise.all(ids.filter(Boolean).map((id) => refreshAccount(id, id === state.accountId && !state.view, false)));
    drawNav();
    await loadMessages();
  }

  const viewUrl = (view, params) => `/api/mail/views/${encodeURIComponent(view)}/messages${q(params)}`;

  /** The first page of whatever the list shows. */
  async function loadMessages() {
    if (state.filter) return runAdvanced();
    if (state.search) return runSearch(state.search);
    const key = state.view || state.folderId;
    if (!key) { state.messages = []; state.important = []; drawList(); return undefined; }
    const unread = state.unreadOnly ? 'true' : '';
    const limit = Math.max(PAGE, state.messages.length);
    try {
      let page;
      let important = [];
      if (state.view === 'priority') {
        const [imp, others] = await Promise.all([
          api.get(viewUrl('important', { limit: 100, unread, sort: state.prefs.sort })),
          api.get(viewUrl('others', { limit, unread, sort: state.prefs.sort })),
        ]);
        important = imp.messages;
        page = others;
      } else if (state.view) {
        page = await api.get(viewUrl(state.view, { limit, unread, sort: state.prefs.sort }));
      } else if (state.prefs.sort === 'oldest') {
        page = await api.get(`/api/mail/folders/${encodeURIComponent(state.folderId)}/messages${q({ sort: 'oldest', limit, unread })}`);
      } else {
        page = await api.get(`/api/mail/folders/${encodeURIComponent(state.folderId)}/messages${q({ limit, unread })}`);
      }
      if (key !== (state.view || state.folderId) || state.search || state.filter) return undefined;
      state.messages = page.messages;
      state.important = important;
      state.hasMore = page.has_more;
    } catch (e) {
      errorToast(e);
    }
    drawList();
    return undefined;
  }

  async function loadMore() {
    if (state.loadingMore || !state.messages.length) return;
    state.loadingMore = true;
    drawList();
    const unread = state.unreadOnly ? 'true' : '';
    try {
      const last = state.messages[state.messages.length - 1];
      let page;
      if (state.view) {
        page = await api.get(viewUrl(state.view === 'priority' ? 'others' : state.view, { before: last.id, limit: PAGE, unread, sort: state.prefs.sort }));
      } else if (state.prefs.sort === 'oldest') {
        page = await api.get(`/api/mail/folders/${encodeURIComponent(state.folderId)}/messages${q({ sort: 'oldest', offset: state.messages.length, limit: PAGE, unread })}`);
      } else {
        page = await api.get(`/api/mail/folders/${encodeURIComponent(state.folderId)}/messages${q({ before: last.id, limit: PAGE, unread })}`);
      }
      const known = new Set(state.messages.map((m) => m.id));
      state.messages.push(...page.messages.filter((m) => !known.has(m.id)));
      state.hasMore = page.has_more;
    } catch (e) {
      errorToast(e);
    }
    state.loadingMore = false;
    drawList();
  }

  /** Quick search: senders, recipients and subjects (and bodies when body search is on),
   * in the account shown, or every inbox in All inboxes. */
  async function runSearch(term) {
    state.search = term;
    state.filter = null;
    state.selected.clear();
    if (!term) { drawNav(); return loadMessages(); }
    drawNav();
    clear(listBody).appendChild(spinner());
    drawListHead();
    try {
      const scope = state.view === 'all' ? { unified: 'true' } : (state.view ? {} : { account_id: state.accountId });
      const { messages } = await api.get(`/api/mail/search/advanced${q({ q: term, ...scope })}`);
      if (term !== state.search) return undefined;
      state.messages = messages;
      state.important = [];
      state.hasMore = false;
    } catch (e) {
      errorToast(e);
    }
    drawList();
    return undefined;
  }

  async function runAdvanced() {
    const f = state.filter;
    drawNav();
    clear(listBody).appendChild(spinner());
    drawListHead();
    try {
      const params = Object.fromEntries(Object.entries(f).map(([k, v]) => [k, v === true ? 'true' : v]));
      const { messages } = await api.get(`/api/mail/search/advanced${q(params)}`);
      if (f !== state.filter) return undefined;
      state.messages = messages;
      state.important = [];
      state.hasMore = false;
    } catch (e) {
      errorToast(e);
    }
    drawList();
    return undefined;
  }

  function resetList() {
    state.search = '';
    state.filter = null;
    state.selected.clear();
    state.messages = [];
    state.important = [];
    state.unreadOnly = false;
  }

  async function openFolder(accountId, folderId) {
    state.accountId = accountId;
    state.folderId = folderId;
    state.view = null;
    resetList();
    setUrl();
    setPane('list');
    drawNav();
    clear(listBody).appendChild(spinner());
    drawListHead();
    await loadMessages();
    listBody.focus();
  }

  async function openView(view) {
    state.view = view;
    resetList();
    setUrl();
    setPane('list');
    drawNav();
    clear(listBody).appendChild(spinner());
    drawListHead();
    await loadMessages();
    listBody.focus();
  }

  // ---- keyboard -----------------------------------------------------------------------------
  function onKey(e) {
    if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
    const t = e.target;
    if (t.closest && t.closest('input, textarea, select, [contenteditable="true"]')) return;
    if (document.querySelector('.modal-backdrop, .menu')) return;
    const ids = shownIds();
    const i = ids.indexOf(state.messageId);
    const all = state.messages.concat(state.important);
    const targets = state.selected.size ? [...state.selected] : (state.messageId ? [state.messageId] : []);
    const targetMsgs = targets.map((id) => all.find((m) => m.id === id) || state.thread.find((m) => m.id === id)).filter(Boolean);
    const current = state.thread.find((m) => m.id === state.messageId);
    switch (e.key) {
      case 'j': case 'ArrowDown':
        if (ids[i + 1] ?? (i < 0 ? ids[0] : undefined)) openMessage(ids[i + 1] ?? ids[0]);
        break;
      case 'k': case 'ArrowUp':
        if (i > 0) openMessage(ids[i - 1]);
        break;
      case 'r': if (current) compose({ fromMessage: current.id, mode: 'reply' }); break;
      case 'a': if (current) compose({ fromMessage: current.id, mode: 'reply_all' }); break;
      case 'f': if (current) compose({ fromMessage: current.id, mode: 'forward' }); break;
      case 'e': if (targets.length && folder()?.role !== 'archive') act('archive', targets); break;
      case '#': case 'Delete': if (targets.length) remove(targets); break;
      case 'u': if (targets.length) act('unread', targets); break;
      case 's': if (targets.length) act(targetMsgs[0]?.flagged ? 'unflag' : 'flag', targets); break;
      case 'i': if (targetMsgs.length) setKeyword(targetMsgs, IMPORTANT, !isImportant(targetMsgs[0])); break;
      case 'b': if (targetMsgs.length) snooze(targetMsgs); break;
      case 't': if (targetMsgs.length) editTags(targetMsgs); break;
      case 'v': if (targetMsgs.length) moveTo(targetMsgs); break;
      case 'c': compose(); break;
      case '/': listHead.querySelector('input[type=search]')?.focus(); break;
      case 'Escape': if (state.selected.size) { state.selected.clear(); drawList(); } else if (state.messageId) closeReader(); break;
      default: return;
    }
    e.preventDefault();
  }
  document.addEventListener('keydown', onKey);

  // ---- live updates ---------------------------------------------------------------------------
  const refreshList = debounce(() => loadMessages(), 300);
  const refreshNav = debounce(() => reloadFolders(), 300);
  const offs = [
    live.on('mail.changed', (ev) => {
      if (ev.what === 'accounts') { loadAccounts().catch(errorToast); return; }
      if (ev.what === 'tags') { loadTags().then(() => { drawNav(); drawList(); }); return; }
      if (ev.what === 'prefs') {
        loadPrefs().then(() => { drawNav(); loadMessages(); });
        return;
      }
      if (ev.what === 'snoozed') loadSnoozed().then(() => drawList());
      refreshNav();
      if (!ev.folder_id || ev.folder_id === state.folderId || state.view || state.search || state.filter) refreshList();
      if (state.messageId && ev.what === 'messages' && state.thread.length && ev.folder_id && state.thread.some((m) => m.folder_id === ev.folder_id)) {
        // Keep the conversation in step (a reply may have arrived).
        api.get(`/api/mail/messages/${encodeURIComponent(state.messageId)}/thread`).then(({ messages }) => {
          if (messages.length !== state.thread.length) { state.thread = messages; drawThread(state.messageId); }
        }).catch(() => {});
      }
    }),
    live.on('hello', () => { reloadFolders(); refreshList(); }),
  ];

  setPane(state.pane);
  drawNav();
  drawListHead();
  clear(listBody).appendChild(spinner());
  drawReaderEmpty();
  try {
    await Promise.all([loadPrefs(), loadTags(), loadQuickActions(), loadSnoozed()]);
    await loadAccounts();
  } catch (e) {
    errorToast(e);
  }
  if (state.messageId) openMessage(state.messageId);
  else if (ctx.query.mailto) openMailto(ctx.query.mailto);
  else if (ctx.query.compose) compose({ to: ctx.query.compose });
  // Back from a provider's consent screen.
  if (ctx.query.oauth === 'connected') toast('Account connected', 'ok');
  if (ctx.query.oauth_error) toast(ctx.query.oauth_error, 'error', { timeout: 9000 });
  if (ctx.query.oauth || ctx.query.oauth_error) setUrl();

  return () => {
    document.removeEventListener('keydown', onKey);
    offs.forEach((off) => off());
  };
}
