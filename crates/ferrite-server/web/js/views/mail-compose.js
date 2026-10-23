// The mail composer. To/Cc/Bcc with suggestions from contacts, users, groups (which expand
// to their members when sent) and past correspondents; From with the accounts and their
// aliases (each with its signature, swapped when the sender changes); a rich text editor or
// plain text (the account's default, switchable, converting the content); replies, reply-all,
// forwards (inline or as an attachment) prefilled and quoted by the server; @-mentions add
// people as recipients; "!" plus a filter (or the menu) inserts text blocks; attachments
// uploaded, dropped (also .eml files) or picked from Files, large ones shared as links; a
// recipient info pane; read receipts; drafts saved automatically to the account's Drafts
// folder while typing. Sending queues the message in the outbox: it leaves after the undo
// delay (Undo brings it back) or at the time picked under "Send later". The composer can be
// minimized to keep working in Mail; a dock brings it back. Ctrl/Cmd+Enter sends.
//
// Hooks for other parts of Mail:
// * insertIntoComposer(text) puts text into the open (or minimized) composer at the caret,
//   or at the top of the message when the caret was never placed (smart replies use it).
// * registerComposerExtension({ tools(api), beforeSend(api) }) adds composer extensions,
//   e.g. S/MIME or Mailvelope signing/encryption toggles and internal-address warnings:
//   tools(api) returns an element for the tools row (or null); beforeSend(api) runs before
//   a message is queued and returns false to stop (after telling the user why). api is
//   { accountId(), aliasId(), recipients() -> {to, cc, bcc}, payload } where payload is the
//   message about to be sent; an extension may add fields to it for the server.
// * composerHooks { tools, beforeSend, beforeDraft } (see below) give an extension the
//   composer itself, e.g. for Mailvelope's editor in place of the body (mail-security.js).

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, menu, modal, toast, errorToast, confirmDialog, bytes, dateTime, spinner, debounce } from '../ui.js';
import { suggestList } from './talk-util.js';
import { createRichEditor } from '../editor/rich.js';
import { sanitizeFragment } from '../editor/sanitize.js';
import { htmlToText, textToFragment } from '../editor/convert.js';
import { attachPicker } from '../editor/picker.js';

/** The composer that is open or minimized (one at a time). */
let active = null;
const extensions = [];

/**
 * Extension points that reach into the composer (mail-security.js uses them for the
 * internal-address check, S/MIME signing/encryption and Mailvelope). Each composer passes
 * them a context `c`: { accountId(), accountEmail() (the sending address, an alias's when one
 * is chosen), recipients(), body (the message body: textarea or rich editor), bodyHost (its
 * container, where another editor may take its place), template, opts, attachmentsCount(),
 * markDirty() }.
 * - tools: (c) => Node | null, shown in the composer's toolbar.
 * - beforeSend: async (c, payload) => false to stop sending; may change the payload (e.g.
 *   `payload.security`, or `payload.text` with an encrypted body).
 * - beforeDraft: async (c, payload) => false to skip saving a draft.
 */
export const composerHooks = { tools: [], beforeSend: [], beforeDraft: [] };

/** Adds a composer extension (see the header); returns a function that removes it. */
export function registerComposerExtension(ext) {
  extensions.push(ext);
  return () => { const i = extensions.indexOf(ext); if (i >= 0) extensions.splice(i, 1); };
}

/** Inserts text into the open composer (resuming a minimized one). False when none is open. */
export function insertIntoComposer(text) {
  if (!active) return false;
  active.resume();
  active.insertText(String(text ?? ''));
  return true;
}

// Sent and failed messages are reported wherever the user is.
live.on('mail.outbox', (ev) => {
  if (ev.what === 'sent') toast('Message sent', 'ok', { timeout: 3000 });
  if (ev.what === 'failed') {
    const again = ev.retry_at ? ` It will be tried again at ${new Date(ev.retry_at).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}.` : ' It is waiting in the Outbox.';
    toast(`Sending “${ev.subject || '(no subject)'}” failed: ${ev.error}.${again}`, 'error', { timeout: 10000 });
  }
});

/** Splits typed text into addresses ("a@b.c, Name <d@e.f>; …"). */
function splitAddresses(text) {
  const out = [];
  let cur = '';
  let quoted = false;
  let angle = false;
  for (const ch of text) {
    if (ch === '"') quoted = !quoted;
    if (ch === '<') angle = true;
    if (ch === '>') angle = false;
    if ((ch === ',' || ch === ';') && !quoted && !angle) {
      if (cur.trim()) out.push(cur.trim());
      cur = '';
    } else cur += ch;
  }
  if (cur.trim()) out.push(cur.trim());
  return out;
}

const isGroup = (s) => s.startsWith('group:');
const looksValid = (s) => isGroup(s) || /^[^\s@<>",;]+@[^\s@<>",;]+$/.test((s.match(/<([^>]+)>\s*$/) || [null, s])[1].trim());
const addressOf = (s) => ((s.match(/<([^>]+)>\s*$/) || [null, s])[1] || '').trim().toLowerCase();

