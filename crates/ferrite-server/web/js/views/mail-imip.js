// Meeting invitations (iMIP) and travel reservations shown above a message in the reader.
//
// Invitation card: what, when (in the viewer's time zone, with the event's own zone when it
// differs), where, who, and what else is in the calendar then. Depending on the message:
// Accept / Tentative / Decline / Propose a new time (REQUEST), the guest's answer and whether
// it was applied (REPLY), the cancellation (CANCEL), a proposed new time to accept or decline
// (COUNTER), or "Add to calendar" for a plain calendar file. Reservation cards (flights,
// hotels, trains, buses, events, rental cars, restaurants) can be added to a calendar too.

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, modal } from '../ui.js';
import { mi } from './mail-integrations-icons.js';

const viewerZone = () => Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';

const dayFmt = (d) => d.toLocaleDateString([], { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric' });
const timeFmt = (d) => d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });

/** "Wed, 10 Mar 2027, 10:00 – 11:00" in the viewer's zone (all-day: dates only). */
export function whenText(start, end, allDay) {
  if (!start) return '';
  const s = new Date(start);
  const e = end ? new Date(end) : null;
  if (allDay) {
    const last = e && e > s ? new Date(e.getTime() - 86400000) : s;
    const fmt = (d) => d.toLocaleDateString([], { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric', timeZone: 'UTC' });
    return last.toDateString() === s.toDateString() || fmt(last) === fmt(s) ? `${fmt(s)} (all day)` : `${fmt(s)} – ${fmt(last)} (all day)`;
  }
  if (!e) return `${dayFmt(s)}, ${timeFmt(s)}`;
  return s.toDateString() === e.toDateString()
    ? `${dayFmt(s)}, ${timeFmt(s)} – ${timeFmt(e)}`
    : `${dayFmt(s)}, ${timeFmt(s)} – ${dayFmt(e)}, ${timeFmt(e)}`;
}

/** Value for a datetime-local input, in the viewer's zone. */
export function localInput(d) {
  const x = new Date(d);
  const p = (n) => String(n).padStart(2, '0');
  return `${x.getFullYear()}-${p(x.getMonth() + 1)}-${p(x.getDate())}T${p(x.getHours())}:${p(x.getMinutes())}`;
}

const PARTSTAT = {
  ACCEPTED: ['Accepted', 'ok', 'check'],
  TENTATIVE: ['Tentative', 'warn', 'clock'],
  DECLINED: ['Declined', 'danger', 'x'],
  DELEGATED: ['Delegated', 'info', 'share'],
  'NEEDS-ACTION': ['No answer yet', '', 'clock'],
};

function partstatPill(ps) {
  const [label, kind, ic] = PARTSTAT[ps] || PARTSTAT['NEEDS-ACTION'];
  return h(`span.pill${kind ? `.${kind}` : ''}`, icon(ic, 'sm'), label);
}

const person = (p) => (p?.name ? `${p.name} <${p.email}>` : p?.email || '');

const TITLES = {
  REQUEST: 'Invitation',
  REPLY: 'Answer to your invitation',
  CANCEL: 'Cancelled event',
  COUNTER: 'New time proposed',
  DECLINECOUNTER: 'Proposed time declined',
  PUBLISH: 'Event',
  ADD: 'Additional dates',
  REFRESH: 'Request for the latest version',
};

/** Picks a calendar for a new copy (own calendars that hold events). */
async function calendarSelect(kind = 'VEVENT') {
  const { calendars } = await api.get('/api/calendar/calendars');
  const own = calendars.filter((c) => !c.shared_by && c.writable && (c.components || '').split(',').includes(kind));
  return h('select.mi-cal-select', { 'aria-label': 'Calendar' }, own.map((c) => h('option', { value: c.uri }, c.name)));
}

function proposeDialog(card, messageId, onDone) {
  const start = h('input', { type: 'datetime-local', value: localInput(card.starts_at || Date.now()), 'aria-label': 'New start' });
  const dur = card.starts_at && card.ends_at ? new Date(card.ends_at) - new Date(card.starts_at) : 3600000;
  const end = h('input', { type: 'datetime-local', value: localInput(new Date(new Date(card.starts_at || Date.now()).getTime() + dur)), 'aria-label': 'New end' });
  start.addEventListener('change', () => { if (start.value) end.value = localInput(new Date(new Date(start.value).getTime() + dur)); });
  const comment = h('textarea', { rows: 2, placeholder: 'A note for the organizer (optional)', 'aria-label': 'Note' });
  return modal({
    title: 'Propose a new time',
    body: h('div.mi-form',
      h('p.small.muted', `The organizer (${person(card.organizer)}) gets your proposal by mail and decides.`),
      h('div.mi-row', h('div.field.grow', h('label', 'Start'), start), h('div.field.grow', h('label', 'End'), end)),
      h('div.field', h('label', 'Note'), comment)),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Send proposal', kind: 'primary', run: async () => {
        if (!start.value || !end.value || new Date(end.value) <= new Date(start.value)) { toast('The end must be after the start', 'warn'); return undefined; }
        const r = await api.post(`/api/mail/messages/${encodeURIComponent(messageId)}/imip/counter`, {
          start: new Date(start.value).toISOString(), end: new Date(end.value).toISOString(), comment: comment.value,
        });
        toast('Proposal sent to the organizer', 'ok', { timeout: 2500 });
        onDone(r.imip);
        return true;
      },
    }],
  });
}

