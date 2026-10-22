// Guest access to a public conversation (/join/:token): pick a display name, then chat,
// react, vote and join calls without an account. The guest secret lives in sessionStorage
// (per tab) and is sent as X-Guest-Token on every Talk request and on the event socket.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, errorToast } from '../ui.js';
import { createRoomPane } from './talk-chat.js';

const storeKey = (token) => `ferrite-guest:${token}`;

function loadStored(token) {
  try { return JSON.parse(sessionStorage.getItem(storeKey(token)) || 'null'); } catch (_) { return null; }
}

export default async function render(root, ctx) {
  const token = ctx.params.token;
  let pane = null;
  let started = false;
  ctx.setTitle('Join conversation');

  const card = (...children) => clear(root).appendChild(h('div.auth.guest-auth', h('div.card', h('h1', h('span.brand-mark', 'Fe'), 'Ferrite'), ...children)));

  let info;
  try {
    ({ room: info } = await api.get(`/api/talk/guest/${encodeURIComponent(token)}`));
  } catch (e) {
    ctx.setTitle('Conversation unavailable');
    card(h('h2', 'This conversation isn’t available'),
      h('p.muted', e.status === 404
        ? 'The link may be mistyped, or the conversation is no longer public. Ask the person who shared it for a new link.'
        : `The server could not be reached (${e.message}). Try again in a moment.`),
      h('a.btn', { href: '/' }, 'Go to Ferrite'));
    return null;
  }
  ctx.setTitle(info.name);

  // Signed in: open the conversation in Talk instead of joining as a guest.
  function signedInOffer() {
    card(h('h2', info.name), info.description ? h('p.muted', info.description) : '',
      h('p', 'You are signed in as ', h('strong', ctx.me.user.display_name), '. Join this conversation with your account?'),
      h('div.row.wrap',
        h('button.btn.primary', { onclick: async (e) => {
          e.currentTarget.disabled = true;
          try {
            await api.post(`/api/talk/rooms/${encodeURIComponent(token)}/join`);
            // A full load starts the signed-in app (shell and event socket).
            location.href = `/talk/${encodeURIComponent(token)}`;
          } catch (err) { errorToast(err); e.currentTarget.disabled = false; }
        } }, icon('talk', 'sm'), 'Open in Talk'),
        h('button.btn.ghost', { onclick: askName }, 'Join as a guest instead')));
  }

  function askName() {
    const input = h('input', { type: 'text', id: 'guest-name', autocomplete: 'nickname', maxlength: 64, required: true });
    const err = h('div.error-text', { role: 'alert' });
    const btn = h('button.btn.primary', { type: 'submit', style: { width: '100%', marginTop: '8px' } }, 'Join conversation');
    card(
      h('h2', info.name),
      info.description ? h('p.muted', info.description) : h('p.muted', 'You were invited to a conversation.'),
      h('form', {
        onsubmit: async (e) => {
          e.preventDefault();
          const name = input.value.trim();
          if (!name) return;
          btn.disabled = true;
          err.textContent = '';
          try {
            const res = await api.post(`/api/talk/guest/${encodeURIComponent(token)}`, { name });
            try { sessionStorage.setItem(storeKey(token), JSON.stringify({ secret: res.secret, guest: res.guest })); } catch (_) { /* private mode */ }
            enter(res.secret, res.guest);
          } catch (ex) {
            err.textContent = ex.message;
            btn.disabled = false;
          }
        },
      }, h('div.field', h('label', { for: 'guest-name' }, 'Your name'), input), err, btn),
      h('p.small.muted', { style: { marginTop: '14px' } }, 'Have an account? ', h('a', { href: `/talk/${encodeURIComponent(token)}` }, 'Sign in'), ' instead.'));
    setTimeout(() => input.focus(), 0);
  }

  function enter(secret, guest) {
    if (!started) {
      live.start({ guest: secret });
      started = true;
    }
    const room = {
      id: info.id, token, kind: 'public', name: info.name, display_name: info.name, description: info.description,
      read_only: !!info.read_only, ai_enabled: !!info.ai_enabled, participant_count: info.participant_count, role: 'guest',
    };
    pane = createRoomPane({
      token,
      room,
      self: { id: guest.id, kind: 'guest', name: guest.name },
      guest: true,
      headers: { 'X-Guest-Token': secret },
      config: ctx.config || {},
    });
    clear(root).appendChild(h('div.guest-shell',
      h('header.guest-head',
        h('span.brand-mark', 'Fe'), h('strong.hide-narrow', 'Ferrite'),
        h('div.grow'),
        h('span.guest-who', avatar(guest.name.replace(/\s*\(guest\)$/, ''), { size: 'sm', seed: guest.id }), h('span.ellipsis', guest.name)),
        h('button.btn.ghost.sm', { onclick: leave, title: 'Leave and forget this guest name' }, icon('logout', 'sm'), h('span.hide-narrow', 'Leave'))),
      pane.el));
  }

  function leave() {
    try { sessionStorage.removeItem(storeKey(token)); } catch (_) { /* ignore */ }
    pane?.destroy();
    pane = null;
    live.stop();
    started = false;
    askName();
  }

  const stored = loadStored(token);
  if (stored && stored.secret && stored.guest) {
    // Check the stored identity still works (the room may have stopped being public).
    try {
      await api.get(`/api/talk/rooms/${encodeURIComponent(token)}/messages?limit=1`, { 'X-Guest-Token': stored.secret });
      enter(stored.secret, stored.guest);
    } catch (_) {
      try { sessionStorage.removeItem(storeKey(token)); } catch (__) { /* ignore */ }
      ctx.me ? signedInOffer() : askName();
    }
  } else if (ctx.me) signedInOffer();
  else askName();

  return () => {
    pane?.destroy();
    if (started) live.stop();
  };
}
