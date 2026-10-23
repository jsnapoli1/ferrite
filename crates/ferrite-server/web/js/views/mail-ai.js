// AI in Mail: a summary above conversations of three or more messages, reply suggestions
// below the conversation, translation of a message, and follow-up reminders for sent mail
// that expects an answer. Everything loads after the message is shown and only when the
// administrator allows AI for mail, a model is configured and the user turned the assistant
// on; the server caches results.

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, modal, spinner, relTime } from '../ui.js';
import { mi } from './mail-integrations-icons.js';
import { insertIntoComposer } from './mail-compose.js';

let status = { at: 0, promise: null };

/** {enabled, llm_processing, configured, consent}, fetched at most once a minute. */
export function aiStatus() {
  if (!status.promise || Date.now() - status.at > 60000) {
    status = { at: Date.now(), promise: api.get('/api/mail/ai/status').catch(() => ({ enabled: false })) };
  }
  return status.promise;
}

// Smart replies go in through the composer's own insertIntoComposer (rich text or plain).

/** Opens a reply and inserts `text` once the composer is there. */
export function replyWith(text, messageId, compose) {
  compose({ fromMessage: messageId, mode: 'reply' });
  let tries = 0;
  const wait = () => {
    if (insertIntoComposer(text)) return;
    if (++tries < 100) setTimeout(wait, 50);
  };
  setTimeout(wait, 50);
}

/** The summary box for a conversation (inserted before `before`). */
export async function threadSummary(box, before, thread, current) {
  if (thread.length < 3) return;
  const st = await aiStatus();
  if (!st.enabled) return;
  const text = h('div.ma-summary-text', spinner());
  const el = h('section.ma-summary', { 'aria-label': 'Summary' },
    h('div.ma-head', mi('sparkle'), h('strong', 'Summary'), h('span.small.muted', `${thread.length} messages`)), text);
  box.insertBefore(el, before);
  try {
    const r = await api.post(`/api/mail/messages/${encodeURIComponent(current.id)}/ai/summary`, {});
    clear(text).appendChild(h('p', r.summary));
    el.appendChild(h('div.small.muted.ma-foot', 'Written by AI; check important details in the messages.'));
  } catch (e) {
    clear(text).appendChild(h('p.small.muted', `No summary: ${e.message}`));
  }
}

/** Reply suggestions under the conversation. */
export async function smartReplies(box, current, me, compose) {
  if (!current || current.from_addr?.toLowerCase() === me?.toLowerCase() || current.draft) return;
  const st = await aiStatus();
  if (!st.enabled) return;
  const chips = h('div.ma-chips', spinner());
  const el = h('section.ma-replies', { 'aria-label': 'Suggested replies' }, h('div.ma-head', mi('sparkle'), h('span.small.muted', 'Suggested replies')), chips);
  box.appendChild(el);
  try {
    const { replies } = await api.post(`/api/mail/messages/${encodeURIComponent(current.id)}/ai/replies`, {});
    if (!replies.length) { el.remove(); return; }
    clear(chips).append(...replies.map((r) => h('button.btn.sm.ma-chip', { type: 'button', title: 'Reply with this', onclick: () => replyWith(r, current.id, compose) }, r)));
  } catch (_) {
    el.remove();
  }
}

/** Shows a translation of a message at the top of its body. */
export async function translateMessage(body, messageId) {
  body.querySelector('.ma-translation')?.remove();
  // "en-US@posix" and similar: keep the language tag only.
  const lang = (navigator.language || 'en').split(/[@.,;]/)[0] || 'en';
  const out = h('div.ma-translation-text', spinner());
  const el = h('section.ma-translation', { 'aria-label': 'Translation' },
    h('div.ma-head', mi('translate'), h('strong.grow', `Translation (${lang})`),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Close the translation', onclick: () => el.remove() }, icon('x', 'sm'))),
    out);
  body.prepend(el);
  try {
    const r = await api.post(`/api/mail/messages/${encodeURIComponent(messageId)}/ai/translate`, { lang });
    clear(out).append(h('div.ma-tr-subject', r.subject), h('div.ma-tr-body', r.text));
  } catch (e) {
    clear(out).appendChild(h('p.small.error-text', e.message));
  }
}

let followUps = { at: 0, list: [] };

/** Makes the next follow-up button fetch the list again. */
export function forgetFollowUps() {
  followUps = { at: 0, list: [] };
}

/** The "Follow up" button for the folder pane (sent mail still waiting for an answer). The
 * list is fetched at most once a minute (the pane is redrawn often). */
export async function followUpButton(container, { openMessage, compose }, { fresh = false } = {}) {
  const st = await aiStatus();
  if (!st.enabled) return;
  if (fresh || Date.now() - followUps.at > 60000) {
    try {
      followUps = { at: Date.now(), list: (await api.get('/api/mail/follow-ups')).follow_ups };
    } catch (_) {
      return;
    }
  }
  const list = followUps.list;
  container.querySelector('.ma-followup-btn')?.remove();
  if (!list.length) return;
  const btn = h('button.ml-folder.ma-followup-btn', { type: 'button', onclick: () => followUpDialog(list, { openMessage, compose }).then(() => followUpButton(container, { openMessage, compose }, { fresh: true })) },
    icon('clock', 'sm'), h('span.grow.ellipsis', 'Follow up'), h('span.badge.ml-unread', String(list.length)));
  container.appendChild(btn);
}

function followUpDialog(list, { openMessage, compose }) {
  const rows = h('div.ma-followups');
  const draw = () => {
    clear(rows);
    if (!list.length) { rows.appendChild(h('div.empty.small', 'Nothing to follow up on.')); return; }
    for (const f of list) {
      const to = f.recipients.map((r) => r.name || r.address).join(', ');
      rows.appendChild(h('div.list-item.ma-followup',
        icon('send', 'sm'),
        h('div.grow',
          h('div.ellipsis', h('strong', f.subject || '(no subject)')),
          h('div.small.muted.ellipsis', `To ${to} · sent ${relTime(f.sent_at)} · no answer yet`)),
        h('div.ma-followup-actions',
          f.id && h('button.btn.sm.ghost', { type: 'button', onclick: () => { close(); openMessage(f.id); } }, 'Open'),
          f.id && h('button.btn.sm.primary', { type: 'button', onclick: () => { close(); compose({ fromMessage: f.id, mode: 'reply' }); } }, icon('reply', 'sm'), 'Follow up'),
          h('button.btn.sm.ghost', {
            type: 'button', title: 'Stop reminding me about this message',
            onclick: async () => {
              try {
                await api.post('/api/mail/follow-ups/disable', { account_id: f.account_id, message_id: f.message_id });
                list = list.filter((x) => x !== f);
                draw();
                toast('You will not be reminded about it again', 'ok', { timeout: 2000 });
              } catch (e) { errorToast(e); }
            },
          }, 'Not needed'))));
    }
  };
  let close = () => {};
  draw();
  return modal({
    title: 'Follow up',
    wide: true,
    body: (c) => { close = c; return h('div', h('p.small.muted', 'Messages you sent that expect an answer (judged by AI) and got none in four days.'), rows); },
    actions: [{ label: 'Close', value: null }],
  });
}