/** The invitation card for a message (`card` from /integrations). */
export function invitationCard(card, message, ctx, { icsIndex } = {}) {
  const el = h('section.mi-card', { 'aria-label': TITLES[card.method] || 'Calendar' });
  const draw = (c) => {
    clear(el);
    el.className = `mi-card mi-${(c.method || '').toLowerCase()}${c.existing?.cancelled || c.method === 'CANCEL' ? ' mi-cancelled' : ''}`;
    const zone = viewerZone();
    const head = h('div.mi-head', mi('calendar'), h('span.mi-kind', TITLES[c.method] || 'Calendar'),
      c.recurring && h('span.pill', icon('repeat', 'sm'), 'Repeats'));
    const showZone = c.tzid && c.tzid !== zone && !c.all_day;
    const facts = h('dl.mi-facts',
      h('dt', 'When'), h('dd', h('span.mi-when', whenText(c.method === 'COUNTER' ? c.proposed_start : c.starts_at, c.method === 'COUNTER' ? c.proposed_end : c.ends_at, c.all_day)),
        !c.all_day && h('span.small.muted', ` (${zone})`),
        showZone && h('div.small.muted', `Organizer's time: ${c.when}`)),
      c.method === 'COUNTER' && c.existing && h('dt', 'Now'), c.method === 'COUNTER' && c.existing && h('dd.muted', c.when),
      c.location && h('dt', 'Where'), c.location && h('dd', c.location),
      c.organizer && h('dt', 'Organizer'), c.organizer && h('dd', person(c.organizer)),
      c.attendees.length > 0 && h('dt', 'Guests'),
      c.attendees.length > 0 && h('dd', h('details.mi-guests',
        h('summary', `${c.attendees.length} ${c.attendees.length === 1 ? 'guest' : 'guests'}`),
        h('ul', c.attendees.map((a) => h('li', h('span.ellipsis', person(a)), partstatPill(a.partstat)))))));
    el.append(head, h('h3.mi-title', c.summary || '(no title)'), facts);
    if (c.description) el.appendChild(h('p.mi-desc', c.description.length > 400 ? `${c.description.slice(0, 400)}…` : c.description));
    if (c.conflicts.length) {
      el.appendChild(h('div.mi-conflicts', icon('clock', 'sm'), h('span', 'Also in your calendar then: ',
        c.conflicts.map((x, i) => [i ? ', ' : '', h('strong', x.summary || '(no title)'), ` ${whenText(x.starts_at, x.ends_at)}`]).flat())));
    }
    if (!c.sender_ok && ['REQUEST', 'CANCEL', 'REPLY', 'COUNTER'].includes(c.method)) {
      const who = ['REPLY', 'COUNTER'].includes(c.method) ? c.from_attendee?.email || 'the guest' : c.organizer?.email || 'the organizer';
      el.appendChild(h('div.mi-warn', icon('lock', 'sm'), h('span', `This message was not sent by ${who}. Check it before you act on it.`)));
    }
    if (c.outdated) el.appendChild(h('div.mi-note.small', 'Your calendar has a newer version of this event.'));
    if (c.existing) {
      el.appendChild(h('div.mi-note.small', icon('calendar', 'sm'),
        c.existing.cancelled ? 'Cancelled in your calendar ' : `In your calendar “${c.existing.calendar_name}” `,
        h('a', { href: c.existing.link, onclick: (e) => { e.preventDefault(); ctx.navigate(c.existing.link); } }, 'Open')));
    }
    const actions = h('div.mi-actions');
    const msgId = message.id;
    const run = async (btn, path, body, done) => {
      actions.querySelectorAll('button, select, textarea').forEach((b) => { b.disabled = true; });
      btn.classList.add('busy');
      try {
        const r = await api.post(`/api/mail/messages/${encodeURIComponent(msgId)}/${path}`, body);
        if (done) done(r);
        if (r.imip) draw(r.imip);
      } catch (e) {
        errorToast(e);
        draw(c);
      }
    };
    if (c.method === 'REQUEST') {
      if (c.can_respond) {
        const mine = c.my_partstat && c.my_partstat !== 'NEEDS-ACTION' ? c.my_partstat : null;
        if (mine) actions.appendChild(h('span.mi-answer', 'Your answer: ', partstatPill(mine)));
        let calSel = null;
        const comment = h('input.mi-comment', { type: 'text', placeholder: 'Add a note (optional)', 'aria-label': 'Note for the organizer' });
        const answer = (ps, label, done, kind) => h(`button.btn.sm${kind ? `.${kind}` : ''}${mine === ps.toUpperCase() ? '.active' : ''}`, {
          type: 'button',
          onclick: (e) => run(e.currentTarget, 'imip/respond', { response: ps, calendar: calSel?.value || null, comment: comment.value || null },
            () => toast(`${done} — the organizer was told`, 'ok', { timeout: 2500 })),
        }, label);
        actions.append(
          answer('accepted', 'Accept', 'Accepted', mine ? '' : 'primary'),
          answer('tentative', 'Tentative', 'Tentatively accepted'),
          answer('declined', 'Decline', 'Declined'));
        el.appendChild(actions);
        const extra = h('div.mi-extra', comment,
          h('button.btn.sm.ghost', { type: 'button', onclick: () => proposeDialog(c, msgId, draw) }, icon('clock', 'sm'), 'Propose new time'));
        if (!c.existing) {
          calendarSelect().then((sel) => { if (sel.options.length > 1) { calSel = sel; extra.prepend(h('label.mi-inline', 'Add to', sel)); } }).catch(() => {});
        }
        el.appendChild(extra);
      } else if (c.organizer_is_me) {
        el.appendChild(h('div.mi-note.small', 'You organize this event.'));
      } else if (!c.me) {
        el.appendChild(h('div.mi-note.small', 'Your address is not on this invitation’s guest list.'));
        if (icsIndex !== undefined && !c.existing) {
          actions.appendChild(h('button.btn.sm', { type: 'button', onclick: (e) => run(e.currentTarget, `attachments/${icsIndex}/import`, {}, () => toast('Added to your calendar', 'ok')) }, icon('plus', 'sm'), 'Add to calendar'));
          el.appendChild(actions);
        }
      }
    }
    if (c.method === 'REPLY' && c.from_attendee) {
      const ps = c.attendees.find((a) => a.email === c.from_attendee.email)?.partstat;
      el.appendChild(h('div.mi-reply', h('strong', person(c.from_attendee)), ' answered ', partstatPill(ps)));
    }
    if (c.method === 'COUNTER' && c.organizer_is_me && c.existing) {
      if (c.comment) el.appendChild(h('blockquote.mi-quote', c.comment.trim()));
      actions.append(
        h('button.btn.sm.primary', { type: 'button', onclick: (e) => run(e.currentTarget, 'imip/counter-decision', { accept: true }, () => toast('The event was moved and the guests updated', 'ok')) }, 'Accept new time'),
        h('button.btn.sm', { type: 'button', onclick: (e) => run(e.currentTarget, 'imip/counter-decision', { accept: false }, () => toast('The guest was told', 'ok')) }, 'Decline'));
      el.appendChild(actions);
    }
    if (['PUBLISH', 'ADD'].includes(c.method) && icsIndex !== undefined && !c.existing) {
      const holder = h('span');
      actions.append(holder, h('button.btn.sm.primary', {
        type: 'button',
        onclick: (e) => run(e.currentTarget, `attachments/${icsIndex}/import`, { calendar: holder.querySelector('select')?.value || null },
          (r) => { toast('Added to your calendar', 'ok', { timeout: 2500 }); refresh(); return r; }),
      }, icon('plus', 'sm'), 'Add to calendar'));
      calendarSelect().then((sel) => { if (sel.options.length > 1) holder.appendChild(sel); }).catch(() => {});
      el.appendChild(actions);
    }
    // What happened to this message: automatic processing or the user's answer.
    const p = c.processed;
    // (An answered invitation already shows "Your answer".)
    if (p && ['applied', 'rejected', 'outdated', 'error', 'answered'].includes(p.status) && !(p.status === 'answered' && c.method === 'REQUEST' && c.can_respond)) {
      const label = { applied: 'Applied', rejected: 'Not applied', outdated: 'Outdated', error: 'Failed', answered: 'Answered' }[p.status];
      const line = h(`div.mi-status.small.${p.status}`, h('strong', `${label}: `), p.detail);
      if (p.status === 'rejected' && ['REPLY', 'CANCEL', 'REQUEST'].includes(c.method)) {
        line.appendChild(h('button.btn.sm.ghost', { type: 'button', onclick: (e) => run(e.currentTarget, 'imip/apply', { force: true }, () => toast('Applied', 'ok')) }, 'Apply anyway'));
      }
      el.appendChild(line);
    } else if (['REPLY', 'CANCEL'].includes(c.method) && !p) {
      el.appendChild(h('div.mi-status.small', h('button.btn.sm', { type: 'button', onclick: (e) => run(e.currentTarget, 'imip/apply', {}, (r) => toast(r.outcome.detail, 'ok')) }, 'Apply to my calendar')));
    }
  };
  async function refresh() {
    try {
      const { imip } = await api.get(`/api/mail/messages/${encodeURIComponent(message.id)}/integrations`);
      if (imip) draw(imip);
    } catch (_) { /* keep the card as it is */ }
  }
  draw(card);
  return el;
}

