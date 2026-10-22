// Shared helpers for the Talk views (talk.js, guest.js, talk-chat.js, talk-call.js).

import { api } from '../api.js';
import { h, icon, avatar, clear, debounce } from '../ui.js';

export const QUICK_EMOJI = ['👍', '❤️', '😂', '🎉', '😮', '😢', '🙏'];
export const MORE_EMOJI = [
  '😀', '😃', '😄', '😁', '😅', '🤣', '🙂', '😉', '😊', '😍', '😘', '😎', '🤔', '🤨', '😐', '🙄',
  '😏', '😴', '😬', '🤯', '😱', '😡', '🥳', '🤗', '🤝', '👏', '🙌', '👀', '💯', '🔥', '✨', '⭐',
  '✅', '❌', '⚠️', '❓', '💡', '📌', '🚀', '🐛', '☕', '🍕', '🎂', '🌞', '🌧️', '👋', '💪', '👎',
];

/** Talk API client that adds the guest header when needed. */
export function talkApi(headers) {
  const hd = headers && Object.keys(headers).length ? headers : undefined;
  return {
    get: (p) => api.get(p, hd),
    post: (p, b = {}) => api.post(p, b, hd),
    patch: (p, b = {}) => api.patch(p, b, hd),
    del: (p) => api.del(p, hd),
  };
}

const LIVE_DOC = /\.(md|txt|csv|fmind|fcanvas)$/i;
export const isLiveDoc = (name) => LIVE_DOC.test(name || '');

/** Icon name for a shared-file message's meta. */
export function metaIcon(meta) {
  const n = (meta.name || '').toLowerCase();
  const mime = meta.mime || '';
  if (meta.kind === 'dir') return 'folder';
  if (/\.(md|txt)$/.test(n)) return 'doc';
  if (n.endsWith('.csv')) return 'sheet';
  if (n.endsWith('.fmind')) return 'mindmap';
  if (n.endsWith('.fcanvas')) return 'canvas';
  if (mime.startsWith('image/')) return 'image';
  if (mime.startsWith('video/')) return 'video';
  if (mime.startsWith('audio/')) return 'mic';
  return 'file';
}

/** Round avatar for a conversation (people get initials, rooms get an icon). */
export function roomAvatar(room, size = '') {
  if (room.kind === 'one_to_one') return avatar(room.display_name, { size });
  const ic = room.kind === 'note_to_self' ? 'note' : room.kind === 'public' ? 'globe' : 'users';
  return h(`span.avatar.room-icon${size ? '.' + size : ''}`, { title: room.display_name }, icon(ic, 'sm'));
}

