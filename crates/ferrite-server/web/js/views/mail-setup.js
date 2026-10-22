// Adding and editing a mail account. Server settings are looked up in Thunderbird's
// provider database from the email address (when the server can reach it), else guessed;
// saving tests the IMAP login and the SMTP connection before anything is stored.

import { api } from '../api.js';
import { h, modal } from '../ui.js';

const SECURITY = [['tls', 'SSL/TLS'], ['starttls', 'STARTTLS'], ['none', 'None (trusted network only)']];
// Providers that file sent mail in Sent themselves.
const SAVES_SENT = /(^|\.)(gmail\.com|googlemail\.com|office365\.com|outlook\.com)$/i;

function serverFields(prefix, label, values) {
  const host = h('input', { type: 'text', id: `${prefix}-host`, value: values.host || '', placeholder: `${prefix}.example.org`, autocomplete: 'off', spellcheck: 'false' });
  const port = h('input', { type: 'number', id: `${prefix}-port`, value: values.port || '', min: 1, max: 65535 });
  const security = h('select', { id: `${prefix}-security` }, SECURITY.map(([v, l]) => h('option', { value: v, selected: v === values.security }, l)));
  const username = h('input', { type: 'text', id: `${prefix}-user`, value: values.username || '', autocomplete: 'off', spellcheck: 'false' });
  const defaults = prefix === 'imap' ? { tls: 993, starttls: 143, none: 143 } : { tls: 465, starttls: 587, none: 25 };
  security.addEventListener('change', () => {
    if (!port.value || Object.values(defaults).includes(Number(port.value))) port.value = defaults[security.value];
  });
  const el = h('fieldset.ml-server',
    h('legend', label),
    h('div.ml-server-row',
      h('div.field.grow', h('label', { for: host.id }, 'Server'), host),
      h('div.field.ml-port', h('label', { for: port.id }, 'Port'), port)),
    h('div.ml-server-row',
      h('div.field.grow', h('label', { for: security.id }, 'Security'), security),
      h('div.field.grow', h('label', { for: username.id }, 'Username'), username)));
  return {
    el,
    set(v) {
      host.value = v.host; port.value = v.port; security.value = v.security; username.value = v.username;
    },
    value: () => ({ host: host.value.trim(), port: Number(port.value) || undefined, security: security.value, username: username.value.trim() }),
    host,
  };
}

