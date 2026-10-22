// Talk: conversation list (left) and the open conversation (right). Switching between
// conversations happens inside the view (history.pushState) so the list, its scroll position
// and filter survive; live events patch single list items instead of re-rendering the list.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, modal, menu, toast, errorToast, emptyState, spinner, promptDialog } from '../ui.js';
import { roomAvatar, messagePreview, shortTime, userSearch } from './talk-util.js';
import { createRoomPane } from './talk-chat.js';

export default async function render(root, ctx) {
  const me = ctx.me.user;
  const self = { id: me.id, kind: 'user', name: me.display_name, username: me.username };
  ctx.setTitle('Talk');

  let rooms = [];
  const items = new Map(); // token -> list item element
  let current = null; // {token, pane}
  let openSeq = 0;
  const offs = [];

  // ---- layout -----------------------------------------------------------------------------
  const filterInput = h('input', { type: 'search', placeholder: 'Filter conversations', 'aria-label': 'Filter conversations' });
  const listEl = h('nav.talk-list-items', { 'aria-label': 'Conversations' });
  const newBtn = h('button.btn.primary.sm', { onclick: () => newMenu(newBtn), 'aria-haspopup': 'menu' }, icon('plus', 'sm'), 'New');
  const listPane = h('aside.talk-list',
    h('div.talk-list-head', h('h1.grow', 'Talk'), newBtn),
    h('div.talk-list-filter', filterInput),
    listEl);
  const roomHost = h('div.talk-main');
  const layout = h('div.talk-layout', listPane, roomHost);
  root.appendChild(h('div.page-full.talk', layout));
  filterInput.addEventListener('input', applyFilter);
  filterInput.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { filterInput.value = ''; applyFilter(); }
    if (e.key === 'Enter') listEl.querySelector('.talk-item:not([hidden])')?.click();
  });

  // ---- list ----------------------------------------------------------------------------------
  const sortKey = (r) => [r.favorite ? 1 : 0, new Date(r.last_message?.created_at || r.last_activity_at).getTime()];
  function sortRooms() {
    rooms.sort((a, b) => {
      const [fa, ta] = sortKey(a);
      const [fb, tb] = sortKey(b);
      return fb - fa || tb - ta;
    });
  }

  function renderItem(r) {
    const active = current?.token === r.token;
    const muted = r.notify === 'none';
    const unread = r.unread > 0;
    let badge = '';
    if (unread) {
      badge = r.unread_mention
        ? h('span.badge.talk-mention', { title: 'You were mentioned', 'aria-label': `${r.unread} unread, mentioned` }, `@ ${r.unread > 99 ? '99+' : r.unread}`)
        : h(`span.badge${muted ? '.muted' : ''}`, { 'aria-label': `${r.unread} unread` }, r.unread > 99 ? '99+' : String(r.unread));
    }
    const el = h(`a.talk-item${active ? '.active' : ''}${unread ? '.unread' : ''}`, {
      href: `/talk/${r.token}`,
      'aria-current': active ? 'page' : null,
      dataset: { token: r.token },
      onclick: (e) => {
        if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
        e.preventDefault();
        openRoom(r.token, { push: true });
      },
    },
    roomAvatar(r),
    h('div.grow.talk-item-text',
      h('div.talk-item-top',
        h('span.talk-item-name.ellipsis', r.display_name),
        r.favorite ? h('span.talk-fav', { title: 'Favorite' }, icon('star', 'sm')) : '',
        r.call_active ? h('span.talk-item-call', { title: 'Call in progress', 'aria-label': 'Call in progress' }, icon('phone', 'sm')) : '',
        h('time.talk-item-time', shortTime(r.last_message?.created_at || r.last_activity_at))),
      h('div.talk-item-bottom',
        h('span.talk-item-preview.ellipsis', messagePreview(r.last_message, me.id) || (r.description || '')),
        muted ? h('span.muted', { title: 'Notifications off' }, icon('bell', 'sm')) : '',
        badge)));
    if (filterInput.value && !matchesFilter(r)) el.hidden = true;
    return el;
  }

  function paintList() {
    sortRooms();
    clear(listEl);
    items.clear();
    if (!rooms.length) {
      listEl.appendChild(emptyState('talk', 'No conversations yet', 'Start one with the New button.'));
      return;
    }
    for (const r of rooms) {
      const el = renderItem(r);
      items.set(r.token, el);
      listEl.appendChild(el);
    }
  }

  /** Re-renders one item and moves items into sorted order (moves only, no rebuilds). */
  function patchRoom(r, { reorder = false } = {}) {
    const old = items.get(r.token);
    if (!old) return paintList();
    const el = renderItem(r);
    old.replaceWith(el);
    items.set(r.token, el);
    if (reorder) {
      sortRooms();
      rooms.forEach((x) => { const it = items.get(x.token); if (it) listEl.appendChild(it); });
    }
  }

  const matchesFilter = (r) => r.display_name.toLowerCase().includes(filterInput.value.trim().toLowerCase());
  function applyFilter() {
    for (const r of rooms) {
      const el = items.get(r.token);
      if (el) el.hidden = !matchesFilter(r);
    }
  }

  async function loadRooms() {
    const res = await api.get('/api/talk/rooms');
    rooms = res.rooms;
    paintList();
  }

  function upsertRoom(view) {
    const i = rooms.findIndex((r) => r.token === view.token);
    if (i >= 0) rooms[i] = view; else rooms.push(view);
    paintList();
  }

  const byId = (id) => rooms.find((r) => r.id === id);

  // ---- open conversation -----------------------------------------------------------------------
  async function openRoom(token, { push = false, replace = false, query = {} } = {}) {
    if (current && current.token === token) return;
    const seq = ++openSeq;
    if (current) {
      current.pane.destroy();
      current = null;
    }
    const path = token ? `/talk/${token}` : '/talk';
    if (push) history.pushState({}, '', path);
    if (replace) history.replaceState({}, '', path);
    layout.classList.toggle('has-room', !!token);
    markActive(token);
    if (!token) {
      ctx.setTitle('Talk');
      clear(roomHost).appendChild(emptyState('talk', 'Pick a conversation', 'Choose a conversation on the left, or start a new one.',
        h('button.btn.primary', { onclick: (e) => newMenu(e.currentTarget) }, icon('plus', 'sm'), 'New conversation')));
      return;
    }
    clear(roomHost).appendChild(spinner());
    let view;
    try {
      ({ room: view } = await api.get(`/api/talk/rooms/${encodeURIComponent(token)}`));
    } catch (e) {
      if (seq !== openSeq) return;
      return showNotJoined(token, e);
    }
    if (seq !== openSeq) return;
    const known = rooms.find((r) => r.token === token);
    // Breakout rooms are reached through their main conversation and are not listed.
    if (view.kind === 'breakout') markActive(view.parent_token);
    else if (!known) upsertRoom(view);
    else Object.assign(known, view);
    ctx.setTitle(view.display_name);
    const pane = createRoomPane({
      token,
      room: view,
      self,
      guest: false,
      config: ctx.config,
      rootId: ctx.me.root_id,
      query,
      onBack: () => openRoom(null, { push: true }),
      onRead: (roomId) => {
        const r = byId(roomId);
        if (r && (r.unread || r.unread_mention)) { r.unread = 0; r.unread_mention = false; patchRoom(r); }
      },
      onRoomChange: (patch) => {
        const r = rooms.find((x) => x.token === token);
        if (!r) return;
        Object.assign(r, patch);
        patchRoom(r, { reorder: 'favorite' in patch });
        if (patch.display_name) ctx.setTitle(patch.display_name);
      },
      onLeft: () => {
        rooms = rooms.filter((r) => r.token !== token);
        paintList();
        openRoom(null, { push: true });
      },
      onOpenRoom: (t, o = {}) => openRoom(t, { push: true, query: o.call ? { call: '1' } : {} }),
    });
    current = { token, pane, kind: view.kind };
    clear(roomHost).appendChild(pane.el);
  }

  function markActive(token) {
    for (const [t, el] of items) {
      el.classList.toggle('active', t === token);
      if (t === token) el.setAttribute('aria-current', 'page'); else el.removeAttribute('aria-current');
    }
  }

  /** Joins a public conversation, asking for its password when it has one. */
  async function joinPublic(token, hasPassword) {
    let password = null;
    if (hasPassword) {
      password = await promptDialog('Password required', { label: 'This conversation is protected by a password', type: 'password', confirm: 'Join' });
      if (!password) return null;
    }
    return (await api.post(`/api/talk/rooms/${encodeURIComponent(token)}/join`, password ? { password } : {})).room;
  }

  /** The user isn't in this conversation: offer to join a public one. */
  async function showNotJoined(token, err) {
    let info = null;
    try { ({ room: info } = await api.get(`/api/talk/guest/${encodeURIComponent(token)}`)); } catch (_) { /* not public */ }
    ctx.setTitle(info ? info.name : 'Not found');
    clear(roomHost).appendChild(info
      ? emptyState('globe', info.name, info.description || 'This is a public conversation you have not joined yet.',
        h('button.btn.primary', { onclick: async () => {
          try {
            const room = await joinPublic(token, info.has_password);
            if (!room) return;
            upsertRoom(room);
            current = null;
            openRoom(token);
          } catch (e) { errorToast(e); }
        } }, info.has_password ? icon('lock', 'sm') : '', 'Join conversation'))
      : emptyState('talk', 'Conversation not found', err.status === 404 ? 'It may have been deleted, or you are no longer a participant.' : err.message,
        h('button.btn', { onclick: () => openRoom(null, { push: true }) }, 'Back to conversations')));
  }

  // ---- new conversations -----------------------------------------------------------------------
  function newMenu(anchor) {
    menu(anchor, [
      { label: 'Group conversation', icon: 'users', run: () => createRoom('group') },
      { label: 'Public conversation', icon: 'globe', run: () => createRoom('public') },
      { label: 'Direct message', icon: 'user', run: directMessage },
      { label: 'Note to self', icon: 'note', run: noteToSelf },
      'sep',
      { label: 'Browse open conversations', icon: 'search', run: browseOpen },
    ]);
  }

  async function opened(view) {
    if (!view) return;
    upsertRoom(view);
    openRoom(view.token, { push: true });
  }

  async function createRoom(kind) {
    const name = h('input', { type: 'text', 'aria-label': 'Conversation name', placeholder: kind === 'public' ? 'e.g. Community' : 'e.g. Project team', maxlength: 200 });
    const invited = new Map();
    const chips = h('div.talk-chips');
    const paintChips = () => clear(chips).append(...[...invited.values()].map((u) => h('span.pill.talk-chip', avatar(u.display_name, { size: 'sm', seed: u.id }), u.display_name,
      h('button', { type: 'button', 'aria-label': `Remove ${u.display_name}`, onclick: () => { invited.delete(u.username); paintChips(); } }, icon('x', 'sm')))));
    const search = userSearch({
      placeholder: 'Search people to invite…',
      label: 'Invite people',
      exclude: () => [me.username, ...invited.keys()],
      onPick: (u) => { invited.set(u.username, u); paintChips(); },
    });
    const v = await modal({
      title: kind === 'public' ? 'New public conversation' : 'New group conversation',
      body: h('form', { onsubmit: (e) => e.preventDefault() },
        h('label', 'Name'), name,
        kind === 'public' ? h('p.small.muted', 'Anyone on this server can find and join it, and you can invite guests with a link.') : '',
        h('label', 'Invite people'), search.el, chips),
      actions: [{ label: 'Cancel', value: null }, {
        label: 'Create', kind: 'primary', run: async () => {
          if (!name.value.trim()) { name.focus(); toast('Give the conversation a name', 'warn'); return undefined; }
          const { room } = await api.post('/api/talk/rooms', { name: name.value.trim(), kind, invite: [...invited.keys()] });
          return room;
        },
      }],
    });
    opened(v);
  }

  async function directMessage() {
    const v = await modal({
      title: 'Direct message',
      body: (close) => {
        const s = userSearch({
          placeholder: 'Who do you want to talk to?',
          label: 'Find a person',
          exclude: () => [me.username],
          onPick: async (u) => {
            try { close((await api.post('/api/talk/direct', { username: u.username })).room); } catch (e) { errorToast(e); }
          },
        });
        return h('div', h('p.small.muted', 'Start a one-to-one conversation.'), s.el);
      },
    });
    opened(v);
  }

  async function noteToSelf() {
    try { opened((await api.post('/api/talk/note-to-self')).room); } catch (e) { errorToast(e); }
  }

  async function browseOpen() {
    const v = await modal({
      title: 'Open conversations',
      body: (close) => {
        const box = h('div.talk-browse', spinner());
        api.get('/api/talk/open').then(({ rooms: open }) => {
          clear(box);
          if (!open.length) { box.appendChild(emptyState('globe', 'No open conversations', 'Public conversations you have not joined appear here.')); return; }
          for (const r of open) {
            box.appendChild(h('div.list-item', roomAvatar({ ...r, display_name: r.name }),
              h('div.grow', h('div.ellipsis', h('strong', r.name)), r.description ? h('div.small.muted.ellipsis', r.description) : ''),
              r.has_password ? h('span.muted', { title: 'Needs a password' }, icon('lock', 'sm')) : '',
              h('button.btn.sm.primary', { onclick: async () => {
                try {
                  const joined = await joinPublic(r.token, r.has_password);
                  if (joined) close(joined);
                } catch (e) { errorToast(e); }
              } }, 'Join')));
          }
        }).catch((e) => { clear(box).appendChild(h('p.error-text', e.message)); });
        return box;
      },
    });
    opened(v);
  }

  // ---- live updates ----------------------------------------------------------------------------
  offs.push(
    live.on('talk.message', (ev) => {
      const r = byId(ev.room_id);
      if (!r) { reloadSoon(); return; }
      const m = ev.message;
      r.last_message = m;
      r.last_activity_at = m.created_at;
      const fromOther = m.actor_id !== me.id && m.kind !== 'system';
      const watching = current?.token === r.token && document.visibilityState === 'visible' && document.hasFocus();
      if (fromOther && !watching) {
        r.unread += 1;
        const mention = new RegExp(`(^|[^\\w@])@(${me.username.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}|all)(?![\\w-])`, 'i');
        if (r.kind === 'one_to_one' || mention.test(m.body)) r.unread_mention = true;
      }
      patchRoom(r, { reorder: true });
    }),
    live.on('talk.message_updated', (ev) => {
      const r = byId(ev.room_id);
      if (r && r.last_message && r.last_message.id === ev.message.id) { r.last_message = ev.message; patchRoom(r); }
    }),
    live.on('talk.read', (ev) => {
      const r = byId(ev.room_id);
      if (r && ev.user_id === me.id) {
        r.last_read_message_id = Math.max(r.last_read_message_id, ev.message_id);
        if (!r.last_message || ev.message_id >= r.last_message.id) { r.unread = 0; r.unread_mention = false; }
        patchRoom(r);
      }
    }),
    live.on('talk.room_updated', (ev) => {
      const r = byId(ev.room.id);
      if (!r) return;
      Object.assign(r, { name: ev.room.name, description: ev.room.description, ai_enabled: ev.room.ai_enabled, read_only: ev.room.read_only });
      if (r.kind !== 'one_to_one') r.display_name = ev.room.name;
      patchRoom(r);
    }),
    live.on('talk.rooms_changed', () => reloadSoon()),
    // Someone in a breakout room asks the moderators (us) for help.
    live.on('talk.breakout_help', (ev) => {
      toast(`${ev.actor.name} asks for help in ${ev.room.name}`, 'warn', {
        timeout: 15000,
        action: { label: 'Go to room', run: () => openRoom(ev.room.token, { push: true }) },
      });
    }),
    live.on('call.joined', (ev) => {
      const r = byId(ev.room_id);
      if (r && !r.call_active) { r.call_active = true; patchRoom(r); }
    }),
    live.on('call.left', (ev) => {
      const r = byId(ev.room_id);
      if (r && ev.ended && r.call_active) { r.call_active = false; patchRoom(r); }
    }),
    live.on('hello', () => reloadSoon()),
  );

  let reloadTimer = null;
  function reloadSoon() {
    clearTimeout(reloadTimer);
    reloadTimer = setTimeout(async () => {
      try {
        await loadRooms();
        // The open conversation was deleted or we were removed from it (breakout rooms are
        // not listed; their pane follows the main conversation instead).
        if (current && current.kind !== 'breakout' && !rooms.some((r) => r.token === current.token)) {
          toast('You are no longer in that conversation', 'info');
          openRoom(null, { replace: true });
        }
      } catch (_) { /* offline: the next hello reloads */ }
    }, 200);
  }

  // ---- start -----------------------------------------------------------------------------------
  listEl.appendChild(spinner());
  await loadRooms();
  await openRoom(ctx.params.token || null, { query: ctx.query });

  return () => {
    offs.forEach((off) => off());
    clearTimeout(reloadTimer);
    current?.pane.destroy();
    current = null;
  };
}
