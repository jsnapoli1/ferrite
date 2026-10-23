// Mail security and privacy: what the reader shows about a message (phishing hints, external
// senders, DKIM, S/MIME signatures and decryption, PGP through Mailvelope), the blocked-content
// banner with "always show images from …", sender avatars (contact photo, favicon or initials;
// fetched by the server, never by the browser), view source, attachments as a ZIP, the
// "Privacy and security" settings (trusted senders, internal addresses, S/MIME certificates,
// decrypted mail), and the composer's security controls: the internal-address check before
// sending, S/MIME sign/encrypt and Mailvelope encryption (through mail-compose.js's hooks).
//
// Mailvelope's client API (window.mailvelope, announced by a "mailvelope" event) renders
// decrypted PGP mail and the encrypting editor in its own sandboxed frames: Ferrite never
// sees PGP plaintext or keys.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, modal, toast, errorToast, confirmDialog, dateTime, spinner, copyText } from '../ui.js';
import { composerHooks } from './mail-compose.js';
import { toggle, pill } from './settings-common.js';

const enc = encodeURIComponent;
let seq = 0;
const nextId = (p) => `${p}-${++seq}`;

// ---- avatars ------------------------------------------------------------------------------------
const avatarCache = new Map(); // email -> Promise<string|null>
let avatarQueue = new Map(); // email -> resolve
let avatarTimer = null;

function flushAvatars() {
  avatarTimer = null;
  const batch = avatarQueue;
  avatarQueue = new Map();
  const emails = [...batch.keys()];
  for (let i = 0; i < emails.length; i += 40) {
    const chunk = emails.slice(i, i + 40);
    api.get(`/api/mail/avatars?emails=${enc(chunk.join(','))}`)
      .then(({ avatars }) => chunk.forEach((e) => batch.get(e)(avatars[e] || null)))
      .catch(() => chunk.forEach((e) => batch.get(e)(null)));
  }
}

function lookupAvatar(email) {
  const key = email.trim().toLowerCase();
  if (!avatarCache.has(key)) {
    avatarCache.set(key, new Promise((resolve) => { avatarQueue.set(key, resolve); }));
    if (!avatarTimer) avatarTimer = setTimeout(flushAvatars, 40);
  }
  return avatarCache.get(key);
}

/** An avatar with initials that turns into the sender's picture when there is one. */
export function senderAvatar(name, email, opts = {}) {
  const el = avatar(name || email || '?', { ...opts, seed: opts.seed || email || name });
  if (email && email.includes('@')) {
    lookupAvatar(email).then((url) => {
      if (!url) return;
      const img = h('img', { src: url, alt: '', decoding: 'async' });
      img.addEventListener('load', () => { el.replaceChildren(img); el.classList.add('has-photo'); });
    });
  }
  return el;
}

// ---- Mailvelope -------------------------------------------------------------------------------------
let mailvelopeReady = null;

/** The Mailvelope API when the extension is active on this site, else null (after a moment). */
export function getMailvelope(wait = 1500) {
  if (window.mailvelope) return Promise.resolve(window.mailvelope);
  if (!mailvelopeReady) {
    mailvelopeReady = new Promise((resolve) => {
      window.addEventListener('mailvelope', () => resolve(window.mailvelope), { once: true });
    });
  }
  return Promise.race([mailvelopeReady, new Promise((r) => setTimeout(() => r(window.mailvelope || null), wait))]);
}

function armoredBlock(text) {
  return h('details.ms-armored', h('summary', 'Show the encrypted text'), h('pre', text));
}

/** Renders a PGP message: through Mailvelope's display container, or a notice. */
async function renderPgp(host, pgp, message) {
  clear(host);
  if (!pgp.armored) {
    host.appendChild(h('div.ms-alert.info', icon('lock', 'sm'), h('div', h('strong', 'PGP signed message'), h('p', 'This message carries an OpenPGP signature. Mailvelope can check it when you open the message in a mail program that supports PGP.'))));
    return;
  }
  const mv = await getMailvelope();
  if (!mv) {
    host.append(
      h('div.ms-alert.info', icon('lock', 'sm'), h('div',
        h('strong', pgp.kind === 'inline_signed' ? 'This message is signed with PGP' : 'This message is encrypted with PGP'),
        h('p', 'Install the Mailvelope browser extension and allow it for this site to read and write PGP mail here. Ferrite never sees your PGP keys.'),
        h('a', { href: 'https://mailvelope.com/', target: '_blank', rel: 'noopener noreferrer' }, 'Get Mailvelope'))),
      armoredBlock(pgp.armored));
    return;
  }
  const id = nextId('ms-pgp');
  const box = h('div.ms-mailvelope', { id });
  host.appendChild(box);
  try {
    const res = await mv.createDisplayContainer(`#${id}`, pgp.armored, undefined, { senderAddress: message.from_addr });
    if (res && res.error) throw new Error(res.error.message || String(res.error));
  } catch (e) {
    clear(host).append(h('div.ms-alert.warn', icon('lock', 'sm'), h('div', h('strong', 'Mailvelope could not open this message'), h('p', e.message || String(e)))), armoredBlock(pgp.armored));
  }
}

