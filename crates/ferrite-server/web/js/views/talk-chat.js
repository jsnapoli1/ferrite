// The conversation pane shared by signed-in users (talk.js) and guests (guest.js): header,
// call banner and stage, message list, typing indicator, composer and participants panel.
//
// The message list is patched in place: each message is one node, live events append,
// replace or re-group single nodes, and older pages are inserted above while keeping the
// scroll position. The list stays pinned to the bottom while the user is at the bottom.

import { api } from '../api.js';
import { live } from '../ws.js';
import { markdown } from '../markdown.js';
import {
  h, clear, icon, avatar, modal, confirmDialog, promptDialog, menu, toast, errorToast,
  timeOnly, dateTime, bytes, spinner, copyText, debounce,
} from '../ui.js';
import {
  QUICK_EMOJI, MORE_EMOJI, talkApi, isLiveDoc, metaIcon, roomAvatar, sameDay, dayLabel,
  suggestList, userSearch,
} from './talk-util.js';
import { Call } from './talk-call.js';

const GROUP_GAP_MS = 5 * 60 * 1000;
const TYPING_SEND_MS = 3000;
const TYPING_SHOW_MS = 5000;
const PAGE = 50;

/** Display name without the server's " (guest)" suffix (for avatars' initials). */
const plainName = (name) => (name || '?').replace(/\s*\(guest\)$/, '');

/**
 * opts: {
 *   token, room,                 // RoomView (guests: {token, name, display_name, description, kind})
 *   self: {id, kind, name, username},
 *   guest, headers, config, rootId, query: {call, message},
 *   onBack(), onRead(roomId, messageId), onRoomChange(patch), onLeft()
 * }
 * Returns {el, destroy(), setRoom(patch)}.
 */