/** One-line preview of a message for the conversation list. */
export function messagePreview(m, selfId) {
  if (!m) return '';
  if (m.deleted) return 'Message deleted';
  let text = m.body || '';
  if (m.kind === 'file') text = `📎 ${m.meta?.name || text}`;
  else if (m.kind === 'poll') text = `📊 ${m.meta?.question || text}`;
  text = text.replace(/\s+/g, ' ').replace(/[*_`~#>]/g, '');
  if (m.kind === 'system') return text;
  const who = m.actor_id === selfId ? 'You' : (m.actor_name || '').split(' ')[0];
  return `${who}: ${text}`;
}

export function sameDay(a, b) {
  const x = new Date(a);
  const y = new Date(b);
  return x.getFullYear() === y.getFullYear() && x.getMonth() === y.getMonth() && x.getDate() === y.getDate();
}

export function dayLabel(ts) {
  const d = new Date(ts);
  const now = new Date();
  if (sameDay(d, now)) return 'Today';
  const y = new Date(now);
  y.setDate(now.getDate() - 1);
  if (sameDay(d, y)) return 'Yesterday';
  return d.toLocaleDateString([], { weekday: 'long', month: 'long', day: 'numeric', year: d.getFullYear() === now.getFullYear() ? undefined : 'numeric' });
}

/** Short time for the conversation list (time today, weekday this week, else date). */
export function shortTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  const now = new Date();
  if (sameDay(d, now)) return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  if (now - d < 6 * 86400e3) return d.toLocaleDateString([], { weekday: 'short' });
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

let suggestSeq = 0;

/**
 * Keyboard-navigable suggestion list (a listbox) attached to an input or textarea.
 * Items are {label, sub, avatar(node), value}. The caller forwards keydown events to
 * `onKey(e)`, which returns true when it handled the key.
 */
export function suggestList(input, { onPick, className = '' } = {}) {
  const id = `talk-suggest-${++suggestSeq}`;
  const box = h(`div.talk-suggest${className ? '.' + className : ''}`, { id, role: 'listbox', hidden: true });
  let items = [];
  let active = 0;
  input.setAttribute('aria-controls', id);
  input.setAttribute('aria-autocomplete', 'list');
  input.setAttribute('aria-expanded', 'false');

  const paint = () => {
    [...box.children].forEach((el, i) => {
      el.classList.toggle('active', i === active);
      el.setAttribute('aria-selected', String(i === active));
      if (i === active) {
        input.setAttribute('aria-activedescendant', el.id);
        el.scrollIntoView({ block: 'nearest' });
      }
    });
  };
  const api = {
    el: box,
    get open() { return !box.hidden; },
    show(list) {
      items = list;
      active = 0;
      clear(box);
      if (!list.length) return api.hide();
      list.forEach((it, i) => box.appendChild(h('div.talk-suggest-item', {
        id: `${id}-${i}`,
        role: 'option',
        onmousedown: (e) => { e.preventDefault(); active = i; api.pick(); },
      }, it.avatar || null, h('span.grow.ellipsis', it.label), it.sub && h('span.small.muted.ellipsis', it.sub))));
      box.hidden = false;
      input.setAttribute('aria-expanded', 'true');
      paint();
    },
    hide() {
      box.hidden = true;
      items = [];
      input.setAttribute('aria-expanded', 'false');
      input.removeAttribute('aria-activedescendant');
    },
    pick() {
      const it = items[active];
      api.hide();
      if (it) onPick(it);
    },
    onKey(e) {
      if (box.hidden) return false;
      if (e.key === 'ArrowDown') { active = (active + 1) % items.length; paint(); }
      else if (e.key === 'ArrowUp') { active = (active - 1 + items.length) % items.length; paint(); }
      else if (e.key === 'Enter' || e.key === 'Tab') api.pick();
      else if (e.key === 'Escape') api.hide();
      else return false;
      e.preventDefault();
      e.stopPropagation();
      return true;
    },
  };
  return api;
}

/**
 * A user search field (GET /api/users?q=) with keyboard autocomplete. `onPick(user)` gets
 * {id, username, display_name, online}. `exclude()` returns usernames to hide.
 */
export function userSearch({ onPick, exclude = () => [], placeholder = 'Search people…', label = 'Search people' } = {}) {
  const input = h('input', { type: 'text', placeholder, 'aria-label': label, autocomplete: 'off', role: 'combobox' });
  const sug = suggestList(input, {
    onPick: (it) => { input.value = ''; onPick(it.value); input.focus(); },
  });
  let seq = 0;
  const search = debounce(async () => {
    const q = input.value.trim();
    const mine = ++seq;
    try {
      const { users } = await api.get(`/api/users?q=${encodeURIComponent(q)}`);
      if (mine !== seq || document.activeElement !== input) return;
      const skip = new Set(exclude());
      sug.show(users.filter((u) => !skip.has(u.username)).slice(0, 8).map((u) => ({
        label: u.display_name, sub: `@${u.username}`, value: u,
        avatar: avatar(u.display_name, { size: 'sm', seed: u.id, online: u.online }),
      })));
    } catch (_) { sug.hide(); }
  }, 150);
  input.addEventListener('input', search);
  input.addEventListener('focus', search);
  input.addEventListener('blur', () => setTimeout(() => sug.hide(), 120));
  input.addEventListener('keydown', (e) => sug.onKey(e));
  return { el: h('div.talk-usersearch', input, sug.el), input };
}