// ---- reader -----------------------------------------------------------------------------------------
const encryptedMessages = new Set(); // ids of messages known to be encrypted (S/MIME or PGP)
const reports = new Map(); // message id -> Promise<report|null>

function loadReport(id) {
  if (!reports.has(id)) {
    reports.set(id, api.get(`/api/mail/messages/${enc(id)}/security`).then(({ security }) => {
      if (security?.smime?.encrypted || security?.pgp?.armored) encryptedMessages.add(id);
      return security;
    }).catch(() => { reports.delete(id); return null; }));
  }
  return reports.get(id);
}

const DKIM = {
  pass: ['ok', 'check', (d) => `Verified sender${d.domain ? ` · ${d.domain}` : ''}`, (d) => `The DKIM signature of ${d.domain} is valid: the message was not changed on the way.`],
  fail: ['warn', 'x', () => 'Signature check failed', (d) => `The DKIM signature of ${d.domain || 'the sender'} does not match. The message may have been changed after it was sent${d.detail ? ` (${d.detail})` : ''}.`],
  permerror: ['warn', 'x', () => 'Signature unreadable', (d) => `The DKIM signature could not be checked${d.detail ? `: ${d.detail}` : ''}.`],
  neutral: ['', 'x', () => 'Signature not checked', (d) => `The DKIM signature could not be applied${d.detail ? `: ${d.detail}` : ''}.`],
  temperror: ['', 'clock', () => 'Signature not checked yet', (d) => `The sender's DKIM key could not be fetched right now${d.detail ? ` (${d.detail})` : ''}. It is checked again later.`],
};

function chip(kind, iconName, text, title) {
  return h(`span.pill.ms-chip${kind ? `.${kind}` : ''}`, { title }, icon(iconName, 'sm'), text);
}

function signatureChip(sig) {
  const who = sig.signer?.name || sig.signer?.emails?.[0] || 'unknown signer';
  if (!sig.valid) return chip('danger', 'x', 'Invalid signature', sig.problem || 'The S/MIME signature does not match the message.');
  const status = sig.chain?.status;
  if (status === 'trusted' && sig.matches_sender) return chip('ok', 'check', `Signed by ${who}`, `S/MIME signature by ${sig.signer?.subject}, trusted (${(sig.chain.path || []).join(' → ')}).`);
  if (!sig.matches_sender) return chip('warn', 'key', `Signed by ${who}`, `The signature is valid, but the certificate (${(sig.signer?.emails || []).join(', ')}) is not for the sender's address.`);
  const why = { untrusted: 'its issuer is not trusted', expired: 'the certificate expired', not_yet_valid: 'the certificate is not valid yet', invalid: 'the certificate is not valid for email' }[status] || 'it could not be checked';
  return chip('warn', 'key', `Signed by ${who} (not trusted)`, `The signature is valid, but ${why}${sig.chain?.detail ? `: ${sig.chain.detail}` : ''}.`);
}

function detailsList(r) {
  const rows = [];
  const add = (k, v) => rows.push(h('div.ms-detail', h('span.muted', k), h('span', v)));
  add('Sender', r.sender);
  add('DKIM', r.dkim.status === 'none' ? 'Not signed' : `${r.dkim.status}${r.dkim.domain ? ` · d=${r.dkim.domain}` : ''}${r.dkim.selector ? ` · s=${r.dkim.selector}` : ''}${r.dkim.detail ? ` · ${r.dkim.detail}` : ''}`);
  if (r.smime) {
    add('S/MIME', [r.smime.encrypted ? (r.smime.decrypted ? 'Encrypted, decrypted for you' : 'Encrypted, not decrypted') : null, r.smime.signatures.length ? `${r.smime.signatures.length} signature(s)` : null].filter(Boolean).join(' · ') || '—');
    for (const s of r.smime.signatures) {
      if (s.signer) {
        add('Signer', s.signer.subject);
        add('Issuer', s.signer.issuer);
        add('Valid', `${dateTime(s.signer.not_before)} – ${dateTime(s.signer.not_after)}`);
        if (s.chain) add('Chain', `${s.chain.status}${s.chain.path?.length ? ` · ${s.chain.path.join(' → ')}` : ''}${s.chain.detail ? ` · ${s.chain.detail}` : ''}`);
        if (s.signed_at) add('Signed', dateTime(s.signed_at));
      } else if (s.problem) add('Signature', s.problem);
    }
  }
  if (r.pgp) add('PGP', r.pgp.kind.replace(/_/g, ' '));
  if (r.keywords.length) add('Server flags', r.keywords.join(' '));
  return h('div.ms-details', rows);
}

