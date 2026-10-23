// Filters and autoresponder of a mail account, kept on the mail server as a Sieve script
// (over ManageSieve): visual filters (conditions on subject, sender or recipient; move to a
// folder, tag, stop), the autoresponder (or following the absence in Settings → Profile),
// the script itself for hand-written rules (the server checks it and reports errors), and
// the account's integration settings (ManageSieve connection, invitations, follow-ups).

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, modal, spinner } from '../ui.js';
import { localInput } from './mail-imip.js';
import { mi } from './mail-integrations-icons.js';
import { aiStatus, forgetFollowUps } from './mail-ai.js';

const FIELDS = [['from', 'Sender'], ['to', 'Recipient'], ['subject', 'Subject']];
const OPERATORS = [['contains', 'contains'], ['is', 'is'], ['matches', 'matches (* and ?)']];
const ACTIONS = [['fileinto', 'Move to folder'], ['addflag', 'Add tag'], ['stop', 'Stop other filters']];
const TAGS = [['$label1', 'Important'], ['$label2', 'Work'], ['$label3', 'Personal'], ['$label4', 'To do'], ['$label5', 'Later'], ['\\Flagged', 'Starred'], ['\\Seen', 'Read']];

const enc = encodeURIComponent;

/** A modified UTF-7 IMAP folder name decoded for display and for Sieve (which takes UTF-8). */
function decodeMailbox(name) {
  return name.replace(/&([^-]*)-/g, (all, b64) => {
    if (!b64) return '&';
    try {
      const bin = atob(b64.replace(/,/g, '/'));
      let out = '';
      for (let i = 0; i + 1 < bin.length; i += 2) out += String.fromCharCode((bin.charCodeAt(i) << 8) | bin.charCodeAt(i + 1));
      return out;
    } catch (_) { return all; }
  });
}

function describe(f) {
  const field = Object.fromEntries(FIELDS);
  const join = f.operator === 'anyof' ? ' or ' : ' and ';
  const cond = f.tests.map((t) => `${field[t.field] || t.field} ${t.operator} “${t.values.join('” or “')}”`).join(join);
  const acts = f.actions.map((a) => {
    if (a.type === 'fileinto') return `move to ${a.mailbox}`;
    if (a.type === 'addflag' || a.type === 'addsystemflag') return `tag ${(TAGS.find(([v]) => v === a.flag) || [0, a.flag])[1]}`;
    if (a.type === 'stop') return 'stop';
    if (a.type === 'keep') return 'keep';
    if (a.type === 'redirect') return `forward to ${a.recipient}`;
    return a.type;
  }).join(', ');
  return `If ${cond}: ${acts}`;
}

const selectOf = (pairs, value, label) => h('select', { 'aria-label': label }, pairs.map(([v, l]) => h('option', { value: v, selected: v === value }, l)));

