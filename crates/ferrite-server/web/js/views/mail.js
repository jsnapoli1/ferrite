// Mail: the user's IMAP/SMTP accounts in three panes (folders, messages, conversation),
// one pane at a time on phones. New mail and changes made elsewhere arrive live over the
// `mail.changed` event (the server watches the inbox with IMAP IDLE while you are online).
//
// Route: /mail?account=<id>&folder=<id>&m=<message id>. Keyboard: j/k next/previous,
// Enter open, r reply, a reply all, f forward, e archive, # delete, u unread, s flag,
// c compose, / search, Escape back.

import { api, q } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, menu, toast, errorToast, confirmDialog, bytes, dateTime, spinner, emptyState, debounce } from '../ui.js';
import { pickFolder } from './files-dialogs.js';
import { accountDialog } from './mail-setup.js';
import { shareDialog, quotaBar, startOAuth, PROVIDER_LABEL } from './mail-accounts.js';
import { openComposer } from './mail-compose.js';
import { senderAvatar, messageSecurity, remoteBanner, zipLink, readerMenuItems, openSecuritySettings } from './mail-security.js';

const ROLE_ICONS = { inbox: 'mail', sent: 'send', drafts: 'edit', trash: 'trash', junk: 'junk', archive: 'archive' };
const PAGE = 50;

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