/**
 * The security banners of a message, filled once the server's report arrives.
 * opts: { onTrusted() — remote images may load; replaceContent(node) — PGP rendering;
 *         onSettings() — open the settings }.
 */
export function messageSecurity(message, opts = {}) {
  const el = h('div.ms-banners', { dataset: { messageId: message.id } });
  const ready = loadReport(message.id).then((r) => {
    if (!r) return null;
    drawReport(el, r, message, opts);
    if (r.trusted_sender && message.remote_content && opts.onTrusted) opts.onTrusted(r.trusted_sender);
    if (r.pgp && opts.pgpHost) renderPgp(opts.pgpHost(), r.pgp, message);
    return r;
  });
  return { el, ready };
}

function drawReport(el, r, message, opts) {
  clear(el);
  const chips = h('div.ms-chips');
  const d = DKIM[r.dkim.status];
  if (d) chips.appendChild(chip(d[0], d[1], d[2](r.dkim), d[3](r.dkim)));
  if (r.smime?.encrypted && r.smime.decrypted) chips.appendChild(chip('ok', 'lock', 'Encrypted', 'This message was encrypted with S/MIME and decrypted for you.'));
  for (const s of r.smime?.signatures || []) chips.appendChild(signatureChip(s));
  if (r.pgp?.armored) chips.appendChild(chip('info', 'lock', r.pgp.kind === 'inline_signed' ? 'PGP signed' : 'PGP encrypted', 'OpenPGP, shown through Mailvelope'));
  if (r.trusted_sender) chips.appendChild(chip('', 'image', 'Images allowed', `Remote images load for ${r.trusted_sender.value} (trusted ${r.trusted_sender.kind}).`));
  const details = h('div.ms-details-host', { hidden: true }, detailsList(r));
  if (chips.children.length || r.smime || r.dkim.status !== 'none') {
    chips.appendChild(h('button.btn.ghost.sm.ms-more', {
      type: 'button', 'aria-expanded': 'false',
      onclick: (e) => { details.hidden = !details.hidden; e.currentTarget.setAttribute('aria-expanded', String(!details.hidden)); e.currentTarget.textContent = details.hidden ? 'Details' : 'Hide details'; },
    }, 'Details'));
    el.append(chips, details);
  }
  if (r.phishing.length) {
    el.appendChild(h('div.ms-alert.warn', { role: 'note' }, icon('junk', 'sm'), h('div.grow',
      h('strong', r.phishing.length === 1 ? 'Take care with this message' : `Take care with this message (${r.phishing.length} reasons)`),
      h('ul', r.phishing.map((f) => h('li', f.message,
        f.links?.length ? h('ul.ms-links', f.links.map(([text, href]) => h('li', h('span', text), ' → ', h('code', href)))) : null))),
      h('p.small.muted', 'These are hints, not proof. When in doubt, contact the sender another way.'))));
  }
  if (r.external) {
    el.appendChild(h('div.ms-alert.info', { role: 'note' }, icon('globe', 'sm'), h('div.grow',
      h('strong', 'External sender'), h('p', `${r.sender} is not one of your internal addresses.`))));
  }
  if (r.smime?.encrypted && !r.smime.decrypted) {
    el.appendChild(h('div.ms-alert.warn', { role: 'note' }, icon('lock', 'sm'), h('div.grow',
      h('strong', 'Encrypted message'), h('p', r.smime.decrypt_error || 'This message could not be decrypted.'),
      opts.onSettings && h('button.btn.sm', { type: 'button', onclick: () => opts.onSettings('certificates') }, icon('key', 'sm'), 'Manage certificates'))));
  }
}

