// Contacts: the user's address books as one searchable list with a detail pane; create,
// edit and delete contacts. The same cards sync over CardDAV, so the list refreshes on
// `groupware.changed` whichever client made the change.
//
// Route: /contacts?c=<id> selects a contact. Editing replaces name, emails, phones,
// organization and note; the server keeps every other vCard field (photo, birthday…).

import { api, q } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, modal, toast, errorToast, confirmDialog, debounce, emptyState, copyText } from '../ui.js';
import { syncHelpButton } from './calendar.js';

const collator = new Intl.Collator(undefined, { sensitivity: 'base', numeric: true });

/** The letter a contact is grouped under ("#" for names not starting with a letter). */
function initial(name) {
  const ch = [...(name || '').trim()][0] || '#';
  return /\p{L}/u.test(ch) ? ch.toLocaleUpperCase() : '#';
}

export default async function render(root, ctx) {
  ctx.setTitle('Contacts');
  const state = { contacts: [], selected: ctx.query.c || null, filter: '', loaded: false };

  const search = h('input', {
    type: 'search', placeholder: 'Search contacts', title: 'Search by name, email, phone or organization', 'aria-label': 'Search contacts',
    oninput: () => { state.filter = search.value.trim().toLowerCase(); drawList(); },
    onkeydown: (e) => {
      if (e.key === 'Escape' && search.value) { e.stopPropagation(); search.value = ''; state.filter = ''; drawList(); }
      if (e.key === 'ArrowDown') { e.preventDefault(); list.querySelector('.ct-row')?.focus(); }
    },
  });
  const list = h('div.ct-list', { role: 'listbox', 'aria-label': 'Contacts', onkeydown: listKeys });
  const count = h('span.small.muted.ct-count');
  const detail = h('section.ct-detail', { 'aria-live': 'polite' });
  const page = h('div.page-full.ct-page',
    h('aside.ct-side',
      h('div.ct-side-head',
        h('h1', 'Contacts'), count,
        h('button.btn.primary.sm.ct-new', { type: 'button', onclick: create, 'aria-label': 'New contact' }, icon('plus', 'sm'), h('span', 'New contact'))),
      h('div.ct-search', icon('search', 'sm'), search),
      list,
      h('div.ct-side-foot', syncHelpButton(ctx, { what: 'CardDAV', apps: 'your contacts app (DAVx⁵, Apple Contacts, Thunderbird, GNOME, Evolution…)' }))),
    detail);
  root.appendChild(page);

  const matches = (c) => !state.filter
    || [c.name, c.org, ...c.emails, ...c.phones].some((v) => (v || '').toLowerCase().includes(state.filter));

  function select(id, { focus = false } = {}) {
    state.selected = id;
    history.replaceState({}, '', `/contacts${q({ c: id })}`);
    drawList();
    drawDetail();
    if (focus) detail.querySelector('h2')?.focus();
  }

  function listKeys(e) {
    const rows = [...list.querySelectorAll('.ct-row')];
    const i = rows.indexOf(document.activeElement);
    if (i < 0) return;
    if (e.key === 'ArrowDown' && rows[i + 1]) { e.preventDefault(); rows[i + 1].focus(); }
    if (e.key === 'ArrowUp') { e.preventDefault(); (rows[i - 1] || search).focus(); }
  }

  function drawList() {
    const focusedId = document.activeElement?.closest?.('.ct-row')?.dataset.id;
    clear(list);
    const shown = state.contacts.filter(matches);
    count.textContent = state.loaded ? String(state.contacts.length) : '';
    if (!state.contacts.length && state.loaded) {
      list.appendChild(h('div.ct-empty.small.muted', 'No contacts yet.'));
      return;
    }
    if (!shown.length && state.loaded) {
      list.appendChild(h('div.ct-empty.small.muted', `No contacts match “${search.value.trim()}”.`));
      return;
    }
    let letter = null;
    for (const c of shown) {
      const l = initial(c.name);
      if (l !== letter) {
        letter = l;
        list.appendChild(h('div.ct-letter', { role: 'presentation' }, l));
      }
      const sel = c.id === state.selected;
      const row = h('button.ct-row', {
        type: 'button', role: 'option', 'aria-selected': String(sel), dataset: { id: c.id }, class: sel ? 'selected' : '',
        onclick: () => select(c.id, { focus: matchMedia('(max-width: 760px)').matches }),
      },
      avatar(c.name || '?', { seed: c.uid }),
      h('span.ct-row-text',
        h('span.ct-row-name', c.name || 'Unnamed'),
        (c.org || c.emails[0] || c.phones[0]) && h('span.ct-row-sub', c.org || c.emails[0] || c.phones[0])));
      list.appendChild(row);
      if (c.id === focusedId) row.focus();
    }
  }

  function drawDetail() {
    clear(detail);
    const c = state.contacts.find((x) => x.id === state.selected);
    page.classList.toggle('has-selection', !!c);
    if (!c) {
      detail.appendChild(state.contacts.length
        ? emptyState('contacts', 'Select a contact', 'Pick someone from the list to see their details.')
        : emptyState('contacts', 'Your address book is empty', 'Add people here, or sync contacts from your phone or desktop over CardDAV.',
          h('button.btn.primary', { type: 'button', onclick: create }, icon('plus'), 'New contact')));
      return;
    }
    const section = (title, ic, items) => items.length > 0 && h('div.ct-section',
      h('h3', icon(ic, 'sm'), title),
      h('ul.ct-values', items));
    const value = (text, href, label) => h('li.ct-value',
      href ? h('a', { href }, text) : h('span', text),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Copy ${label}`, title: 'Copy', onclick: () => copyText(text, `${label[0].toUpperCase()}${label.slice(1)} copied`) }, icon('copy', 'sm')));
    detail.appendChild(h('article.ct-card', { dataset: { id: c.id } },
      h('div.ct-card-top',
        h('button.btn.ghost.sm.ct-back', { type: 'button', onclick: () => select(null) }, icon('arrow-left', 'sm'), 'All contacts'),
        h('div.grow'),
        h('button.btn.sm', { type: 'button', onclick: () => contactForm(c) }, icon('edit', 'sm'), 'Edit'),
        h('button.btn.sm.danger', { type: 'button', onclick: () => remove(c) }, icon('trash', 'sm'), 'Delete')),
      h('header.ct-hero',
        avatar(c.name || '?', { seed: c.uid, size: 'lg' }),
        h('div.grow',
          h('h2', { tabindex: '-1' }, c.name || 'Unnamed'),
          c.org && h('div.ct-org', c.org))),
      section('Email', 'mail', c.emails.map((e) => value(e, `mailto:${e}`, 'email address'))),
      section('Phone', 'phone', c.phones.map((p) => value(p, `tel:${p.replace(/[^\d+*#,;]/g, '')}`, 'phone number'))),
      c.note && h('div.ct-section', h('h3', icon('note', 'sm'), 'Notes'), h('div.ct-note', c.note)),
      !c.emails.length && !c.phones.length && !c.note && h('p.muted.small', 'No email addresses, phone numbers or notes.')));
  }

  async function load() {
    try {
      const { contacts } = await api.get('/api/contacts');
      contacts.sort((a, b) => collator.compare(a.name, b.name));
      state.contacts = contacts;
      state.loaded = true;
      if (state.selected && !contacts.some((c) => c.id === state.selected)) {
        state.selected = null;
        history.replaceState({}, '', '/contacts');
      }
    } catch (e) {
      errorToast(e);
    }
    drawList();
    drawDetail();
  }

  /** A list of inputs that grows with an "Add" button (emails, phones). */
  function multiField(label, type, placeholder, addLabel, values = []) {
    const box = h('div.ct-multi');
    const add = (value = '') => {
      const input = h('input', { type, value, placeholder, 'aria-label': label, autocomplete: 'off' });
      const row = h('div.row', input, h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Remove ${label.toLowerCase()}`, onclick: () => { row.remove(); if (!box.querySelector('input')) add(); } }, icon('x', 'sm')));
      box.appendChild(row);
      return input;
    };
    if (values.length) values.forEach((v) => add(v)); else add();
    const wrap = h('div.field', h('label', label), box,
      h('button.btn.ghost.sm.ct-add', { type: 'button', onclick: () => add().focus() }, icon('plus', 'sm'), addLabel));
    wrap.values = () => [...box.querySelectorAll('input')].map((i) => i.value.trim()).filter(Boolean);
    wrap.invalid = () => [...box.querySelectorAll('input')].find((i) => !i.checkValidity());
    return wrap;
  }

  function create() { return contactForm(null); }

  /** The create (c = null) or edit dialog. */
  async function contactForm(c) {
    const editing = !!c;
    let books = [];
    if (!editing) {
      try { ({ addressbooks: books } = await api.get('/api/calendar/calendars')); } catch (_) { /* default book */ }
    }
    const name = h('input', { type: 'text', id: 'ct-name', value: c?.name || '', placeholder: 'Full name', required: true, autocomplete: 'off' });
    const emails = multiField('Email', 'email', 'name@example.com', 'Add email', c?.emails || []);
    const phones = multiField('Phone', 'tel', '+1 555 0100', 'Add phone', c?.phones || []);
    const org = h('input', { type: 'text', id: 'ct-org', value: c?.org || '', placeholder: 'Company or organization', autocomplete: 'off' });
    const note = h('textarea', { id: 'ct-note', rows: 3, placeholder: 'Anything worth remembering' }, c?.note || '');
    // Only worth asking when there is a choice (more books come from CardDAV clients).
    const book = books.length > 1 && h('select', { id: 'ct-book' }, books.map((b) => h('option', { value: b.uri }, b.name)));
    const err = h('div.error-text', { role: 'alert' });
    const form = h('form.ct-form', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
      h('div.field', h('label', { for: name.id }, 'Name'), name),
      emails, phones,
      h('div.field', h('label', { for: org.id }, 'Organization'), org),
      h('div.field', h('label', { for: note.id }, 'Notes'), note),
      book && h('div.field', h('label', { for: book.id }, 'Address book'), book),
      h('button', { type: 'submit', hidden: true, tabindex: '-1', 'aria-hidden': 'true' }),
      err);
    setTimeout(() => name.focus(), 0);
    const saved = await modal({
      title: editing ? 'Edit contact' : 'New contact',
      body: form,
      actions: [
        { label: 'Cancel', value: null },
        {
          label: editing ? 'Save' : 'Create contact', kind: 'primary', run: async () => {
            err.textContent = '';
            if (!name.value.trim()) { err.textContent = 'Enter a name.'; name.focus(); return undefined; }
            const bad = emails.invalid();
            if (bad) { err.textContent = `“${bad.value}” is not an email address.`; bad.focus(); return undefined; }
            const body = { name: name.value.trim(), emails: emails.values(), phones: phones.values(), org: org.value.trim(), note: note.value.trim() };
            if (book) body.addressbook = book.value;
            const { contact } = editing
              ? await api.patch(`/api/contacts/${encodeURIComponent(c.id)}`, body)
              : await api.post('/api/contacts', body);
            return contact;
          },
        },
      ],
    });
    if (!saved) return;
    if (!editing) {
      search.value = '';
      state.filter = '';
    }
    state.selected = saved.id;
    await load();
    select(saved.id);
    toast(editing ? `${saved.name} updated` : `${saved.name} added`, 'ok', { timeout: 2500 });
  }

  async function remove(c) {
    if (!await confirmDialog('Delete contact', `Delete ${c.name || 'this contact'}? It is removed from your synced devices too.`, { confirm: 'Delete', danger: true })) return;
    try {
      await api.del(`/api/contacts/${encodeURIComponent(c.id)}`);
      state.contacts = state.contacts.filter((x) => x.id !== c.id);
      select(null);
      toast(`${c.name || 'Contact'} deleted`, 'ok', { timeout: 2500 });
    } catch (e) {
      errorToast(e);
    }
  }

  list.appendChild(h('div.loading', h('span.spinner')));
  await load();
  const reload = debounce(load, 200);
  const offs = [live.on('groupware.changed', (ev) => { if (ev.kind !== 'calendar') reload(); }), live.on('hello', reload)];
  return () => offs.forEach((off) => off());
}
