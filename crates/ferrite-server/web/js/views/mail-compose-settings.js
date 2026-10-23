// Composer settings (opened from Mail settings, the account menu or the composer): the
// undo-send delay; per account the default editor (plain or rich text), the signature
// (edited with the rich editor) and whether it goes above or below
// the quoted text; aliases (other addresses an account sends as, each with an optional
// signature of its own); and text blocks, which can be shared read-only with users and
// groups.

import { api } from '../api.js';
import { h, clear, icon, modal, toast, errorToast, confirmDialog, spinner, emptyState, debounce } from '../ui.js';
import { createRichEditor } from '../editor/rich.js';
import { sanitizeFragment } from '../editor/sanitize.js';

const UNDO_LABELS = { 0: 'Off (send at once)', 5: '5 seconds', 10: '10 seconds', 20: '20 seconds', 30: '30 seconds' };

async function uploadImage(file) {
  const resp = await fetch(`/api/mail/uploads?name=${encodeURIComponent(file.name || 'image.png')}`, {
    method: 'POST', body: file, credentials: 'same-origin', headers: { 'X-Ferrite-Request': '1', 'Content-Type': file.type || 'application/octet-stream' },
  });
  const data = await resp.json().catch(() => ({}));
  if (!resp.ok) throw new Error(data.error || resp.statusText);
  return `/api/mail/uploads/${encodeURIComponent(data.upload.id)}/content`;
}

/** A small rich editor for signatures and text blocks. */
function smallEditor(html, label) {
  const ed = createRichEditor({ html, label, uploadImage });
  ed.el.classList.add('ed-small');
  return ed;
}

/** The signature's inner HTML (without the marker block the server adds). */
function signatureInner(html) {
  const frag = sanitizeFragment(html || '');
  const block = frag.firstElementChild;
  if (block && block.classList.contains('ferrite-signature')) {
    const inner = document.createDocumentFragment();
    while (block.firstChild) inner.appendChild(block.firstChild);
    return inner;
  }
  return frag;
}

function accountPicker(accounts, current, onChange) {
  if (accounts.length < 2) return null;
  const sel = h('select', { 'aria-label': 'Account', onchange: () => onChange(sel.value) },
    accounts.map((a) => h('option', { value: a.id, selected: a.id === current }, a.email)));
  return h('div.field', h('label', 'Account'), sel);
}

// ---- composing ---------------------------------------------------------------------------------
function composingTab(state, panel) {
  const a = state.context.accounts.find((x) => x.id === state.accountId) || state.context.accounts[0];
  const undo = h('select', { id: 'ml-undo', 'aria-label': 'Undo send' },
    state.context.undo_choices.map((s) => h('option', { value: s, selected: s === state.context.send.undo_send_seconds }, UNDO_LABELS[s] || `${s} seconds`)));
  undo.addEventListener('change', async () => {
    try {
      const { send } = await api.put('/api/mail/send-settings', { undo_send_seconds: Number(undo.value) });
      state.context.send = send;
      toast('Saved', 'ok', { timeout: 1500 });
    } catch (e) { errorToast(e); }
  });
  const mode = (v) => h('label.check', h('input', { type: 'radio', name: 'ml-editor-mode', value: v, checked: a.prefs.editor_mode === v }), v === 'richtext' ? 'Rich text (formatting, images, links)' : 'Plain text');
  const modes = h('div.ml-set-modes', mode('plaintext'), mode('richtext'));
  const sig = smallEditor(signatureInner(a.prefs.signature_html), 'Signature');
  const above = h('input', { type: 'checkbox', checked: a.prefs.signature_above_quote });
  const save = h('button.btn.primary', {
    type: 'button',
    onclick: async () => {
      save.disabled = true;
      try {
        const { prefs } = await api.put(`/api/mail/accounts/${encodeURIComponent(a.id)}/compose-settings`, {
          editor_mode: modes.querySelector('input:checked')?.value || 'plaintext',
          signature_html: sig.getHtml(),
          signature_above_quote: above.checked,
        });
        a.prefs = prefs;
        toast('Composer settings saved', 'ok', { timeout: 2000 });
      } catch (e) { errorToast(e); }
      save.disabled = false;
    },
  }, 'Save');
  clear(panel).append(
    h('section.ml-set-section',
      h('h3', 'Sending'),
      h('div.field', h('label', { for: undo.id }, 'Undo send'), undo,
        h('div.small.muted', 'Sent messages wait this long in the Outbox, so “Undo” can still take them back.'))),
    h('section.ml-set-section',
      h('h3', 'Writing'),
      accountPicker(state.context.accounts, a.id, (id) => { state.accountId = id; composingTab(state, panel); }),
      h('div.field', h('label', 'New messages and replies use'), modes),
      h('div.field', h('label', `Signature for ${a.email}`), sig.el,
        h('div.small.muted', 'Added to new messages, replies and forwards; aliases can have their own.')),
      h('label.check', above, 'Put the signature above the quoted message (below it when unchecked)'),
      h('div.ml-set-actions', save)));
}