/** A recipients field with chips and keyboard autocomplete. `labels` names group chips. */
function recipientField(label, initial, onChange, labels) {
  const values = [...initial];
  const chips = h('div.ml-chips');
  const input = h('input', { type: 'text', autocomplete: 'off', 'aria-label': label, role: 'combobox', placeholder: initial.length ? '' : 'Name, email or group' });
  const sug = suggestList(input, { onPick: (it) => { if (it.label && isGroup(it.value)) labels.set(it.value, it.label); add(it.value); input.value = ''; input.focus(); } });
  const wrap = h('div.ml-field', { onclick: (e) => { if (e.target === wrap || e.target === chips) input.focus(); } },
    h('label.ml-field-label', label), chips, sug.el);
  function draw() {
    clear(chips);
    values.forEach((v, i) => {
      const group = isGroup(v);
      const shown = group ? (labels.get(v) || v.slice(6)) : (v.replace(/\s*<[^>]+>\s*$/, '') || v);
      chips.appendChild(h(`span.ml-chip${looksValid(v) ? '' : '.invalid'}${group ? '.group' : ''}`, { title: group ? `Group ${shown}: everyone in it with an email address` : (looksValid(v) ? v : `${v} is not an email address`) },
        group && icon('users', 'sm'),
        h('span.ellipsis', shown),
        h('button', { type: 'button', 'aria-label': `Remove ${shown}`, onclick: (e) => { e.stopPropagation(); values.splice(i, 1); draw(); onChange(); input.focus(); } }, icon('x', 'sm'))));
    });
    chips.appendChild(input);
  }
  function add(v) {
    for (const a of (isGroup(v) ? [v] : splitAddresses(v))) if (!values.some((x) => x === a || (!isGroup(a) && addressOf(x) === addressOf(a)))) values.push(a);
    draw();
    onChange();
  }
  function commit() {
    if (input.value.trim()) { add(input.value); input.value = ''; }
  }
  let seq = 0;
  const search = debounce(async () => {
    const term = input.value.trim();
    const mine = ++seq;
    if (!term) { sug.hide(); return; }
    try {
      const { recipients } = await api.get(`/api/mail/recipients?q=${encodeURIComponent(term)}`);
      if (mine !== seq || document.activeElement !== input) return;
      sug.show(recipients.filter((r) => !values.some((v) => v === r.address || addressOf(v) === r.address.toLowerCase())).slice(0, 8).map((r) => (r.source === 'group'
        ? { label: r.name, sub: `Group · ${r.members} ${r.members === 1 ? 'member' : 'members'}`, value: r.address, avatar: h('span.avatar.sm.ml-group-avatar', icon('users', 'sm')) }
        : {
          label: r.name || r.address, sub: r.address, value: r.name ? `${r.name} <${r.address}>` : r.address,
          avatar: avatar(r.name || r.address, { size: 'sm', seed: r.address }),
        })));
    } catch (_) { sug.hide(); }
  }, 150);
  input.addEventListener('input', () => {
    if (/[,;]\s*$/.test(input.value)) { commit(); sug.hide(); return; }
    search();
  });
  input.addEventListener('keydown', (e) => {
    if (sug.onKey(e)) return;
    if (e.key === 'Enter' && input.value.trim()) { e.preventDefault(); commit(); }
    if (e.key === 'Backspace' && !input.value && values.length) { values.pop(); draw(); onChange(); }
  });
  input.addEventListener('blur', () => setTimeout(() => { sug.hide(); commit(); }, 150));
  input.addEventListener('paste', (e) => {
    const text = e.clipboardData?.getData('text');
    if (text && /[,;]/.test(text)) { e.preventDefault(); add(text); }
  });
  draw();
  return { el: wrap, input, add, has: (addr) => values.some((v) => addressOf(v) === addr.toLowerCase()), values: () => { commit(); return [...values]; } };
}

/** Lets the user pick files from Files (several, across folders). Resolves with entries. */
function pickFiles(ctx) {
  const chosen = new Map();
  const crumbs = h('div.picker-crumbs');
  const list = h('div.picker-list.panel', { role: 'listbox', 'aria-label': 'Files', 'aria-multiselectable': 'true' });
  const count = h('span.small.muted');
  const stack = [{ id: ctx.me.root_id, name: 'Files' }];
  const load = async () => {
    clear(crumbs).append(...stack.map((c, i) => [i ? h('span.muted', '/') : '', h('button.btn.ghost.sm', { type: 'button', onclick: () => { stack.length = i + 1; load(); } }, c.name)]).flat());
    clear(list).appendChild(spinner());
    try {
      const { entries } = await api.get(`/api/files/${encodeURIComponent(stack[stack.length - 1].id)}/children`);
      clear(list);
      if (!entries.length) list.appendChild(h('div.empty.small', 'This folder is empty'));
      for (const en of entries) {
        const isDir = en.kind === 'dir';
        const item = h(`div.list-item.clickable${chosen.has(en.id) ? '.selected' : ''}`, {
          role: 'option', tabindex: 0, 'aria-selected': String(chosen.has(en.id)),
          onclick: () => {
            if (isDir) { stack.push({ id: en.id, name: en.name }); load(); return; }
            if (chosen.has(en.id)) chosen.delete(en.id); else chosen.set(en.id, en);
            item.classList.toggle('selected', chosen.has(en.id));
            item.setAttribute('aria-selected', String(chosen.has(en.id)));
            count.textContent = chosen.size ? `${chosen.size} selected` : '';
          },
          onkeydown: (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); item.click(); } },
        }, icon(isDir ? 'folder' : 'file'), h('span.grow.ellipsis', en.name), isDir ? icon('chevron-right', 'sm') : h('span.small.muted', bytes(en.size)));
        list.appendChild(item);
      }
    } catch (e) {
      clear(list).appendChild(h('div.empty.small', e.message));
    }
  };
  load();
  return modal({
    title: 'Attach from Files',
    body: h('div.picker', crumbs, list, count),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Attach', kind: 'primary', run: () => {
        if (!chosen.size) { toast('Choose at least one file', 'warn'); return undefined; }
        return [...chosen.values()];
      },
    }],
  });
}

// ---- send later ---------------------------------------------------------------------------
function at(d, hours, minutes = 0) {
  const x = new Date(d);
  x.setHours(hours, minutes, 0, 0);
  return x;
}

/** "Send later" presets from now: [label, Date]. */
export function sendLaterPresets(now = new Date()) {
  const out = [];
  const inHour = new Date(now.getTime() + 3600e3);
  inHour.setSeconds(0, 0);
  out.push(['In 1 hour', inHour]);
  if (now.getHours() < 17) out.push(['This evening', at(now, 18)]);
  const tomorrow = at(new Date(now.getTime() + 86400e3), 8);
  out.push(['Tomorrow morning', tomorrow]);
  const monday = new Date(now);
  monday.setDate(now.getDate() + (((8 - now.getDay()) % 7) || 7));
  const mon = at(monday, 8);
  if (mon.toDateString() !== tomorrow.toDateString()) out.push(['Monday morning', mon]);
  return out;
}

