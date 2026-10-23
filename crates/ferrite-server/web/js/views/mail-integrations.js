// The reader's integrations, hooked into mail.js: above a message the invitation card,
// reservation cards, the unsubscribe offer and who the sender is in the user's contacts;
// around a conversation the AI summary and reply suggestions; in the message menu events and
// tasks from the message, a filter from it, adding the sender to contacts and translation;
// in the account menu the filters and autoresponder; in the folder pane follow-up reminders.

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, modal, spinner } from '../ui.js';
import { invitationCard, reservationCard, localInput } from './mail-imip.js';
import { filtersDialog, filterFromMessage } from './mail-filters.js';
import { aiStatus, threadSummary, smartReplies, translateMessage, followUpButton } from './mail-ai.js';
import { mi } from './mail-integrations-icons.js';

export { insertIntoComposer } from './mail-ai.js';

const enc = encodeURIComponent;

// ---- above a message ------------------------------------------------------------------------

function senderLine(info, message, accounts) {
  // Not for the user's own messages.
  if (!info || accounts.some((a) => a.email.toLowerCase() === info.email.toLowerCase())) return null;
  const el = h('div.mx-sender.small');
  const draw = (i) => {
    clear(el);
    if (i.contact) {
      el.append(icon('contacts', 'sm'), h('span', 'In your contacts as ', h('strong', i.contact.name)));
    } else {
      el.append(icon('contacts', 'sm'), h('span.muted', 'Not in your contacts'),
        h('button.btn.ghost.sm', { type: 'button', onclick: () => contactDialog(message).then((c) => { if (c) draw({ ...i, contact: c }); }) }, icon('plus', 'sm'), 'Add'));
    }
    const ab = i.user?.absence;
    if (ab) {
      el.appendChild(h('span.pill.warn', { title: ab.message || '' }, icon('calendar', 'sm'),
        `${ab.status || 'Out of office'} until ${new Date(`${ab.last_day}T00:00:00`).toLocaleDateString([], { day: 'numeric', month: 'short' })}`));
    }
  };
  draw(info);
  return el;
}

function unsubscribeLine(u, message) {
  if (!u) return null;
  const el = h('div.mx-unsub.small', mi('unsubscribe', 'sm'),
    h('span.grow', u.list ? `From the mailing list “${u.list}”.` : 'From a mailing list.'));
  if (u.one_click || u.mailto) {
    el.appendChild(h('button.btn.sm', {
      type: 'button',
      onclick: async (e) => {
        const b = e.currentTarget;
        b.disabled = true;
        try {
          const r = await api.post(`/api/mail/messages/${enc(message.id)}/unsubscribe`, {});
          clear(el).append(icon('check', 'sm'), h('span.grow', r.method === 'mail' ? `Unsubscribe request sent to ${r.to}.` : 'Unsubscribed.'));
          el.classList.add('done');
        } catch (err) {
          b.disabled = false;
          errorToast(err);
        }
      },
    }, 'Unsubscribe'));
  } else if (u.url) {
    el.appendChild(h('a.btn.sm', { href: u.url, target: '_blank', rel: 'noopener noreferrer' }, icon('external', 'sm'), 'Unsubscribe page'));
  }
  return el;
}

/**
 * Fills the integrations above a message body (called after the body is drawn; loads in
 * the background and never delays reading).
 */
export async function decorateMessage(body, message, ctx, { accounts = [] } = {}) {
  const host = h('div.mx-extras');
  body.prepend(host);
  let info;
  try {
    info = await api.get(`/api/mail/messages/${enc(message.id)}/integrations`);
  } catch (_) {
    host.remove();
    return;
  }
  if (!host.isConnected) return;
  const icsIndex = info.ics_attachments?.[0];
  const parts = [
    info.imip && invitationCard(info.imip, message, ctx, { icsIndex }),
    ...(info.itineraries || []).map(reservationCard),
    unsubscribeLine(info.unsubscribe, message),
    senderLine(info.sender, message, accounts),
  ].filter(Boolean);
  if (!parts.length) { host.remove(); return; }
  host.append(...parts);
}

/** The AI summary (3+ messages) above the cards and reply suggestions below them. */
export function decorateThread(box, cards, thread, current, ctx, { compose, accounts }) {
  threadSummary(box, cards, thread, current).catch(() => {});
  const me = accounts.find((a) => a.id === current?.account_id)?.email;
  smartReplies(box, current, me, compose).catch(() => {});
}

// ---- dialogs ------------------------------------------------------------------------------