/** Editor for one filter, shown in `host`. Resolves with the filter, or null. */
function filterEditor(initial, folders, host) {
  const f = JSON.parse(JSON.stringify(initial));
  const name = h('input', { type: 'text', value: f.name || '', 'aria-label': 'Filter name', placeholder: 'Name' });
  const match = selectOf([['allof', 'all conditions'], ['anyof', 'any condition']], f.operator || 'allof', 'Match');
  const tests = h('div.mf-rows');
  const actions = h('div.mf-rows');
  const err = h('div.error-text', { role: 'alert' });
  const testRows = [];
  const actionRows = [];
  function addTest(t = { field: 'from', operator: 'contains', values: [''] }) {
    const field = selectOf(FIELDS, t.field, 'Field');
    const op = selectOf(OPERATORS, t.operator, 'Comparison');
    const value = h('input', { type: 'text', value: t.values.join(', '), placeholder: 'Value (several: comma separated)', 'aria-label': 'Value' });
    const row = h('div.mf-row', field, op, value,
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Remove condition', onclick: () => { row.remove(); testRows.splice(testRows.indexOf(entry), 1); } }, icon('x', 'sm')));
    const entry = { row, get: () => ({ field: field.value, operator: op.value, values: value.value.split(',').map((v) => v.trim()).filter(Boolean) }) };
    testRows.push(entry);
    tests.appendChild(row);
  }
  function addAction(a = { type: 'fileinto', mailbox: folders[0] || '' }) {
    const type = selectOf(ACTIONS, a.type === 'addsystemflag' ? 'addflag' : a.type, 'Action');
    const folder = selectOf(folders.map((x) => [x, x]), a.mailbox, 'Folder');
    const tag = h('input', { type: 'text', value: a.flag || '$label1', list: 'mf-tags', 'aria-label': 'Tag', placeholder: '$label1' });
    const arg = h('span.mf-arg');
    const show = () => clear(arg).appendChild(type.value === 'fileinto' ? folder : type.value === 'addflag' ? tag : h('span.small.muted', 'Later filters are skipped'));
    type.addEventListener('change', show);
    show();
    const row = h('div.mf-row', type, arg,
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Remove action', onclick: () => { row.remove(); actionRows.splice(actionRows.indexOf(entry), 1); } }, icon('x', 'sm')));
    const entry = {
      row,
      get: () => (type.value === 'fileinto' ? { type: 'fileinto', mailbox: folder.value } : type.value === 'addflag' ? { type: 'addflag', flag: tag.value.trim() } : { type: 'stop' }),
    };
    actionRows.push(entry);
    actions.appendChild(row);
  }
  (f.tests?.length ? f.tests : [undefined]).forEach((t) => addTest(t));
  (f.actions?.length ? f.actions : [undefined]).forEach((a) => addAction(a));
  const body = h('div.mf-editor',
    h('div.field', h('label', 'Name'), name),
    h('div.mf-match', 'Apply when', match, 'match:'),
    tests,
    h('button.btn.ghost.sm', { type: 'button', onclick: () => addTest() }, icon('plus', 'sm'), 'Add condition'),
    h('h4', 'Then'),
    actions,
    h('button.btn.ghost.sm', { type: 'button', onclick: () => addAction({ type: 'addflag', flag: '$label1' }) }, icon('plus', 'sm'), 'Add action'),
    h('datalist', { id: 'mf-tags' }, TAGS.map(([v, l]) => h('option', { value: v }, l))),
    err);
  setTimeout(() => name.focus(), 0);
  return new Promise((resolve) => {
    const save = h('button.btn.primary.sm', {
      type: 'button',
      onclick: () => {
        const out = { ...f, name: name.value.trim(), operator: match.value, enable: f.enable !== false, tests: testRows.map((r) => r.get()), actions: actionRows.map((r) => r.get()) };
        if (!out.name) { err.textContent = 'Give the filter a name.'; return; }
        if (!out.tests.length || out.tests.some((t) => !t.values.length)) { err.textContent = 'Every condition needs a value.'; return; }
        if (!out.actions.length) { err.textContent = 'Add at least one action.'; return; }
        resolve(out);
      },
    }, 'Save filter');
    clear(host).append(
      h('h3.mf-editor-title', initial.name ? `Edit filter “${initial.name}”` : 'New filter'),
      body,
      h('div.mf-foot', h('button.btn.sm', { type: 'button', onclick: () => resolve(null) }, 'Cancel'), save));
  });
}

function filtersTab(account, folders, prefill) {
  const el = h('div.mf-tab', spinner());
  let filters = [];
  async function save(next, what) {
    try {
      ({ filters } = await api.put(`/api/mail/accounts/${enc(account.id)}/filters`, { filters: next }));
      toast(what, 'ok', { timeout: 2000 });
    } catch (e) {
      errorToast(e);
    }
    draw();
  }
  async function edit(i, initial) {
    const out = await filterEditor(initial || filters[i], folders, el);
    if (!out) { draw(); return; }
    const next = [...filters];
    if (i === null) next.push(out); else next[i] = out;
    await save(next, 'Filter saved');
  }
  function draw() {
    clear(el);
    el.appendChild(h('p.small.muted', 'Filters run on the mail server as mail arrives, in this order, also when Ferrite is closed.'));
    const list = h('div.mf-list.panel');
    if (!filters.length) list.appendChild(h('div.empty.small', 'No filters yet.'));
    filters.forEach((f, i) => {
      const on = h('input', { type: 'checkbox', checked: f.enable !== false, 'aria-label': `Filter ${f.name} on`, onchange: () => save(filters.map((x, j) => (j === i ? { ...x, enable: on.checked } : x)), on.checked ? 'Filter turned on' : 'Filter turned off') });
      list.appendChild(h(`div.list-item.mf-item${f.enable === false ? '.off' : ''}`,
        on,
        h('div.grow', h('div.ellipsis', h('strong', f.name)), h('div.small.muted.mf-desc', describe(f))),
        h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Move ${f.name} up`, disabled: i === 0, onclick: () => { const n = [...filters]; [n[i - 1], n[i]] = [n[i], n[i - 1]]; save(n, 'Order saved'); } }, mi('chevron-up', 'sm')),
        h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Move ${f.name} down`, disabled: i === filters.length - 1, onclick: () => { const n = [...filters]; [n[i + 1], n[i]] = [n[i], n[i + 1]]; save(n, 'Order saved'); } }, icon('chevron-down', 'sm')),
        h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Edit ${f.name}`, onclick: () => edit(i) }, icon('edit', 'sm')),
        h('button.btn.ghost.icon.sm.danger', {
          type: 'button', 'aria-label': `Delete ${f.name}`, title: 'Delete',
          // Two steps instead of a second dialog: the first click asks, the second deletes.
          onclick: (e) => {
            const b = e.currentTarget;
            if (b.dataset.armed) { save(filters.filter((_, j) => j !== i), 'Filter deleted'); return; }
            b.dataset.armed = '1';
            b.classList.remove('icon');
            b.textContent = 'Delete?';
            setTimeout(() => { if (b.isConnected) { delete b.dataset.armed; b.classList.add('icon'); clear(b).appendChild(icon('trash', 'sm')); } }, 4000);
          },
        }, icon('trash', 'sm'))));
    });
    el.append(list, h('div.mf-foot', h('button.btn.sm.primary', { type: 'button', onclick: () => edit(null, { name: '', operator: 'allof', tests: [], actions: [] }) }, icon('plus', 'sm'), 'New filter')));
  }
  (async () => {
    try {
      ({ filters } = await api.get(`/api/mail/accounts/${enc(account.id)}/filters`));
      draw();
      if (prefill) edit(null, prefill);
    } catch (e) {
      clear(el).appendChild(h('p.error-text', e.message));
    }
  })();
  return el;
}

function vacationTab(account, ctx, close) {
  const el = h('div.mf-tab', spinner());
  (async () => {
    let v;
    try {
      ({ vacation: v } = await api.get(`/api/mail/accounts/${enc(account.id)}/vacation`));
    } catch (e) {
      clear(el).appendChild(h('p.error-text', e.message));
      return;
    }
    clear(el);
    if (v.follows_absence) {
      el.append(
        h('div.mf-notice', icon('calendar', 'sm'), h('div', h('strong', 'Following your absence. '),
          v.enabled ? `The autoresponder is set for ${v.start ? new Date(v.start).toLocaleDateString() : ''} – ${v.end ? new Date(v.end).toLocaleDateString() : ''}.` : 'No absence is set, so it is off.',
          ' Change it in ', h('a', { href: '/settings/profile', onclick: (e) => { e.preventDefault(); close(); ctx.navigate('/settings/profile'); } }, 'Settings → Profile'), '.')),
        v.message && h('blockquote.mf-quote', v.message));
      return;
    }
    const enabled = h('input', { type: 'checkbox', checked: v.enabled });
    const start = h('input', { type: 'datetime-local', value: v.start ? localInput(v.start) : localInput(Date.now()), 'aria-label': 'First day' });
    const end = h('input', { type: 'datetime-local', value: v.end ? localInput(v.end) : '', 'aria-label': 'Last day' });
    const subject = h('input', { type: 'text', value: v.subject || 'Re: ${subject}', 'aria-label': 'Subject' });
    const message = h('textarea', { rows: 5, 'aria-label': 'Message', placeholder: 'I am out of the office until …' }, v.message || '');
    const days = h('input', { type: 'number', min: 1, max: 365, value: v.days || 4, 'aria-label': 'Days between replies' });
    const addresses = h('input', { type: 'text', value: (v.addresses || []).join(', '), 'aria-label': 'Answer mail sent to' });
    const err = h('div.error-text', { role: 'alert' });
    const save = h('button.btn.primary', {
      type: 'button',
      onclick: async () => {
        err.textContent = '';
        save.disabled = true;
        try {
          await api.put(`/api/mail/accounts/${enc(account.id)}/vacation`, {
            enabled: enabled.checked,
            start: enabled.checked && start.value ? new Date(start.value).toISOString() : null,
            end: enabled.checked && end.value ? new Date(end.value).toISOString() : null,
            subject: subject.value, message: message.value, days: Number(days.value) || 4,
            addresses: addresses.value.split(',').map((a) => a.trim()).filter(Boolean),
          });
          toast(enabled.checked ? 'Autoresponder on' : 'Autoresponder off', 'ok', { timeout: 2000 });
        } catch (e) { err.textContent = e.message; } finally { save.disabled = false; }
      },
    }, 'Save autoresponder');
    el.append(
      h('p.small.muted', 'Answers incoming mail automatically while you are away (each sender once per interval; mailing lists and no-reply senders are skipped). To follow your absence instead, turn that on under Settings.'),
      h('label.check', enabled, 'Reply automatically'),
      h('div.mf-grid', h('div.field', h('label', 'From'), start), h('div.field', h('label', 'Until (optional)'), end)),
      h('div.field', h('label', 'Subject'), subject, h('div.small.muted', '${subject} is the subject of the message being answered.')),
      h('div.field', h('label', 'Message'), message),
      h('div.mf-grid', h('div.field', h('label', 'Days between replies to the same sender'), days), h('div.field', h('label', 'Answer mail sent to'), addresses)),
      err, h('div.mf-foot', save));
  })();
  return el;
}

function scriptTab(account) {
  const el = h('div.mf-tab', spinner());
  (async () => {
    let s;
    try {
      s = await api.get(`/api/mail/accounts/${enc(account.id)}/sieve`);
    } catch (e) {
      clear(el).appendChild(h('p.error-text', e.message));
      return;
    }
    const text = h('textarea.mf-script', { rows: 18, spellcheck: 'false', 'aria-label': 'Sieve script' }, s.script);
    const err = h('pre.mf-error', { role: 'alert', hidden: true });
    const save = h('button.btn.primary', {
      type: 'button',
      onclick: async () => {
        err.hidden = true;
        save.disabled = true;
        try {
          const r = await api.put(`/api/mail/accounts/${enc(account.id)}/sieve`, { script: text.value });
          text.value = r.script;
          toast('Script saved', 'ok', { timeout: 2000 });
        } catch (e) {
          err.textContent = e.message;
          err.hidden = false;
        } finally { save.disabled = false; }
      },
    }, 'Save script');
    clear(el).append(
      h('p.small.muted', `The active script${s.name ? ` “${s.name}”` : ''} on ${s.implementation || 'the server'}. Filters and the autoresponder live between the marked lines; write your own rules outside them. The server checks the script before saving.`),
      text, err,
      h('details.small.muted.mf-ext', h('summary', 'Supported extensions'), (s.extensions || []).join(' ')),
      h('div.mf-foot', save));
  })();
  return el;
}

function settingsTab(account, onChange) {
  const el = h('div.mf-tab', spinner());
  (async () => {
    let i;
    try {
      ({ integrations: i } = await api.get(`/api/mail/accounts/${enc(account.id)}/integrations`));
    } catch (e) {
      clear(el).appendChild(h('p.error-text', e.message));
      return;
    }
    const enabled = h('input', { type: 'checkbox', checked: i.sieve_enabled });
    const host = h('input', { type: 'text', value: i.sieve_host, placeholder: account.imap_host, 'aria-label': 'ManageSieve server', spellcheck: 'false' });
    const port = h('input', { type: 'number', value: i.sieve_port, min: 1, max: 65535, 'aria-label': 'Port' });
    const security = selectOf([['starttls', 'STARTTLS'], ['tls', 'SSL/TLS'], ['none', 'None (trusted network only)']], i.sieve_security, 'Security');
    const user = h('input', { type: 'text', value: i.sieve_username || '', placeholder: account.imap_username, 'aria-label': 'User name', spellcheck: 'false' });
    const pass = h('input', { type: 'password', autocomplete: 'off', placeholder: i.sieve_custom_password ? 'Unchanged' : 'Same as the IMAP password', 'aria-label': 'Password' });
    const follow = h('input', { type: 'checkbox', checked: i.ooo_follows_absence });
    const update = h('input', { type: 'checkbox', checked: i.imip_auto_update });
    const create = h('input', { type: 'checkbox', checked: i.imip_auto_create });
    const followups = h('input', { type: 'checkbox', checked: i.followup_enabled });
    const err = h('div.error-text', { role: 'alert' });
    const status = h('span.small.muted', { role: 'status' });
    const checkNow = checkNowButton();
    const save = h('button.btn.primary', {
      type: 'button',
      onclick: async () => {
        err.textContent = '';
        save.disabled = true;
        status.textContent = enabled.checked ? 'Testing the connection…' : '';
        const body = {
          sieve_enabled: enabled.checked, ooo_follows_absence: follow.checked, imip_auto_update: update.checked, imip_auto_create: create.checked,
          followup_enabled: followups.checked,
        };
        // The connection is sent only when it changed (changing it needs the right to change
        // how the account signs in).
        const conn = { sieve_host: host.value.trim(), sieve_port: Number(port.value) || 4190, sieve_security: security.value, sieve_username: user.value.trim() };
        const was = { sieve_host: i.sieve_host, sieve_port: i.sieve_port, sieve_security: i.sieve_security, sieve_username: i.sieve_username || '' };
        for (const k of Object.keys(conn)) if (conn[k] !== was[k]) body[k] = conn[k];
        if (pass.value) body.sieve_password = pass.value;
        try {
          const r = await api.patch(`/api/mail/accounts/${enc(account.id)}/integrations`, body);
          toast('Settings saved', 'ok', { timeout: 2000 });
          onChange(r.integrations);
        } catch (e) { err.textContent = e.message; } finally { save.disabled = false; status.textContent = ''; }
      },
    }, 'Save settings');
    // Delegates and administrator-provisioned accounts cannot change the sign-in.
    const locked = account.permissions ? !account.permissions.authenticate : false;
    const server = h('fieldset.mf-server', { disabled: locked || !enabled.checked },
      h('div.mf-grid3', h('div.field', h('label', 'Server'), host), h('div.field', h('label', 'Port'), port), h('div.field', h('label', 'Security'), security)),
      h('div.mf-grid', h('div.field', h('label', 'User name'), user), h('div.field', h('label', 'Password'), pass)),
      (locked || i.sieve_provisioned) && h('p.small.muted', i.sieve_provisioned ? 'The Sieve server was set up by your administrator.' : 'Only the account owner can change the Sieve server and sign-in.'));
    enabled.addEventListener('change', () => { server.disabled = locked || !enabled.checked; });
    clear(el).append(
      h('h4', 'Filters on the mail server (Sieve)'),
      h('label.check', enabled, 'Manage filters and the autoresponder for this account'),
      h('p.small.muted', 'Needs a mail server with ManageSieve (port 4190, e.g. Dovecot). Empty fields use the IMAP server, user name and password.'),
      server,
      h('label.check', follow, 'Autoresponder follows my absence (Settings → Profile)'),
      h('h4', 'Meeting invitations'),
      h('label.check', update, 'Apply updates and cancellations from organizers to events already in my calendar'),
      h('label.check', create, 'Put new invitations in my calendar as tentative until I answer'),
      h('h4', 'Follow-up reminders'),
      h('label.check', followups, 'Remind me of sent mail that expects an answer (needs AI)'),
      checkNow);
    aiStatus().then((st) => { checkNow.hidden = !st.enabled; }).catch(() => {});
    el.append(err, h('div.mf-foot', status, save));
  })();
  return el;
}

/** "Check sent mail now" (shown when AI is available for mail). */
function checkNowButton() {
  return h('div.mf-check-now', { hidden: true }, h('button.btn.sm', {
    type: 'button',
    onclick: async (e) => {
      const b = e.currentTarget;
      b.disabled = true;
      try {
        const r = await api.post('/api/mail/follow-ups/check', {});
        forgetFollowUps();
        toast(`${r.checked} sent ${r.checked === 1 ? 'message' : 'messages'} checked; ${r.follow_ups.length} waiting for an answer`, 'ok');
      } catch (ex) { errorToast(ex); } finally { b.disabled = false; }
    },
  }, icon('refresh', 'sm'), 'Check sent mail now'));
}

/**
 * The filters dialog of an account. opts: { tab: filters | vacation | script | settings,
 * prefill: a filter to start a new one from (create a filter from a message) }.
 */
export async function filtersDialog(ctx, account, opts = {}) {
  let integrations;
  let folders = [];
  try {
    const [i, f] = await Promise.all([
      api.get(`/api/mail/accounts/${enc(account.id)}/integrations`),
      api.get(`/api/mail/accounts/${enc(account.id)}/folders`),
    ]);
    integrations = i.integrations;
    folders = f.folders.filter((x) => x.selectable).map((x) => decodeMailbox(x.name));
  } catch (e) {
    errorToast(e);
    return;
  }
  const tabs = [['filters', 'Filters', 'filter'], ['vacation', 'Autoresponder', 'mail'], ['script', 'Sieve script', 'doc'], ['settings', 'Settings', 'settings']];
  let current = integrations.sieve_enabled ? (opts.tab || 'filters') : 'settings';
  const nav = h('nav.tabs.mf-tabs', { role: 'tablist' });
  const pane = h('div.mf-pane');
  let close = () => {};
  function show(tab) {
    current = tab;
    clear(nav).append(...tabs.map(([k, l, ic]) => h(`button${k === current ? '.active' : ''}`, {
      type: 'button', role: 'tab', 'aria-selected': String(k === current), disabled: !integrations.sieve_enabled && k !== 'settings',
      onclick: () => show(k),
    }, mi(ic, 'sm'), l)));
    clear(pane);
    if (!integrations.sieve_enabled && tab !== 'settings') return;
    if (tab === 'filters') pane.appendChild(filtersTab(account, folders, opts.prefill));
    if (tab === 'vacation') pane.appendChild(vacationTab(account, ctx, () => close()));
    if (tab === 'script') pane.appendChild(scriptTab(account));
    if (tab === 'settings') {
      if (!integrations.sieve_enabled) pane.appendChild(h('div.mf-notice', icon('settings', 'sm'), h('span', 'Turn on Sieve below to manage filters and the autoresponder.')));
      pane.appendChild(settingsTab(account, (i) => { integrations = i; if (i.sieve_enabled && opts.prefill) show('filters'); else show('settings'); }));
    }
    opts.prefill = tab === 'filters' ? null : opts.prefill;
  }
  show(current);
  await modal({
    title: `Filters and autoresponder · ${account.email}`,
    wide: true,
    body: (c) => { close = c; return h('div.mf', nav, pane); },
    actions: [{ label: 'Close', value: null }],
  });
}

/** A new filter for mail like `message` (from its sender). */
export function filterFromMessage(message) {
  return {
    name: `From ${message.from_name || message.from_addr}`,
    operator: 'allof',
    enable: true,
    tests: [{ field: 'from', operator: 'is', values: [message.from_addr] }],
    actions: [{ type: 'addflag', flag: '$label1' }],
  };
}