export default async function render(root, ctx) {
  ctx.setTitle('Mail');
  const state = {
    accounts: [],
    folders: {}, // account id -> folders
    accountId: ctx.query.account || null,
    folderId: ctx.query.folder || null,
    messageId: ctx.query.m || null,
    messages: [],
    hasMore: false,
    loadingMore: false,
    selected: new Set(),
    unreadOnly: false,
    search: '',
    lastClicked: null,
    thread: [],
    pane: ctx.query.m ? 'reader' : 'list',
    openSeq: 0,
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
  const folder = () => folders().find((f) => f.id === state.folderId);
  const folderByRole = (role, accountId = state.accountId) => (state.folders[accountId] || []).find((f) => f.role === role);

  function setUrl() {
    history.replaceState({}, '', `/mail${q({ account: state.accountId, folder: state.folderId, m: state.messageId })}`);
  }

  function setPane(p) {
    state.pane = p;
    page.classList.toggle('pane-folders', p === 'folders');
    page.classList.toggle('pane-reader', p === 'reader');
    page.classList.toggle('pane-list', p === 'list');
  }

  // ---- folders pane ---------------------------------------------------------------------
  function drawNav() {
    clear(nav);
    nav.appendChild(h('div.ml-nav-head',
      h('h1', 'Mail'),
      state.accounts.length > 0 && h('button.btn.primary.sm.ml-compose-btn', { type: 'button', onclick: () => compose(), title: 'Compose (c)' }, icon('edit', 'sm'), h('span', 'Compose'))));
    const tree = h('div.ml-folders');
    const many = state.accounts.length > 1;
    state.accounts.forEach((a, index) => {
      const p = a.permissions || {};
      const items = [
        { label: 'Refresh', icon: 'refresh', run: () => refreshAccount(a.id) },
        p.configure && { label: 'Edit account', icon: 'settings', run: () => editAccount(a) },
        p.share && { label: 'Share…', icon: 'share', run: () => shareDialog(a).then(loadAccounts).catch(errorToast) },
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
      for (const f of state.folders[a.id] || []) {
        const depth = f.delimiter ? f.name.split(f.delimiter).length - 1 : 0;
        const active = f.id === state.folderId && !state.search;
        tree.appendChild(h(`button.ml-folder${active ? '.active' : ''}${f.selectable ? '' : '.disabled'}`, {
          type: 'button', disabled: !f.selectable, dataset: { id: f.id, role: f.role || '' },
          style: { paddingLeft: `${10 + Math.min(depth, 4) * 14}px` },
          onclick: () => openFolder(a.id, f.id),
          ondragover: (e) => { if (f.selectable && e.dataTransfer.types.includes('application/x-ferrite-mail')) { e.preventDefault(); e.currentTarget.classList.add('drop'); } },
          ondragleave: (e) => e.currentTarget.classList.remove('drop'),
          ondrop: (e) => {
            e.preventDefault();
            e.currentTarget.classList.remove('drop');
            const ids = JSON.parse(e.dataTransfer.getData('application/x-ferrite-mail') || '[]');
            if (ids.length) act('move', ids, { folder_id: f.id });
          },
        }, icon(ROLE_ICONS[f.role] || 'folder', 'sm'), h('span.grow.ellipsis', f.display_name),
        f.unread > 0 && f.role !== 'sent' && f.role !== 'drafts' && f.role !== 'trash' && h('span.badge.ml-unread', String(f.unread))));
      }
      const quota = quotaBar(a);
      if (quota) tree.appendChild(quota);
    });
    nav.appendChild(tree);
    if (state.canAdd || state.accounts.length) {
      nav.appendChild(h('div.ml-nav-foot',
        state.canAdd && h('button.btn.ghost.sm', { type: 'button', onclick: addAccount }, icon('plus', 'sm'), 'Add account'),
        state.accounts.length > 0 && h('button.btn.ghost.sm', { type: 'button', onclick: () => securitySettings() }, icon('lock', 'sm'), 'Privacy and security')));
    }
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

  // ---- message list -----------------------------------------------------------------------
  function drawListHead() {
    clear(listHead);
    const f = folder();
    const n = state.selected.size;
    if (n) {
      const ids = [...state.selected];
      const anyUnread = state.messages.some((m) => state.selected.has(m.id) && !m.seen);
      const anyUnflagged = state.messages.some((m) => state.selected.has(m.id) && !m.flagged);
      listHead.appendChild(h('div.ml-toolbar.ml-selection',
        h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Clear selection', onclick: () => { state.selected.clear(); drawList(); } }, icon('x', 'sm')),
        h('strong.grow', `${n} selected`),
        h('button.btn.ghost.icon.sm', { type: 'button', title: anyUnread ? 'Mark read' : 'Mark unread', 'aria-label': anyUnread ? 'Mark read' : 'Mark unread', onclick: () => act(anyUnread ? 'read' : 'unread', ids) }, icon(anyUnread ? 'eye' : 'mail', 'sm')),
        h('button.btn.ghost.icon.sm', { type: 'button', title: anyUnflagged ? 'Flag' : 'Unflag', 'aria-label': anyUnflagged ? 'Flag' : 'Unflag', onclick: () => act(anyUnflagged ? 'flag' : 'unflag', ids) }, icon('star', 'sm')),
        h('button.btn.ghost.icon.sm', { type: 'button', title: 'Move to…', 'aria-label': 'Move to folder', onclick: (e) => moveMenu(e.currentTarget, ids) }, icon('move', 'sm')),
        h('button.btn.ghost.icon.sm', { type: 'button', title: f?.role === 'junk' ? 'Not spam' : 'Report spam', 'aria-label': f?.role === 'junk' ? 'Not spam' : 'Report spam', onclick: () => act(f?.role === 'junk' ? 'not_spam' : 'spam', ids) }, icon(f?.role === 'junk' ? 'mail' : 'junk', 'sm')),
        f?.role !== 'archive' && h('button.btn.ghost.icon.sm', { type: 'button', title: 'Archive (e)', 'aria-label': 'Archive', onclick: () => act('archive', ids) }, icon('archive', 'sm')),
        h('button.btn.ghost.icon.sm.danger', { type: 'button', title: 'Delete (#)', 'aria-label': 'Delete', onclick: () => remove(ids) }, icon('trash', 'sm'))));
      return;
    }
    const search = h('input', {
      type: 'search', value: state.search, placeholder: account() ? `Search ${account().email}` : 'Search mail', 'aria-label': 'Search mail',
      onkeydown: (e) => {
        if (e.key === 'Enter') { e.preventDefault(); runSearch(search.value.trim()); }
        if (e.key === 'Escape' && search.value) { e.stopPropagation(); search.value = ''; runSearch(''); }
      },
      onsearch: () => { if (!search.value) runSearch(''); },
    });
    listHead.appendChild(h('div.ml-toolbar',
      h('button.btn.ghost.icon.sm.ml-show-folders', { type: 'button', 'aria-label': 'Folders', onclick: () => setPane('folders') }, icon('menu', 'sm')),
      h('h2.grow.ellipsis', state.search ? `Results for “${state.search}”` : (f ? f.display_name : 'Mail')),
      !state.search && h(`button.btn.ghost.sm${state.unreadOnly ? '.active' : ''}`, { type: 'button', 'aria-pressed': String(state.unreadOnly), onclick: () => { state.unreadOnly = !state.unreadOnly; loadMessages(); } }, 'Unread'),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Refresh', title: 'Refresh', onclick: () => refreshAccount(state.accountId, true) }, icon('refresh', 'sm'))),
    h('div.ml-search', icon('search', 'sm'), search));
  }

  function row(m) {
    const sel = state.selected.has(m.id);
    const open = m.id === state.messageId;
    const sent = ['sent', 'drafts'].includes(folder()?.role);
    const who = sent ? (m.to.length ? m.to.map((a) => a.name || a.address).join(', ') : '(no recipients)') : person(m.from_name, m.from_addr);
    const el = h(`div.ml-row${m.seen ? '' : '.unread'}${sel ? '.selected' : ''}${open ? '.open' : ''}`, {
      role: 'option', tabindex: '-1', 'aria-selected': String(open || sel), dataset: { id: m.id }, draggable: 'true',
      onclick: (e) => {
        if (e.target.closest('.ml-check')) return;
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
      h('input', { type: 'checkbox', checked: sel, 'aria-label': `Select message from ${who}`, onclick: (e) => toggleSelect(m.id, e.shiftKey) })),
    senderAvatar(who, sent ? (m.to[0]?.address || '') : m.from_addr),
    h('div.ml-row-main',
      h('div.ml-row-top',
        h('span.ml-from.ellipsis', who),
        m.thread_size > 1 && h('span.ml-count', String(m.thread_size)),
        h('span.ml-date', { title: dateTime(m.date) }, listDate(m.date))),
      h('div.ml-row-mid',
        h('span.ml-subject.ellipsis', m.subject || '(no subject)'),
        m.has_attachments && h('span.ml-icon', { title: 'Has attachments' }, icon('paperclip', 'sm')),
        h('button.ml-flag', {
          type: 'button', 'aria-label': m.flagged ? 'Unflag' : 'Flag', 'aria-pressed': String(m.flagged), class: m.flagged ? 'on' : '',
          onclick: (e) => { e.stopPropagation(); act(m.flagged ? 'unflag' : 'flag', [m.id]); },
        }, icon('star', 'sm'))),
      h('div.ml-preview.ellipsis', m.draft ? h('span.ml-draft', 'Draft ') : null, m.preview)));
    return el;
  }

  function drawList() {
    drawListHead();
    clear(listBody);
    if (!state.accounts.length) return;
    if (!state.messages.length) {
      listBody.appendChild(h('div.ml-empty.small.muted', state.search ? 'No messages match.' : (state.unreadOnly ? 'No unread messages.' : 'This folder is empty.')));
      return;
    }
    for (const m of state.messages) listBody.appendChild(row(m));
    if (state.hasMore && !state.search) {
      listBody.appendChild(h('div.ml-more', h('button.btn.ghost.sm', { type: 'button', onclick: loadMore }, state.loadingMore ? spinner() : 'Load older messages')));
    }
  }

  function toggleSelect(id, range) {
    if (range && state.lastClicked) {
      const ids = state.messages.map((m) => m.id);
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
      const { messages } = await api.get(`/api/mail/messages/${encodeURIComponent(id)}/thread`);
      if (seq !== state.openSeq) return;
      state.thread = messages;
      // Opened from a link (search, notification): show the folder the message lives in.
      const opened = messages.find((m) => m.id === id);
      if (opened && opened.folder_id !== state.folderId && !state.search) {
        state.accountId = opened.account_id;
        state.folderId = opened.folder_id;
        setUrl();
        drawNav();
        loadMessages();
      }
      await drawThread(id);
      const unseen = messages.filter((m) => !m.seen).map((m) => m.id);
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
          { label: 'Download (.eml)', icon: 'download', run: () => { location.href = `/api/mail/messages/${encodeURIComponent(current.id)}/raw`; } },
          ...readerMenuItems(current),
        ]),
      }, icon('more', 'sm'))));
    const cards = h('div.ml-cards');
    box.appendChild(cards);
    for (const m of thread) {
      // The opened message and unread ones start expanded; the rest show one line.
      const expanded = m.id === current.id || !m.seen;
      cards.appendChild(messageCard(m, expanded));
    }
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
    senderAvatar(person(m.from_name, m.from_addr), m.from_addr),
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
    // Security banners (phishing, external sender, DKIM, S/MIME, PGP), filled when the
    // server's report arrives; trusted senders' images load right away.
    const content = h('div.ml-content');
    let showRemote = () => {};
    const security = messageSecurity(message, {
      onTrusted: () => showRemote(),
      pgpHost: () => content,
      onSettings: (tab) => securitySettings(tab),
    });
    body.appendChild(security.el);
    body.appendChild(content);
    if (message.has_html) {
      let remote = false;
      showRemote = () => { if (remote) return; remote = true; banner?.remove(); frame.src = src(); };
      const banner = message.remote_content && remoteBanner(message, () => showRemote());
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
      if (banner) content.appendChild(banner);
      content.appendChild(frame);
    } else {
      content.appendChild(textBody(message.text));
    }
    const files = message.attachments.filter((a) => !a.inline);
    if (files.length) {
      body.appendChild(h('div.ml-attachments', zipLink(message, files), files.map((a) => {
        const href = `/api/mail/messages/${encodeURIComponent(message.id)}/attachments/${a.index}`;
        return h('div.ml-attachment',
          icon('paperclip', 'sm'),
          h('a.ellipsis', { href, download: a.name, title: `Download ${a.name}` }, a.name),
          h('span.small.muted', bytes(a.size)),
          h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Save ${a.name} to Files`, title: 'Save to Files', onclick: () => saveToFiles(message.id, a) }, icon('files', 'sm')));
      })));
    }
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
  async function act(action, ids, { folder_id, quiet = false } = {}) {
    if (!ids.length) return;
    const leaving = ['archive', 'delete', 'move', 'spam', 'not_spam'].includes(action);
    // Optimistic update of the list; the server event refreshes it for real.
    const idx = state.messages.findIndex((m) => ids.includes(m.id));
    for (const m of state.messages) {
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
    let next = null;
    let closedOpen = false;
    if (leaving) {
      const remaining = state.messages.filter((m) => !ids.includes(m.id));
      if (state.messageId && ids.includes(state.messageId)) {
        next = remaining[Math.min(Math.max(idx, 0), remaining.length - 1)]?.id || null;
        // The open message is leaving this folder: stop showing (and loading) it now.
        closedOpen = true;
        state.messageId = null;
        state.thread = [];
        state.openSeq++;
        setUrl();
        drawReaderEmpty();
      }
      state.messages = remaining;
      state.selected.clear();
    }
    const deleteForGood = folder()?.role === 'trash';
    drawList();
    try {
      await api.post('/api/mail/actions', { ids, action, folder_id });
      if (!quiet && leaving) {
        const what = { archive: 'Archived', delete: deleteForGood ? 'Deleted' : 'Moved to Trash', move: 'Moved', spam: 'Reported as spam:', not_spam: 'Moved to Inbox:' }[action];
        toast(`${what} ${ids.length === 1 ? '1 message' : `${ids.length} messages`}`, 'ok', { timeout: 2500 });
      }
      if (closedOpen) {
        if (next && !matchMedia('(max-width: 760px)').matches) openMessage(next);
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
    if (folder()?.role === 'trash') {
      const ok = await confirmDialog('Delete for good', `Permanently delete ${ids.length === 1 ? 'this message' : `${ids.length} messages`}? This cannot be undone.`, { confirm: 'Delete', danger: true });
      if (!ok) return;
    }
    act('delete', ids);
  }

  function moveMenu(anchor, ids) {
    const items = folders().filter((f) => f.selectable && f.id !== state.folderId)
      .map((f) => ({ label: f.display_name, icon: ROLE_ICONS[f.role] || 'folder', run: () => act('move', ids, { folder_id: f.id }) }));
    menu(anchor, items.length ? items : [{ label: 'No other folders', run: () => {} }]);
  }

  // ---- composing ------------------------------------------------------------------------------
  function compose(opts = {}) {
    if (!state.accounts.length) { addAccount(); return; }
    openComposer(ctx, { accounts: state.accounts, accountId: state.accountId, ...opts });
  }

  function securitySettings(tab) {
    openSecuritySettings(ctx, { tab, accounts: state.accounts }).then(() => {
      // Trust changes may show images now: redraw the open conversation.
      if (state.messageId && state.thread.length) drawThread(state.messageId);
    });
  }

  // ---- accounts -------------------------------------------------------------------------------
  async function addAccount() {
    const a = await accountDialog(ctx, null);
    if (!a) return;
    state.accountId = a.id;
    state.folderId = null;
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

  async function refreshAccount(accountId, withFolder = false) {
    if (!accountId) return;
    try {
      const { folders: fs } = await api.post(`/api/mail/accounts/${encodeURIComponent(accountId)}/sync`, withFolder && state.folderId ? { folder_id: state.folderId } : {});
      state.folders[accountId] = fs;
      drawNav();
      if (accountId === state.accountId) await loadMessages();
    } catch (e) {
      errorToast(e);
    }
  }

  // ---- loading ------------------------------------------------------------------------------
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
    drawNav();
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

  async function loadMessages() {
    if (state.search) return runSearch(state.search);
    if (!state.folderId) { state.messages = []; drawList(); return undefined; }
    const folderId = state.folderId;
    try {
      const page = await api.get(`/api/mail/folders/${encodeURIComponent(folderId)}/messages${q({ limit: Math.max(PAGE, state.messages.length), unread: state.unreadOnly ? 'true' : '' })}`);
      if (folderId !== state.folderId) return undefined;
      state.messages = page.messages;
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
    try {
      const last = state.messages[state.messages.length - 1];
      const page = await api.get(`/api/mail/folders/${encodeURIComponent(state.folderId)}/messages${q({ before: last.id, limit: PAGE, unread: state.unreadOnly ? 'true' : '' })}`);
      const known = new Set(state.messages.map((m) => m.id));
      state.messages.push(...page.messages.filter((m) => !known.has(m.id)));
      state.hasMore = page.has_more;
    } catch (e) {
      errorToast(e);
    }
    state.loadingMore = false;
    drawList();
  }

  async function runSearch(term) {
    state.search = term;
    state.selected.clear();
    if (!term) { drawNav(); return loadMessages(); }
    drawNav();
    clear(listBody).appendChild(spinner());
    drawListHead();
    try {
      const { messages } = await api.get(`/api/mail/search${q({ q: term, account_id: state.accountId })}`);
      if (term !== state.search) return undefined;
      state.messages = messages;
      state.hasMore = false;
    } catch (e) {
      errorToast(e);
    }
    drawList();
    return undefined;
  }

  async function openFolder(accountId, folderId) {
    state.accountId = accountId;
    state.folderId = folderId;
    state.search = '';
    state.selected.clear();
    state.messages = [];
    state.unreadOnly = false;
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
    const ids = state.messages.map((m) => m.id);
    const i = ids.indexOf(state.messageId);
    const targets = state.selected.size ? [...state.selected] : (state.messageId ? [state.messageId] : []);
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
      case 's': if (targets.length) act(state.messages.find((m) => m.id === targets[0])?.flagged ? 'unflag' : 'flag', targets); break;
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
      refreshNav();
      if (!ev.folder_id || ev.folder_id === state.folderId || state.search) refreshList();
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
    await loadAccounts();
  } catch (e) {
    errorToast(e);
  }
  if (state.messageId) openMessage(state.messageId);
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