/** Adds the sender of a message to a new or an existing contact. Resolves with the contact. */
function contactDialog(message) {
  const name = h('input', { type: 'text', value: message.from_name || '', 'aria-label': 'Name' });
  const mode = { value: 'new' };
  const search = h('input', { type: 'search', placeholder: 'Search your contacts', 'aria-label': 'Search contacts' });
  const results = h('div.mx-contacts.panel');
  let picked = null;
  const newBox = h('div.field', h('label', 'Name'), name);
  const existingBox = h('div', { hidden: true }, search, results);
  const radio = (v, label) => h('label.check', h('input', {
    type: 'radio', name: 'mx-contact-mode', value: v, checked: v === 'new',
    onchange: () => { mode.value = v; newBox.hidden = v !== 'new'; existingBox.hidden = v === 'new'; if (v !== 'new') { search.focus(); find(); } },
  }), label);
  let seq = 0;
  async function find() {
    const mine = ++seq;
    clear(results).appendChild(spinner());
    try {
      const { contacts } = await api.get(`/api/contacts?q=${enc(search.value.trim())}`);
      if (mine !== seq) return;
      clear(results);
      if (!contacts.length) results.appendChild(h('div.empty.small', 'No contacts found'));
      for (const c of contacts.slice(0, 30)) {
        const row = h(`div.list-item.clickable${picked === c.id ? '.selected' : ''}`, {
          role: 'option', tabindex: 0,
          onclick: () => { picked = c.id; results.querySelectorAll('.selected').forEach((x) => x.classList.remove('selected')); row.classList.add('selected'); },
        }, icon('user', 'sm'), h('div.grow', h('div.ellipsis', c.name), h('div.small.muted.ellipsis', c.emails.join(', '))));
        results.appendChild(row);
      }
    } catch (e) { clear(results).appendChild(h('p.error-text', e.message)); }
  }
  search.addEventListener('input', () => find());
  return modal({
    title: `Add ${message.from_addr} to contacts`,
    body: h('div.mx-form', radio('new', 'New contact'), newBox, radio('existing', 'Add to an existing contact'), existingBox),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Add', kind: 'primary', run: async () => {
        const body = mode.value === 'new' ? { email: message.from_addr, name: name.value.trim() } : { email: message.from_addr, contact_id: picked };
        if (mode.value !== 'new' && !picked) { toast('Pick a contact', 'warn'); return undefined; }
        const { contact } = await api.post('/api/mail/contacts', body);
        toast(`${message.from_addr} added to ${contact.name}`, 'ok', { timeout: 2500 });
        return contact;
      },
    }],
  });
}

async function writableCalendars(component) {
  const { calendars } = await api.get('/api/calendar/calendars');
  return calendars.filter((c) => c.writable && (c.components || '').split(',').includes(component));
}

/** Creates an event from a message, prefilled (and suggested by AI when available). */
async function eventDialog(message) {
  let draft;
  let cals;
  try {
    [{ draft }, cals] = await Promise.all([api.get(`/api/mail/messages/${enc(message.id)}/agenda-draft`), writableCalendars('VEVENT')]);
  } catch (e) { errorToast(e); return; }
  const title = h('input', { type: 'text', value: draft.summary, 'aria-label': 'Title' });
  const start = h('input', { type: 'datetime-local', value: localInput(draft.start), 'aria-label': 'Start' });
  const end = h('input', { type: 'datetime-local', value: localInput(draft.end), 'aria-label': 'End' });
  const allDay = h('input', { type: 'checkbox' });
  const location = h('input', { type: 'text', 'aria-label': 'Location' });
  const description = h('textarea', { rows: 5, 'aria-label': 'Description' }, draft.description);
  const cal = h('select', { 'aria-label': 'Calendar' }, cals.map((c) => h('option', { value: c.uri }, c.name)));
  const invite = draft.attendees.map((a) => ({ a, box: h('input', { type: 'checkbox' }) }));
  const ai = h('div.small.muted.mx-ai', { role: 'status' });
  let touched = false;
  [title, start, end, description].forEach((i) => i.addEventListener('input', () => { touched = true; }));
  start.addEventListener('change', () => {
    const d = new Date(end.value) - new Date(start.value);
    if (!(d > 0)) end.value = localInput(new Date(new Date(start.value).getTime() + 3600000));
  });
  allDay.addEventListener('change', () => {
    // Read the dates before the input type changes (that clears values of the other type).
    const s = start.value.slice(0, 10);
    const e = (end.value || start.value).slice(0, 10);
    const type = allDay.checked ? 'date' : 'datetime-local';
    start.type = type;
    end.type = type;
    start.value = allDay.checked ? s : `${s}T09:00`;
    end.value = allDay.checked ? e : `${e}T10:00`;
  });
  aiStatus().then(async (st) => {
    if (!st.enabled) return;
    ai.replaceChildren(mi('sparkle', 'sm'), ' Asking AI for suggestions…');
    try {
      const { suggestion: s } = await api.post(`/api/mail/messages/${enc(message.id)}/ai/event`, {});
      if (touched) { ai.textContent = 'AI suggestions are ready but you already edited the event.'; return; }
      if (s.title) title.value = s.title;
      if (s.description) description.value = `${s.description}\n\n${draft.description}`;
      if (s.start) { start.value = localInput(s.start); end.value = localInput(s.end || new Date(new Date(s.start).getTime() + 3600000)); }
      ai.replaceChildren(mi('sparkle', 'sm'), ' Suggested by AI from the conversation; check before saving.');
    } catch (e) { ai.textContent = `No AI suggestion: ${e.message}`; }
  });
  return modal({
    title: 'Create event',
    wide: true,
    body: h('div.mx-form',
      h('div.field', h('label', 'Title'), title),
      h('div.mx-grid', h('div.field', h('label', 'Start'), start), h('div.field', h('label', 'End'), end)),
      h('label.check', allDay, 'All day'),
      h('div.mx-grid', h('div.field', h('label', 'Location'), location), h('div.field', h('label', 'Calendar'), cal)),
      invite.length > 0 && h('fieldset.mx-invite', h('legend', 'Invite'), invite.map(({ a, box }) => h('label.check', box, a.name ? `${a.name} <${a.address}>` : a.address))),
      h('div.field', h('label', 'Description'), description),
      ai),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Create event', kind: 'primary', run: async () => {
        if (!title.value.trim() || !start.value) { toast('Give the event a title and a start', 'warn'); return undefined; }
        const toIso = (v) => (allDay.checked ? `${v}T00:00:00Z` : new Date(v).toISOString());
        const attendees = invite.filter((x) => x.box.checked).map(({ a }) => (a.name ? `${a.name} <${a.address}>` : a.address));
        const endIso = allDay.checked ? new Date(new Date(`${end.value || start.value}T00:00:00Z`).getTime() + 86400000).toISOString() : toIso(end.value || start.value);
        const { event } = await api.post('/api/calendar/events', {
          summary: title.value.trim(), start: toIso(start.value), end: endIso, all_day: allDay.checked,
          location: location.value.trim() || null, description: description.value, calendar: cal.value || null,
          attendees: attendees.length ? attendees : null,
        });
        toast(attendees.length ? 'Event created and invitations sent' : 'Event created', 'ok', { timeout: 2500 });
        return event;
      },
    }],
  });
}