/** The blocked-content banner: show images once, or always for the sender or their domain. */
export function remoteBanner(message, onShow) {
  const addr = message.from_addr || '';
  const domain = addr.includes('@') ? addr.split('@').pop().toLowerCase() : '';
  const trust = async (kind, value) => {
    try {
      await api.post('/api/mail/trusted-senders', { kind, value });
      reports.delete(message.id);
      toast(kind === 'domain' ? `Images from ${value} will always be shown` : `Images from ${value} will always be shown`, 'ok', { timeout: 3000 });
      onShow();
    } catch (e) { errorToast(e); }
  };
  const el = h('div.ml-remote.ms-remote.small', { role: 'note' },
    icon('image', 'sm'), h('span.grow', 'Remote images are hidden to protect your privacy.'),
    h('div.ms-remote-actions',
      h('button.btn.sm', { type: 'button', onclick: () => onShow() }, 'Show images'),
      addr && h('button.btn.sm', { type: 'button', title: `Always show images from ${addr}`, onclick: () => trust('address', addr) }, 'Always from sender'),
      domain && h('button.btn.sm', { type: 'button', title: `Always show images from anyone at ${domain}`, onclick: () => trust('domain', domain) }, `Always from ${domain}`)));
  return el;
}

/** "Download all" for messages with several attachments. */
export function zipLink(message, files) {
  if (files.length < 2) return null;
  const base = (message.subject || '').replace(/[/\\\0"]/g, '').replace(/[\u0000-\u001f]/g, '').slice(0, 80).trim();
  const name = base ? `${base} - attachments.zip` : 'attachments.zip';
  return h('a.btn.ghost.sm.ms-zip', { href: `/api/mail/messages/${enc(message.id)}/attachments.zip`, download: name, title: 'Download every attachment as one ZIP file' }, icon('archive', 'sm'), 'Download all');
}

/** Entries for the reader's "More" menu. */
export function readerMenuItems(message) {
  return [
    { label: 'View source', icon: 'doc', run: () => showSource(message) },
    message.has_attachments && { label: 'Download attachments (.zip)', icon: 'archive', run: () => { location.href = `/api/mail/messages/${enc(message.id)}/attachments.zip`; } },
  ].filter(Boolean);
}

/** The raw message as text (never interpreted as HTML). */
export async function showSource(message) {
  const pre = h('pre.ms-source', { tabindex: '0', 'aria-label': 'Message source' }, spinner());
  const note = h('p.small.muted');
  const open = modal({
    title: 'Message source',
    wide: true,
    body: h('div', note, pre),
    actions: [
      { label: 'Copy', run: () => { copyText(pre.textContent, 'Source copied'); return undefined; } },
      { label: 'Download .eml', run: () => { location.href = `/api/mail/messages/${enc(message.id)}/raw`; return undefined; } },
      { label: 'Close', kind: 'primary', value: true },
    ],
  });
  try {
    const { source, truncated } = await api.get(`/api/mail/messages/${enc(message.id)}/source`);
    pre.textContent = source;
    if (truncated) note.textContent = 'The message is large: only its beginning is shown. Download it to see everything.';
  } catch (e) {
    pre.textContent = e.message;
  }
  return open;
}

// ---- settings -----------------------------------------------------------------------------------
function listEditor(endpoint, { placeholder, empty, label }) {
  const list = h('div.list.panel.ms-list', { role: 'list', 'aria-label': label });
  const input = h('input', { type: 'text', placeholder, 'aria-label': `Add to ${label}` });
  async function load() {
    try {
      const { entries } = await api.get(endpoint);
      clear(list);
      if (!entries.length) list.appendChild(h('div.empty.small', empty));
      for (const e of entries) {
        list.appendChild(h('div.list-item', { role: 'listitem' },
          icon(e.kind === 'domain' ? 'globe' : 'user', 'sm'), h('span.grow.ellipsis', e.kind === 'domain' ? `Anyone at ${e.value}` : e.value),
          h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': `Remove ${e.value}`, title: 'Remove', onclick: async () => { try { await api.del(`${endpoint}/${enc(e.id)}`); load(); } catch (err) { errorToast(err); } } }, icon('trash', 'sm'))));
      }
    } catch (e) { clear(list).appendChild(h('div.empty.small', e.message)); }
  }
  async function add() {
    const value = input.value.trim();
    if (!value) return;
    try {
      await api.post(endpoint, { value });
      input.value = '';
      load();
    } catch (e) { errorToast(e); }
  }
  input.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); add(); } });
  load();
  return h('div.ms-list-editor', h('div.ms-add', input, h('button.btn', { type: 'button', onclick: add }, icon('plus', 'sm'), 'Add')), list);
}

function readFile(file, asText) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(asText ? r.result : r.result.split(',').pop());
    r.onerror = () => reject(new Error(`${file.name} could not be read`));
    if (asText) r.readAsText(file); else r.readAsDataURL(file);
  });
}