export function createRoomPane(opts) {
  const { token, self, guest, config = {} } = opts;
  const tapi = talkApi(opts.headers);
  const base = `/api/talk/rooms/${encodeURIComponent(token)}`;
  let room = { ...opts.room };
  let roomId = room.id || null;
  const isMod = () => ['owner', 'moderator'].includes(room.role);
  const groupish = () => room.kind === 'group' || room.kind === 'public';

  const entries = new Map(); // id -> {m, el, sep, divider}
  const order = []; // message ids, ascending
  let hasMore = true;
  let loadingOlder = false;
  let pinned = true;
  let participants = [];
  let replyTo = null;
  let editing = null; // {id, pending}
  let lastReadSent = room.last_read_message_id || 0;
  let lastTypingSent = 0;
  let call = null;
  const callMembers = new Map();
  const typers = new Map();
  const offs = [];
  let destroyed = false;
  let popover = null;

  // ---- skeleton ------------------------------------------------------------------------
  const head = h('header.talk-room-head');
  const banner = h('div.talk-call-banner', { hidden: true, role: 'status' });
  const callHost = h('div.talk-call-host');
  const list = h('div.talk-messages', { role: 'log', 'aria-live': 'polite', 'aria-label': 'Messages' });
  const olderStatus = h('div.talk-older');
  const scroller = h('div.talk-scroll', { tabindex: 0 }, olderStatus, list);
  const jumpBtn = h('button.btn.sm.talk-jump', { hidden: true, onclick: () => scrollToBottom(true) }, icon('chevron-down', 'sm'), 'Latest');
  const typingLine = h('div.talk-typing', { 'aria-live': 'polite' });
  const composer = buildComposer();
  const chat = h('div.talk-chat', h('div.talk-scroll-wrap', scroller, jumpBtn), typingLine, composer.el);
  const side = h('aside.talk-side', { hidden: true, 'aria-label': 'Participants' });
  const body = h('div.talk-room-body', chat, side);
  const el = h('section.talk-room', { 'aria-label': room.display_name }, head, banner, callHost, body);

  paintHeader();

  // ---- helpers ------------------------------------------------------------------------
  const nearBottom = (px = 60) => scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < px;
  function scrollToBottom(smooth = false) {
    pinned = true;
    jumpBtn.hidden = true;
    scroller.scrollTo({ top: scroller.scrollHeight, behavior: smooth ? 'smooth' : 'auto' });
  }
  const participantName = (id) => {
    if (id === self.id) return 'You';
    const p = participants.find((x) => x.user_id === id);
    return p ? p.display_name : (id === 'assistant' ? 'Assistant' : 'Guest');
  };
  function closePopover() {
    if (popover) { popover.remove(); popover = null; }
  }

  // ---- header ---------------------------------------------------------------------------
  function paintHeader() {
    const sub = [];
    if (room.kind !== 'note_to_self' && room.participant_count) sub.push(`${room.participant_count} ${room.participant_count === 1 ? 'participant' : 'participants'}`);
    if (room.description) sub.push(room.description);
    const callBtn = h('button.btn.talk-call-btn', {
      'aria-label': callMembers.size ? 'Join call' : 'Start call',
      disabled: room.kind === 'note_to_self',
      onclick: (e) => menu(e.currentTarget, [
        { label: 'Video call', icon: 'video', run: () => startCall(true) },
        { label: 'Audio only', icon: 'phone', run: () => startCall(false) },
      ]),
    }, icon(callMembers.size ? 'phone' : 'video'), h('span.hide-narrow', callMembers.size ? 'Join call' : 'Call'));
    if (callMembers.size && !call) callBtn.classList.add('primary');
    callBtn.hidden = !!call;
    clear(head).append(
      opts.onBack ? h('button.btn.ghost.icon.talk-back', { 'aria-label': 'Back to conversations', onclick: opts.onBack }, icon('arrow-left')) : '',
      roomAvatar(room),
      h('div.grow.talk-room-titles',
        h('h2.talk-room-title.ellipsis', room.display_name, room.favorite ? h('span.talk-fav', { title: 'Favorite' }, icon('star', 'sm')) : ''),
        sub.length ? h('div.talk-room-sub.ellipsis', { title: room.description || '' }, sub.join(' · ')) : ''),
      callBtn,
      h('button.btn.ghost.icon', { 'aria-label': 'Participants', title: 'Participants', 'aria-pressed': String(!side.hidden), onclick: toggleSide }, icon('users')),
      guest ? '' : h('button.btn.ghost.icon', { 'aria-label': 'Conversation menu', title: 'More', onclick: (e) => roomMenu(e.currentTarget) }, icon('more')));
    composer.setPlaceholder();
  }

  function roomMenu(anchor) {
    menu(anchor, [
      isMod() && groupish() && { label: 'Rename', icon: 'edit', run: rename },
      isMod() && groupish() && { label: 'Edit description', icon: 'note', run: editDescription },
      room.kind !== 'note_to_self' && { label: 'Participants', icon: 'users', run: () => { if (side.hidden) toggleSide(); } },
      { label: 'Notifications…', icon: 'bell', run: () => notifyMenu(anchor) },
      { label: room.favorite ? 'Remove from favorites' : 'Add to favorites', icon: 'star', run: toggleFavorite },
      isMod() && groupish() && { label: room.ai_enabled ? 'Turn assistant off' : 'Turn assistant on', icon: 'assistant', run: () => patchRoom({ ai_enabled: !room.ai_enabled }) },
      room.kind === 'public' && { label: 'Copy guest link', icon: 'link', run: () => copyText(`${config.public_url || location.origin}/join/${token}`, 'Guest link copied') },
      'sep',
      room.kind !== 'note_to_self' && room.kind !== 'one_to_one' && { label: 'Leave conversation', icon: 'logout', danger: true, run: leaveRoom },
      room.role === 'owner' && { label: 'Delete conversation', icon: 'trash', danger: true, run: deleteRoom },
    ]);
  }

  function notifyMenu(anchor) {
    const opt = (v, label) => ({ label, icon: room.notify === v ? 'check' : null, run: async () => {
      try {
        await tapi.post(`${base}/prefs`, { notify: v });
        room.notify = v;
        opts.onRoomChange?.({ notify: v });
        toast(`Notifications: ${label.toLowerCase()}`, 'ok', { timeout: 2000 });
      } catch (e) { errorToast(e); }
    } });
    menu(anchor, [opt('all', 'All messages'), opt('mentions', 'Mentions only'), opt('none', 'Off')]);
  }

  async function patchRoom(patch) {
    try {
      const { room: r } = await tapi.patch(base, patch);
      applyRoomUpdate(r);
    } catch (e) { errorToast(e); }
  }

  function applyRoomUpdate(r) {
    const patch = { name: r.name, description: r.description, ai_enabled: r.ai_enabled, read_only: r.read_only };
    if (room.kind !== 'one_to_one') patch.display_name = r.name;
    Object.assign(room, patch);
    paintHeader();
    opts.onRoomChange?.(patch);
  }

  async function rename() {
    const name = await promptDialog('Rename conversation', { label: 'Name', value: room.name, confirm: 'Rename' });
    if (name) patchRoom({ name });
  }

  async function editDescription() {
    const ta = h('textarea', { 'aria-label': 'Description', rows: 4, value: room.description || '', placeholder: 'What is this conversation about?' });
    const v = await modal({
      title: 'Edit description',
      body: h('div', h('label', 'Description'), ta),
      actions: [{ label: 'Cancel', value: null }, { label: 'Save', kind: 'primary', run: () => ({ text: ta.value.trim() }) }],
    });
    if (v) patchRoom({ description: v.text });
  }

  async function toggleFavorite() {
    try {
      await tapi.post(`${base}/prefs`, { favorite: !room.favorite });
      room.favorite = !room.favorite;
      paintHeader();
      opts.onRoomChange?.({ favorite: room.favorite });
    } catch (e) { errorToast(e); }
  }

  async function leaveRoom() {
    if (!await confirmDialog('Leave conversation', `Leave “${room.display_name}”? You can be added again later.`, { confirm: 'Leave', danger: true })) return;
    try {
      await tapi.del(`${base}/participants/${encodeURIComponent(self.id)}`);
      opts.onLeft?.();
    } catch (e) { errorToast(e); }
  }

  async function deleteRoom() {
    if (!await confirmDialog('Delete conversation', `Delete “${room.display_name}” and all of its messages for everyone? This cannot be undone.`, { confirm: 'Delete', danger: true })) return;
    try {
      await tapi.del(base);
      opts.onLeft?.();
    } catch (e) { errorToast(e); }
  }

  // ---- participants panel ------------------------------------------------------------
  async function loadParticipants() {
    try {
      ({ participants } = await tapi.get(`${base}/participants`));
      if (room.participant_count !== participants.length && room.kind !== 'note_to_self') {
        room.participant_count = participants.length;
        paintHeader();
      }
      if (!side.hidden) paintSide();
      const mine = participants.find((p) => p.user_id === self.id);
      if (mine && mine.role !== room.role && !guest) { room.role = mine.role; paintHeader(); }
    } catch (_) { /* removed from the room: the list view handles it */ }
  }
  const reloadParticipants = debounce(loadParticipants, 400);

  function toggleSide() {
    side.hidden = !side.hidden;
    if (!side.hidden) { paintSide(); loadParticipants(); }
    paintHeader();
  }

  function paintSide() {
    const canManage = !guest && isMod() && groupish();
    const add = canManage ? userSearch({
      placeholder: 'Add people…',
      label: 'Add a participant',
      exclude: () => participants.map((p) => p.username),
      onPick: async (u) => {
        try {
          await tapi.post(`${base}/participants`, { username: u.username });
          toast(`${u.display_name} was added`, 'ok', { timeout: 2000 });
          loadParticipants();
        } catch (e) { errorToast(e); }
      },
    }) : null;
    const roleLabel = { owner: 'Owner', moderator: 'Moderator' };
    clear(side).append(
      h('div.talk-side-head', h('h3.grow', `Participants (${participants.length})`),
        h('button.btn.ghost.icon.sm', { 'aria-label': 'Close participants', onclick: toggleSide }, icon('x'))),
      add ? h('div.talk-side-add', add.el) : '',
      h('div.talk-side-list', participants.map((p) => {
        const me = p.user_id === self.id;
        const actions = [];
        if (canManage && !me && p.role !== 'owner') {
          actions.push(p.role === 'moderator'
            ? { label: 'Demote to member', icon: 'user', run: () => setRole(p, 'member') }
            : { label: 'Promote to moderator', icon: 'key', run: () => setRole(p, 'moderator') });
          actions.push({ label: 'Remove', icon: 'x', danger: true, run: () => removeParticipant(p) });
        }
        return h('div.list-item.talk-person',
          avatar(p.display_name, { seed: p.user_id, online: p.online }),
          h('div.grow', h('div.ellipsis', p.display_name, me ? h('span.muted', ' (you)') : ''), h('div.small.muted.ellipsis', `@${p.username}`)),
          roleLabel[p.role] ? h('span.pill', roleLabel[p.role]) : '',
          actions.length ? h('button.btn.ghost.icon.sm', { 'aria-label': `Manage ${p.display_name}`, onclick: (e) => menu(e.currentTarget, actions) }, icon('more')) : '');
      })),
      !guest && groupish() ? h('div.talk-side-foot', h('button.btn.danger.sm', { onclick: leaveRoom }, icon('logout', 'sm'), 'Leave conversation')) : '');
  }

  async function setRole(p, role) {
    try { await tapi.patch(`${base}/participants/${encodeURIComponent(p.user_id)}`, { role }); loadParticipants(); } catch (e) { errorToast(e); }
  }

  async function removeParticipant(p) {
    if (!await confirmDialog('Remove participant', `Remove ${p.display_name} from this conversation?`, { confirm: 'Remove', danger: true })) return;
    try { await tapi.del(`${base}/participants/${encodeURIComponent(p.user_id)}`); loadParticipants(); } catch (e) { errorToast(e); }
  }

  // ---- calls ----------------------------------------------------------------------------
  function paintBanner() {
    const others = [...callMembers.values()].filter((m) => !call || m.session !== call.session);
    banner.hidden = !!call || !others.length;
    if (banner.hidden) return;
    const names = [...new Set(others.map((m) => m.actor.name))];
    clear(banner).append(
      h('span.talk-call-pulse', icon('phone', 'sm')),
      h('div.grow.ellipsis', h('strong', 'Call in progress'), ' · ', names.slice(0, 3).join(', '), names.length > 3 ? ` +${names.length - 3}` : ''),
      h('button.btn.sm.primary', { onclick: () => startCall(true) }, icon('video', 'sm'), 'Join'),
      h('button.btn.sm', { onclick: () => startCall(false) }, 'Audio only'));
  }

  async function startCall(video) {
    if (call) return;
    call = new Call({
      token, config,
      self: { id: self.id, name: self.name },
      onEnd: () => {
        call = null;
        el.classList.remove('in-call');
        paintBanner();
        paintHeader();
      },
    });
    callHost.appendChild(call.el);
    el.classList.add('in-call');
    paintBanner();
    paintHeader();
    try {
      await call.join({ audio: true, video });
    } catch (e) {
      errorToast(e);
      call?.leave();
    }
  }

  async function loadCall() {
    try {
      const { members } = await tapi.get(`${base}/call`);
      callMembers.clear();
      members.forEach((m) => callMembers.set(m.session, m));
      paintBanner();
      paintHeader();
    } catch (_) { /* ignore */ }
  }

  // ---- message rendering -----------------------------------------------------------------
  function grouped(prev, m) {
    return !!prev && prev.kind !== 'system' && m.kind !== 'system' && prev.actor_id === m.actor_id
      && prev.actor_kind === m.actor_kind && !m.parent
      && new Date(m.created_at) - new Date(prev.created_at) < GROUP_GAP_MS && sameDay(prev.created_at, m.created_at);
  }

  const mentionRe = self.username ? new RegExp(`(^|[^\\w@])@(${self.username.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}|all)(?![\\w-])`, 'i') : null;

  function renderMessage(m) {
    if (m.kind === 'system') {
      return h('div.msg.msg-system', { dataset: { id: m.id } },
        h('span', m.body), h('time', { datetime: m.created_at, title: dateTime(m.created_at) }, timeOnly(m.created_at)));
    }
    const own = m.actor_id === self.id && m.actor_kind === self.kind;
    const bot = m.actor_kind === 'bot';
    const cls = ['msg', own && 'own', bot && 'bot', !own && mentionRe && !m.deleted && mentionRe.test(m.body) && 'mentioned', m.deleted && 'deleted'].filter(Boolean).join('.');
    let content;
    if (m.deleted) content = h('div.msg-deleted', icon('trash', 'sm'), 'This message was deleted');
    else if (m.kind === 'file') content = fileCard(m);
    else if (m.kind === 'poll') content = pollCard(m);
    else content = markdown(m.body, { me: self.username });
    if (m.edited_at && !m.deleted) {
      // Inline after the text (also visible on grouped messages, which hide the header).
      const edited = h('span.msg-edited', { title: `Edited ${dateTime(m.edited_at)}` }, ' (edited)');
      const last = content.lastElementChild;
      if (last && last.tagName === 'P') last.appendChild(edited); else content.appendChild(edited);
    }
    return h(`div.${cls}`, { dataset: { id: m.id } },
      h('div.msg-side',
        bot ? h('span.avatar.bot-avatar', { title: 'Assistant' }, icon('assistant', 'sm')) : avatar(plainName(m.actor_name), { seed: m.actor_id }),
        h('time.msg-gutter', { datetime: m.created_at, title: dateTime(m.created_at) }, timeOnly(m.created_at))),
      h('div.msg-main',
        h('div.msg-head',
          h('span.msg-author', m.actor_name),
          bot ? h('span.pill.info.msg-tag', 'AI') : '',
          m.actor_kind === 'guest' ? h('span.pill.msg-tag', 'Guest') : '',
          h('time', { datetime: m.created_at, title: dateTime(m.created_at) }, timeOnly(m.created_at))),
        m.parent ? h('button.msg-quote', { type: 'button', title: 'Jump to the original message', onclick: () => jumpTo(m.parent.id) },
          h('span.msg-quote-author', icon('reply', 'sm'), m.parent.actor_name),
          h('span.msg-quote-body', m.parent.deleted ? 'Deleted message' : m.parent.body)) : '',
        h('div.msg-body', content),
        reactionsRow(m)),
      m.deleted ? '' : actionsBar(m, own));
  }

  function actionsBar(m, own) {
    const more = [];
    if (m.kind === 'text') more.push({ label: 'Copy text', icon: 'copy', run: () => copyText(m.body, 'Message copied') });
    if (own && m.kind === 'text') more.push({ label: 'Edit', icon: 'edit', run: () => startEdit(m.id) });
    if (own || (!guest && isMod())) more.push({ label: 'Delete', icon: 'trash', danger: true, run: () => deleteMessage(m) });
    return h('div.msg-actions', { role: 'toolbar', 'aria-label': 'Message actions' },
      h('button.btn.ghost.icon.sm', { 'aria-label': 'Add reaction', title: 'React', onclick: (e) => emojiPicker(e.currentTarget, (emoji) => react(m.id, emoji)) }, icon('smile', 'sm')),
      h('button.btn.ghost.icon.sm', { 'aria-label': 'Reply', title: 'Reply', onclick: () => setReply(m) }, icon('reply', 'sm')),
      more.length ? h('button.btn.ghost.icon.sm', { 'aria-label': 'More actions', title: 'More', onclick: (e) => menu(e.currentTarget, more) }, icon('more', 'sm')) : '');
  }

  function reactionsRow(m) {
    const list2 = Object.entries(m.reactions || {}).filter(([, ids]) => ids.length);
    if (!list2.length) return '';
    return h('div.msg-reactions', list2.map(([emoji, ids]) => {
      const mine = ids.includes(self.id);
      return h(`button.reaction${mine ? '.mine' : ''}`, {
        type: 'button',
        'aria-pressed': String(mine),
        'aria-label': `${emoji} ${ids.length}${mine ? ', including you' : ''}`,
        title: ids.map(participantName).join(', '),
        onclick: () => react(m.id, emoji, !mine),
      }, h('span.reaction-emoji', emoji), h('span.reaction-count', String(ids.length)));
    }), h('button.reaction.reaction-add', { type: 'button', 'aria-label': 'Add reaction', onclick: (e) => emojiPicker(e.currentTarget, (emoji) => react(m.id, emoji)) }, icon('smile', 'sm')));
  }

  function fileCard(m) {
    const meta = m.meta || {};
    const id = encodeURIComponent(meta.node_id || '');
    const live = isLiveDoc(meta.name);
    const isImage = (meta.mime || '').startsWith('image/');
    const openHref = live ? `/edit/${id}` : `/api/files/${id}/download?inline=true`;
    const nameEl = guest
      ? h('span.tk-file-name.ellipsis', meta.name)
      : h('a.tk-file-name.ellipsis', live ? { href: openHref, dataset: { route: '1' } } : { href: openHref, target: '_blank', rel: 'noopener' }, meta.name);
    const caption = m.body && m.body !== meta.name ? markdown(m.body, { me: self.username }) : '';
    return h('div.file-msg', caption,
      h('div.file-card',
        isImage && !guest ? h('a.file-thumb', { href: openHref, target: '_blank', rel: 'noopener', 'aria-label': `Open ${meta.name}` },
          h('img', { src: `/api/files/${id}/download?inline=true`, alt: meta.name, loading: 'lazy' })) : '',
        h('div.tk-file-row',
          h('span.tk-file-icon', icon(metaIcon(meta))),
          h('div.grow', nameEl, h('div.small.muted', [bytes(meta.size), live ? 'Live document' : (meta.mime || '')].filter(Boolean).join(' · '))),
          guest ? '' : h('a.btn.ghost.icon.sm', { href: `/api/files/${id}/download`, download: meta.name, 'aria-label': `Download ${meta.name}`, title: 'Download' }, icon('download', 'sm')))),
      guest ? h('div.small.muted', 'Sign in to open shared files.') : '');
  }

  function pollCard(m) {
    const { question = m.body, options = [], multiple = false, votes = {}, closed = false } = m.meta || {};
    const counts = options.map(() => 0);
    let voters = 0;
    for (const choices of Object.values(votes)) {
      if (!choices || !choices.length) continue;
      voters++;
      choices.forEach((i) => { if (i < counts.length) counts[i]++; });
    }
    const mine = new Set(votes[self.id] || []);
    const own = m.actor_id === self.id;
    const vote = async (i) => {
      let choices;
      if (multiple) {
        const s = new Set(mine);
        if (s.has(i)) s.delete(i); else s.add(i);
        choices = [...s].sort();
      } else choices = mine.has(i) ? [] : [i];
      try { updateMessage((await tapi.post(`/api/talk/messages/${m.id}/vote`, { choices })).message); } catch (e) { errorToast(e); }
    };
    return h(`div.poll${closed ? '.closed' : ''}`, { role: 'group', 'aria-label': `Poll: ${question}` },
      h('div.poll-q', icon('poll', 'sm'), h('strong', question)),
      h('div.poll-options', options.map((o, i) => {
        const pct = voters ? Math.round((counts[i] / voters) * 100) : 0;
        return h(`button.poll-option${mine.has(i) ? '.mine' : ''}`, {
          type: 'button', disabled: closed, 'aria-pressed': String(mine.has(i)),
          'aria-label': `${o}: ${counts[i]} ${counts[i] === 1 ? 'vote' : 'votes'}`,
          onclick: () => vote(i),
        },
        h('span.poll-bar', { style: { width: `${pct}%` } }),
        h(`span.poll-mark${multiple ? '.multi' : ''}`, mine.has(i) ? icon('check', 'sm') : ''),
        h('span.poll-label', o),
        h('span.poll-count', `${counts[i]} · ${pct}%`));
      })),
      h('div.poll-foot.small.muted',
        h('span.grow', `${voters} ${voters === 1 ? 'vote' : 'votes'} · ${multiple ? 'Multiple choice' : 'Single choice'}${closed ? ' · Closed' : ''}`),
        own && !closed ? h('button.btn.sm', { onclick: async () => {
          try { updateMessage((await tapi.post(`/api/talk/messages/${m.id}/close`)).message); } catch (e) { errorToast(e); }
        } }, 'Close poll') : ''));
  }

  function emojiPicker(anchor, onPick) {
    closePopover();
    const btn = (e) => h('button.emoji-btn', { type: 'button', 'aria-label': `React with ${e}`, onclick: () => { closePopover(); onPick(e); } }, e);
    const more = h('div.emoji-more', { hidden: true }, MORE_EMOJI.map(btn));
    const pop = h('div.talk-emoji', { role: 'dialog', 'aria-label': 'Pick a reaction' },
      h('div.emoji-quick', QUICK_EMOJI.map(btn),
        h('button.emoji-btn.emoji-toggle', { type: 'button', 'aria-label': 'More emoji', 'aria-expanded': 'false', onclick: (e) => {
          more.hidden = !more.hidden;
          e.currentTarget.setAttribute('aria-expanded', String(!more.hidden));
          place();
        } }, icon('plus', 'sm'))),
      more);
    document.body.appendChild(pop);
    popover = pop;
    const place = () => {
      const r = anchor.getBoundingClientRect();
      const pr = pop.getBoundingClientRect();
      const top = r.top - pr.height - 6 > 8 ? r.top - pr.height - 6 : r.bottom + 6;
      pop.style.top = `${Math.min(top, window.innerHeight - pr.height - 8)}px`;
      pop.style.left = `${Math.max(8, Math.min(r.right - pr.width, window.innerWidth - pr.width - 8))}px`;
    };
    place();
    pop.querySelector('button').focus();
    const away = (e) => { if (!pop.contains(e.target) && e.target !== anchor) done(); };
    const key = (e) => { if (e.key === 'Escape') { e.stopPropagation(); done(); anchor.focus(); } };
    const done = () => {
      document.removeEventListener('mousedown', away, true);
      pop.removeEventListener('keydown', key);
      if (popover === pop) closePopover();
    };
    pop.addEventListener('keydown', key);
    setTimeout(() => document.addEventListener('mousedown', away, true), 0);
  }

  // ---- message list maintenance -----------------------------------------------------------
  const daySep = (ts) => h('div.talk-day', { role: 'separator' }, h('span', dayLabel(ts)));
  const firstNode = (e) => e.sep || e.divider || e.el;

  function indexFor(id) {
    let lo = 0;
    let hi = order.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (order[mid] < id) lo = mid + 1; else hi = mid;
    }
    return lo;
  }

  /** Inserts one message at its place (by id), fixing day separators and grouping. */
  function insertMessage(m) {
    if (entries.has(m.id)) return updateMessage(m);
    const i = indexFor(m.id);
    const prev = i > 0 ? entries.get(order[i - 1]) : null;
    const next = i < order.length ? entries.get(order[i]) : null;
    const e = { m, el: renderMessage(m), sep: null, divider: null };
    const anchor = next ? firstNode(next) : null;
    if (!prev || !sameDay(prev.m.created_at, m.created_at)) {
      e.sep = daySep(m.created_at);
      list.insertBefore(e.sep, anchor);
    }
    list.insertBefore(e.el, anchor);
    e.el.classList.toggle('cont', grouped(prev?.m, m));
    order.splice(i, 0, m.id);
    entries.set(m.id, e);
    if (next) {
      const needSep = !sameDay(m.created_at, next.m.created_at);
      if (needSep && !next.sep) { next.sep = daySep(next.m.created_at); list.insertBefore(next.sep, next.divider || next.el); }
      if (!needSep && next.sep) { next.sep.remove(); next.sep = null; }
      next.el.classList.toggle('cont', grouped(m, next.m));
    }
    return e;
  }

  function updateMessage(m) {
    const e = entries.get(m.id);
    if (!e) return null;
    if (editing && editing.id === m.id) { editing.pending = m; return e; }
    const fresh = renderMessage(m);
    fresh.classList.toggle('cont', e.el.classList.contains('cont'));
    if (e.el.classList.contains('highlight')) fresh.classList.add('highlight');
    e.el.replaceWith(fresh);
    e.el = fresh;
    e.m = m;
    return e;
  }

  /** Adds a message that arrived live or from our own POST (deduplicated by id). */
  function addLive(m, { own = false } = {}) {
    const wasPinned = pinned || nearBottom();
    const isNew = !entries.has(m.id);
    insertMessage(m);
    if (!isNew) return;
    typers.delete(m.actor_id) && paintTyping();
    if (own || wasPinned) requestAnimationFrame(() => scrollToBottom(own));
    else jumpBtn.hidden = false;
    if (m.kind === 'system') reloadParticipants();
    markReadSoon();
  }

  async function loadOlder() {
    if (loadingOlder || !hasMore || !order.length) return;
    loadingOlder = true;
    olderStatus.replaceChildren(spinner());
    try {
      const { messages } = await tapi.get(`${base}/messages?before=${order[0]}&limit=${PAGE}`);
      if (destroyed) return;
      hasMore = messages.length === PAGE;
      const fromBottom = scroller.scrollHeight - scroller.scrollTop;
      messages.forEach(insertMessage);
      scroller.scrollTop = scroller.scrollHeight - fromBottom;
    } catch (e) {
      errorToast(e);
    } finally {
      loadingOlder = false;
      olderStatus.replaceChildren(hasMore ? '' : h('div.talk-start', roomAvatar(room, 'lg'), h('strong', room.display_name), h('span.small.muted', 'This is the beginning of the conversation.')));
    }
  }

  async function jumpTo(id) {
    for (let i = 0; i < 20 && !entries.has(id) && hasMore; i++) await loadOlder();
    const e = entries.get(id);
    if (!e) { toast('That message is no longer available', 'warn'); return; }
    pinned = false;
    e.el.scrollIntoView({ block: 'center', behavior: 'smooth' });
    e.el.classList.remove('highlight');
    void e.el.offsetWidth; // restart the highlight animation
    e.el.classList.add('highlight');
    setTimeout(() => e.el.classList.remove('highlight'), 2600);
  }

  async function deleteMessage(m) {
    if (!await confirmDialog('Delete message', 'Delete this message for everyone?', { confirm: 'Delete', danger: true })) return;
    try { await tapi.del(`/api/talk/messages/${m.id}`); } catch (e) { errorToast(e); }
  }

  async function react(id, emoji, add = true) {
    const e = entries.get(id);
    if (e && add && (e.m.reactions?.[emoji] || []).includes(self.id)) add = false;
    try { updateMessage((await tapi.post(`/api/talk/messages/${id}/reactions`, { emoji, add })).message); } catch (err) { errorToast(err); }
  }

  function startEdit(id) {
    const e = entries.get(id);
    if (!e || editing) return;
    editing = { id, pending: null };
    const bodyEl = e.el.querySelector('.msg-body');
    const ta = h('textarea.talk-edit', { 'aria-label': 'Edit message', value: e.m.body, rows: 1 });
    const finish = (m) => {
      const pending = editing?.pending;
      editing = null;
      updateMessage(m || pending || e.m);
      composer.focus();
    };
    const save = async () => {
      const text = ta.value.trim();
      if (!text || text === e.m.body) return finish();
      try { finish((await tapi.patch(`/api/talk/messages/${id}`, { body: text })).message); } catch (err) { errorToast(err); }
    };
    ta.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' && !ev.shiftKey && !ev.isComposing) { ev.preventDefault(); save(); }
      if (ev.key === 'Escape') { ev.preventDefault(); ev.stopPropagation(); finish(); }
    });
    ta.addEventListener('input', () => grow(ta));
    bodyEl.replaceChildren(ta, h('div.talk-edit-hint.small.muted', 'Enter to save · Escape to cancel',
      h('button.btn.sm.ghost', { onclick: () => finish() }, 'Cancel'), h('button.btn.sm.primary', { onclick: save }, 'Save')));
    grow(ta);
    ta.focus();
    ta.setSelectionRange(ta.value.length, ta.value.length);
  }

  // ---- read markers --------------------------------------------------------------------
  const markReadSoon = debounce(() => {
    if (guest || destroyed || !order.length) return;
    const last = order[order.length - 1];
    if (last <= lastReadSent) return;
    if (document.visibilityState !== 'visible' || !document.hasFocus() || !nearBottom(80)) return;
    lastReadSent = last;
    tapi.post(`${base}/read`, { message_id: last })
      .then(() => opts.onRead?.(roomId, last))
      .catch(() => { lastReadSent = 0; });
  }, 250);

  // ---- typing --------------------------------------------------------------------------
  function paintTyping() {
    const names = [...typers.values()].map((t) => t.name);
    typingLine.classList.toggle('on', names.length > 0);
    let text = '';
    if (names.length === 1) text = `${names[0]} is typing…`;
    else if (names.length === 2) text = `${names[0]} and ${names[1]} are typing…`;
    else if (names.length > 2) text = 'Several people are typing…';
    typingLine.replaceChildren(names.length ? h('span.talk-dots', h('i'), h('i'), h('i')) : '', text);
  }

  // ---- composer -----------------------------------------------------------------------------
  function grow(ta) {
    ta.style.height = 'auto';
    ta.style.height = `${Math.min(ta.scrollHeight + 2, 200)}px`;
  }

  function setReply(m) {
    replyTo = m;
    composer.paintReply();
    if (m) composer.focus();
  }

  function buildComposer() {
    const ta = h('textarea.talk-input', { rows: 1, 'aria-label': 'Write a message', placeholder: 'Write a message…' });
    const replyChip = h('div.talk-reply-chip', { hidden: true });
    const uploads = h('div.talk-uploads');
    const fileInput = h('input', { type: 'file', multiple: true, hidden: true, onchange: () => { uploadFiles([...fileInput.files]); fileInput.value = ''; } });
    const sug = suggestList(ta, {
      className: 'up',
      onPick: (it) => {
        const pos = ta.selectionStart;
        const before = ta.value.slice(0, pos).replace(/@([\w.-]*)$/, `@${it.value} `);
        ta.value = before + ta.value.slice(pos);
        ta.setSelectionRange(before.length, before.length);
        ta.focus();
        grow(ta);
      },
    });
    const mention = () => {
      const before = ta.value.slice(0, ta.selectionStart);
      const mm = before.match(/(?:^|[^\w@])@([\w.-]*)$/);
      if (!mm) return sug.hide();
      const q = mm[1].toLowerCase();
      const cands = participants.filter((p) => p.user_id !== self.id).map((p) => ({
        label: p.display_name, sub: `@${p.username}`, value: p.username, avatar: avatar(p.display_name, { size: 'sm', seed: p.user_id }),
      }));
      if (room.ai_enabled !== false) cands.push({ label: 'Assistant', sub: '@assistant', value: 'assistant', avatar: h('span.avatar.sm.bot-avatar', icon('assistant', 'sm')) });
      if (room.kind !== 'one_to_one' && room.kind !== 'note_to_self') cands.push({ label: 'Everyone', sub: '@all', value: 'all', avatar: h('span.avatar.sm.room-icon', icon('users', 'sm')) });
      sug.show(cands.filter((c) => c.value.toLowerCase().startsWith(q) || c.label.toLowerCase().includes(q)).slice(0, 8));
    };
    ta.addEventListener('keydown', (e) => {
      if (sug.onKey(e)) return;
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); }
      else if (e.key === 'Escape' && replyTo) { e.preventDefault(); setReply(null); }
      else if (e.key === 'ArrowUp' && !ta.value) {
        const lastOwn = [...order].reverse().map((id) => entries.get(id).m).find((m) => m.actor_id === self.id && m.kind === 'text' && !m.deleted);
        if (lastOwn) { e.preventDefault(); startEdit(lastOwn.id); }
      }
    });
    ta.addEventListener('input', () => {
      grow(ta);
      mention();
      const now = Date.now();
      if (ta.value.trim() && now - lastTypingSent > TYPING_SEND_MS) {
        lastTypingSent = now;
        live.send({ type: 'typing', room: token });
      }
    });
    ta.addEventListener('click', mention);
    ta.addEventListener('blur', () => setTimeout(() => sug.hide(), 120));
    ta.addEventListener('paste', (e) => {
      const files = [...(e.clipboardData?.files || [])];
      if (files.length && !guest) { e.preventDefault(); uploadFiles(files); }
    });

    // Guests can't share files, so they get a direct poll button instead of the menu.
    const attach = guest
      ? h('button.btn.ghost.icon', { type: 'button', 'aria-label': 'Create poll', title: 'Create poll', onclick: () => createPoll() }, icon('poll'))
      : h('button.btn.ghost.icon', {
        type: 'button', 'aria-label': 'Attach', title: 'Attach a file or create a poll',
        onclick: (e) => menu(e.currentTarget, [
          { label: 'Upload file', icon: 'upload', run: () => fileInput.click() },
          { label: 'Share from Files', icon: 'files', run: shareExisting },
          { label: 'Create poll', icon: 'poll', run: createPoll },
        ]),
      }, icon('paperclip'));
    const sendBtn = h('button.btn.primary.icon.talk-send', { type: 'button', 'aria-label': 'Send message', title: 'Send (Enter)', onclick: () => send() }, icon('send'));
    const readOnlyNote = h('div.talk-readonly.small.muted', { hidden: true }, icon('lock', 'sm'), 'Only moderators can write in this conversation.');
    const box = h('div.talk-composer-box', attach, h('div.talk-input-wrap', ta, sug.el), sendBtn);
    const wrap = h('div.talk-composer', replyChip, uploads, readOnlyNote, box, fileInput);

    async function send() {
      const text = ta.value.trim();
      if (!text) return;
      const rt = replyTo;
      ta.value = '';
      grow(ta);
      sug.hide();
      setReply(null);
      try {
        const { message } = await tapi.post(`${base}/messages`, { body: text, reply_to: rt ? rt.id : null });
        addLive(message, { own: true });
      } catch (e) {
        errorToast(e);
        if (!ta.value) { ta.value = text; grow(ta); }
        if (rt) setReply(rt);
      }
    }

    return {
      el: wrap,
      focus: () => ta.focus(),
      setPlaceholder() {
        ta.placeholder = room.kind === 'note_to_self' ? 'Write a note…' : `Message ${room.display_name || ''}`.trim();
        const ro = room.read_only && !isMod();
        readOnlyNote.hidden = !ro;
        box.hidden = ro;
      },
      paintReply() {
        replyChip.hidden = !replyTo;
        if (!replyTo) return clear(replyChip);
        clear(replyChip).append(icon('reply', 'sm'),
          h('div.grow.ellipsis', 'Replying to ', h('strong', replyTo.actor_name), ': ', h('span.muted', replyTo.kind === 'poll' ? replyTo.meta?.question : replyTo.body)),
          h('button.btn.ghost.icon.sm', { 'aria-label': 'Cancel reply', onclick: () => setReply(null) }, icon('x', 'sm')));
      },
      uploads,
    };
  }

  // ---- files & polls --------------------------------------------------------------------------
  /** The "Talk" folder in the user's root (created on first use) and its current names. */
  async function talkFolder() {
    const { entries: rootEntries } = await api.get(`/api/files/${encodeURIComponent(opts.rootId)}/children`);
    let folder = rootEntries.find((e) => e.kind === 'dir' && e.name === 'Talk');
    if (!folder) folder = (await api.post(`/api/files/${encodeURIComponent(opts.rootId)}/mkdir`, { name: 'Talk' })).node;
    const { entries: inside } = await api.get(`/api/files/${encodeURIComponent(folder.id)}/children`);
    return { id: folder.id, names: new Set(inside.map((e) => e.name.toLowerCase())) };
  }

  function uniqueName(name, names) {
    if (!names.has(name.toLowerCase())) return name;
    const dot = name.lastIndexOf('.');
    const stem = dot > 0 ? name.slice(0, dot) : name;
    const ext = dot > 0 ? name.slice(dot) : '';
    for (let i = 2; ; i++) {
      const n = `${stem} (${i})${ext}`;
      if (!names.has(n.toLowerCase())) return n;
    }
  }

  async function uploadFiles(files) {
    if (guest || !files.length) return;
    let folder;
    try { folder = await talkFolder(); } catch (e) { errorToast(e); return; }
    for (const file of files) {
      const name = uniqueName(file.name, folder.names);
      folder.names.add(name.toLowerCase());
      const bar = h('div', { style: { width: '0%' } });
      const chip = h('div.talk-upload', icon('upload', 'sm'), h('span.ellipsis', name), h('div.progress.grow', bar));
      composer.uploads.appendChild(chip);
      try {
        const node = await api.upload(folder.id, file, (p) => { bar.style.width = `${Math.round(p * 100)}%`; }, name);
        const { message } = await tapi.post(`${base}/share-file`, { node_id: node.id });
        addLive(message, { own: true });
      } catch (e) {
        errorToast(e);
      } finally {
        chip.remove();
      }
    }
  }

  async function shareExisting() {
    const node = await pickFile(opts.rootId);
    if (!node) return;
    try {
      const { message } = await tapi.post(`${base}/share-file`, { node_id: node.id });
      addLive(message, { own: true });
    } catch (e) { errorToast(e); }
  }

  async function createPoll() {
    const q = h('input', { type: 'text', 'aria-label': 'Question', placeholder: 'What should we decide?', maxlength: 300 });
    const optsBox = h('div.poll-edit-options');
    const addOpt = (v = '') => {
      const inp = h('input', { type: 'text', 'aria-label': `Option ${optsBox.children.length + 1}`, placeholder: `Option ${optsBox.children.length + 1}`, value: v });
      const row = h('div.row.poll-edit-row', inp, h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Remove option', onclick: () => { if (optsBox.children.length > 2) row.remove(); } }, icon('x', 'sm')));
      inp.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') { e.preventDefault(); if (row === optsBox.lastChild && optsBox.children.length < 20) addOpt().querySelector('input').focus(); }
      });
      optsBox.appendChild(row);
      return row;
    };
    addOpt(); addOpt();
    const multiple = h('input', { type: 'checkbox', id: 'poll-multiple' });
    const done = await modal({
      title: 'Create poll',
      body: h('div.poll-edit',
        h('label', 'Question'), q,
        h('label', 'Options'), optsBox,
        h('button.btn.sm.ghost', { type: 'button', onclick: () => { if (optsBox.children.length < 20) addOpt().querySelector('input').focus(); } }, icon('plus', 'sm'), 'Add option'),
        h('label.check', multiple, 'Allow multiple answers')),
      actions: [{ label: 'Cancel', value: null }, {
        label: 'Create poll', kind: 'primary', run: async () => {
          const options = [...optsBox.querySelectorAll('input')].map((i) => i.value.trim()).filter(Boolean);
          if (!q.value.trim() || options.length < 2) { toast('Add a question and at least two options', 'warn'); return undefined; }
          const { message } = await tapi.post(`${base}/polls`, { question: q.value.trim(), options, multiple: multiple.checked });
          return message;
        },
      }],
    });
    if (done) addLive(done, { own: true });
  }

  // ---- drag & drop ----------------------------------------------------------------------------
  if (!guest) {
    let depth = 0;
    chat.addEventListener('dragenter', (e) => { if (e.dataTransfer?.types.includes('Files')) { depth++; chat.classList.add('dropping'); } });
    chat.addEventListener('dragleave', () => { if (--depth <= 0) { depth = 0; chat.classList.remove('dropping'); } });
    chat.addEventListener('dragover', (e) => { if (e.dataTransfer?.types.includes('Files')) e.preventDefault(); });
    chat.addEventListener('drop', (e) => {
      depth = 0;
      chat.classList.remove('dropping');
      if (e.dataTransfer?.files.length) { e.preventDefault(); uploadFiles([...e.dataTransfer.files]); }
    });
  }

  // ---- scrolling ------------------------------------------------------------------------------
  scroller.addEventListener('scroll', () => {
    pinned = nearBottom();
    if (pinned) jumpBtn.hidden = true;
    if (scroller.scrollTop < 300) loadOlder();
    markReadSoon();
  }, { passive: true });
  // Content or viewport size changes (images loading, composer growing, call stage opening)
  // keep the view pinned to the newest message when it was there.
  const ro = new ResizeObserver(() => { if (pinned) scroller.scrollTop = scroller.scrollHeight; });
  ro.observe(list);
  ro.observe(scroller);
  const onFocus = () => markReadSoon();
  window.addEventListener('focus', onFocus);
  document.addEventListener('visibilitychange', onFocus);
  const onPageHide = () => call?.leave();
  window.addEventListener('pagehide', onPageHide);

  // ---- live events -----------------------------------------------------------------------------
  const mine = (ev) => roomId && ev.room_id === roomId;
  offs.push(
    live.on('talk.message', (ev) => { if (mine(ev)) addLive(ev.message); }),
    live.on('talk.message_updated', (ev) => { if (mine(ev)) updateMessage(ev.message); }),
    live.on('talk.typing', (ev) => {
      if (!mine(ev) || (ev.actor.id === self.id && ev.actor.kind === self.kind)) return;
      const prev = typers.get(ev.actor.id);
      if (prev) clearTimeout(prev.t);
      typers.set(ev.actor.id, { name: ev.actor.name, t: setTimeout(() => { typers.delete(ev.actor.id); paintTyping(); }, TYPING_SHOW_MS) });
      paintTyping();
    }),
    live.on('talk.room_updated', (ev) => { if (ev.room && ev.room.id === roomId) applyRoomUpdate(ev.room); }),
    live.on('talk.rooms_changed', () => reloadParticipants()),
    live.on('call.joined', (ev) => {
      if (!mine(ev)) return;
      callMembers.set(ev.member.session, ev.member);
      paintBanner(); paintHeader();
    }),
    live.on('call.left', (ev) => {
      if (!mine(ev)) return;
      callMembers.delete(ev.session);
      paintBanner(); paintHeader();
    }),
    live.on('call.state', (ev) => { if (mine(ev) && callMembers.has(ev.member.session)) callMembers.set(ev.member.session, ev.member); }),
    live.on('hello', () => resync()),
  );

  /** After a reconnect: fetch what we missed and refresh recent messages and the call. */
  async function resync() {
    if (destroyed || !order.length) return;
    try {
      let after = order[order.length - 1];
      for (let i = 0; i < 10; i++) {
        const { messages } = await tapi.get(`${base}/messages?after=${after}&limit=200`);
        messages.forEach((m) => addLive(m));
        if (messages.length < 200) break;
        after = messages[messages.length - 1].id;
      }
      const { messages } = await tapi.get(`${base}/messages?limit=${PAGE}`);
      messages.forEach((m) => (entries.has(m.id) ? updateMessage(m) : addLive(m)));
      loadCall();
      loadParticipants();
    } catch (_) { /* next reconnect retries */ }
  }

  // ---- initial load ------------------------------------------------------------------------------
  (async () => {
    list.appendChild(spinner());
    try {
      const [{ messages }] = await Promise.all([tapi.get(`${base}/messages?limit=${PAGE}`), loadParticipants(), loadCall()]);
      if (destroyed) return;
      clear(list);
      if (!roomId && messages.length) roomId = messages[0].room_id;
      hasMore = messages.length === PAGE;
      messages.forEach(insertMessage);
      if (!hasMore) olderStatus.replaceChildren(h('div.talk-start', roomAvatar(room, 'lg'), h('strong', room.display_name), h('span.small.muted', 'This is the beginning of the conversation.')));
      // Unread divider before the first message from someone else after our read marker.
      const lastRead = room.last_read_message_id || 0;
      const firstUnread = !guest && lastRead ? order.map((id) => entries.get(id)).find((e) => e.m.id > lastRead && e.m.actor_id !== self.id && e.m.kind !== 'system') : null;
      if (firstUnread && firstUnread !== entries.get(order[0])) {
        firstUnread.divider = h('div.talk-unread', { role: 'separator' }, h('span', 'New messages'));
        list.insertBefore(firstUnread.divider, firstUnread.el);
      }
      requestAnimationFrame(() => {
        if (opts.query?.message) { jumpTo(Number(opts.query.message)); return; }
        if (firstUnread?.divider && firstUnread.divider.offsetTop > scroller.clientHeight) {
          scroller.scrollTop = firstUnread.divider.offsetTop - 40;
          pinned = nearBottom();
          jumpBtn.hidden = pinned;
        } else scrollToBottom();
        markReadSoon();
      });
      if (opts.query?.call === '1') startCall(true);
      if (!document.querySelector('.modal')) composer.focus();
    } catch (e) {
      clear(list).appendChild(h('div.empty', h('p', e.message)));
    }
  })();

  return {
    el,
    get roomId() { return roomId; },
    setRoom(patch) { Object.assign(room, patch); paintHeader(); },
    destroy() {
      destroyed = true;
      offs.forEach((off) => off());
      ro.disconnect();
      window.removeEventListener('focus', onFocus);
      document.removeEventListener('visibilitychange', onFocus);
      window.removeEventListener('pagehide', onPageHide);
      typers.forEach((t) => clearTimeout(t.t));
      closePopover();
      call?.leave();
    },
  };
}

