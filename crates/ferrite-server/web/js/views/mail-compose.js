// The mail composer: To/Cc/Bcc with suggestions from contacts, users and past
// correspondents; replies, reply-all and forwards prefilled by the server (quoting and
// threading); attachments uploaded or picked from Files; drafts saved to the account's
// Drafts folder; sending through the account's SMTP server. Closing a changed message keeps
// it as a draft. Ctrl/Cmd+Enter sends.

import { api } from '../api.js';
import { h, clear, icon, avatar, modal, toast, errorToast, confirmDialog, bytes, spinner, debounce } from '../ui.js';
import { suggestList } from './talk-util.js';

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

/**
 * Extension points for other mail modules (mail-security.js uses them for the internal-address
 * check, S/MIME signing/encryption and Mailvelope). Each composer passes them a context `c`:
 * { accountId(), accountEmail(), recipients(), body (the textarea), bodyHost (its container,
 * where an editor may take its place), template, opts, attachmentsCount(), markDirty() }.
 * - tools: (c) => Node | null, shown in the composer's toolbar.
 * - beforeSend: async (c, payload) => false to stop sending; may change the payload (e.g.
 *   `payload.security`, or `payload.text` with an encrypted body).
 * - beforeDraft: async (c, payload) => false to skip saving a draft.
 */
export const composerHooks = { tools: [], beforeSend: [], beforeDraft: [] };

const looksValid = (s) => /^[^\s@<>",;]+@[^\s@<>",;]+$/.test((s.match(/<([^>]+)>\s*$/) || [null, s])[1].trim());