function certificatesPane(accounts) {
  const list = h('div.ms-certs');
  const idHost = h('div.ms-identities');
  let certs = [];
  const kindLabel = { own: 'Yours', contact: 'Contact', authority: 'Trusted issuer' };
  async function load() {
    try {
      ({ certificates: certs } = await api.get('/api/mail/smime/certificates'));
      const { identities } = await api.get('/api/mail/smime/identities');
      drawList();
      drawIdentities(identities);
    } catch (e) { clear(list).appendChild(h('div.empty.small', e.message)); }
  }
  function drawList() {
    clear(list);
    if (!certs.length) { list.appendChild(h('div.empty.small', 'No certificates yet. Import yours (with its private key) to sign and decrypt; certificates of people who send you signed mail are collected automatically.')); return; }
    for (const c of certs) {
      const status = c.expired ? pill('Expired', 'danger') : c.expires_soon ? pill(`Expires ${new Date(c.not_after).toLocaleDateString()}`, 'warn') : null;
      list.appendChild(h('div.list-item.ms-cert', { dataset: { id: c.id } },
        icon(c.kind === 'own' ? 'key' : c.kind === 'authority' ? 'lock' : 'user', 'sm'),
        h('div.grow',
          h('div.ms-cert-name', h('strong', c.name || c.emails[0] || c.subject), pill(kindLabel[c.kind] || c.kind, c.kind === 'own' ? 'info' : ''), status),
          h('div.small.muted.ellipsis', { title: c.subject }, c.emails.join(', ') || c.subject),
          h('div.small.muted.ms-cert-meta', { title: c.issuer }, `Issued by ${c.issuer} · valid until ${new Date(c.not_after).toLocaleDateString()}${c.source === 'collected' ? ' · from signed mail' : ''}`)),
        h('a.btn.ghost.icon.sm', { href: `/api/mail/smime/certificates/${enc(c.id)}/pem`, download: '', title: 'Download the certificate', 'aria-label': `Download the certificate of ${c.emails[0] || c.subject}` }, icon('download', 'sm')),
        h('button.btn.ghost.icon.sm.danger', {
          type: 'button', title: 'Delete', 'aria-label': `Delete the certificate of ${c.emails[0] || c.subject}`,
          onclick: async () => {
            const msg = c.kind === 'own' ? 'Delete this certificate and its private key? Mail encrypted to it can no longer be read here.' : 'Delete this certificate?';
            if (!await confirmDialog('Delete certificate', msg, { confirm: 'Delete', danger: true })) return;
            try { await api.del(`/api/mail/smime/certificates/${enc(c.id)}`); load(); } catch (e) { errorToast(e); }
          },
        }, icon('trash', 'sm'))));
    }
  }
  function drawIdentities(identities) {
    clear(idHost);
    const own = certs.filter((c) => c.kind === 'own' && c.has_key);
    if (!accounts.length) return;
    idHost.appendChild(h('h3', 'Certificates for your accounts'));
    for (const a of accounts) {
      const cur = identities.find((i) => i.account_id === a.id && i.email === a.email.toLowerCase());
      const usable = own.filter((c) => c.emails.includes(a.email.toLowerCase()));
      const select = h('select', { 'aria-label': `Certificate for ${a.email}` },
        h('option', { value: '' }, usable.length ? 'None' : 'No certificate for this address'),
        usable.map((c) => h('option', { value: c.id, selected: cur?.cert_id === c.id }, `${c.name || c.emails[0]} · until ${new Date(c.not_after).toLocaleDateString()}`)));
      const sign = h('input', { type: 'checkbox', checked: !!cur?.sign_default });
      const encrypt = h('input', { type: 'checkbox', checked: !!cur?.encrypt_default });
      const save = async () => {
        try {
          await api.put('/api/mail/smime/identities', { account_id: a.id, email: a.email, cert_id: select.value || null, sign_default: sign.checked, encrypt_default: encrypt.checked });
          toast('Saved', 'ok', { timeout: 1500 });
        } catch (e) { errorToast(e); }
      };
      [select, sign, encrypt].forEach((x) => x.addEventListener('change', save));
      idHost.appendChild(h('div.ms-identity',
        h('div.ms-identity-head', h('strong.ellipsis', a.email), select),
        h('div.ms-identity-opts', h('label.check', sign, 'Sign by default'), h('label.check', encrypt, 'Encrypt by default'))));
    }
  }
  // Import
  const mode = { value: 'pkcs12' };
  const p12 = h('input', { type: 'file', accept: '.p12,.pfx,application/x-pkcs12', 'aria-label': 'PKCS#12 file' });
  const certFile = h('input', { type: 'file', accept: '.pem,.crt,.cer,.der', 'aria-label': 'Certificate file' });
  const keyFile = h('input', { type: 'file', accept: '.pem,.key', 'aria-label': 'Private key file (optional)' });
  const password = h('input', { type: 'password', autocomplete: 'off', placeholder: 'Password', 'aria-label': 'Password of the file' });
  const p12Row = h('div.ms-import-row', h('label.small', 'PKCS#12 file (.p12, .pfx)'), p12);
  const pemRows = h('div.ms-import-pem', { hidden: true },
    h('div.ms-import-row', h('label.small', 'Certificate (PEM or DER)'), certFile),
    h('div.ms-import-row', h('label.small', 'Private key (PEM, optional: without it the certificate is someone else’s, for encrypting to them)'), keyFile));
  const tabs = h('div.btn-group.ms-import-mode', { role: 'radiogroup', 'aria-label': 'File type' },
    ['pkcs12', 'pem'].map((m) => h(`button.btn.sm${m === 'pkcs12' ? '.active' : ''}`, {
      type: 'button', role: 'radio', 'aria-checked': String(m === 'pkcs12'), dataset: { mode: m },
      onclick: (e) => {
        mode.value = m;
        tabs.querySelectorAll('button').forEach((b) => { b.classList.toggle('active', b.dataset.mode === m); b.setAttribute('aria-checked', String(b.dataset.mode === m)); });
        p12Row.hidden = m !== 'pkcs12';
        pemRows.hidden = m !== 'pem';
        e.currentTarget.focus();
      },
    }, m === 'pkcs12' ? 'PKCS#12' : 'PEM / DER')));
  const importBtn = h('button.btn.primary', {
    type: 'button',
    onclick: async () => {
      try {
        let body;
        if (mode.value === 'pkcs12') {
          if (!p12.files[0]) { toast('Choose a .p12 or .pfx file', 'warn'); return; }
          body = { pkcs12: await readFile(p12.files[0], false), password: password.value };
        } else {
          if (!certFile.files[0]) { toast('Choose a certificate file', 'warn'); return; }
          const isPem = async (f) => /-----BEGIN/.test(await f.slice(0, 64).text());
          body = {
            certificate: await readFile(certFile.files[0], await isPem(certFile.files[0])),
            private_key: keyFile.files[0] ? await readFile(keyFile.files[0], true) : null,
            password: password.value,
          };
        }
        importBtn.disabled = true;
        const { certificate } = await api.post('/api/mail/smime/certificates', body);
        toast(`Imported ${certificate.name || certificate.emails[0] || 'the certificate'}`, 'ok');
        p12.value = ''; certFile.value = ''; keyFile.value = ''; password.value = '';
        load();
      } catch (e) { errorToast(e); } finally { importBtn.disabled = false; }
    },
  }, icon('upload', 'sm'), 'Import');
  load();
  return h('div.ms-pane',
    h('p.small.muted', 'S/MIME signs and encrypts mail with certificates. Private keys are stored encrypted on this server.'),
    list,
    h('h3', 'Import a certificate'),
    h('div.ms-import', tabs, p12Row, pemRows, h('div.ms-import-row', h('label.small', 'Password'), password), importBtn),
    idHost);
}