/** A small file browser that resolves with the chosen file entry (or null). */
export async function pickFile(rootId) {
  let selected = null;
  const crumbs = h('div.picker-crumbs');
  const listEl = h('div.picker-list.panel', { role: 'listbox', 'aria-label': 'Files' });
  const stack = [{ id: rootId, name: 'Files' }];
  const load = async () => {
    selected = null;
    clear(crumbs).append(...stack.map((c, i) => [i ? h('span.muted', '/') : '', h('button.btn.ghost.sm', {
      type: 'button', onclick: () => { stack.length = i + 1; load(); },
    }, c.name)]).flat());
    clear(listEl).appendChild(spinner());
    try {
      const { entries } = await api.get(`/api/files/${encodeURIComponent(stack[stack.length - 1].id)}/children`);
      clear(listEl);
      if (!entries.length) listEl.appendChild(h('div.empty.small', 'This folder is empty'));
      for (const en of entries) {
        const item = h('div.list-item.clickable', {
          role: 'option', tabindex: 0, 'aria-selected': 'false',
          onclick: () => {
            if (en.kind === 'dir') { stack.push({ id: en.id, name: en.name }); load(); return; }
            listEl.querySelectorAll('.selected').forEach((x) => { x.classList.remove('selected'); x.setAttribute('aria-selected', 'false'); });
            item.classList.add('selected');
            item.setAttribute('aria-selected', 'true');
            selected = en;
          },
          ondblclick: () => { if (en.kind !== 'dir') listEl.closest('.modal').querySelector('.modal-foot .btn.primary').click(); },
          onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); item.click(); } },
        }, icon(en.kind === 'dir' ? 'folder' : metaIcon(en)), h('span.grow.ellipsis', en.name), en.kind === 'dir' ? icon('chevron-right', 'sm') : h('span.small.muted', bytes(en.size)));
        listEl.appendChild(item);
      }
    } catch (e) {
      clear(listEl).appendChild(h('div.empty.small', e.message));
    }
  };
  load();
  return modal({
    title: 'Share a file',
    body: h('div.picker', crumbs, listEl),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Share', kind: 'primary', run: () => {
        if (!selected) { toast('Choose a file first', 'warn'); return undefined; }
        return selected;
      },
    }],
  });
}