/** Add (account = null) or edit an account. Resolves with the saved account, or null. */
export async function accountDialog(ctx, account) {
  const editing = !!account;
  const name = h('input', { type: 'text', id: 'ml-name', value: account?.name ?? ctx.me.user.display_name ?? '', autocomplete: 'name' });
  const email = h('input', { type: 'email', id: 'ml-email', value: account?.email || '', placeholder: 'you@example.org', autocomplete: 'email', required: true });
  const password = h('input', { type: 'password', id: 'ml-password', autocomplete: 'off', placeholder: editing ? 'Unchanged' : '' });
  const imap = serverFields('imap', 'Incoming mail (IMAP)', editing
    ? { host: account.imap_host, port: account.imap_port, security: account.imap_security, username: account.imap_username }
    : { security: 'tls', port: 993 });
  const smtp = serverFields('smtp', 'Outgoing mail (SMTP)', editing
    ? { host: account.smtp_host, port: account.smtp_port, security: account.smtp_security, username: account.smtp_username }
    : { security: 'starttls', port: 587 });
  const smtpAuth = h('input', { type: 'checkbox', checked: !editing || !!account.smtp_username });
  const smtpPassword = h('input', { type: 'password', id: 'ml-smtp-password', autocomplete: 'off', placeholder: 'Same as the IMAP password' });
  const saveSent = h('input', { type: 'checkbox', checked: editing ? account.save_sent : true });
  const signature = h('textarea', { id: 'ml-signature', rows: 3, placeholder: 'Added below new messages' }, account?.signature || '');
  const hint = h('div.small.muted.ml-autoconfig', { role: 'status' });
  const details = h('details.ml-advanced', { open: editing },
    h('summary', 'Server settings'),
    imap.el, smtp.el,
    h('label.check', smtpAuth, 'The SMTP server requires a login'),
    h('div.field', h('label', { for: smtpPassword.id }, 'SMTP password (if different)'), smtpPassword));
  const err = h('div.error-text', { role: 'alert' });
  let looked = editing ? (account.email || '') : '';
  let manual = editing;
  [imap.host, smtp.host].forEach((i) => i.addEventListener('input', () => { manual = true; }));

  async function lookup() {
    const addr = email.value.trim();
    const domain = addr.split('@')[1];
    if (!domain || addr === looked) return;
    looked = addr;
    saveSent.checked = true;
    hint.textContent = 'Looking up the settings for this provider…';
    let found = null;
    try {
      ({ config: found } = await api.get(`/api/mail/autoconfig?email=${encodeURIComponent(addr)}`));
    } catch (_) { /* fall back to the usual names */ }
    if (manual) { hint.textContent = ''; return; }
    if (found) {
      imap.set(found.imap);
      smtp.set(found.smtp);
      hint.textContent = `Settings for ${found.provider || domain} found.`;
      if (SAVES_SENT.test(found.smtp.host)) saveSent.checked = false;
    } else {
      imap.set({ host: `imap.${domain}`, port: 993, security: 'tls', username: addr });
      smtp.set({ host: `smtp.${domain}`, port: 587, security: 'starttls', username: addr });
      hint.textContent = 'No published settings found; check the server settings below.';
      details.open = true;
    }
  }
  email.addEventListener('change', lookup);
  email.addEventListener('blur', lookup);

  const form = h('form.ml-setup', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
    h('div.field', h('label', { for: name.id }, 'Your name'), name),
    h('div.field', h('label', { for: email.id }, 'Email address'), email),
    h('div.field', h('label', { for: password.id }, 'Password'), password,
      h('div.small.muted', 'Stored encrypted. With two-factor sign-in, use an app password from your provider.')),
    hint,
    details,
    h('label.check', saveSent, 'Save a copy of sent mail in the Sent folder'),
    h('div.small.muted', 'Turn this off for Gmail and Outlook, which file sent mail themselves.'),
    h('div.field', h('label', { for: signature.id }, 'Signature'), signature),
    h('button', { type: 'submit', hidden: true, tabindex: '-1', 'aria-hidden': 'true' }),
    err);
  setTimeout(() => (editing ? name : email).focus(), 0);

  return modal({
    title: editing ? `Edit ${account.email}` : 'Add mail account',
    body: form,
    actions: [
      { label: 'Cancel', value: null },
      {
        label: editing ? 'Save' : 'Connect', kind: 'primary', run: async () => {
          err.textContent = '';
          if (!email.value.trim() || !email.checkValidity()) { err.textContent = 'Enter your email address.'; email.focus(); return undefined; }
          if (!editing && !password.value) { err.textContent = 'Enter your password.'; password.focus(); return undefined; }
          if (!editing && !looked) await lookup();
          const i = imap.value();
          const s = smtp.value();
          if (!i.host || !s.host) { details.open = true; err.textContent = 'Enter the IMAP and SMTP servers.'; return undefined; }
          const body = {
            name: name.value.trim(), email: email.value.trim(),
            imap_host: i.host, imap_port: i.port, imap_security: i.security, imap_username: i.username,
            smtp_host: s.host, smtp_port: s.port, smtp_security: s.security, smtp_username: s.username,
            smtp_auth: smtpAuth.checked, save_sent: saveSent.checked, signature: signature.value,
          };
          if (password.value) body.password = password.value;
          if (smtpPassword.value) body.smtp_password = smtpPassword.value;
          hint.textContent = 'Testing the connection…';
          try {
            const { account: saved } = editing
              ? await api.patch(`/api/mail/accounts/${encodeURIComponent(account.id)}`, body)
              : await api.post('/api/mail/accounts', body);
            return saved;
          } catch (e) {
            hint.textContent = '';
            err.textContent = e.message;
            details.open = true;
            return undefined;
          }
        },
      },
    ],
  });
}