/** Mail's "Privacy and security" settings. tab: images | internal | certificates | encrypted */
export async function openSecuritySettings(ctx, { tab = 'images', accounts = [] } = {}) {
  let settings = { internal_enabled: false, index_decrypted: false, assistant_decrypted: false };
  try { ({ settings } = await api.get('/api/mail/security/settings')); } catch (e) { errorToast(e); return; }
  const patch = async (p) => {
    try { ({ settings } = await api.patch('/api/mail/security/settings', p)); } catch (e) { errorToast(e); }
  };
  const panes = {
    images: () => h('div.ms-pane',
      h('p.small.muted', 'Remote images can tell senders when and where you read their mail. They stay hidden unless you show them; when you do, they load through Ferrite, so senders never see your address. Images from these senders and domains are shown right away.'),
      listEditor('/api/mail/trusted-senders', { placeholder: 'ada@example.org or example.org', empty: 'No trusted senders yet. Use “Always from sender” on a message, or add one here.', label: 'trusted senders' })),
    internal: () => {
      const t = toggle(settings.internal_enabled, 'Warn about external senders and recipients', (on) => patch({ internal_enabled: on }));
      return h('div.ms-pane',
        h('p.small.muted', 'Messages from outside these addresses and domains are marked as external, and you are asked before sending to anyone outside them.'),
        h('div.ms-toggle', t.el),
        listEditor('/api/mail/internal-addresses', { placeholder: 'example.org or colleague@example.org', empty: 'No internal addresses yet.', label: 'internal addresses' }));
    },
    certificates: () => certificatesPane(accounts),
    encrypted: () => {
      const idx = toggle(settings.index_decrypted, 'Include decrypted messages in Mail search', (on) => patch({ index_decrypted: on }));
      const ai = toggle(settings.assistant_decrypted, 'Let the assistant read decrypted messages', (on) => patch({ assistant_decrypted: on }));
      const mv = h('p.small.ms-mailvelope-status', spinner());
      getMailvelope(800).then((m) => {
        clear(mv).append(m
          ? h('span', icon('check', 'sm'), ' Mailvelope is active: PGP mail opens here, and the composer can encrypt with it.')
          : h('span', 'PGP: install the ', h('a', { href: 'https://mailvelope.com/', target: '_blank', rel: 'noopener noreferrer' }, 'Mailvelope'), ' browser extension and allow it for this site to read and write PGP mail.'));
      });
      return h('div.ms-pane',
        h('p.small.muted', 'Decrypted S/MIME mail is shown to you, but kept out of previews, global search and the assistant unless you allow it here. Search only covers it in Mail.'),
        h('div.ms-toggle', idx.el), h('div.ms-toggle', ai.el),
        h('h3', 'PGP'), mv);
    },
  };
  const names = { images: 'Images', internal: 'Internal addresses', certificates: 'Certificates', encrypted: 'Encrypted mail' };
  const host = h('div.ms-settings-body');
  const tabBar = h('div.tabs.ms-tabs', { role: 'tablist' });
  const show = (t) => {
    tabBar.querySelectorAll('button').forEach((b) => { b.classList.toggle('active', b.dataset.tab === t); b.setAttribute('aria-selected', String(b.dataset.tab === t)); });
    clear(host).appendChild(panes[t]());
  };
  for (const t of Object.keys(panes)) tabBar.appendChild(h('button', { type: 'button', role: 'tab', dataset: { tab: t }, onclick: () => show(t) }, names[t]));
  show(panes[tab] ? tab : 'images');
  // Changes made elsewhere (another tab, the assistant) are reflected when the list refreshes.
  const off = live.on('mail.security', () => { reports.clear(); });
  await modal({ title: 'Privacy and security', wide: true, body: h('div.ms-settings', tabBar, host), actions: [{ label: 'Done', kind: 'primary', value: true }], onClose: off });
  reports.clear();
}