/** Creates a task (VTODO) from a message. */
async function taskDialog(message) {
  let draft;
  let cals;
  try {
    [{ draft }, cals] = await Promise.all([api.get(`/api/mail/messages/${enc(message.id)}/agenda-draft`), writableCalendars('VTODO')]);
  } catch (e) { errorToast(e); return; }
  if (!cals.length) { toast('None of your calendars holds tasks', 'warn'); return; }
  const title = h('input', { type: 'text', value: draft.summary, 'aria-label': 'Title' });
  const due = h('input', { type: 'datetime-local', 'aria-label': 'Due' });
  const description = h('textarea', { rows: 5, 'aria-label': 'Description' }, draft.description);
  const cal = h('select', { 'aria-label': 'Calendar' }, cals.map((c) => h('option', { value: c.uri }, c.name)));
  await modal({
    title: 'Create task',
    body: h('div.mx-form',
      h('div.field', h('label', 'Title'), title),
      h('div.mx-grid', h('div.field', h('label', 'Due (optional)'), due), h('div.field', h('label', 'Calendar'), cal)),
      h('div.field', h('label', 'Description'), description)),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Create task', kind: 'primary', run: async () => {
        if (!title.value.trim()) { toast('Give the task a title', 'warn'); return undefined; }
        await api.post('/api/mail/tasks', {
          summary: title.value.trim(), description: description.value, due: due.value ? new Date(due.value).toISOString() : null, calendar: cal.value,
        });
        toast('Task created', 'ok', { timeout: 2500 });
        return true;
      },
    }],
  });
}

// ---- menus and the folder pane ---------------------------------------------------------------

/** Entries for the reader's "More actions" menu. */
export function messageMenuItems(message, ctx, { accounts }) {
  if (!message) return [];
  const account = accounts.find((a) => a.id === message.account_id);
  return [
    'sep',
    { label: 'Create event', icon: 'calendar', run: () => eventDialog(message) },
    { label: 'Create task', icon: 'check', run: () => taskDialog(message) },
    message.from_addr && { label: 'Add sender to contacts', icon: 'contacts', run: () => contactDialog(message) },
    account && message.from_addr && { label: 'Create filter from message', icon: 'settings', run: () => filtersDialog(ctx, account, { tab: 'filters', prefill: filterFromMessage(message) }) },
    {
      label: 'Translate', icon: 'globe',
      run: async () => {
        const st = await aiStatus();
        if (!st.enabled) { toast(st.llm_processing ? 'Turn on the assistant in Settings → AI to translate' : 'AI for mail is not available on this server', 'warn'); return; }
        const body = document.querySelector(`.ml-card[data-id="${CSS.escape(message.id)}"] .ml-card-body`);
        if (body) translateMessage(body, message.id);
      },
    },
  ].filter(Boolean);
}

/** Entries for an account's menu in the folder pane. */
export function accountMenuItems(account, ctx) {
  return [{ label: 'Filters and autoresponder', icon: 'settings', run: () => filtersDialog(ctx, account) }];
}

/** The folder pane's follow-up reminders. */
export function navExtras(tree, ctx, { openMessage, compose }) {
  const slot = h('div.mx-nav-extra');
  tree.prepend(slot);
  followUpButton(slot, { openMessage, compose }).catch(() => {});
}
