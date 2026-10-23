// The Outbox: messages waiting to be sent: held back for the undo delay, scheduled with
// "Send later", set aside after Undo, or failed (with the error and when it is tried again).
// Each can be reopened in the composer, sent now or deleted. The folder list shows an Outbox
// entry with a count while anything is waiting; the list itself is drawn in the reading pane.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, toast, errorToast, confirmDialog, dateTime, spinner, emptyState, debounce } from '../ui.js';
import { openComposer } from './mail-compose.js';

const state = { list: [], loaded: false, views: new Set() };
let navEl = null;
let badge = null;

async function load() {
  try {
    state.list = (await api.get('/api/mail/outbox')).messages;
    state.loaded = true;
  } catch (_) { /* keep the last list */ }
  paintNav();
  state.views.forEach((v) => v());
}
const reload = debounce(load, 200);
live.on('mail.outbox', reload);
live.on('hello', reload);

function paintNav() {
  if (!navEl) return;
  const n = state.list.length;
  navEl.hidden = !n && !navEl.classList.contains('active');
  badge.textContent = String(n);
  badge.hidden = !n;
  const failed = state.list.some((m) => m.status === 'failed');
  badge.classList.toggle('danger', failed);
  navEl.title = failed ? 'Some messages could not be sent' : 'Messages waiting to be sent';
}

/** The Outbox entry of the folder list (one element, kept across redraws). */
export function outboxNavItem({ active = false, onOpen }) {
  if (!navEl) {
    badge = h('span.badge.ml-unread');
    navEl = h('button.ml-folder.ml-outbox-folder', { type: 'button', dataset: { role: 'outbox' }, onclick: () => navEl.onOpen?.() },
      icon('send', 'sm'), h('span.grow.ellipsis', 'Outbox'), badge);
    load();
  }
  navEl.onOpen = onOpen;
  navEl.classList.toggle('active', active);
  paintNav();
  return navEl;
}

function statusOf(m) {
  const now = Date.now();
  if (m.status === 'sending') return ['Sending…', 'info'];
  if (m.status === 'failed') {
    if (!m.send_at) return [`Not sent: ${m.last_error}`, 'danger'];
    const when = m.next_attempt_at ? ` · next try ${dateTime(m.next_attempt_at)}` : '';
    return [`Failed ${m.attempts} ${m.attempts === 1 ? 'time' : 'times'}: ${m.last_error}${when}`, 'danger'];
  }
  if (!m.send_at) return ['On hold: not scheduled', 'warn'];
  const t = new Date(m.send_at).getTime();
  if (t - now > 60e3) return [`Scheduled for ${dateTime(m.send_at)}`, 'info'];
  return ['Sending in a moment', 'info'];
}

/** Draws the outbox into `container` (the reading pane). Returns a cleanup function. */
export function renderOutbox(container, ctx, { accounts, onBack }) {
  const listEl = h('div.ml-outbox-list', { role: 'list' });
  const box = h('article.ml-thread.ml-outbox',
    h('div.ml-thread-head',
      h('button.btn.ghost.icon.sm.ml-back', { type: 'button', 'aria-label': 'Back to the list', onclick: onBack }, icon('arrow-left', 'sm')),
      h('h2.grow', 'Outbox'),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Refresh', title: 'Refresh', onclick: load }, icon('refresh', 'sm'))),
    h('p.small.muted', 'Messages wait here until they are sent. Failed messages are tried again automatically.'),
    listEl);
  clear(container).appendChild(box);

  async function edit(m) {
    let restore = null;
    try {
      if (m.send_at && new Date(m.send_at).getTime() - Date.now() > 60e3) {
        // A scheduled message is set aside while it is edited, and keeps its time unless the
        // user sends it differently.
        restore = m.send_at;
      }
      if (m.send_at) await api.post(`/api/mail/outbox/${encodeURIComponent(m.id)}/stop`, {});
    } catch (e) {
      errorToast(e);
      return;
    }
    openComposer(ctx, { accounts, outboxId: m.id, restoreSendAt: restore });
  }

  function draw() {
    clear(listEl);
    if (!state.loaded) { listEl.appendChild(spinner()); return; }
    if (!state.list.length) {
      listEl.appendChild(emptyState('send', 'The Outbox is empty', 'Messages you send, schedule or that fail to go out appear here until they are sent.'));
      return;
    }
    for (const m of state.list) {
      const [text, kind] = statusOf(m);
      const sending = m.status === 'sending';
      const account = accounts.find((a) => a.id === m.account_id);
      listEl.appendChild(h('div.ml-outbox-item', { role: 'listitem', dataset: { id: m.id } },
        h('div.ml-outbox-main',
          h('div.ml-outbox-subject.ellipsis', m.subject || '(no subject)'),
          h('div.small.muted.ellipsis', `To ${m.recipients.map((r) => r.replace(/^group:/, 'group ')).join(', ') || '—'}${account ? ` · from ${account.email}` : ''}`),
          h(`div.small.ml-outbox-status.${kind}`, text)),
        h('div.ml-outbox-actions',
          h('button.btn.sm', { type: 'button', disabled: sending, onclick: () => edit(m) }, icon('edit', 'sm'), h('span', 'Edit')),
          h('button.btn.sm', {
            type: 'button', disabled: sending,
            onclick: async () => {
              try { await api.post(`/api/mail/outbox/${encodeURIComponent(m.id)}/send`, {}); toast('Sending now', 'info', { timeout: 2000 }); load(); } catch (e) { errorToast(e); }
            },
          }, icon('send', 'sm'), h('span', 'Send now')),
          h('button.btn.ghost.icon.sm.danger', {
            type: 'button', disabled: sending, 'aria-label': `Delete ${m.subject || 'message'}`, title: 'Delete',
            onclick: async () => {
              if (!await confirmDialog('Delete message', `Delete “${m.subject || '(no subject)'}” from the Outbox? It will not be sent.`, { confirm: 'Delete', danger: true })) return;
              try { await api.del(`/api/mail/outbox/${encodeURIComponent(m.id)}`); load(); } catch (e) { errorToast(e); }
            },
          }, icon('trash', 'sm')))));
    }
  }
  state.views.add(draw);
  draw();
  load();
  // Relative times ("in a moment") move on.
  const tick = setInterval(draw, 30000);
  return () => { state.views.delete(draw); clearInterval(tick); };
}