const TRIP_ICONS = { flight: 'plane', train: 'train', bus: 'bus', lodging: 'bed', event: 'ticket', rental_car: 'car', restaurant: 'food' };

function tripTime(w) {
  if (!w) return null;
  if (w.utc) return new Date(w.utc);
  if (w.date_only) return new Date(`${w.value}T00:00:00Z`);
  // A local time without zone (the place's own time): shown as written.
  return new Date(w.value);
}

/** A travel or booking reservation card. */
export function reservationCard(r) {
  const start = tripTime(r.start);
  const end = tripTime(r.end);
  const allDay = !!r.start?.date_only;
  // Trips already list where they start under "From".
  const where = r.location && !r.details.some(([, v]) => v === r.location) ? r.location : null;
  const el = h('section.mi-card.mi-trip', { 'aria-label': r.title },
    h('div.mi-head', mi(TRIP_ICONS[r.kind] || 'calendar'), h('span.mi-kind', { flight: 'Flight', train: 'Train', bus: 'Bus', lodging: 'Stay', event: 'Event ticket', rental_car: 'Rental car', restaurant: 'Table reservation' }[r.kind] || 'Reservation')),
    h('h3.mi-title', r.title),
    h('dl.mi-facts',
      start && h('dt', 'When'), start && h('dd', whenText(start, end, allDay), !allDay && r.start && !r.start.utc && h('span.small.muted', ' (local time)')),
      where && h('dt', 'Where'), where && h('dd', where),
      ...r.details.flatMap(([k, v]) => [h('dt', k), h('dd', v)]),
      r.reservation_number && h('dt', 'Booking'), r.reservation_number && h('dd', h('code', r.reservation_number)),
      r.under_name && h('dt', 'Name'), r.under_name && h('dd', r.under_name)));
  const actions = h('div.mi-actions');
  if (start) {
    actions.appendChild(h('button.btn.sm', {
      type: 'button',
      onclick: async (e) => {
        const btn = e.currentTarget;
        btn.disabled = true;
        const description = [...r.details.map(([k, v]) => `${k}: ${v}`), r.reservation_number && `Booking: ${r.reservation_number}`, r.url].filter(Boolean).join('\n');
        try {
          const endTime = end && end > start ? end : new Date(start.getTime() + (allDay ? 86400000 : 3600000));
          await api.post('/api/calendar/events', {
            summary: r.title, start: start.toISOString(), end: endTime.toISOString(), all_day: allDay,
            location: r.location || null, description: description || null,
          });
          btn.replaceWith(h('span.pill.ok', icon('check', 'sm'), 'In your calendar'));
        } catch (err) {
          btn.disabled = false;
          errorToast(err);
        }
      },
    }, icon('plus', 'sm'), 'Add to calendar'));
  }
  if (r.url) actions.appendChild(h('a.btn.sm.ghost', { href: r.url, target: '_blank', rel: 'noopener noreferrer' }, icon('external', 'sm'), 'Manage booking'));
  if (actions.childNodes.length) el.appendChild(actions);
  return el;
}