const shortWhen = (d) => d.toLocaleString([], { weekday: 'short', hour: '2-digit', minute: '2-digit' });

async function pickSendTime() {
  const pad = (n) => String(n).padStart(2, '0');
  const local = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
  const start = new Date(Date.now() + 3600e3);
  start.setMinutes(0, 0, 0);
  const input = h('input', { type: 'datetime-local', id: 'ml-send-at', value: local(start), min: local(new Date()), required: true });
  const err = h('div.error-text', { role: 'alert' });
  setTimeout(() => input.focus(), 0);
  return modal({
    title: 'Send later',
    body: h('div', h('div.field', h('label', { for: input.id }, 'Send on'), input), err),
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Schedule', kind: 'primary', run: () => {
        const d = new Date(input.value);
        if (!input.value || Number.isNaN(d.getTime()) || d.getTime() < Date.now() + 30e3) { err.textContent = 'Pick a time in the future.'; return undefined; }
        return d;
      },
    }],
  });
}

// ---- signatures -----------------------------------------------------------------------------
const QUOTE_LINE = /^(On .+ wrote:|---------- Forwarded message ----------)$/m;

/** Plain text body with the signature `next` instead of `prev`, placed like the server does. */
function swapTextSignature(body, prev, next, above) {
  let text = body;
  const oldBlock = prev ? `-- \n${prev.replace(/\s+$/, '')}` : null;
  const newBlock = next && next.trim() ? `-- \n${next.replace(/\s+$/, '')}` : '';
  if (oldBlock && text.includes(oldBlock)) {
    if (newBlock) return text.replace(oldBlock, newBlock);
    return text.replace(new RegExp(`\\n*${oldBlock.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`), '');
  }
  if (!newBlock) return text;
  const q = text.search(QUOTE_LINE);
  if (above && q >= 0) return `${text.slice(0, q).replace(/\n+$/, '')}\n\n${newBlock}\n\n${text.slice(q)}`;
  return `${text.replace(/\s+$/, '')}\n\n${newBlock}`;
}

/** Replaces (or places) the signature block of the rich editor's content. */
function swapRichSignature(area, html, above) {
  const old = area.querySelector('.ferrite-signature');
  const next = html ? sanitizeFragment(html).firstElementChild : null;
  if (old) {
    if (next) old.replaceWith(next); else old.remove();
    return;
  }
  if (!next) return;
  const quote = [...area.children].find((c) => c.classList.contains('ferrite-quote'));
  if (above && quote) quote.before(next); else area.appendChild(next);
}

// ---- the composer -----------------------------------------------------------------------------

/**
 * Opens the composer. opts: { accounts, accountId, fromMessage, mode (reply | reply_all |
 * forward | forward_as_attachment | edit), to, outboxId (reopen a queued message), prefill
 * ({ to, cc, bcc, subject, text } for a new message, e.g. from a mailto: link) }.
 */