// ---- aliases ----------------------------------------------------------------------------------
async function aliasDialog(account, alias) {
  const name = h('input', { type: 'text', id: 'ml-alias-name', value: alias?.name ?? account.name ?? '' });
  const email = h('input', { type: 'email', id: 'ml-alias-email', value: alias?.email || '', placeholder: 'support@example.org', required: true });
  const own = h('input', { type: 'checkbox', checked: !!alias?.own_signature });
  const sig = smallEditor(alias?.own_signature ? signatureInner(alias.signature_html) : '', 'Alias signature');
  const sigBox = h('div.field', sig.el);
  sigBox.hidden = !own.checked;
  own.addEventListener('change', () => { sigBox.hidden = !own.checked; });
  const err = h('div.error-text', { role: 'alert' });
  setTimeout(() => (alias ? name : email).focus(), 0);
  return modal({
    title: alias ? `Edit ${alias.email}` : `New alias for ${account.email}`,
    wide: true,
    body: h('div.ml-set-form',
      h('div.field', h('label', { for: name.id }, 'Name'), name),
      h('div.field', h('label', { for: email.id }, 'Address'), email,
        h('div.small.muted', 'An address the mail server lets this account send as (an alias or shared address on the server).')),
      h('label.check', own, 'Use its own signature'),
      sigBox, err),
    actions: [{ label: 'Cancel', value: null }, {
      label: alias ? 'Save' : 'Add alias', kind: 'primary', run: async () => {
        err.textContent = '';
        const body = { name: name.value.trim(), email: email.value.trim(), signature_html: own.checked ? sig.getHtml() : '' };
        try {
          return alias
            ? (await api.patch(`/api/mail/aliases/${encodeURIComponent(alias.id)}`, body)).alias
            : (await api.post(`/api/mail/accounts/${encodeURIComponent(account.id)}/aliases`, body)).alias;
        } catch (e) { err.textContent = e.message; return undefined; }
      },
    }],
  });
}

async function aliasesTab(state, panel) {
  const a = state.context.accounts.find((x) => x.id === state.accountId) || state.context.accounts[0];
  const reload = async () => {
    state.context = await api.get('/api/mail/compose/context');
    aliasesTab(state, panel);
  };
  const list = h('div.ml-set-list', { role: 'list' });
  if (!a.aliases.length) list.appendChild(h('p.small.muted', 'No aliases yet. Add the other addresses this account may send from.'));
  for (const al of a.aliases) {
    list.appendChild(h('div.ml-set-row', { role: 'listitem' },
      icon('mail', 'sm'),
      h('div.grow', h('div.ellipsis', al.name ? `${al.name} <${al.email}>` : al.email), h('div.small.muted', al.own_signature ? 'Own signature' : 'Uses the account’s signature')),
      h('button.btn.ghost.sm', { type: 'button', onclick: async () => { if (await aliasDialog(a, al)) reload(); } }, 'Edit'),
      h('button.btn.ghost.icon.sm.danger', {
        type: 'button', 'aria-label': `Delete ${al.email}`,
        onclick: async () => {
          if (!await confirmDialog('Delete alias', `Stop offering ${al.email} as a sender?`, { confirm: 'Delete', danger: true })) return;
          try { await api.del(`/api/mail/aliases/${encodeURIComponent(al.id)}`); reload(); } catch (e) { errorToast(e); }
        },
      }, icon('trash', 'sm'))));
  }
  clear(panel).append(
    h('section.ml-set-section',
      h('h3', 'Aliases'),
      h('p.small.muted', 'Choose an alias as the sender in the composer. Replies go out from the alias a message was sent to.'),
      accountPicker(state.context.accounts, a.id, (id) => { state.accountId = id; aliasesTab(state, panel); }),
      list,
      h('div.ml-set-actions', h('button.btn', { type: 'button', onclick: async () => { if (await aliasDialog(a, null)) reload(); } }, icon('plus', 'sm'), 'Add alias'))));
}