/** A recipients field with chips and keyboard autocomplete. */
function recipientField(label, initial, onChange) {
  const values = [...initial];
  const chips = h('div.ml-chips');
  const input = h('input', { type: 'text', autocomplete: 'off', 'aria-label': label, role: 'combobox', placeholder: initial.length ? '' : 'Name or email' });
  const sug = suggestList(input, { onPick: (it) => { add(it.value); input.value = ''; input.focus(); } });
  const wrap = h('div.ml-field', { onclick: (e) => { if (e.target === wrap || e.target === chips) input.focus(); } },
    h('label.ml-field-label', label), chips, sug.el);
  function draw() {
    clear(chips);
    values.forEach((v, i) => chips.appendChild(h(`span.ml-chip${looksValid(v) ? '' : '.invalid'}`, { title: looksValid(v) ? v : `${v} is not an email address` },
      h('span.ellipsis', v.replace(/\s*<[^>]+>\s*$/, '') || v),
      h('button', { type: 'button', 'aria-label': `Remove ${v}`, onclick: (e) => { e.stopPropagation(); values.splice(i, 1); draw(); onChange(); input.focus(); } }, icon('x', 'sm')))));
    chips.appendChild(input);
  }
  function add(v) {
    for (const a of splitAddresses(v)) if (!values.includes(a)) values.push(a);
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
      sug.show(recipients.filter((r) => !values.some((v) => v.toLowerCase().includes(r.address.toLowerCase()))).slice(0, 8).map((r) => ({
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
  return { el: wrap, input, values: () => { commit(); return [...values]; } };
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

/**
 * Opens the composer. opts: { accounts, accountId, fromMessage, mode (reply | reply_all |
 * forward | edit), to }.
 */
export async function openComposer(ctx, opts) {
  // `prefill` (e.g. from a mailto: link) fills the fields like a reply template does.
  let template = opts.prefill || null;
  if (opts.fromMessage) {
    try {
      ({ compose: template } = await api.get(`/api/mail/messages/${encodeURIComponent(opts.fromMessage)}/compose?mode=${opts.mode}`));
    } catch (e) {
      errorToast(e);
      return;
    }
  }
  const accountId = template?.account_id || opts.accountId || opts.accounts[0].id;
  const account = opts.accounts.find((a) => a.id === accountId) || opts.accounts[0];
  let dirty = false;
  let finished = false;
  let draftId = template?.draft_id || null;
  const markDirty = () => { dirty = true; };
  // Attachments: {ref, name, size}; ref is what the server needs.
  const attachments = (template?.attachments || []).map((ref, i) => ({ ref, name: template.attachment_names[i]?.[0] || 'attachment', size: template.attachment_names[i]?.[1] || 0 }));

  const from = opts.accounts.length > 1
    ? h('select', { 'aria-label': 'From', onchange: markDirty }, opts.accounts.map((a) => h('option', { value: a.id, selected: a.id === account.id }, a.name ? `${a.name} <${a.email}>` : a.email)))
    : null;
  const to = recipientField('To', template?.to || (opts.to ? [opts.to] : []), markDirty);
  const cc = recipientField('Cc', template?.cc || [], markDirty);
  const bcc = recipientField('Bcc', template?.bcc || [], markDirty);
  const showCc = !!(template?.cc?.length || template?.bcc?.length);
  cc.el.hidden = !showCc;
  bcc.el.hidden = !showCc;
  const ccToggle = h('button.btn.ghost.sm.ml-cc-toggle', { type: 'button', hidden: showCc, onclick: () => { cc.el.hidden = false; bcc.el.hidden = false; ccToggle.hidden = true; cc.input.focus(); } }, 'Cc/Bcc');
  to.el.appendChild(ccToggle);
  const subject = h('input.ml-subject-input', { type: 'text', value: template?.subject || '', placeholder: 'Subject', 'aria-label': 'Subject', oninput: markDirty });
  const signature = !template && account.signature ? `\n\n-- \n${account.signature}` : '';
  const body = h('textarea.ml-body-input', { 'aria-label': 'Message', placeholder: 'Write your message…', oninput: markDirty }, template?.text ?? signature);
  const bodyHost = h('div.ml-body-host', body);
  const attachList = h('div.ml-compose-files');
  const status = h('span.small.muted.ml-compose-status', { role: 'status' });

  function drawAttachments() {
    clear(attachList);
    attachments.forEach((a, i) => attachList.appendChild(h('span.ml-file-chip', icon('paperclip', 'sm'), h('span.ellipsis', a.name), a.size ? h('span.small.muted', bytes(a.size)) : null,
      a.uploading ? spinner() : h('button', { type: 'button', 'aria-label': `Remove ${a.name}`, onclick: () => { attachments.splice(i, 1); markDirty(); drawAttachments(); } }, icon('x', 'sm')))));
  }

  async function upload(files) {
    for (const file of files) {
      const entry = { name: file.name, size: file.size, uploading: true, ref: null };
      attachments.push(entry);
      drawAttachments();
      try {
        const resp = await fetch(`/api/mail/uploads?name=${encodeURIComponent(file.name)}`, {
          method: 'POST', body: file, credentials: 'same-origin',
          headers: { 'X-Ferrite-Request': '1', 'Content-Type': file.type || 'application/octet-stream' },
        });
        const data = await resp.json();
        if (!resp.ok) throw new Error(data.error || resp.statusText);
        entry.ref = { kind: 'upload', id: data.upload.id };
        entry.uploading = false;
        markDirty();
      } catch (e) {
        attachments.splice(attachments.indexOf(entry), 1);
        errorToast(e);
      }
      drawAttachments();
    }
  }

  const fileInput = h('input', { type: 'file', multiple: true, hidden: true, onchange: () => { upload([...fileInput.files]); fileInput.value = ''; } });

  function payload() {
    return {
      to: to.values(), cc: cc.values(), bcc: bcc.values(),
      subject: subject.value, text: body.value,
      in_reply_to: template?.in_reply_to || null,
      attachments: attachments.filter((a) => a.ref).map((a) => a.ref),
      draft_id: draftId,
    };
  }
  const chosenAccount = () => (from ? from.value : account.id);
  const busy = () => attachments.some((a) => a.uploading);
  const hookCtx = {
    accountId: chosenAccount,
    accountEmail: () => (opts.accounts.find((a) => a.id === chosenAccount()) || account).email,
    recipients: () => [...to.values(), ...cc.values(), ...bcc.values()],
    body, bodyHost, template, opts,
    attachmentsCount: () => attachments.length,
    markDirty,
  };

  async function saveDraft({ quiet = false } = {}) {
    if (busy()) { toast('Wait for the attachments to finish uploading', 'warn'); return false; }
    for (const hook of composerHooks.beforeDraft) if (await hook(hookCtx, payload()) === false) return false;
    status.textContent = 'Saving draft…';
    try {
      const { id } = await api.post(`/api/mail/accounts/${encodeURIComponent(chosenAccount())}/drafts`, payload());
      draftId = id;
      dirty = false;
      status.textContent = 'Draft saved';
      if (!quiet) toast('Draft saved', 'ok', { timeout: 2000 });
      return true;
    } catch (e) {
      status.textContent = '';
      errorToast(e);
      return false;
    }
  }

  async function send() {
    if (busy()) { toast('Wait for the attachments to finish uploading', 'warn'); return undefined; }
    const p = payload();
    if (!p.to.length && !p.cc.length && !p.bcc.length) { toast('Add at least one recipient', 'warn'); to.input.focus(); return undefined; }
    const bad = [...p.to, ...p.cc, ...p.bcc].find((v) => !looksValid(v));
    if (bad) { toast(`“${bad}” is not an email address`, 'warn'); return undefined; }
    if (!p.subject.trim() && !await confirmDialog('No subject', 'Send this message without a subject?', { confirm: 'Send anyway' })) return undefined;
    for (const hook of composerHooks.beforeSend) if (await hook(hookCtx, p) === false) return undefined;
    status.textContent = 'Sending…';
    try {
      await api.post(`/api/mail/accounts/${encodeURIComponent(chosenAccount())}/send`, p);
    } catch (e) {
      status.textContent = '';
      throw e;
    }
    finished = true;
    toast('Message sent', 'ok', { timeout: 2500 });
    return true;
  }

  const form = h('div.ml-compose', {
    onkeydown: (e) => {
      if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); }
    },
    ondragover: (e) => { if (e.dataTransfer.types.includes('Files')) { e.preventDefault(); form.classList.add('drop'); } },
    ondragleave: () => form.classList.remove('drop'),
    ondrop: (e) => { if (e.dataTransfer.files.length) { e.preventDefault(); form.classList.remove('drop'); upload([...e.dataTransfer.files]); } },
  },
  from && h('div.ml-field', h('label.ml-field-label', 'From'), from),
  to.el, cc.el, bcc.el,
  h('div.ml-field', subject),
  bodyHost,
  attachList,
  h('div.ml-compose-tools',
    h('button.btn.ghost.sm', { type: 'button', onclick: () => fileInput.click() }, icon('upload', 'sm'), 'Attach file'),
    h('button.btn.ghost.sm', {
      type: 'button',
      onclick: async () => {
        const picked = await pickFiles(ctx);
        for (const f of picked || []) attachments.push({ ref: { kind: 'file', id: f.id }, name: f.name, size: f.size });
        if (picked?.length) { markDirty(); drawAttachments(); }
      },
    }, icon('files', 'sm'), 'From Files'),
    composerHooks.tools.map((t) => t(hookCtx)),
    fileInput, h('div.grow'), status));
  drawAttachments();
  const title = { reply: 'Reply', reply_all: 'Reply all', forward: 'Forward', edit: 'Edit draft' }[opts.mode] || 'New message';
  setTimeout(() => {
    if (template && to.values().length) { body.focus(); body.setSelectionRange(0, 0); } else to.input.focus();
  }, 0);
  await modal({
    title,
    wide: true,
    body: form,
    actions: [
      { label: 'Discard', run: () => { finished = true; return false; } },
      { label: 'Save draft', run: async () => ((await saveDraft()) ? true : undefined) },
      { label: 'Send', kind: 'primary', run: send },
    ],
    // Closing a changed message (Escape, the X, a click outside) keeps it as a draft.
    onClose: () => {
      if (!finished && dirty) saveDraft({ quiet: true }).then((ok) => { if (ok) toast('Saved to Drafts', 'ok', { timeout: 2500 }); });
    },
  });
}
