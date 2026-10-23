// Mail dialogs for inbox management: moving to a folder, snoozing, tagging, quick actions,
// folder statistics, advanced search and an account's inbox settings.

import { api, q } from '../api.js';
import { h, clear, icon, modal, toast, errorToast, confirmDialog, bytes, dateTime, spinner } from '../ui.js';

export const ROLE_ICONS = { inbox: 'mail', sent: 'send', drafts: 'edit', trash: 'trash', junk: 'junk', archive: 'archive', snoozed: 'clock' };
export const ROLE_NAMES = { drafts: 'Drafts', sent: 'Sent', trash: 'Trash', archive: 'Archive', junk: 'Junk', snoozed: 'Snoozed' };
export const TAG_COLORS = ['#ff7a66', '#b4a443', '#31cc7c', '#317ccc', '#a85bf7', '#e9322d', '#f0a30a', '#0082c9', '#6b7280', '#db2777'];

/** Nesting depth of a folder (from its IMAP name). */
export const depth = (f) => (f.delimiter ? f.name.split(f.delimiter).length - 1 : 0);

/** A folder's full path for people ("Projects / 2026"). */
export function folderPath(f, all) {
  if (!f.delimiter) return f.display_name;
  const parts = f.name.split(f.delimiter);
  const names = [];
  for (let i = 1; i <= parts.length; i++) {
    const name = parts.slice(0, i).join(f.delimiter);
    names.push(all.find((x) => x.name === name)?.display_name || parts[i - 1]);
  }
  return names.join(' / ');
}

/** A tag chip in a tag's colour. */
export function tagChip(tag) {
  return h('span.ml-tag', { style: `--tag: ${tag.color}`, title: tag.name }, h('span.ml-tag-dot'), h('span.ellipsis', tag.name));
}

/** Picks a folder (filterable). Resolves with its id, or null. */
export async function moveDialog(folders, { title = 'Move to folder', exclude = null } = {}) {
  const usable = folders.filter((f) => f.selectable && f.id !== exclude);
  const filter = h('input', { type: 'search', placeholder: 'Filter folders', 'aria-label': 'Filter folders' });
  const list = h('div.ml-picker', { role: 'listbox', 'aria-label': 'Folders' });
  let pick = null;
  const draw = () => {
    clear(list);
    const term = filter.value.trim().toLowerCase();
    const shown = usable.filter((f) => !term || folderPath(f, folders).toLowerCase().includes(term));
    if (!shown.length) list.appendChild(h('p.small.muted', 'No folder matches.'));
    for (const f of shown) {
      list.appendChild(h('button.ml-picker-item', {
        type: 'button', role: 'option', dataset: { id: f.id },
        style: { paddingLeft: `${10 + (term ? 0 : Math.min(depth(f), 4) * 14)}px` },
        onclick: () => pick(f.id),
      }, icon(ROLE_ICONS[f.role] || (f.shared ? 'users' : 'folder'), 'sm'), h('span.grow.ellipsis', term ? folderPath(f, folders) : f.display_name)));
    }
  };
  filter.addEventListener('input', draw);
  filter.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); list.querySelector('.ml-picker-item')?.click(); }
  });
  draw();
  return modal({
    title,
    body: (close) => { pick = (id) => close(id); return h('div', filter, list); },
    actions: [{ label: 'Cancel', value: null }],
  });
}

/** Asks until when to snooze. Resolves with an ISO time, or null. */
export async function snoozeDialog() {
  const tz = -new Date().getTimezoneOffset();
  let presets = [];
  try {
    ({ presets } = await api.get(`/api/mail/snooze/presets${q({ tz })}`));
  } catch (e) {
    errorToast(e);
  }
  const labels = { later_today: 'Later today', tomorrow: 'Tomorrow', this_weekend: 'This weekend', next_week: 'Next week' };
  const tomorrow = new Date(Date.now() + 864e5);
  tomorrow.setHours(9, 0, 0, 0);
  const pad = (n) => String(n).padStart(2, '0');
  const local = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const custom = h('input', { type: 'datetime-local', id: 'ml-snooze-at', value: local(tomorrow), min: local(new Date()) });
  return modal({
    title: 'Snooze until',
    body: (close) => h('div.ml-snooze',
      h('div.ml-snooze-presets', presets.map((p) => h('button.btn.ml-snooze-preset', { type: 'button', onclick: () => close(p.until) },
        icon('clock', 'sm'), h('span.grow', labels[p.key] || p.key), h('span.small.muted', dateTime(p.until))))),
      h('label', { for: custom.id }, 'Pick a date and time'), custom),
    actions: [
      { label: 'Cancel', value: null },
      {
        label: 'Snooze', kind: 'primary',
        run: () => {
          const d = new Date(custom.value);
          if (Number.isNaN(d.getTime()) || d <= new Date()) throw new Error('Pick a time in the future.');
          return d.toISOString();
        },
      },
    ],
  });
}