export async function openComposer(ctx, opts = {}) {
  if (active) await active.closeQuietly();
  let template = null;
  let outboxId = opts.outboxId || null;
  let context;
  try {
    [context, template] = await Promise.all([
      api.get('/api/mail/compose/context'),
      opts.fromMessage
        ? api.get(`/api/mail/messages/${encodeURIComponent(opts.fromMessage)}/compose?mode=${opts.mode}`).then((r) => r.compose)
        : outboxId ? api.get(`/api/mail/outbox/${encodeURIComponent(outboxId)}`).then((r) => ({ ...r.message.compose, account_id: r.message.account_id, attachment_names: r.message.attachment_names })) : null,
    ]);
  } catch (e) {
    errorToast(e);
    return;
  }
  if (!context.accounts.length) return;
  // Fields of a new message filled in from elsewhere (a mailto: link); the body goes above
  // the signature.
  const prefill = template ? null : opts.prefill || null;
  const seed = template || prefill;
  const labels = new Map();
  if (template && [...(template.to || []), ...(template.cc || []), ...(template.bcc || [])].some(isGroup)) {
    try {
      for (const g of (await api.get('/api/admin/groups')).groups) labels.set(`group:${g.id}`, g.name);
    } catch (_) { /* chips show the id */ }
  }

  // Identities: each account, then its aliases.
  const identities = context.accounts.flatMap((a) => [
    { key: `${a.id}:`, account: a, aliasId: null, name: a.name, email: a.email, sigHtml: a.prefs.signature_html, sigText: a.prefs.signature_text },
    ...a.aliases.map((al) => ({ key: `${a.id}:${al.id}`, account: a, aliasId: al.id, name: al.name || a.name, email: al.email, sigHtml: al.signature_html, sigText: al.signature_text })),
  ]);
  const startAccount = template?.account_id || opts.accountId || context.accounts[0].id;
  let ident = identities.find((x) => x.key === `${startAccount}:${template?.alias_id || ''}`) || identities.find((x) => x.account.id === startAccount) || identities[0];
  const above = () => ident.account.prefs.signature_above_quote;

  let dirty = false;
  let finished = false;
  let draftId = template?.draft_id || null;
  let draftsThisSession = false;
  let saving = Promise.resolve(true);
  let autosaveTimer = null;
  // Attachments: {ref, name, size, uploading}; ref is what the server needs.
  const attachments = (template?.attachments || []).map((ref, i) => ({ ref, name: template.attachment_names[i]?.[0] || 'attachment', size: template.attachment_names[i]?.[1] || 0 }));
  const status = h('span.small.muted.ml-compose-status', { role: 'status' });

  function markDirty() {
    dirty = true;
    clearTimeout(autosaveTimer);
    autosaveTimer = setTimeout(() => autosave(), 4000);
  }

  // ---- fields ----
  const from = identities.length > 1
    ? h('select', { 'aria-label': 'From', onchange: () => setIdentity(identities.find((x) => x.key === from.value)) },
      identities.map((x) => h('option', { value: x.key, selected: x.key === ident.key }, x.name ? `${x.name} <${x.email}>` : x.email)))
    : null;
  const to = recipientField('To', seed?.to?.length ? seed.to : (opts.to ? [opts.to] : []), () => { markDirty(); refreshInfo(); }, labels);
  const cc = recipientField('Cc', seed?.cc || [], () => { markDirty(); refreshInfo(); }, labels);
  const bcc = recipientField('Bcc', seed?.bcc || [], markDirty, labels);
  const showCc = !!(seed?.cc?.length || seed?.bcc?.length);
  cc.el.hidden = !showCc;
  bcc.el.hidden = !showCc;
  const ccToggle = h('button.btn.ghost.sm.ml-cc-toggle', { type: 'button', hidden: showCc, onclick: () => { cc.el.hidden = false; bcc.el.hidden = false; ccToggle.hidden = true; cc.input.focus(); } }, 'Cc/Bcc');
  to.el.appendChild(ccToggle);
  const subject = h('input.ml-subject-input', { type: 'text', value: seed?.subject || '', placeholder: 'Subject', 'aria-label': 'Subject', oninput: () => { markDirty(); updateDock(); } });

  // ---- body: rich editor or plain textarea ----
  const editorMode = opts.mode === 'edit' || outboxId ? (template?.html ? 'richtext' : 'plaintext') : ident.account.prefs.editor_mode;
  let rich = editorMode === 'richtext';
  const bodyBox = h('div.ml-body');
  const bodyHost = h('div.ml-body-host', bodyBox);
  let textarea = null;
  let editor = null;
  let plainCaret = false;
  const lead = prefill?.text || '';
  const leadHtml = () => {
    if (!lead) return '<div><br></div>';
    const box = document.createElement('div');
    box.appendChild(textToFragment(lead));
    return `${box.innerHTML}<div><br></div>`;
  };
  const newHtml = () => `${leadHtml()}${ident.sigHtml || ''}`;
  const newText = () => `${lead}${ident.sigText ? `\n\n-- \n${ident.sigText}` : ''}`;

  let blocks = null;
  const loadBlocks = async () => {
    if (!blocks) blocks = (await api.get('/api/mail/text-blocks')).blocks;
    return blocks;
  };
  const picker = attachPicker({
    field: () => (rich ? editor?.area : textarea),
    mode: () => (rich ? 'rich' : 'plain'),
    triggers: {
      '@': async (q) => {
        if (!q) return [];
        try {
          const { recipients } = await api.get(`/api/mail/recipients?q=${encodeURIComponent(q)}`);
          return recipients.filter((r) => r.source !== 'group' && r.address.includes('@')).map((r) => ({ label: r.name || r.address, sub: r.address, data: r }));
        } catch (_) { return []; }
      },
      '!': async (q) => {
        const list = await loadBlocks().catch(() => []);
        const lq = q.toLowerCase();
        return list.filter((b) => !lq || b.title.toLowerCase().includes(lq) || b.text.toLowerCase().includes(lq)).map((b) => ({ label: b.title, sub: b.text.split('\n')[0], data: b }));
      },
    },
    onPick: (trigger, item, replaceWith) => {
      if (trigger === '@') {
        const r = item.data;
        replaceWith(`@${r.name || r.address} `);
        if (!to.has(r.address) && !cc.has(r.address) && !bcc.has(r.address)) {
          to.add(r.name ? `${r.name} <${r.address}>` : r.address);
          toast(`${r.name || r.address} was added as a recipient`, 'ok', { timeout: 2500 });
        }
      } else if (rich) {
        replaceWith(sanitizeFragment(item.data.html));
        editor.leaveInline();
      } else replaceWith(item.data.text);
      markDirty();
    },
  });

  async function uploadFile(file) {
    const resp = await fetch(`/api/mail/uploads?name=${encodeURIComponent(file.name || 'image.png')}`, {
      method: 'POST', body: file, credentials: 'same-origin',
      headers: { 'X-Ferrite-Request': '1', 'Content-Type': file.type || 'application/octet-stream' },
    });
    const data = await resp.json().catch(() => ({}));
    if (!resp.ok) throw new Error(data.error || resp.statusText);
    return data.upload;
  }

  function makeEditor(content) {
    editor?.destroy();
    editor = createRichEditor({
      html: content,
      label: 'Message',
      onChange: markDirty,
      uploadImage: async (file) => `/api/mail/uploads/${encodeURIComponent((await uploadFile(file)).id)}/content`,
      onFiles: (files) => addFiles(files),
      onKeydown: (e) => picker.onKey(e),
    });
    return editor;
  }
  function makeTextarea(value) {
    textarea = h('textarea.ml-body-input', { 'aria-label': 'Message', placeholder: 'Write your message…', oninput: markDirty, onfocus: () => { plainCaret = true; } }, value);
    textarea.addEventListener('keydown', (e) => { picker.onKey(e); });
    return textarea;
  }
  if (rich) bodyBox.appendChild(makeEditor(template ? template.html || '' : newHtml()).el);
  else bodyBox.appendChild(makeTextarea(template ? template.text ?? '' : newText()));

  function setMode(next) {
    if ((next === 'richtext') === rich) return;
    picker.close();
    if (next === 'plaintext') {
      // An empty first line (where a reply is written) stays.
      const first = editor.area.firstElementChild;
      const lead = first && !first.textContent.trim() && !first.querySelector('img') && editor.area.children.length > 1 ? '\n\n' : '';
      const text = lead + editor.getText();
      editor.destroy();
      editor = null;
      rich = false;
      clear(bodyBox).appendChild(makeTextarea(text));
    } else {
      const text = textarea.value;
      const frag = document.createDocumentFragment();
      const block = ident.sigText ? `-- \n${ident.sigText}` : null;
      const at = block ? text.indexOf(block) : -1;
      if (at >= 0) {
        frag.appendChild(textToFragment(text.slice(0, at).replace(/\n+$/, '')));
        if (ident.sigHtml) frag.appendChild(sanitizeFragment(ident.sigHtml));
        const after = text.slice(at + block.length).replace(/^\n+/, '');
        if (after.trim()) frag.appendChild(textToFragment(after));
      } else frag.appendChild(textToFragment(text));
      textarea = null;
      rich = true;
      clear(bodyBox).appendChild(makeEditor(frag).el);
    }
    modeBtn.replaceChildren(icon(rich ? 'doc' : 'edit', 'sm'), h('span', rich ? 'Plain text' : 'Rich text'));
    modeBtn.title = rich ? 'Switch to plain text' : 'Switch to rich text';
    markDirty();
    focusBody();
  }

  function setIdentity(next) {
    if (!next || next === ident) return;
    const prev = ident;
    ident = next;
    if (rich) swapRichSignature(editor.area, next.sigHtml, above());
    else textarea.value = swapTextSignature(textarea.value, prev.sigText, next.sigText, above());
    markDirty();
  }

  /** Text or nodes at the end of what the user wrote (before the signature and the quote). */
  function appendToMessage(nodes, text) {
    if (rich) {
      const area = editor.area;
      const stop = [...area.children].find((c) => c.classList.contains('ferrite-signature') || c.classList.contains('ferrite-quote'));
      area.insertBefore(nodes, stop || null);
    } else {
      const v = textarea.value;
      let at = v.indexOf('\n-- \n');
      const q = v.search(QUOTE_LINE);
      if (q >= 0 && (at < 0 || q < at)) at = q;
      if (at < 0) at = v.length;
      const before = v.slice(0, at).replace(/\n+$/, '');
      textarea.value = `${before}${before ? '\n' : ''}${text}\n${v.slice(at)}`;
    }
    markDirty();
  }

  function insertText(text) {
    if (rich) {
      if (!editor.hasCaret()) editor.caretToStart();
      editor.insertText(text);
    } else {
      if (!plainCaret) textarea.setSelectionRange(0, 0);
      textarea.focus();
      textarea.setRangeText(text, textarea.selectionStart, textarea.selectionEnd, 'end');
      textarea.dispatchEvent(new Event('input', { bubbles: true }));
    }
    markDirty();
  }

  function focusBody() {
    if (rich) {
      if (!editor.hasCaret()) editor.caretToStart(); else editor.focus();
    } else {
      textarea.focus();
      if (!plainCaret) textarea.setSelectionRange(0, 0);
    }
  }

  // ---- attachments ----
  const attachList = h('div.ml-compose-files');
  const limit = context.attachment_limit || 0;
  const hardLimit = context.max_attachments;
  const tooBig = (size) => size > hardLimit || (limit > 0 && size > limit);
  const limitText = () => bytes(limit > 0 ? Math.min(limit, hardLimit) : hardLimit);

  function drawAttachments() {
    clear(attachList);
    attachments.forEach((a, i) => {
      attachList.appendChild(h(`span.ml-file-chip${a.ref?.kind === 'eml' || /\.eml$/i.test(a.name) ? '.eml' : ''}`,
        icon(/\.eml$/i.test(a.name) ? 'mail' : 'paperclip', 'sm'), h('span.ellipsis', a.name), a.size ? h('span.small.muted', bytes(a.size)) : null,
        a.uploading ? spinner() : h('button', { type: 'button', 'aria-label': `Remove ${a.name}`, onclick: () => { attachments.splice(i, 1); markDirty(); drawAttachments(); } }, icon('x', 'sm'))));
    });
  }

  function insertLink(link) {
    const until = link.expires_at ? `, available until ${new Date(link.expires_at).toLocaleDateString()}` : '';
    const note = ` (${bytes(link.size)}${until})`;
    const nodes = document.createDocumentFragment();
    nodes.appendChild(h('div', h('a', { href: link.url }, link.name), note));
    appendToMessage(nodes, `${link.name}${note}: ${link.url}`);
  }

  async function convertToLink(a) {
    a.uploading = true;
    drawAttachments();
    try {
      const body = a.ref.kind === 'upload' ? { upload_id: a.ref.id } : { file_id: a.ref.id };
      const { link } = await api.post('/api/mail/compose/share-link', body);
      attachments.splice(attachments.indexOf(a), 1);
      insertLink(link);
      toast(`${link.name} is shared as a link in the message`, 'ok', { timeout: 3000 });
    } catch (e) {
      a.uploading = false;
      errorToast(e);
    }
    drawAttachments();
  }

  async function shareLarge(file) {
    const entry = { name: file.name, size: file.size, uploading: true, ref: null };
    attachments.push(entry);
    drawAttachments();
    try {
      const resp = await fetch(`/api/mail/compose/large-attachment?name=${encodeURIComponent(file.name)}`, {
        method: 'PUT', body: file, credentials: 'same-origin', headers: { 'X-Ferrite-Request': '1' },
      });
      const data = await resp.json().catch(() => ({}));
      if (!resp.ok) throw new Error(data.error || resp.statusText);
      insertLink(data.link);
    } catch (e) {
      errorToast(e);
    }
    attachments.splice(attachments.indexOf(entry), 1);
    drawAttachments();
  }

  async function addFiles(files) {
    for (const file of files) {
      if (tooBig(file.size)) {
        const ok = await confirmDialog('Large attachment', `${file.name} (${bytes(file.size)}) is larger than ${limitText()}, the most a message can carry here. Share it as a link instead? It is saved in Files under “Mail attachments” and the link works for ${context.link_days} days.`, { confirm: 'Share as link' });
        if (ok) await shareLarge(file);
        continue;
      }
      const entry = { name: file.name, size: file.size, uploading: true, ref: null };
      attachments.push(entry);
      drawAttachments();
      try {
        const up = await uploadFile(file);
        entry.ref = { kind: 'upload', id: up.id };
        entry.uploading = false;
        markDirty();
      } catch (e) {
        attachments.splice(attachments.indexOf(entry), 1);
        errorToast(e);
      }
      drawAttachments();
    }
  }

  const fileInput = h('input', { type: 'file', multiple: true, hidden: true, onchange: () => { addFiles([...fileInput.files]); fileInput.value = ''; } });
  const linkInput = h('input', { type: 'file', multiple: true, hidden: true, onchange: async () => { const files = [...linkInput.files]; linkInput.value = ''; for (const f of files) await shareLarge(f); } });
  /** Files shared as links instead of attached (by choice; larger ones always are). */
  function linkMenu(anchor) {
    menu(anchor, [
      { label: 'Upload a file and share a link…', icon: 'upload', run: () => linkInput.click() },
      {
        label: 'Share a link to a file in Files…', icon: 'files', run: async () => {
          const picked = await pickFiles(ctx);
          for (const f of picked || []) {
            try { insertLink((await api.post('/api/mail/compose/share-link', { file_id: f.id })).link); } catch (e) { errorToast(e); }
          }
        },
      },
    ]);
  }

  // ---- recipient info pane ----
  const infoPane = h('aside.ml-rcpt-pane', { 'aria-label': 'Recipient info' });
  const infoCache = new Map();
  let infoOpen = (() => { try { return localStorage.getItem('ferrite.mail.rcptinfo') === '1'; } catch (_) { return false; } })();
  const refreshInfo = debounce(async () => {
    if (!infoOpen) return;
    const addrs = [...new Set([...to.values(), ...cc.values()].filter((v) => !isGroup(v) && looksValid(v)).map(addressOf))].slice(0, 6);
    clear(infoPane).appendChild(h('div.ml-rcpt-head', h('strong.grow', 'Recipients'),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Hide recipient info', onclick: () => toggleInfo(false) }, icon('x', 'sm'))));
    if (!addrs.length) { infoPane.appendChild(h('p.small.muted', 'Details of saved contacts appear here when you add recipients.')); return; }
    for (const addr of addrs) {
      if (!infoCache.has(addr)) infoCache.set(addr, api.get(`/api/mail/compose/recipient-info?address=${encodeURIComponent(addr)}`).then((r) => r.info).catch(() => null));
      const info = await infoCache.get(addr);
      if (!info) continue;
      const c = info.contacts[0];
      const name = c?.name || info.user?.display_name || addr;
      const rows = [];
      if (c?.org) rows.push(h('div', icon('users', 'sm'), h('span', c.org)));
      for (const p of c?.phones || []) rows.push(h('div', icon('phone', 'sm'), h('a', { href: `tel:${p}` }, p)));
      for (const e of (c?.emails || []).filter((x) => x.toLowerCase() !== addr)) rows.push(h('div', icon('mail', 'sm'), h('span.ellipsis', e)));
      if (info.user) rows.push(h('div', icon('user', 'sm'), h('span', `Ferrite user @${info.user.username}`)));
      if (info.messages) rows.push(h('div', icon('history', 'sm'), h('span', `${info.messages} ${info.messages === 1 ? 'message' : 'messages'} from them${info.last_message ? `, last ${new Date(info.last_message).toLocaleDateString()}` : ''}`)));
      if (c?.note) rows.push(h('p.small.ml-rcpt-note', c.note));
      infoPane.appendChild(h('details.ml-rcpt', { open: true },
        h('summary', avatar(name, { size: 'sm', seed: addr }), h('span.ml-rcpt-name', h('strong.ellipsis', name), h('span.small.muted.ellipsis', addr))),
        rows.length ? h('div.ml-rcpt-rows.small', rows) : h('p.small.muted', c ? 'No other details saved.' : 'Not in your contacts.')));
    }
  }, 250);
  function toggleInfo(open = !infoOpen) {
    infoOpen = open;
    try { localStorage.setItem('ferrite.mail.rcptinfo', open ? '1' : '0'); } catch (_) { /* private mode */ }
    layout.classList.toggle('with-info', open);
    infoBtn.setAttribute('aria-pressed', String(open));
    infoBtn.classList.toggle('active', open);
    if (open) refreshInfo(); else clear(infoPane);
  }

  // ---- tools ----
  const modeBtn = h('button.btn.ghost.sm', { type: 'button', title: rich ? 'Switch to plain text' : 'Switch to rich text', onclick: () => setMode(rich ? 'plaintext' : 'richtext') },
    icon(rich ? 'doc' : 'edit', 'sm'), h('span', rich ? 'Plain text' : 'Rich text'));
  const blocksBtn = h('button.btn.ghost.sm', {
    type: 'button', title: 'Insert a text block (or type ! in the message)',
    onclick: async (e) => {
      const anchor = e.currentTarget;
      if (rich) editor.saveSelection();
      const list = await loadBlocks().catch((err) => { errorToast(err); return []; });
      menu(anchor, [
        ...list.map((b) => ({ label: b.title, icon: 'note', run: () => { if (rich) editor.insertSnippet(sanitizeFragment(b.html)); else insertText(b.text); } })),
        !list.length && { label: 'No text blocks yet', run: () => {} },
        'sep',
        { label: 'Manage text blocks…', icon: 'settings', run: async () => { const { mailSettingsDialog } = await import('./mail-settings.js'); await mailSettingsDialog(ctx, { tab: 'blocks' }); blocks = null; } },
      ]);
    },
  }, icon('note', 'sm'), h('span', 'Text blocks'));
  const mdn = h('input', { type: 'checkbox', checked: !!template?.request_mdn, onchange: markDirty });
  const infoBtn = h('button.btn.ghost.sm', { type: 'button', 'aria-pressed': String(infoOpen), title: 'Show what you have saved about the recipients', onclick: () => toggleInfo() }, icon('contacts', 'sm'), h('span', 'Recipient info'));
  const extApi = {
    accountId: () => ident.account.id,
    aliasId: () => ident.aliasId,
    recipients: () => ({ to: to.values(), cc: cc.values(), bcc: bcc.values() }),
    payload: null,
  };
  const hookCtx = {
    accountId: () => ident.account.id,
    accountEmail: () => ident.email,
    recipients: () => [...to.values(), ...cc.values(), ...bcc.values()],
    body: bodyBox, bodyHost, template, opts,
    attachmentsCount: () => attachments.length,
    markDirty,
  };

  const layout = h('div.ml-compose-layout');
  const form = h('div.ml-compose', {
    ondragover: (e) => { if (e.dataTransfer.types.includes('Files')) { e.preventDefault(); form.classList.add('drop'); } },
    ondragleave: () => form.classList.remove('drop'),
    ondrop: (e) => { if (e.dataTransfer.files.length) { e.preventDefault(); form.classList.remove('drop'); addFiles([...e.dataTransfer.files]); } },
  },
  from && h('div.ml-field', h('label.ml-field-label', 'From'), from),
  to.el, cc.el, bcc.el,
  h('div.ml-field', subject),
  bodyHost,
  attachList,
  h('div.ml-compose-tools',
    h('button.btn.ghost.sm', { type: 'button', onclick: () => fileInput.click() }, icon('upload', 'sm'), h('span', 'Attach file')),
    h('button.btn.ghost.sm', {
      type: 'button',
      onclick: async () => {
        const picked = await pickFiles(ctx);
        for (const f of picked || []) {
          const entry = { ref: { kind: 'file', id: f.id }, name: f.name, size: f.size };
          attachments.push(entry);
          if (tooBig(f.size)) convertToLink(entry);
        }
        if (picked?.length) { markDirty(); drawAttachments(); }
      },
    }, icon('files', 'sm'), h('span', 'From Files')),
    h('button.btn.ghost.sm', { type: 'button', title: `Share files as links (works for ${context.link_days} days) instead of attaching them`, onclick: (e) => linkMenu(e.currentTarget) }, icon('link', 'sm'), h('span', 'Share link')),
    blocksBtn, modeBtn, infoBtn,
    h('label.check.ml-mdn-check', { title: 'Ask the recipients to confirm that they have displayed the message' }, mdn, 'Request read receipt'),
    extensions.map((x) => x.tools?.(extApi)).filter(Boolean),
    composerHooks.tools.map((t) => t(hookCtx)).filter(Boolean),
    fileInput, h('div.grow'), status));
  layout.append(form, infoPane);
  drawAttachments();

  // ---- saving and sending ----
  function payload() {
    return {
      to: to.values(), cc: cc.values(), bcc: bcc.values(),
      subject: subject.value,
      text: rich ? '' : textarea.value,
      html: rich ? editor.getHtml() : null,
      in_reply_to: template?.in_reply_to || null,
      attachments: attachments.filter((a) => a.ref).map((a) => a.ref),
      draft_id: draftId,
      alias_id: ident.aliasId,
      request_mdn: mdn.checked,
    };
  }
  const busy = () => attachments.some((a) => a.uploading);
  const hasContent = () => subject.value.trim() || to.values().length || cc.values().length || attachments.length
    || (rich ? editor.getText().replace(`-- \n${ident.sigText}`, '').trim() : textarea.value.replace(`-- \n${ident.sigText}`, '').trim());

  function saveDraft({ quiet = false } = {}) {
    clearTimeout(autosaveTimer);
    saving = saving.then(async () => {
      if (busy()) { if (!quiet) toast('Wait for the attachments to finish uploading', 'warn'); return false; }
      const draft = payload();
      for (const hook of composerHooks.beforeDraft) if (await hook(hookCtx, draft) === false) return false;
      status.textContent = 'Saving draft…';
      const wasDirty = dirty;
      dirty = false;
      try {
        const { id } = await api.post(`/api/mail/accounts/${encodeURIComponent(ident.account.id)}/drafts`, draft);
        draftId = id;
        draftsThisSession = true;
        status.textContent = `Draft saved ${new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`;
        if (!quiet) toast('Draft saved', 'ok', { timeout: 2000 });
        return true;
      } catch (e) {
        dirty = dirty || wasDirty;
        status.textContent = '';
        if (!quiet) errorToast(e);
        return false;
      }
    });
    return saving;
  }

  function autosave() {
    if (finished || !dirty || busy() || !hasContent()) return;
    saveDraft({ quiet: true });
  }

  async function send(sendAt = null) {
    if (busy()) { toast('Wait for the attachments to finish uploading', 'warn'); return false; }
    const p = payload();
    if (!p.to.length && !p.cc.length && !p.bcc.length) { toast('Add at least one recipient', 'warn'); to.input.focus(); return false; }
    const bad = [...p.to, ...p.cc, ...p.bcc].find((v) => !looksValid(v));
    if (bad) { toast(`“${bad}” is not an email address`, 'warn'); return false; }
    if (!p.subject.trim() && !await confirmDialog('No subject', 'Send this message without a subject?', { confirm: 'Send anyway' })) return false;
    extApi.payload = p;
    for (const x of extensions) {
      if (x.beforeSend && (await x.beforeSend(extApi)) === false) return false;
    }
    for (const hook of composerHooks.beforeSend) if (await hook(hookCtx, p) === false) return false;
    clearTimeout(autosaveTimer);
    await saving;
    p.draft_id = draftId;
    status.textContent = sendAt ? 'Scheduling…' : 'Sending…';
    let entry;
    try {
      const body = { account_id: ident.account.id, compose: p, send_at: sendAt ? sendAt.toISOString() : null };
      entry = outboxId
        ? (await api.put(`/api/mail/outbox/${encodeURIComponent(outboxId)}`, body)).message
        : (await api.post('/api/mail/outbox', body)).message;
    } catch (e) {
      status.textContent = '';
      errorToast(e);
      return false;
    }
    finished = true;
    close();
    const undo = { label: 'Undo', run: () => undoSend(ctx, opts, entry.id) };
    if (sendAt) toast(`Scheduled for ${dateTime(entry.send_at)}`, 'ok', { timeout: 6000, action: undo });
    else {
      const wait = Math.max(0, Math.round((new Date(entry.send_at).getTime() - Date.now()) / 1000));
      if (wait > 0) toast(`Sending in ${wait} s`, 'info', { timeout: wait * 1000, action: undo });
      else toast('Sending…', 'info', { timeout: 2000 });
    }
    return true;
  }

  async function sendLater(anchor) {
    const items = sendLaterPresets().map(([label, d]) => ({ label: `${label} · ${shortWhen(d)}`, icon: 'clock', run: () => send(d) }));
    menu(anchor, [...items, 'sep', { label: 'Pick date and time…', icon: 'calendar', run: async () => { const d = await pickSendTime(); if (d) send(d); } }]);
  }

  // ---- window: open, minimized, closed ----
  const title = h('h2', { id: `ml-compose-title-${Date.now()}` }, ({ reply: 'Reply', reply_all: 'Reply all', forward: 'Forward', forward_as_attachment: 'Forward', edit: 'Edit draft' }[opts.mode] || (outboxId ? 'Queued message' : 'New message')));
  const sendBtn = h('button.btn.primary', { type: 'button', onclick: async () => { sendBtn.disabled = true; try { await send(); } finally { sendBtn.disabled = false; } } }, 'Send');
  const dialog = h('div.modal.wide.ml-composer', {
    role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': title.id,
    onkeydown: (e) => {
      if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); sendBtn.click(); }
      if (e.key === 'Escape' && !e.defaultPrevented && !document.querySelector('.menu, .ed-pop')) { e.preventDefault(); close(); }
    },
  },
  h('div.modal-head', title,
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Minimize', title: 'Minimize (keep working in Mail)', onclick: () => minimize() }, icon('chevron-down')),
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Close', title: 'Close (keeps a draft)', onclick: () => close() }, icon('x'))),
  h('div.modal-body', layout),
  linkInput,
  h('div.modal-foot',
    h('button.btn', { type: 'button', onclick: () => discard() }, 'Discard'),
    h('button.btn', { type: 'button', onclick: () => saveDraft() }, 'Save draft'),
    h('div.grow'),
    h('button.btn', { type: 'button', title: 'Send later', onclick: (e) => sendLater(e.currentTarget) }, icon('clock', 'sm'), h('span', 'Send later')),
    sendBtn));
  const backdrop = h('div.modal-backdrop.ml-composer-backdrop', dialog);
  const dockLabel = h('span.ellipsis.grow');
  const dock = h('div.ml-compose-dock', { role: 'region', 'aria-label': 'Minimized message' },
    icon('edit', 'sm'), dockLabel,
    h('button.btn.sm.primary', { type: 'button', onclick: () => resume() }, 'Resume'),
    h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Close the minimized message', title: 'Close (keeps a draft)', onclick: () => close() }, icon('x', 'sm')));
  function updateDock() { dockLabel.textContent = subject.value.trim() || 'New message'; }

  function minimize() {
    picker.close();
    if (rich) editor.saveSelection();
    backdrop.remove();
    updateDock();
    document.body.appendChild(dock);
    document.body.classList.add('has-compose-dock');
  }
  function resume() {
    dock.remove();
    document.body.classList.remove('has-compose-dock');
    if (!backdrop.isConnected) document.body.appendChild(backdrop);
    focusBody();
  }
  function teardown() {
    clearTimeout(autosaveTimer);
    picker.destroy();
    editor?.destroy();
    backdrop.remove();
    dock.remove();
    document.body.classList.remove('has-compose-dock');
    if (active === handle) active = null;
  }
  async function close() {
    if (finished) { teardown(); return; }
    finished = true;
    const changed = dirty && hasContent();
    teardown();
    if (outboxId) {
      const url = `/api/mail/outbox/${encodeURIComponent(outboxId)}`;
      try {
        if (opts.restoreSendAt) {
          // Opened from its schedule: closing keeps (or updates) it for the same time.
          await api.put(url, { account_id: ident.account.id, compose: payload(), send_at: opts.restoreSendAt });
          toast(`Still scheduled for ${dateTime(opts.restoreSendAt)}`, 'ok', { timeout: 3000 });
          return;
        }
        if (changed) await api.put(url, { account_id: ident.account.id, compose: payload(), hold: true });
      } catch (e) { errorToast(e); return; }
      toast('Kept in the Outbox', 'ok', { timeout: 2500 });
      return;
    }
    if (changed) {
      if (await saveDraft({ quiet: true })) toast('Saved to Drafts', 'ok', { timeout: 2500 });
      else toast('The draft could not be saved', 'error');
    } else if (draftsThisSession) {
      await saving;
      toast('Saved to Drafts', 'ok', { timeout: 2500 });
    }
  }
  async function discard() {
    finished = true;
    teardown();
    await saving;
    try {
      if (outboxId) {
        await api.del(`/api/mail/outbox/${encodeURIComponent(outboxId)}`);
        toast('Message discarded', 'ok', { timeout: 2000 });
      } else if (draftsThisSession && draftId) {
        // Drafts saved automatically while writing go too.
        await api.post('/api/mail/actions', { ids: [draftId], action: 'delete' });
      }
    } catch (e) {
      errorToast(e);
    }
  }
  const handle = {
    resume,
    insertText,
    closeQuietly: async () => { await close(); },
  };
  active = handle;
  document.body.appendChild(backdrop);
  updateDock();
  // The pane opens by itself only where there is room for it.
  if (infoOpen && matchMedia('(min-width: 1100px)').matches) toggleInfo(true); else infoOpen = false;
  setTimeout(() => {
    if (template && to.values().length) focusBody(); else to.input.focus();
  }, 0);
}

/** Undo send: takes the message back from the outbox and reopens it. */
async function undoSend(ctx, opts, id) {
  try {
    await api.post(`/api/mail/outbox/${encodeURIComponent(id)}/stop`, {});
  } catch (e) {
    errorToast(e);
    return;
  }
  openComposer(ctx, { accounts: opts.accounts, outboxId: id });
}