// ---- composer -----------------------------------------------------------------------------------
let identityCache = null;
function identitiesFor() {
  if (!identityCache) {
    identityCache = Promise.all([api.get('/api/mail/smime/identities'), api.get('/api/mail/smime/certificates')])
      .then(([{ identities }, { certificates }]) => ({ identities, certificates }))
      .catch(() => ({ identities: [], certificates: [] }));
    setTimeout(() => { identityCache = null; }, 30000);
  }
  return identityCache;
}

const composerState = new WeakMap(); // hook ctx -> { sign, encrypt, editor }

function securityTools(c) {
  const st = { sign: false, encrypt: false, editor: null, pgp: false };
  composerState.set(c, st);
  const wrap = h('div.ms-compose-tools');
  const signBtn = h('button.btn.ghost.sm.ms-toggle-btn', { type: 'button', hidden: true, 'aria-pressed': 'false', title: 'Sign with S/MIME' }, icon('key', 'sm'), 'Sign');
  const encBtn = h('button.btn.ghost.sm.ms-toggle-btn', { type: 'button', hidden: true, 'aria-pressed': 'false', title: 'Encrypt with S/MIME' }, icon('lock', 'sm'), 'Encrypt');
  const pgpBtn = h('button.btn.ghost.sm.ms-toggle-btn', { type: 'button', hidden: true, 'aria-pressed': 'false', title: 'Encrypt with Mailvelope (PGP)' }, icon('lock', 'sm'), 'PGP');
  const set = (btn, on) => { btn.classList.toggle('active', on); btn.setAttribute('aria-pressed', String(on)); };
  signBtn.addEventListener('click', () => { st.sign = !st.sign; set(signBtn, st.sign); });
  encBtn.addEventListener('click', () => {
    st.encrypt = !st.encrypt;
    set(encBtn, st.encrypt);
    if (st.encrypt && st.pgp) togglePgp(false);
  });
  async function togglePgp(on) {
    const mv = await getMailvelope(0);
    if (on && !mv) return;
    st.pgp = on;
    set(pgpBtn, on);
    if (on) {
      if (st.encrypt) { st.encrypt = false; set(encBtn, false); }
      const id = nextId('ms-editor');
      const box = h('div.ms-mailvelope.ms-editor', { id });
      c.body.hidden = true;
      c.bodyHost.appendChild(box);
      try {
        const quoted = c.template?.text && /-----BEGIN PGP MESSAGE-----/.test(c.template.text) ? c.template.text : undefined;
        st.editor = await mv.createEditorContainer(`#${id}`, undefined, { quotedMail: quoted });
      } catch (e) {
        errorToast(e);
        togglePgp(false);
      }
    } else {
      st.editor = null;
      c.bodyHost.querySelectorAll('.ms-editor').forEach((x) => x.remove());
      c.body.hidden = false;
    }
  }
  pgpBtn.addEventListener('click', () => togglePgp(!st.pgp));
  // S/MIME controls appear when the sending address has a certificate (or recipients might).
  identitiesFor().then(({ identities, certificates }) => {
    const email = c.accountEmail().toLowerCase();
    const mine = certificates.filter((x) => x.kind === 'own' && x.has_key && !x.expired && x.emails.includes(email));
    if (!mine.length) return;
    signBtn.hidden = false;
    encBtn.hidden = false;
    const id = identities.find((i) => i.account_id === c.accountId() && i.email === email);
    const replyToEncrypted = c.template?.in_reply_to && encryptedMessages.has(c.template.in_reply_to);
    st.sign = !!id?.sign_default;
    st.encrypt = !!id?.encrypt_default || !!replyToEncrypted;
    set(signBtn, st.sign);
    set(encBtn, st.encrypt);
  });
  getMailvelope(1200).then((mv) => {
    if (!mv) return;
    pgpBtn.hidden = false;
    const replyToPgp = c.template?.text && /-----BEGIN PGP MESSAGE-----/.test(c.template.text);
    if (replyToPgp) togglePgp(true);
  });
  wrap.append(signBtn, encBtn, pgpBtn);
  return wrap;
}