/** Creates a tag; resolves with it, or null. */
export async function newTagDialog(name = '') {
  const input = h('input', { type: 'text', id: 'ml-tag-name', value: name, maxlength: 60, placeholder: 'Tag name' });
  let color = TAG_COLORS[Math.floor(Math.random() * TAG_COLORS.length)];
  const swatches = colorPicker(color, (c) => { color = c; });
  return modal({
    title: 'New tag',
    body: h('form', { onsubmit: (e) => { e.preventDefault(); e.target.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
      h('label', { for: input.id }, 'Name'), input, h('label', 'Colour'), swatches),
    actions: [
      { label: 'Cancel', value: null },
      { label: 'Create', kind: 'primary', run: async () => (await api.post('/api/mail/tags', { name: input.value.trim(), color })).tag },
    ],
  });
}

function colorPicker(current, onPick) {
  const box = h('div.ml-colors', { role: 'radiogroup', 'aria-label': 'Colour' });
  for (const c of TAG_COLORS) {
    box.appendChild(h(`button.ml-color${c === current ? '.on' : ''}`, {
      type: 'button', role: 'radio', 'aria-checked': String(c === current), 'aria-label': c, style: { background: c },
      onclick: (e) => {
        box.querySelectorAll('.ml-color').forEach((b) => { b.classList.remove('on'); b.setAttribute('aria-checked', 'false'); });
        e.currentTarget.classList.add('on');
        e.currentTarget.setAttribute('aria-checked', 'true');
        onPick(c);
      },
    }));
  }
  return box;
}

/**
 * Tags of messages: a checkbox per tag (indeterminate when only some have it), applied as
 * they are toggled. `onChange(keyword, on)` performs the change. Resolves when closed.
 */
export async function tagsDialog(tags, messages, onChange, { onCreated } = {}) {
  const list = h('div.ml-tag-list');
  const has = (kw) => messages.filter((m) => (m.keywords || []).includes(kw)).length;
  const draw = () => {
    clear(list);
    for (const t of tags) {
      const n = has(t.keyword);
      const box = h('input', { type: 'checkbox', checked: n === messages.length && n > 0, 'aria-label': t.name });
      box.indeterminate = n > 0 && n < messages.length;
      box.addEventListener('change', async () => {
        box.disabled = true;
        try {
          await onChange(t.keyword, box.checked);
          for (const m of messages) {
            m.keywords = (m.keywords || []).filter((k) => k !== t.keyword);
            if (box.checked) m.keywords.push(t.keyword);
          }
        } catch (e) {
          errorToast(e);
          box.checked = !box.checked;
        }
        box.disabled = false;
        box.indeterminate = false;
      });
      list.appendChild(h('label.check.ml-tag-row', box, tagChip(t)));
    }
  };
  draw();
  const add = h('button.btn.ghost.sm', {
    type: 'button',
    onclick: async () => {
      const tag = await newTagDialog();
      if (!tag) return;
      tags.push(tag);
      if (onCreated) onCreated(tag);
      draw();
    },
  }, icon('plus', 'sm'), 'New tag');
  await modal({ title: messages.length === 1 ? 'Tags' : `Tags of ${messages.length} messages`, body: h('div', list, add), actions: [{ label: 'Done', kind: 'primary', value: true }] });
}

/** Renames, recolours and deletes tags; creates new ones. `tags` is updated in place. */
export async function manageTagsDialog(tags) {
  const list = h('div.ml-tag-list');
  const draw = () => {
    clear(list);
    for (const t of tags) {
      const name = h('input', { type: 'text', value: t.name, maxlength: 60, 'aria-label': `Name of ${t.name}` });
      const save = async (patch) => {
        try {
          const { tag } = await api.patch(`/api/mail/tags/${encodeURIComponent(t.id)}`, patch);
          Object.assign(t, tag);
          draw();
        } catch (e) {
          errorToast(e);
          name.value = t.name;
        }
      };
      name.addEventListener('change', () => { if (name.value.trim() && name.value.trim() !== t.name) save({ name: name.value.trim() }); });
      const color = h('input.ml-color-input', { type: 'color', value: t.color, 'aria-label': `Colour of ${t.name}`, onchange: (e) => save({ color: e.target.value }) });
      list.appendChild(h('div.ml-tag-edit', color, name,
        h('span.small.muted.mono.ml-tag-kw', { title: 'IMAP keyword' }, t.keyword),
        t.is_default
          ? h('span.ml-tag-fixed.small.muted', { title: 'Default tags can be renamed but not deleted' }, 'default')
          : h('button.btn.ghost.icon.sm.danger', {
            type: 'button', 'aria-label': `Delete ${t.name}`,
            onclick: async () => {
              if (!await confirmDialog('Delete tag', `Delete “${t.name}”? It is removed from every message that has it.`, { confirm: 'Delete', danger: true })) return;
              try {
                await api.del(`/api/mail/tags/${encodeURIComponent(t.id)}`);
                tags.splice(tags.indexOf(t), 1);
                draw();
              } catch (e) { errorToast(e); }
            },
          }, icon('trash', 'sm'))));
    }
  };
  draw();
  const add = h('button.btn.ghost.sm', {
    type: 'button',
    onclick: async () => { const tag = await newTagDialog(); if (tag) { tags.push(tag); draw(); } },
  }, icon('plus', 'sm'), 'New tag');
  await modal({ title: 'Tags', wide: true, body: h('div', list, add), actions: [{ label: 'Done', kind: 'primary', value: true }] });
}

const STEP_LABELS = {
  read: 'Mark read', unread: 'Mark unread', flag: 'Favourite', important: 'Mark important', spam: 'Mark as spam',
  tag: 'Apply tag', move: 'Move to folder', delete: 'Delete',
};
const STEP_ICONS = { read: 'eye', unread: 'mail', flag: 'star', important: 'important', spam: 'junk', tag: 'tag', move: 'move', delete: 'trash' };
export const stepIcon = (s) => STEP_ICONS[s.type] || 'zap';

/** Describes a step ("Apply tag Work"). */
function stepText(s, tags, folders) {
  if (s.type === 'tag') return `${STEP_LABELS.tag} ${tags.find((t) => t.keyword === s.keyword)?.name || s.keyword}`;
  if (s.type === 'move') return `${STEP_LABELS.move} ${folders.find((f) => f.id === s.folder_id)?.display_name || '(missing folder)'}`;
  return STEP_LABELS[s.type] || s.type;
}

/** Edits one quick action. Resolves with {name, steps}, or null. */
async function quickActionEditor(action, tags, folders) {
  const name = h('input', { type: 'text', id: 'ml-qa-name', value: action?.name || '', maxlength: 60, placeholder: 'e.g. File as done' });
  const steps = (action?.steps || []).map((s) => ({ ...s }));
  const list = h('ol.ml-qa-steps');
  const addType = h('select', { 'aria-label': 'Step to add' }, Object.entries(STEP_LABELS).map(([v, l]) => h('option', { value: v }, l)));
  const tagSel = h('select', { 'aria-label': 'Tag' }, tags.map((t) => h('option', { value: t.keyword }, t.name)));
  const folderSel = h('select', { 'aria-label': 'Folder' }, folders.filter((f) => f.selectable).map((f) => h('option', { value: f.id }, folderPath(f, folders))));
  const ends = (s) => ['move', 'delete', 'spam'].includes(s.type);
  const sync = () => {
    tagSel.hidden = addType.value !== 'tag';
    folderSel.hidden = addType.value !== 'move';
  };
  addType.addEventListener('change', sync);
  const draw = () => {
    clear(list);
    steps.forEach((s, i) => list.appendChild(h('li.ml-qa-step', icon(stepIcon(s), 'sm'), h('span.grow', stepText(s, tags, folders)),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Move step up', disabled: i === 0 || ends(s), onclick: () => { [steps[i - 1], steps[i]] = [steps[i], steps[i - 1]]; draw(); } }, icon('chevron-down', 'sm flip')),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Remove ${stepText(s, tags, folders)}`, onclick: () => { steps.splice(i, 1); draw(); } }, icon('x', 'sm')))));
    if (!steps.length) list.appendChild(h('li.small.muted.ml-qa-none', 'No steps yet.'));
  };
  draw();
  sync();
  const add = h('button.btn.sm', {
    type: 'button',
    onclick: () => {
      if (steps.some(ends)) { toast('Moving, deleting or marking as spam ends the sequence; remove that step first.', 'error'); return; }
      const s = { type: addType.value };
      if (s.type === 'tag') s.keyword = tagSel.value;
      if (s.type === 'move') s.folder_id = folderSel.value;
      if (steps.length >= 10) return;
      steps.push(s);
      draw();
    },
  }, icon('plus', 'sm'), 'Add step');
  return modal({
    title: action ? 'Edit quick action' : 'New quick action',
    body: h('div', h('label', { for: name.id }, 'Name'), name, h('label', 'Steps'), list, h('div.row.wrap.ml-qa-add', addType, tagSel, folderSel, add)),
    actions: [
      { label: 'Cancel', value: null },
      {
        label: 'Save', kind: 'primary',
        run: () => {
          if (!name.value.trim()) throw new Error('Give the quick action a name.');
          if (!steps.length) throw new Error('Add at least one step.');
          return { name: name.value.trim(), steps };
        },
      },
    ],
  });
}

/** Manages an account's quick actions. Resolves when closed. */
export async function quickActionsDialog(account, folders, tags) {
  const list = h('div.ml-qa-list');
  let actions = [];
  const load = async () => {
    clear(list).appendChild(spinner());
    try {
      ({ quick_actions: actions } = await api.get(`/api/mail/quick-actions${q({ account_id: account.id })}`));
    } catch (e) { errorToast(e); }
    draw();
  };
  const draw = () => {
    clear(list);
    if (!actions.length) list.appendChild(h('p.small.muted', 'No quick actions yet. They appear on messages of this account as one-click buttons.'));
    for (const a of actions) {
      list.appendChild(h('div.ml-qa-item',
        icon(stepIcon(a.steps[0] || {}), 'sm'),
        h('div.grow', h('div', h('strong', a.name)), h('div.small.muted', a.steps.map((s) => stepText(s, tags, folders)).join(' → '))),
        h('button.btn.ghost.icon.sm', {
          type: 'button', 'aria-label': `Edit ${a.name}`,
          onclick: async () => {
            const v = await quickActionEditor(a, tags, folders);
            if (!v) return;
            try { await api.patch(`/api/mail/quick-actions/${encodeURIComponent(a.id)}`, v); await load(); } catch (e) { errorToast(e); }
          },
        }, icon('edit', 'sm')),
        h('button.btn.ghost.icon.sm.danger', {
          type: 'button', 'aria-label': `Delete ${a.name}`,
          onclick: async () => {
            if (!await confirmDialog('Delete quick action', `Delete “${a.name}”?`, { confirm: 'Delete', danger: true })) return;
            try { await api.del(`/api/mail/quick-actions/${encodeURIComponent(a.id)}`); await load(); } catch (e) { errorToast(e); }
          },
        }, icon('trash', 'sm'))));
    }
  };
  const add = h('button.btn.sm', {
    type: 'button',
    onclick: async () => {
      const v = await quickActionEditor(null, tags, folders);
      if (!v) return;
      try { await api.post(`/api/mail/accounts/${encodeURIComponent(account.id)}/quick-actions`, v); await load(); } catch (e) { errorToast(e); }
    },
  }, icon('plus', 'sm'), 'New quick action');
  load();
  await modal({ title: `Quick actions · ${account.email}`, wide: true, body: h('div', list, add), actions: [{ label: 'Done', kind: 'primary', value: true }] });
}

/** Shows a folder's statistics. */
export async function folderStatsDialog(folder) {
  const body = h('div', spinner());
  const show = modal({ title: `“${folder.display_name}”`, body, actions: [{ label: 'Close', kind: 'primary', value: true }] });
  try {
    const { stats } = await api.get(`/api/mail/folders/${encodeURIComponent(folder.id)}/stats`);
    const row = (k, v) => h('div.ml-stat', h('span.muted', k), h('strong', v));
    clear(body).appendChild(h('div.ml-stats',
      row('Messages', String(stats.total)),
      row('Unread', String(stats.unread)),
      row('Favourites (cached)', String(stats.flagged)),
      row('Cached envelopes', String(stats.cached)),
      row('Size of cached messages', bytes(stats.cached_bytes)),
      row('Bodies cached', String(stats.bodies_cached)),
      row('Subscribed', stats.subscribed ? 'Yes' : 'No'),
      row('Shared', stats.shared ? 'Yes, by another user' : 'No'),
      row('Last synced', stats.synced_at ? dateTime(stats.synced_at) : 'Never')));
  } catch (e) {
    clear(body).appendChild(h('p.error-text', e.message));
  }
  return show;
}

/** The advanced search form. Resolves with the filter (only filled-in criteria), or null. */
export async function advancedSearchDialog(initial, { tags, accounts, folders, bodySearch }) {
  const v = initial || {};
  const field = (id, label, input) => h('div.field', h('label', { for: id }, label), input);
  const text = (id, key, placeholder = '') => h('input', { type: 'text', id, value: v[key] || '', placeholder });
  const from = text('ml-as-from', 'from', 'Name or address');
  const to = text('ml-as-to', 'to', 'Name or address');
  const cc = text('ml-as-cc', 'cc', 'Name or address');
  const subject = text('ml-as-subject', 'subject');
  const body = text('ml-as-body', 'body', bodySearch ? 'Words in the message' : 'Turned off in Mail settings');
  body.disabled = !bodySearch;
  const since = h('input', { type: 'date', id: 'ml-as-since', value: v.since || '' });
  const until = h('input', { type: 'date', id: 'ml-as-until', value: v.until || '' });
  const check = (key, label) => {
    const box = h('input', { type: 'checkbox', checked: !!v[key], dataset: { key } });
    return h('label.check', box, label);
  };
  const flags = h('div.row.wrap', check('unread', 'Unread'), check('flagged', 'Favourite'), check('attachments', 'Has attachments'));
  const chosen = new Set((v.tags || '').split(',').filter(Boolean));
  const tagBoxes = h('div.ml-tag-pick', tags.map((t) => h('label.check', h('input', { type: 'checkbox', checked: chosen.has(t.keyword), dataset: { kw: t.keyword } }), tagChip(t))));
  const where = h('select', { id: 'ml-as-where' },
    h('option', { value: '' }, 'Inbox, Sent and Archive of all accounts'),
    h('option', { value: 'unified', selected: v.unified }, 'All inboxes'),
    accounts.map((a) => h('optgroup', { label: a.email }, (folders[a.id] || []).filter((f) => f.selectable)
      .map((f) => h('option', { value: `folder:${f.id}`, selected: v.folder_id === f.id }, folderPath(f, folders[a.id] || []))))));
  const form = h('form.ml-adv', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
    h('div.ml-adv-grid',
      field(from.id, 'From', from), field(to.id, 'To', to), field(cc.id, 'Cc', cc), field(subject.id, 'Subject', subject)),
    field(body.id, 'Body', body),
    h('div.ml-adv-grid', field(since.id, 'From date', since), field(until.id, 'Until date', until)),
    h('label', 'Show only'), flags,
    tags.length ? [h('label', 'Tags (any of)'), tagBoxes] : null,
    field(where.id, 'Search in', where));
  return modal({
    title: 'Advanced search',
    wide: true,
    body: form,
    actions: [
      { label: 'Cancel', value: null },
      {
        label: 'Search', kind: 'primary',
        run: () => {
          const out = {};
          for (const [k, el] of [['from', from], ['to', to], ['cc', cc], ['subject', subject], ['body', body]]) if (el.value.trim() && !el.disabled) out[k] = el.value.trim();
          if (since.value) out.since = since.value;
          if (until.value) out.until = until.value;
          flags.querySelectorAll('input').forEach((b) => { if (b.checked) out[b.dataset.key] = true; });
          const kws = [...tagBoxes.querySelectorAll('input:checked')].map((b) => b.dataset.kw);
          if (kws.length) out.tags = kws.join(',');
          if (where.value === 'unified') out.unified = true;
          else if (where.value.startsWith('folder:')) {
            out.folder_id = where.value.slice(7);
            out.account_id = accounts.find((a) => (folders[a.id] || []).some((f) => f.id === out.folder_id))?.id;
          }
          if (!Object.keys(out).filter((k) => k !== 'unified' && k !== 'folder_id' && k !== 'account_id').length) throw new Error('Fill in at least one criterion.');
          return out;
        },
      },
    ],
  });
}

/** One line describing a search filter ("from ada · unread"). */
export function filterSummary(f, tags) {
  const parts = [];
  for (const k of ['from', 'to', 'cc', 'subject', 'body']) if (f[k]) parts.push(`${k} “${f[k]}”`);
  if (f.q) parts.push(`“${f.q}”`);
  if (f.since || f.until) parts.push(`${f.since || '…'} – ${f.until || '…'}`);
  if (f.unread) parts.push('unread');
  if (f.flagged) parts.push('favourite');
  if (f.attachments) parts.push('with attachments');
  if (f.tags) parts.push(f.tags.split(',').map((k) => tags.find((t) => t.keyword === k)?.name || k).join(' or '));
  return parts.join(' · ');
}

/** An account's inbox settings: importance, junk, trash retention, default folders. */
export async function inboxSettingsDialog(account, folders, { onQuickActions } = {}) {
  let data;
  try {
    data = await api.get(`/api/mail/accounts/${encodeURIComponent(account.id)}/inbox-settings`);
  } catch (e) {
    errorToast(e);
    return null;
  }
  const s = data.settings;
  const classify = h('input', { type: 'checkbox', checked: s.classify });
  const moveJunk = h('input', { type: 'checkbox', checked: s.move_junk });
  const keep = h('input', { type: 'checkbox', checked: s.trash_retention_days > 0 });
  const days = h('input', { type: 'number', id: 'ml-retention', min: 1, max: 3650, value: s.trash_retention_days || 30, disabled: s.trash_retention_days <= 0, 'aria-label': 'Days' });
  keep.addEventListener('change', () => { days.disabled = !keep.checked; });
  const status = h('div.small.muted.ml-classifier-status');
  const drawStatus = (c) => {
    clear(status).append(c.mode === 'model'
      ? `Learned from ${c.samples} messages (${c.important} important)${c.accuracy != null ? `, ${Math.round(c.accuracy * 100)}% right on held-out mail` : ''}${c.trained_at ? `, trained ${dateTime(c.trained_at)}` : ''}.`
      : `Using simple rules until you have marked ${20} messages important (${c.important} so far).`);
  };
  drawStatus(data.classifier);
  const retrain = h('button.btn.ghost.sm', {
    type: 'button',
    onclick: async () => {
      retrain.disabled = true;
      try { drawStatus((await api.post(`/api/mail/accounts/${encodeURIComponent(account.id)}/classifier`, {})).classifier); } catch (e) { errorToast(e); }
      retrain.disabled = false;
    },
  }, icon('refresh', 'sm'), 'Retrain now');
  const usable = folders.filter((f) => f.selectable && f.role !== 'inbox' && !f.shared);
  const roleSelects = Object.entries(ROLE_NAMES).map(([role, label]) => {
    const sel = h('select', { id: `ml-role-${role}`, dataset: { role } },
      h('option', { value: '' }, '(automatic)'),
      usable.map((f) => h('option', { value: f.id, selected: s.roles[role] === f.id }, folderPath(f, folders))));
    sel.dataset.initial = s.roles[role] || '';
    return h('div.field.ml-role-field', h('label', { for: sel.id }, label), sel);
  });
  const body = h('div.ml-inbox-settings',
    h('h3', 'Priority inbox'),
    h('label.check', classify, 'Sort mail into Important and Others, learning from how I treat each sender'),
    status, retrain,
    h('h3', 'Junk'),
    h('label.check', moveJunk, 'Move messages marked as junk to the Junk folder'),
    h('h3', 'Trash'),
    h('div.row', h('label.check', keep, 'Delete messages in Trash after'), days, h('span', 'days')),
    h('h3', 'Folders'),
    h('p.small.muted', 'Which folder to use for each purpose.'),
    h('div.ml-roles', roleSelects),
    onQuickActions && [h('h3', 'Quick actions'), h('button.btn.sm', { type: 'button', onclick: onQuickActions }, icon('zap', 'sm'), 'Manage quick actions…')]);
  return modal({
    title: `Inbox settings · ${account.email}`,
    wide: true,
    body,
    actions: [
      { label: 'Cancel', value: null },
      {
        label: 'Save', kind: 'primary',
        run: async () => {
          const roles = {};
          body.querySelectorAll('select[data-role]').forEach((sel) => { if (sel.value !== sel.dataset.initial) roles[sel.dataset.role] = sel.value; });
          const patch = {
            classify: classify.checked,
            move_junk: moveJunk.checked,
            trash_retention_days: keep.checked ? Math.max(1, Number(days.value) || 30) : 0,
          };
          if (Object.keys(roles).length) patch.roles = roles;
          await api.patch(`/api/mail/accounts/${encodeURIComponent(account.id)}/inbox-settings`, patch);
          toast('Inbox settings saved', 'ok', { timeout: 2000 });
          return true;
        },
      },
    ],
  });
}
