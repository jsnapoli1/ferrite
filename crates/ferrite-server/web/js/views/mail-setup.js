// Adding and editing a mail account. Server settings are looked up from the email address
// (Thunderbird's provider database, the domain's own autoconfig or autodiscover, then its MX
// records) when the server can reach them, else guessed; saving tests the IMAP login and the
// SMTP connection before anything is stored. Google and Microsoft accounts can sign in through
// the provider (OAuth) when the administrator set that up. Shared (delegated) and provisioned
// accounts only let the sender name, signature and Sent copy be changed here.

import { api } from '../api.js';
import { h, icon, modal } from '../ui.js';
import { oauthProviders, startOAuth, accountNotice, PROVIDER_LABEL } from './mail-accounts.js';

const SECURITY = [['tls', 'SSL/TLS'], ['starttls', 'STARTTLS'], ['none', 'None (trusted network only)']];
// Providers that file sent mail in Sent themselves.
const SAVES_SENT = /(^|\.)(gmail\.com|googlemail\.com|office365\.com|outlook\.com)$/i;
// Server names that belong to an OAuth provider.
const OAUTH_HOSTS = { google: /(^|\.)(gmail\.com|googlemail\.com)$/i, microsoft: /(^|\.)(office365\.com|outlook\.com)$/i };
const SOURCES = { ispdb: 'the provider database', autoconfig: 'the provider', autodiscover: 'the provider (autodiscover)', mx: 'its mail servers (MX records)' };

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
  const oauth = editing && account.auth_method === 'xoauth2';
  // Delegates, provisioned and OAuth accounts: only the sender name, signature and Sent copy.
  const limited = editing && (!account.permissions?.authenticate || oauth);
  const providers = editing ? [] : await oauthProviders();
  const name = h('input', { type: 'text', id: 'ml-name', value: account?.name ?? ctx.me.user.display_name ?? '', autocomplete: 'name' });
  const email = h('input', { type: 'email', id: 'ml-email', value: account?.email || '', placeholder: 'you@example.org', autocomplete: 'email', required: true, disabled: limited });
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
  const testResult = h('div.ml-test.small', { role: 'status', 'aria-live': 'polite' });
  const testBtn = h('button.btn.sm', { type: 'button', onclick: () => testConnection() }, icon('plug', 'sm'), 'Test servers');
  const details = h('details.ml-advanced', { open: editing },
    h('summary', 'Server settings'),
    imap.el, smtp.el,
    h('label.check', smtpAuth, 'The SMTP server requires a login'),
    h('div.field', h('label', { for: smtpPassword.id }, 'SMTP password (if different)'), smtpPassword),
    h('div.ml-test-row', testBtn, testResult));
  const err = h('div.error-text', { role: 'alert' });
  let looked = editing ? (account.email || '') : '';
  let manual = editing;
  [imap.host, smtp.host].forEach((i) => i.addEventListener('input', () => { manual = true; }));

  // "Sign in with Google" / "Microsoft": OAuth instead of a password.
  const oauthBox = providers.length ? h('div.ml-oauth',
    h('div.ml-oauth-buttons', providers.map((p) => h('button.btn.ml-oauth-btn', {
      type: 'button', dataset: { provider: p.id },
      onclick: async (e) => {
        err.textContent = '';
        if (!email.value.trim() || !email.checkValidity()) { err.textContent = `Enter your ${p.label} email address first.`; email.focus(); return; }
        e.currentTarget.disabled = true;
        try { await startOAuth(p.id, { email: email.value.trim(), name: name.value.trim() }); } catch (ex) { err.textContent = ex.message; e.currentTarget.disabled = false; }
      },
    }, icon('key', 'sm'), `Sign in with ${p.label}`))),
    h('div.small.muted', 'Uses the provider’s own sign-in page; Ferrite stores an encrypted access grant instead of your password.')) : null;

  function suggestOAuth(host) {
    const p = providers.find((x) => OAUTH_HOSTS[x.id]?.test(host));
    return p ? ` You can also use “Sign in with ${p.label}” above.` : '';
  }

  async function lookup() {
    const addr = email.value.trim();
    const domain = addr.split('@')[1];
    if (!domain || addr === looked) return;
    looked = addr;
    saveSent.checked = true;
    hint.textContent = 'Looking up the settings for this provider…';
    let found = null;
    let source = null;
    try {
      ({ config: found, source } = await api.get(`/api/mail/autoconfig?email=${encodeURIComponent(addr)}`));
    } catch (_) { /* fall back to the usual names */ }
    if (manual) { hint.textContent = ''; return; }
    if (found) {
      imap.set(found.imap);
      smtp.set(found.smtp);
      hint.textContent = `Settings for ${found.provider || domain} found (from ${SOURCES[source] || 'the provider'}).${suggestOAuth(found.imap.host)}`;
      if (SAVES_SENT.test(found.smtp.host)) saveSent.checked = false;
    } else {
      imap.set({ host: `imap.${domain}`, port: 993, security: 'tls', username: addr });
      smtp.set({ host: `smtp.${domain}`, port: 587, security: 'starttls', username: addr });
      hint.textContent = 'No published settings found; check the server settings below.';
      details.open = true;
    }
  }
  if (!limited) {
    email.addEventListener('change', lookup);
    email.addEventListener('blur', lookup);
  }

  async function testConnection() {
    const i = imap.value();
    const s = smtp.value();
    if (!i.host || !s.host) { testResult.textContent = 'Enter the IMAP and SMTP servers first.'; return; }
    testBtn.disabled = true;
    testResult.textContent = 'Testing…';
    try {
      const { result } = await api.post('/api/mail/autoconfig/test', {
        imap_host: i.host, imap_port: i.port, imap_security: i.security, imap_username: i.username || email.value.trim(),
        smtp_host: s.host, smtp_port: s.port, smtp_security: s.security, smtp_username: smtpAuth.checked ? s.username : '',
        password: password.value, smtp_password: smtpPassword.value,
      });
      const line = (label, r) => h(`div.ml-test-line${r.ok ? '.ok' : '.bad'}`, icon(r.ok ? 'check' : 'x', 'sm'),
        h('span', `${label}: ${r.ok ? (r.login ? 'signed in' : 'reachable (enter the password to test the login)') : r.error}`));
      testResult.replaceChildren(line('IMAP', result.imap), line('SMTP', result.smtp));
    } catch (e) {
      testResult.textContent = e.message;
    } finally {
      testBtn.disabled = false;
    }
  }

  const summary = editing ? h('div.ml-server-summary.small.muted',
    h('div', `Incoming: ${account.imap_host}:${account.imap_port} (${account.imap_security})`),
    h('div', `Outgoing: ${account.smtp_host}:${account.smtp_port} (${account.smtp_security})`)) : null;
  const reconnect = oauth && account.permissions?.authenticate ? h('div.ml-oauth',
    h('div.small', `Signs in with ${PROVIDER_LABEL[account.oauth_provider] || 'OAuth'}.`),
    h('button.btn.sm', {
      type: 'button',
      onclick: async (e) => {
        e.currentTarget.disabled = true;
        try { await startOAuth(account.oauth_provider, { email: account.email, accountId: account.id }); } catch (ex) { err.textContent = ex.message; e.currentTarget.disabled = false; }
      },
    }, icon('refresh', 'sm'), `Reconnect with ${PROVIDER_LABEL[account.oauth_provider] || 'the provider'}`)) : null;

  const form = h('form.ml-setup', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
    editing ? accountNotice(account) : null,
    h('div.field', h('label', { for: name.id }, 'Your name'), name),
    h('div.field', h('label', { for: email.id }, 'Email address'), email),
    oauthBox,
    limited ? null : h('div.field', h('label', { for: password.id }, 'Password'), password,
      h('div.small.muted', 'Stored encrypted. With two-factor sign-in, use an app password from your provider.')),
    limited ? null : hint,
    limited ? [summary, reconnect] : details,
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
          if (limited) {
            try {
              const { account: saved } = await api.patch(`/api/mail/accounts/${encodeURIComponent(account.id)}`, {
                name: name.value.trim(), save_sent: saveSent.checked, signature: signature.value,
              });
              return saved;
            } catch (e) { err.textContent = e.message; return undefined; }
          }
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