// ---- text blocks ----------------------------------------------------------------------------------
async function blockDialog(block) {
  const title = h('input', { type: 'text', id: 'ml-block-title', value: block?.title || '', placeholder: 'e.g. Greeting', required: true });
  const ed = smallEditor(block?.html || '', 'Text block');
  const err = h('div.error-text', { role: 'alert' });
  setTimeout(() => title.focus(), 0);
  return modal({
    title: block ? 'Edit text block' : 'New text block',
    wide: true,
    body: h('div.ml-set-form', h('div.field', h('label', { for: title.id }, 'Title'), title,
      h('div.small.muted', 'Type ! and part of the title in a message to insert it.')), h('div.field', ed.el), err),
    actions: [{ label: 'Cancel', value: null }, {
      label: block ? 'Save' : 'Create', kind: 'primary', run: async () => {
        err.textContent = '';
        const body = { title: title.value.trim(), html: ed.getHtml() };
        try {
          return block
            ? (await api.patch(`/api/mail/text-blocks/${encodeURIComponent(block.id)}`, body)).block
            : (await api.post('/api/mail/text-blocks', body)).block;
        } catch (e) { err.textContent = e.message; return undefined; }
      },
    }],
  });
}

async function shareDialog(block) {
  let shares = block.shares || [];
  const list = h('div.ml-set-list', { role: 'list' });
  const kind = h('select', { 'aria-label': 'Share with' }, h('option', { value: 'user' }, 'User'), h('option', { value: 'group' }, 'Group'));
  const target = h('input', { type: 'text', 'aria-label': 'User or group', placeholder: 'Username or group name', list: 'ml-share-targets', autocomplete: 'off' });
  const options = h('datalist', { id: 'ml-share-targets' });
  const err = h('div.error-text', { role: 'alert' });
  const draw = () => {
    clear(list);
    if (!shares.length) list.appendChild(h('p.small.muted', 'Not shared yet.'));
    for (const s of shares) {
      list.appendChild(h('div.ml-set-row', { role: 'listitem' }, icon(s.kind === 'group' ? 'users' : 'user', 'sm'), h('span.grow', s.target_name),
        h('button.btn.ghost.icon.sm', {
          type: 'button', 'aria-label': `Stop sharing with ${s.target_name}`,
          onclick: async () => {
            try { shares = (await api.del(`/api/mail/text-blocks/${encodeURIComponent(block.id)}/shares/${s.kind}/${encodeURIComponent(s.target_id)}`)).shares; draw(); } catch (e) { errorToast(e); }
          },
        }, icon('x', 'sm'))));
    }
  };
  const suggest = debounce(async () => {
    clear(options);
    try {
      if (kind.value === 'user') {
        const { users } = await api.get(`/api/users?q=${encodeURIComponent(target.value.trim())}`);
        users.forEach((u) => options.appendChild(h('option', { value: u.username }, u.display_name)));
      } else {
        const { groups } = await api.get('/api/admin/groups');
        groups.forEach((g) => options.appendChild(h('option', { value: g.name })));
      }
    } catch (_) { /* suggestions are optional */ }
  }, 200);
  target.addEventListener('input', suggest);
  kind.addEventListener('change', suggest);
  const add = async () => {
    err.textContent = '';
    if (!target.value.trim()) return;
    try {
      shares = (await api.post(`/api/mail/text-blocks/${encodeURIComponent(block.id)}/shares`, { kind: kind.value, target: target.value.trim() })).shares;
      target.value = '';
      draw();
    } catch (e) { err.textContent = e.message; }
  };
  target.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); add(); } });
  draw();
  suggest();
  await modal({
    title: `Share “${block.title}”`,
    body: h('div.ml-set-form', h('p.small.muted', 'People you share with can insert the text block but not change it.'), list,
      h('div.ml-set-share', kind, target, options, h('button.btn', { type: 'button', onclick: add }, 'Share')), err),
    actions: [{ label: 'Done', kind: 'primary', value: true }],
  });
}

