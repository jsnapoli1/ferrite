// Calendar invitations and sharing, used by the Calendar view: the attendees field of the
// event form (with suggestions from Ferrite users and contacts), attendee lists with answer
// chips and the accept / maybe / decline bar of the event dialog, and the share dialog of a
// calendar (people and groups with a permission each, plus a public subscription link).
//
// Invitations are sent by the server whichever app changed the event (see
// ferrite_core::scheduling), so nothing here sends mail; the form only says when people
// without an account cannot be emailed because the server has no outgoing mail.

import { api, q } from '../api.js';
import { h, clear, icon, avatar, modal, toast, errorToast, spinner, copyText, debounce, confirmDialog } from '../ui.js';

let uid = 0;
const nextId = (p) => `${p}-${++uid}`;

/** Label, pill tone and icon of each participation status. */
export const PARTSTAT = {
  ACCEPTED: ['Going', 'ok', 'check'],
  TENTATIVE: ['Maybe', 'warn', 'clock'],
  DECLINED: ['Not going', 'danger', 'x'],
  'NEEDS-ACTION': ['Awaiting reply', '', 'mail'],
};
const status = (p) => PARTSTAT[p] || PARTSTAT['NEEDS-ACTION'];

const EMAIL = /^[^\s@<>()",;:]+@[^\s@<>()",;:]+\.[^\s@<>()",;:]+$/;

/** "Name <email>" or an address → { email, name }, or null when it is not an address. */
export function parsePerson(text) {
  const t = String(text || '').trim();
  const m = /^(.*)<([^>]+)>$/.exec(t);
  const email = (m ? m[2] : t).trim().replace(/^mailto:/i, '').toLowerCase();
  if (!EMAIL.test(email)) return null;
  const name = m ? m[1].trim().replace(/^"|"$/g, '') : '';
  return { email, name };
}

const label = (p) => p.name || p.email;

/** A pill showing an attendee's answer. */
export function statusPill(partstat) {
  const [text, tone, ic] = status(partstat);
  return h(`span.pill.cal-rsvp-pill${tone ? '.' + tone : ''}`, { dataset: { partstat } }, icon(ic, 'sm'), text);
}

/** Organizer and attendees of an event, each with their answer. */
export function attendeeList(ev) {
  const counts = {};
  for (const a of ev.attendees) counts[a.partstat] = (counts[a.partstat] || 0) + 1;
  const summary = ['ACCEPTED', 'TENTATIVE', 'DECLINED', 'NEEDS-ACTION']
    .filter((p) => counts[p]).map((p) => `${counts[p]} ${status(p)[0].toLowerCase()}`).join(' · ');
  const person = (p, extra) => h('li.cal-person',
    avatar(label(p), { size: 'sm', seed: p.email }),
    h('span.cal-person-name',
      h('span.ellipsis', label(p)),
      p.name && h('span.small.muted.ellipsis', p.email)),
    extra);
  return h('div.cal-people',
    h('div.cal-people-head.small.muted', `${ev.attendees.length} ${ev.attendees.length === 1 ? 'guest' : 'guests'}`, summary && ` · ${summary}`),
    h('ul.cal-people-list', { 'aria-label': 'Guests' },
      ev.organizer && person(ev.organizer, h('span.pill.info.cal-rsvp-pill', 'Organizer')),
      ev.attendees.map((a) => person(a, statusPill(a.partstat)))));
}

/**
 * Accept / maybe / decline for an invitation. Calls onAnswered(event) with the updated
 * event after the server recorded the answer.
 */
export function rsvpBar(ev, onAnswered) {
  const choices = [['ACCEPTED', 'Yes', 'accepted'], ['TENTATIVE', 'Maybe', 'tentative'], ['DECLINED', 'No', 'declined']];
  const buttons = choices.map(([p, text, resp]) => {
    const b = h('button.btn.sm.cal-rsvp-btn', {
      type: 'button', dataset: { response: resp }, 'aria-pressed': String(ev.my_partstat === p),
      onclick: async () => {
        buttons.forEach((x) => { x.disabled = true; });
        try {
          const { event } = await api.post(`/api/calendar/events/${encodeURIComponent(ev.id)}/respond`, { response: resp });
          buttons.forEach((x) => x.setAttribute('aria-pressed', String(x === b)));
          toast(p === 'DECLINED' ? 'You declined the invitation' : p === 'TENTATIVE' ? 'You answered maybe' : 'You accepted the invitation', 'ok', { timeout: 2500 });
          onAnswered(event);
        } catch (e) {
          errorToast(e);
        } finally {
          buttons.forEach((x) => { x.disabled = false; });
        }
      },
    }, icon(status(p)[2], 'sm'), text);
    return b;
  });
  return h('div.cal-rsvp', { role: 'group', 'aria-label': 'Your answer' },
    h('span.cal-rsvp-q', ev.my_partstat === 'NEEDS-ACTION' ? `${ev.organizer ? label(ev.organizer) : 'Someone'} invited you. Going?` : 'Going?'),
    h('div.cal-rsvp-btns', buttons));
}

/**
 * The attendees field of the event form. `initial`: the event's attendees; `info`: the
 * calendars response ({can_email, email}). Returns { el, value() } where value() lists
 * "Name <email>" strings.
 */
export function attendeesField(initial, info) {
  const people = (initial || []).map((a) => ({ email: a.email, name: a.name || '', partstat: a.partstat }));
  const inputId = nextId('ev-guests');
  const listId = nextId('ev-guests-list');
  const chips = h('ul.cal-guests-chips', { 'aria-label': 'Guests' });
  const input = h('input.cal-guests-input', {
    type: 'text', id: inputId, autocomplete: 'off', placeholder: people.length ? 'Add more guests' : 'Add guests by name or email',
    role: 'combobox', 'aria-expanded': 'false', 'aria-controls': listId, 'aria-autocomplete': 'list',
    disabled: !info.email,
  });
  const box = h('div.cal-guests', { onclick: (e) => { if (e.target === box || e.target === chips) input.focus(); } }, chips, input);
  const notes = h('div.cal-guests-notes');
  let pop = null;
  let options = [];
  let active = -1;

  function drawChips() {
    clear(chips);
    people.forEach((p, i) => chips.appendChild(h('li.cal-guest', { title: p.email, dataset: { email: p.email } },
      avatar(label(p), { size: 'sm', seed: p.email }),
      h('span.ellipsis', label(p)),
      p.partstat && p.partstat !== 'NEEDS-ACTION' && h('span.cal-guest-status', { class: `tone-${status(p.partstat)[1]}`, title: status(p.partstat)[0], 'aria-label': status(p.partstat)[0] }, icon(status(p.partstat)[2], 'sm')),
      h('button.cal-guest-x', { type: 'button', 'aria-label': `Remove ${label(p)}`, onclick: () => { people.splice(i, 1); drawChips(); input.focus(); } }, icon('x', 'sm')))));
    input.placeholder = people.length ? 'Add more guests' : 'Add guests by name or email';
  }

  function drawNotes() {
    clear(notes);
    if (!info.email) {
      notes.appendChild(h('p.cal-guests-note.warn.small', icon('mail', 'sm'), h('span', 'Add your email address in Settings › Profile to invite people: invitations come from your address.')));
    } else if (!info.can_email) {
      notes.appendChild(h('p.cal-guests-note.small', icon('mail', 'sm'), h('span', 'This server has no outgoing mail, so invitations can’t be emailed. People with a Ferrite account still get them in their calendar.')));
    }
  }

  function add(p) {
    if (!p || people.some((x) => x.email === p.email)) return false;
    if (info.email && p.email === info.email.toLowerCase()) return false;
    people.push({ email: p.email, name: p.name || '' });
    drawChips();
    return true;
  }

  function closePop() {
    if (!pop) return;
    pop.remove();
    pop = null;
    options = [];
    active = -1;
    input.setAttribute('aria-expanded', 'false');
    input.removeAttribute('aria-activedescendant');
    window.removeEventListener('resize', place);
    document.removeEventListener('scroll', place, true);
  }

  function place() {
    if (!pop) return;
    const r = box.getBoundingClientRect();
    pop.style.left = `${r.left}px`;
    pop.style.width = `${r.width}px`;
    const below = window.innerHeight - r.bottom;
    pop.style.top = below > 200 || below > r.top ? `${r.bottom + 4}px` : `${Math.max(8, r.top - pop.offsetHeight - 4)}px`;
  }

  function setActive(i) {
    active = i;
    options.forEach((o, k) => o.el.setAttribute('aria-selected', String(k === i)));
    if (options[i]) {
      input.setAttribute('aria-activedescendant', options[i].el.id);
      options[i].el.scrollIntoView({ block: 'nearest' });
    }
  }

  function pick(p) {
    add(p);
    input.value = '';
    closePop();
    input.focus();
  }

  const suggest = debounce(async () => {
    const text = input.value.trim();
    if (!text || document.activeElement !== input) { closePop(); return; }
    let found = [];
    try {
      ({ people: found } = await api.get(`/api/calendar/attendees${q({ q: text })}`));
    } catch (_) { found = []; }
    if (input.value.trim() !== text || document.activeElement !== input) return;
    found = found.filter((p) => !people.some((x) => x.email === p.email));
    const typed = parsePerson(text);
    const items = found.map((p) => ({ p, title: p.name || p.email, sub: p.user ? `${p.email} · Ferrite user` : p.email }));
    if (typed && !found.some((p) => p.email === typed.email) && !people.some((x) => x.email === typed.email)) {
      items.push({ p: typed, title: `Invite ${typed.email}`, sub: 'By email', typed: true });
    }
    if (!items.length) { closePop(); return; }
    if (!pop) {
      pop = h('ul.popover.cal-guests-pop', { id: listId, role: 'listbox', 'aria-label': 'Suggestions' });
      document.body.appendChild(pop);
      window.addEventListener('resize', place);
      document.addEventListener('scroll', place, true);
    }
    clear(pop);
    options = items.map((it) => {
      const el = h('li.cal-guests-opt', {
        id: nextId('ev-guest-opt'), role: 'option', 'aria-selected': 'false',
        onmousedown: (e) => { e.preventDefault(); pick(it.p); },
      },
      it.typed ? h('span.cal-guests-opt-icon', icon('mail', 'sm')) : avatar(it.title, { size: 'sm', seed: it.p.email }),
      h('span.cal-guests-opt-text', h('span.ellipsis', it.title), h('span.small.muted.ellipsis', it.sub)));
      pop.appendChild(el);
      return { el, p: it.p };
    });
    input.setAttribute('aria-expanded', 'true');
    setActive(0);
    place();
  }, 150);

  input.addEventListener('input', suggest);
  input.addEventListener('blur', () => {
    // Typing an address and leaving the field adds it.
    const typed = parsePerson(input.value);
    if (typed && add(typed)) input.value = '';
    setTimeout(closePop, 0);
  });
  input.addEventListener('keydown', (e) => {
    if (e.key === 'ArrowDown' && options.length) { e.preventDefault(); setActive((active + 1) % options.length); }
    else if (e.key === 'ArrowUp' && options.length) { e.preventDefault(); setActive((active - 1 + options.length) % options.length); }
    else if (e.key === 'Escape' && pop) { e.preventDefault(); e.stopPropagation(); closePop(); }
    else if (e.key === 'Enter' || e.key === ',' || e.key === ';' || (e.key === 'Tab' && input.value.trim())) {
      const choice = options[active]?.p || parsePerson(input.value);
      if (choice) { e.preventDefault(); pick(choice); }
      else if (e.key !== 'Tab' && input.value.trim()) { e.preventDefault(); toast('Enter an email address, or pick a suggestion', 'warn', { timeout: 2500 }); }
    } else if (e.key === 'Backspace' && !input.value && people.length) {
      people.pop();
      drawChips();
    }
  });

  drawChips();
  drawNotes();
  const el = h('div.field.cal-guests-field', h('label', { for: inputId }, 'Guests'), box, notes);
  el.cleanup = closePop;
  return {
    el,
    value: () => {
      const typed = parsePerson(input.value);
      if (typed) { add(typed); input.value = ''; }
      return people.map((p) => (p.name ? `${p.name} <${p.email}>` : p.email));
    },
    close: closePop,
  };
}

// ---- sharing --------------------------------------------------------------------------

/** Opens the share dialog of one of the user's calendars. `onChange` runs after changes. */
export async function shareCalendarDialog(cal, onChange) {
  const base = `/api/calendar/calendars/${encodeURIComponent(cal.uri)}`;
  const list = h('div.cal-share-list', spinner());
  const linkHost = h('div.cal-share-link');
  let kind = 'user';
  let publicUrl = null;

  async function refresh() {
    try {
      const { shares, public_url: url } = await api.get(`${base}/shares`);
      publicUrl = url;
      clear(list);
      if (!shares.length) list.appendChild(h('p.small.muted.cal-share-none', 'Only you can see this calendar. Share it with people or groups below.'));
      for (const s of shares) list.appendChild(shareRow(s));
      drawLink();
    } catch (e) {
      clear(list).appendChild(h('p.error-text', e.message));
    }
  }

  function shareRow(s) {
    const perm = h('select.sm', {
      'aria-label': `Permission for ${s.target_name}`,
      onchange: async () => {
        try {
          await api.patch(`${base}/shares/${encodeURIComponent(s.id)}`, { writable: perm.value === 'write' });
          toast(`${s.target_name} ${perm.value === 'write' ? 'can now edit' : 'can now only view'}`, 'ok', { timeout: 2000 });
          onChange();
        } catch (e) { errorToast(e); }
      },
    }, h('option', { value: 'read', selected: !s.writable }, 'Can view'), h('option', { value: 'write', selected: s.writable }, 'Can edit'));
    return h('div.cal-share-row', { dataset: { target: s.target } },
      s.kind === 'user' ? avatar(s.target_name, { size: 'sm', seed: s.target_id }) : h('span.cal-share-kind', icon('users', 'sm')),
      h('div.grow', h('div.ellipsis', s.target_name), h('div.small.muted', s.kind === 'user' ? `@${s.target}` : 'Group')),
      perm,
      h('button.btn.ghost.icon.sm', {
        type: 'button', 'aria-label': `Stop sharing with ${s.target_name}`, title: 'Stop sharing',
        onclick: async () => {
          try {
            await api.del(`${base}/shares/${encodeURIComponent(s.id)}`);
            await refresh();
            onChange();
          } catch (e) { errorToast(e); }
        },
      }, icon('x')));
  }

  function drawLink() {
    clear(linkHost);
    if (publicUrl) {
      const field = h('input.mono', { type: 'text', value: publicUrl, readonly: true, 'aria-label': 'Public subscription link', onfocus: (e) => e.target.select() });
      linkHost.append(
        h('p.small.muted', 'Anyone with this link can subscribe to the calendar in their calendar app (read only). Guests’ addresses and private events are left out.'),
        h('div.cal-share-copy', field, h('button.btn', { type: 'button', onclick: () => copyText(publicUrl, 'Link copied') }, icon('copy', 'sm'), 'Copy')),
        h('button.btn.sm.danger', {
          type: 'button',
          onclick: async () => {
            if (!await confirmDialog('Revoke public link', 'The link stops working for everyone who subscribed with it. A new link would be different.', { confirm: 'Revoke link', danger: true })) return;
            try {
              await api.del(`${base}/publish`);
              publicUrl = null;
              drawLink();
              toast('Public link revoked', 'ok', { timeout: 2000 });
              onChange();
            } catch (e) { errorToast(e); }
          },
        }, icon('trash', 'sm'), 'Revoke link'));
    } else {
      linkHost.append(
        h('p.small.muted', 'A read-only link people can subscribe to without an account.'),
        h('button.btn.sm', {
          type: 'button',
          onclick: async () => {
            try {
              ({ public_url: publicUrl } = await api.post(`${base}/publish`, {}));
              drawLink();
              onChange();
            } catch (e) { errorToast(e); }
          },
        }, icon('link', 'sm'), 'Create public link'));
    }
  }

  const err = h('div.error-text', { role: 'alert' });
  const listIdUsers = nextId('cal-share-users');
  const datalist = h('datalist', { id: listIdUsers });
  const target = h('input', { type: 'text', list: listIdUsers, autocomplete: 'off', placeholder: 'Name or username', 'aria-label': 'Person to share with' });
  const perm = h('select', { 'aria-label': 'Permission' }, h('option', { value: 'read' }, 'Can view'), h('option', { value: 'write' }, 'Can edit'));
  const segBtns = [['user', 'Person', 'user'], ['group', 'Group', 'users']].map(([k, text, ic]) => h('button', {
    type: 'button', role: 'tab', 'aria-selected': String(k === kind),
    onclick: () => {
      kind = k;
      segBtns.forEach((b) => b.setAttribute('aria-selected', String(b === segBtns[k === 'user' ? 0 : 1])));
      target.placeholder = k === 'user' ? 'Name or username' : 'Group name';
      target.setAttribute('aria-label', k === 'user' ? 'Person to share with' : 'Group to share with');
      target.value = '';
      clear(datalist);
      target.focus();
    },
  }, icon(ic, 'sm'), text));
  const fillTargets = debounce(async () => {
    try {
      const { users, groups } = await api.get(`/api/calendar/share-targets${q({ q: target.value.trim() })}`);
      clear(datalist);
      if (kind === 'user') users.forEach((u) => datalist.appendChild(h('option', { value: u.username }, u.display_name)));
      else groups.forEach((g) => datalist.appendChild(h('option', { value: g.name })));
    } catch (_) { /* suggestions are optional */ }
  }, 150);
  target.addEventListener('input', fillTargets);
  target.addEventListener('focus', fillTargets);
  const form = h('form.cal-share-form', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      if (!target.value.trim()) { err.textContent = kind === 'user' ? 'Choose a person.' : 'Choose a group.'; target.focus(); return; }
      try {
        await api.post(`${base}/shares`, { kind, target: target.value.trim(), writable: perm.value === 'write' });
        toast(`Shared “${cal.name}”`, 'ok', { timeout: 2000 });
        target.value = '';
        await refresh();
        onChange();
      } catch (ex) {
        err.textContent = ex.message;
      }
    },
  },
  h('div.fl-seg', { role: 'tablist', 'aria-label': 'Share with' }, segBtns),
  h('div.row.cal-share-add', target, datalist, perm, h('button.btn.primary', { type: 'submit' }, 'Share')),
  err);

  refresh();
  await modal({
    title: `Share “${cal.name}”`,
    body: h('div.cal-share-dialog',
      list,
      h('h3.cal-share-sub', 'Add people or a group'),
      form,
      h('h3.cal-share-sub', 'Public link'),
      linkHost),
    actions: [{ label: 'Done', kind: 'primary', value: true }],
  });
}