const emailOf = (v) => ((v.match(/<([^>]+)>\s*$/) || [null, v])[1] || '').trim().toLowerCase();

async function beforeSend(c, p) {
  const st = composerState.get(c) || {};
  const recipients = [...p.to, ...p.cc, ...p.bcc];
  let check = { external: [], smime: [] };
  try {
    check = await api.post('/api/mail/security/check-recipients', { recipients });
  } catch (e) { errorToast(e); return false; }
  if (check.external.length) {
    const ok = await confirmDialog('Send outside your organization?',
      `${check.external.length === 1 ? 'This recipient is' : 'These recipients are'} not in your internal addresses: ${check.external.join(', ')}. Send anyway?`,
      { confirm: 'Send anyway' });
    if (!ok) return false;
  }
  if (st.encrypt) {
    const missing = check.smime.filter((r) => !r.has_certificate).map((r) => r.email);
    if (missing.length) {
      toast(`No S/MIME certificate for ${missing.join(', ')}. They need to send you signed mail first (or import their certificate), or send without encryption.`, 'error', { timeout: 9000 });
      return false;
    }
  }
  p.security = { smime_sign: !!st.sign, smime_encrypt: !!st.encrypt, pgp_mime: false };
  if (st.pgp && st.editor) {
    if (c.attachmentsCount()) { toast('Attachments cannot be encrypted with Mailvelope. Remove them or send them separately.', 'error', { timeout: 7000 }); return false; }
    const mv = await getMailvelope(0);
    const emails = recipients.map(emailOf).filter(Boolean);
    try {
      const keyring = await mv.getKeyring();
      const valid = await keyring.validKeyForAddress(emails);
      const missing = emails.filter((e) => !valid[e]);
      if (missing.length) { toast(`Mailvelope has no public key for ${missing.join(', ')}. Import their key in Mailvelope first.`, 'error', { timeout: 9000 }); return false; }
      p.text = await st.editor.encrypt(emails);
    } catch (e) { errorToast(e); return false; }
    p.security = { smime_sign: false, smime_encrypt: false, pgp_mime: true };
  }
  return true;
}

async function beforeDraft(c) {
  const st = composerState.get(c);
  if (st?.pgp) { toast('Drafts are not saved while you write with Mailvelope.', 'warn'); return false; }
  return true;
}

composerHooks.tools.push(securityTools);
composerHooks.beforeSend.push(beforeSend);
composerHooks.beforeDraft.push(beforeDraft);
live.on('mail.security', () => { identityCache = null; reports.clear(); });