async function blocksTab(state, panel) {
  clear(panel).appendChild(spinner());
  let blocks;
  try {
    ({ blocks } = await api.get('/api/mail/text-blocks'));
  } catch (e) {
    clear(panel).appendChild(h('p.error-text', e.message));
    return;
  }
  const again = () => blocksTab(state, panel);
  const list = h('div.ml-set-list', { role: 'list' });
  if (!blocks.length) list.appendChild(emptyState('note', 'No text blocks yet', 'Save phrases you write often and insert them with ! in any message.'));
  for (const b of blocks) {
    const preview = h('div.ml-block-preview.small');
    preview.appendChild(sanitizeFragment(b.html));
    list.appendChild(h('div.ml-set-row.ml-block-row', { role: 'listitem' },
      icon('note', 'sm'),
      h('div.grow.ml-block-main',
        h('div.ellipsis', h('strong', b.title)),
        h('div.small.muted', b.editable ? (b.shares.length ? `Shared with ${b.shares.map((s) => s.target_name).join(', ')}` : 'Only you') : `Shared by ${b.owner_name} · read-only`),
        preview),
      b.editable && [
        h('button.btn.ghost.sm', { type: 'button', onclick: async () => { if (await blockDialog(b)) again(); } }, 'Edit'),
        h('button.btn.ghost.sm', { type: 'button', onclick: async () => { await shareDialog(b); again(); } }, icon('share', 'sm'), 'Share'),
        h('button.btn.ghost.icon.sm.danger', {
          type: 'button', 'aria-label': `Delete ${b.title}`,
          onclick: async () => {
            if (!await confirmDialog('Delete text block', `Delete “${b.title}”? People it is shared with lose it too.`, { confirm: 'Delete', danger: true })) return;
            try { await api.del(`/api/mail/text-blocks/${encodeURIComponent(b.id)}`); again(); } catch (e) { errorToast(e); }
          },
        }, icon('trash', 'sm')),
      ]));
  }
  clear(panel).append(h('section.ml-set-section',
    h('h3', 'Text blocks'),
    list,
    h('div.ml-set-actions', h('button.btn', { type: 'button', onclick: async () => { if (await blockDialog(null)) again(); } }, icon('plus', 'sm'), 'New text block'))));
}

/** Opens the settings. opts: { tab: composing | aliases | blocks, accountId }. */
export async function composeSettingsDialog(ctx, opts = {}) {
  let context;
  try {
    context = await api.get('/api/mail/compose/context');
  } catch (e) {
    errorToast(e);
    return;
  }
  const state = { context, accountId: opts.accountId || context.accounts[0]?.id, tab: opts.tab || 'composing' };
  const panel = h('div.ml-set-panel');
  const tabs = [['composing', 'Composing'], ['aliases', 'Aliases'], ['blocks', 'Text blocks']].filter(([id]) => id === 'blocks' || context.accounts.length);
  const bar = h('div.tabs', { role: 'tablist' }, tabs.map(([id, label]) => h(`button${id === state.tab ? '.active' : ''}`, {
    type: 'button', role: 'tab', 'aria-selected': String(id === state.tab), dataset: { tab: id },
    onclick: () => show(id),
  }, label)));
  function show(id) {
    state.tab = id;
    bar.querySelectorAll('button').forEach((b) => { b.classList.toggle('active', b.dataset.tab === id); b.setAttribute('aria-selected', String(b.dataset.tab === id)); });
    if (id === 'composing') composingTab(state, panel);
    else if (id === 'aliases') aliasesTab(state, panel);
    else blocksTab(state, panel);
  }
  if (!tabs.some(([id]) => id === state.tab)) state.tab = 'blocks';
  show(state.tab);
  await modal({ title: 'Composing', wide: true, body: h('div.ml-settings', bar, panel), actions: [{ label: 'Close', value: true }] });
}
