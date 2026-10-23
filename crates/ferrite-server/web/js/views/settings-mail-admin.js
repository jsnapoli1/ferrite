// Settings → Mail admin (administrators): who may add accounts, connection security, Google
// and Microsoft sign-in, spam reporting, sending limits and anti-abuse alerts, background
// sync, AI and privacy, defaults for users, provisioning templates and shared accounts.

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, modal, confirmDialog, relTime, spinner } from '../ui.js';
import { card, field, toggle, codeBlock, pill } from './settings-common.js';

const MB = 1024 * 1024;
const SECURITY = [['tls', 'SSL/TLS'], ['starttls', 'STARTTLS'], ['none', 'None']];

const select = (options, value, attrs = {}) => h('select', attrs, options.map(([v, l]) => h('option', { value: v, selected: v === value }, l)));
const num = (value, attrs = {}) => h('input', { type: 'number', min: '0', value: value ?? '', ...attrs });
const text = (value, attrs = {}) => h('input', { type: 'text', value: value ?? '', autocomplete: 'off', spellcheck: 'false', ...attrs });
const saveRow = (onclick, label = 'Save') => {
  const btn = h('button.btn.primary', { type: 'button' }, label);
  btn.addEventListener('click', async () => {
    btn.disabled = true;
    try { await onclick(); } catch (e) { errorToast(e); } finally { btn.disabled = false; }
  });
  return h('div.set-actions', btn);
};

export async function mailAdminTab(host) {
  let data = await api.get('/api/admin/mail/settings');
  let s = data.settings;
  async function patch(body, what = 'Saved') {
    data = await api.patch('/api/admin/mail/settings', body);
    s = data.settings;
    toast(what, 'ok', { timeout: 2000 });
  }

  // ---- accounts and connections ----
  const allowAdd = toggle(s.allow_user_accounts, 'Users can add their own mail accounts', (v) => patch({ allow_user_accounts: v }).catch(errorToast));
  const verifyTls = toggle(s.verify_tls, 'Verify the mail servers’ TLS certificates', async (v, input) => {
    if (!v && !(await confirmDialog('Stop verifying certificates?', 'Ferrite will then accept any certificate from mail servers, so someone on the network path could read the mail and passwords. Only do this for servers with self-signed certificates on a network you trust.', { confirm: 'Stop verifying', danger: true }))) {
      input.checked = true;
      return;
    }
    patch({ verify_tls: v }).catch(errorToast);
  });
  const allowLocal = toggle(s.allow_local_servers, 'Allow mail servers on local and private networks', (v) => patch({ allow_local_servers: v }).catch(errorToast));

  // ---- OAuth ----
  function providerForm(id, label, c, secretSet) {
    const clientId = text(c.client_id, { 'aria-label': `${label} client ID` });
    const secret = h('input', { type: 'password', autocomplete: 'new-password', placeholder: secretSet ? 'Stored (leave empty to keep)' : '' });
    const tenant = id === 'microsoft' ? text(c.tenant, { placeholder: 'common' }) : null;
    const authUrl = text(c.auth_url, { placeholder: 'Provider default' });
    const tokenUrl = text(c.token_url, { placeholder: 'Provider default' });
    const imapHost = text(c.imap_host, { placeholder: id === 'google' ? 'imap.gmail.com' : 'outlook.office365.com' });
    const imapPort = num(c.imap_port || '', { max: '65535', placeholder: '993' });
    const imapSec = select([['', 'Default'], ...SECURITY], c.imap_security || '');
    const smtpHost = text(c.smtp_host, { placeholder: id === 'google' ? 'smtp.gmail.com' : 'smtp.office365.com' });
    const smtpPort = num(c.smtp_port || '', { max: '65535', placeholder: id === 'google' ? '465' : '587' });
    const smtpSec = select([['', 'Default'], ...SECURITY], c.smtp_security || '');
    const el = h('fieldset.set-mail-provider', { dataset: { provider: id } },
      h('legend', label, c.client_id ? pill('configured', 'ok') : null),
      h('div.set-grid', field('Client ID', clientId), field('Client secret', secret)),
      tenant && field('Tenant', tenant, 'Your directory (tenant) ID or domain; “common” allows any work or personal Microsoft account.'),
      h('details.set-mail-advanced', h('summary', 'Endpoints and servers'),
        h('p.small.muted', 'Leave empty for the provider’s public endpoints and servers. Change them only for national clouds or a test provider.'),
        h('div.set-grid', field('Authorization URL', authUrl), field('Token URL', tokenUrl)),
        h('div.set-grid', field('IMAP server', imapHost), h('div.set-grid', field('Port', imapPort), field('Security', imapSec))),
        h('div.set-grid', field('SMTP server', smtpHost), h('div.set-grid', field('Port', smtpPort), field('Security', smtpSec)))),
      saveRow(async () => {
        const body = {
          client_id: clientId.value.trim(), auth_url: authUrl.value.trim(), token_url: tokenUrl.value.trim(),
          imap_host: imapHost.value.trim(), imap_port: Number(imapPort.value) || 0, imap_security: imapSec.value,
          smtp_host: smtpHost.value.trim(), smtp_port: Number(smtpPort.value) || 0, smtp_security: smtpSec.value,
        };
        if (secret.value) body.client_secret = secret.value;
        if (tenant) body.tenant = tenant.value.trim();
        await patch({ [id]: body }, `${label} sign-in saved`);
        secret.value = '';
        secret.placeholder = 'Stored (leave empty to keep)';
      }, `Save ${label}`));
    return el;
  }

  // ---- spam, limits, abuse ----
  const spam = h('input', { type: 'email', value: s.spam_report_address, placeholder: 'spam@example.org' });
  const ham = h('input', { type: 'email', value: s.ham_report_address, placeholder: 'ham@example.org' });
  const attach = num(s.attachment_size_limit ? +(s.attachment_size_limit / MB).toFixed(1) : '', { step: '0.5', placeholder: 'No limit' });
  const abuseOn = h('input', { type: 'checkbox', checked: s.abuse.enabled });
  const maxRcpt = num(s.abuse.max_recipients || '', { placeholder: 'Off' });
  const per15 = num(s.abuse.per_15m || '', { placeholder: 'Off' });
  const perHour = num(s.abuse.per_hour || '', { placeholder: 'Off' });
  const perDay = num(s.abuse.per_day || '', { placeholder: 'Off' });
  const alertsHost = h('div', spinner());
  async function loadAlerts() {
    try {
      const { alerts } = await api.get('/api/admin/mail/abuse');
      const rule = { recipients: 'Recipients in one message', '15m': 'Messages in 15 minutes', '1h': 'Messages in an hour', '1d': 'Messages in a day' };
      clear(alertsHost).appendChild(alerts.length
        ? h('div.list.panel.set-mail-alerts', alerts.slice(0, 20).map((a) => h('div.list-item',
          h('span.set-provider-icon', icon('bell', 'sm')),
          h('div.grow', h('strong', `@${a.username}`), h('div.small.muted', `${rule[a.rule] || a.rule}: ${a.observed} (alert above ${a.threshold})`)),
          h('span.small.muted', relTime(a.created_at)))))
        : h('p.small.muted.set-empty', 'No alerts.'));
    } catch (e) { clear(alertsHost).appendChild(h('p.error-text', e.message)); }
  }

  // ---- sync, AI, defaults ----
  const interval = num(Math.round(s.sync_interval / 60), { min: '1', max: '1440' });
  const syncReport = h('span.small.muted', { role: 'status' });
  const llm = toggle(s.llm_processing, 'Allow AI features to process mail (summaries, smart replies, …)', (v) => patch({ llm_processing: v }).catch(errorToast));
  const importance = toggle(s.importance_default, 'Sort mail by importance (priority inbox) unless a user turns it off', (v) => patch({ importance_default: v }).catch(errorToast));
  const gravatar = toggle(s.gravatar, 'Look up sender avatars on Gravatar', (v) => patch({ gravatar: v }).catch(errorToast));
  const layout = select([['vertical-split', 'Three panes (side by side)'], ['horizontal-split', 'List above the message'], ['list', 'List only']], s.ui_defaults.layout);
  const sort = select([['newest', 'Newest first'], ['oldest', 'Oldest first']], s.ui_defaults.sort);
  const threaded = h('input', { type: 'checkbox', checked: s.ui_defaults.threaded });
  const compact = h('input', { type: 'checkbox', checked: s.ui_defaults.compact });

  // ---- provisioning ----
  const provHost = h('div', spinner());
  async function loadProvisionings() {
    try {
      const [{ provisionings }, { groups }] = await Promise.all([api.get('/api/admin/mail/provisionings'), api.get('/api/admin/groups')]);
      const groupName = (id) => groups.find((g) => g.id === id)?.name || id;
      clear(provHost).appendChild(provisionings.length
        ? h('div.list.panel', provisionings.map((p) => h(`div.list-item.set-mail-template${p.enabled ? '' : '.set-disabled'}`, { dataset: { id: p.id } },
          h('span.set-provider-icon', icon('mail', 'sm')),
          h('div.grow',
            h('div.row.wrap', { style: { gap: '6px' } }, h('strong', p.name), pill(p.password_mode === 'master' ? 'master password' : 'login password', p.password_mode === 'master' ? 'info' : ''), p.enabled ? null : pill('off', 'danger')),
            h('div.small.muted', `${p.match_kind === 'all' ? 'Everyone' : p.match_kind === 'group' ? `Group ${groupName(p.match_value)}` : `Email domain ${p.match_value}`} · ${p.email_pattern} · ${p.imap_host}`),
            h('div.small.muted', `${p.accounts} account${p.accounts === 1 ? '' : 's'}${p.pending ? ` · ${p.pending} waiting for a password sign-in` : ''}`)),
          h('button.btn.sm', { type: 'button', onclick: () => templateDialog(p, groups) }, icon('edit', 'sm'), h('span.fl-hide-sm', 'Edit')),
          p.enabled && h('button.btn.sm.ghost', {
            type: 'button', title: 'Remove its accounts and turn it off',
            onclick: async () => {
              if (!(await confirmDialog(`Deprovision “${p.name}”?`, `Its ${p.accounts} account${p.accounts === 1 ? '' : 's'} are removed from Ferrite (the mail stays on the server) and the template is turned off.`, { confirm: 'Deprovision', danger: true }))) return;
              try { const { removed } = await api.post(`/api/admin/mail/provisionings/${p.id}/deprovision`); toast(`Removed ${removed} account${removed === 1 ? '' : 's'}`, 'ok'); loadProvisionings(); } catch (e) { errorToast(e); }
            },
          }, h('span', 'Deprovision')),
          h('button.btn.sm.ghost.danger', {
            type: 'button', 'aria-label': `Delete template ${p.name}`, title: 'Delete',
            onclick: async () => {
              if (!(await confirmDialog(`Delete “${p.name}”?`, 'Its accounts are removed from Ferrite (the mail stays on the server).', { confirm: 'Delete', danger: true }))) return;
              try { await api.del(`/api/admin/mail/provisionings/${p.id}`); loadProvisionings(); } catch (e) { errorToast(e); }
            },
          }, icon('trash', 'sm')))))
        : h('p.small.muted.set-empty', 'No templates. A template gives every matching user a mail account automatically.'));
    } catch (e) { clear(provHost).appendChild(h('p.error-text', e.message)); }
  }

  async function templateDialog(p, groups) {
    const editing = !!p;
    const v = p || { match_kind: 'domain', match_value: '', email_pattern: '%EMAIL%', name_pattern: '', imap_port: 993, imap_security: 'tls', imap_username_pattern: '%EMAIL%', smtp_port: 587, smtp_security: 'starttls', smtp_username_pattern: '%EMAIL%', sieve_port: 4190, sieve_security: 'starttls', password_mode: 'master', enabled: true };
    const name = text(v.name, { placeholder: 'Staff mailboxes', required: true });
    const kind = select([['domain', 'Email domain'], ['group', 'Group'], ['all', 'Everyone']], v.match_kind);
    const domain = text(v.match_kind === 'domain' ? v.match_value : '', { placeholder: 'example.org (or * for any)' });
    const group = select(groups.map((g) => [g.id, g.name]), v.match_kind === 'group' ? v.match_value : groups[0]?.id);
    const domainField = field('Domain of the user’s email', domain);
    const groupField = field('Group', group);
    const syncMatch = () => { domainField.hidden = kind.value !== 'domain'; groupField.hidden = kind.value !== 'group'; };
    kind.addEventListener('change', syncMatch);
    syncMatch();
    const email = text(v.email_pattern, { placeholder: '%USERID%@example.org' });
    const sender = text(v.name_pattern, { placeholder: 'The user’s display name' });
    const imapHost = text(v.imap_host, { placeholder: 'imap.example.org' });
    const imapPort = num(v.imap_port, { max: '65535' });
    const imapSec = select(SECURITY, v.imap_security);
    const imapUser = text(v.imap_username_pattern);
    const smtpHost = text(v.smtp_host, { placeholder: 'smtp.example.org' });
    const smtpPort = num(v.smtp_port, { max: '65535' });
    const smtpSec = select(SECURITY, v.smtp_security);
    const smtpUser = text(v.smtp_username_pattern, { placeholder: 'Empty: no SMTP login' });
    const sieveOn = h('input', { type: 'checkbox', checked: v.sieve_enabled });
    const sieveHost = text(v.sieve_host, { placeholder: 'imap.example.org' });
    const sievePort = num(v.sieve_port, { max: '65535' });
    const sieveSec = select(SECURITY, v.sieve_security);
    const modeName = `set-mail-mode-${Math.random().toString(36).slice(2)}`;
    const mode = (val, title, hint) => h('label.set-mail-mode', h('input', { type: 'radio', name: modeName, value: val, checked: v.password_mode === val }),
      h('span', h('strong', title), h('span.small.muted', hint)));
    const master = h('input', { type: 'password', autocomplete: 'new-password', placeholder: editing && v.master_password_set ? 'Stored (leave empty to keep)' : '' });
    const masterField = field('Master password', master, 'Encrypted with the server secret and never shown again.');
    const enabled = h('input', { type: 'checkbox', checked: v.enabled });
    const modes = h('div.set-mail-modes', { role: 'radiogroup', 'aria-label': 'How Ferrite signs in to the mailboxes' },
      mode('master', 'Master password', 'One password (for example a Dovecot master user, %USERID%*master) opens every provisioned mailbox. Users’ own passwords are never stored, but whoever obtains this password and the server secret can read everyone’s mail.'),
      mode('login', 'Each user’s sign-in password', 'Ferrite keeps each matching user’s Ferrite password, encrypted, from their next sign-in (only for users this template matches). Whoever obtains the database and the server secret can recover those passwords, which people may reuse elsewhere. Single sign-on users get no account until they sign in with a password.'));
    const syncMode = () => { masterField.hidden = modes.querySelector('input:checked')?.value !== 'master'; };
    modes.addEventListener('change', syncMode);
    syncMode();
    const body = h('form.set-mail-template-form', { onsubmit: (e) => e.preventDefault() },
      field('Name', name),
      h('div.set-grid', field('Applies to', kind), h('div', domainField, groupField)),
      h('p.small.muted', 'Patterns: %USERID% is the Ferrite username, %EMAIL% the user’s email address.'),
      h('div.set-grid', field('Email address', email), field('Sender name', sender)),
      h('fieldset.set-mail-provider', h('legend', 'Incoming mail (IMAP)'),
        h('div.set-grid', field('Server', imapHost), h('div.set-grid', field('Port', imapPort), field('Security', imapSec))), field('Username', imapUser)),
      h('fieldset.set-mail-provider', h('legend', 'Outgoing mail (SMTP)'),
        h('div.set-grid', field('Server', smtpHost), h('div.set-grid', field('Port', smtpPort), field('Security', smtpSec))), field('Username', smtpUser)),
      h('fieldset.set-mail-provider', h('legend', 'Filters (ManageSieve)'),
        h('label.check', sieveOn, h('span', 'Users get Sieve filters on this server')),
        h('div.set-grid', field('Server', sieveHost), h('div.set-grid', field('Port', sievePort), field('Security', sieveSec)))),
      h('fieldset.set-mail-provider', h('legend', 'Password'), modes, masterField),
      h('label.check', enabled, h('span', 'Enabled')));
    await modal({
      title: editing ? `Edit “${p.name}”` : 'New provisioning template',
      body,
      wide: true,
      actions: [{ label: 'Cancel', value: null }, {
        label: editing ? 'Save and apply' : 'Create and provision', kind: 'primary',
        run: async () => {
          const out = {
            name: name.value.trim(), match_kind: kind.value, match_value: kind.value === 'domain' ? domain.value.trim() : (kind.value === 'group' ? group.value : ''),
            email_pattern: email.value.trim(), name_pattern: sender.value.trim(),
            imap_host: imapHost.value.trim(), imap_port: Number(imapPort.value) || undefined, imap_security: imapSec.value, imap_username_pattern: imapUser.value.trim(),
            smtp_host: smtpHost.value.trim(), smtp_port: Number(smtpPort.value) || undefined, smtp_security: smtpSec.value, smtp_username_pattern: smtpUser.value.trim(),
            sieve_enabled: sieveOn.checked, sieve_host: sieveHost.value.trim(), sieve_port: Number(sievePort.value) || undefined, sieve_security: sieveSec.value,
            password_mode: modes.querySelector('input:checked')?.value || 'master', enabled: enabled.checked,
          };
          if (master.value) out.master_password = master.value;
          const { report } = editing
            ? await api.patch(`/api/admin/mail/provisionings/${p.id}`, out)
            : await api.post('/api/admin/mail/provisionings', out);
          toast(`Provisioned: ${report.created} created, ${report.updated} updated, ${report.removed} removed${report.pending ? `, ${report.pending} waiting` : ''}`, 'ok');
          loadProvisionings();
          return true;
        },
      }],
    });
  }

  // ---- delegations ----
  const delegHost = h('div', spinner());
  async function loadDelegations() {
    try {
      const { delegations } = await api.get('/api/admin/mail/delegations');
      clear(delegHost).appendChild(delegations.length
        ? h('div.list.panel', delegations.map((d) => h('div.list-item',
          h('span.set-provider-icon', icon('users', 'sm')),
          h('div.grow', h('strong', d.account_email), h('div.small.muted', `@${d.owner_username} shares it with @${d.username} · ${relTime(d.created_at)}`)),
          h('button.btn.sm.ghost.danger', {
            type: 'button', 'aria-label': `Revoke @${d.username}'s access to ${d.account_email}`,
            onclick: async () => {
              if (!(await confirmDialog('Revoke access?', `@${d.username} can no longer use ${d.account_email}.`, { confirm: 'Revoke', danger: true }))) return;
              try { await api.del(`/api/admin/mail/delegations/${d.id}`); loadDelegations(); } catch (e) { errorToast(e); }
            },
          }, h('span', 'Revoke')))))
        : h('p.small.muted.set-empty', 'No mail account is shared.'));
    } catch (e) { clear(delegHost).appendChild(h('p.error-text', e.message)); }
  }

  host.append(
    card('Mail accounts', 'Provisioned accounts (below) are always available.', [allowAdd.el]),
    card('Connection security', 'Both are on the safe side by default.', [
      h('div.set-mail-switches', verifyTls.el, allowLocal.el),
      h('p.small.muted.set-help', 'Local addresses (loopback, private and link-local networks) are refused unless allowed, so mail settings can’t be used to reach services inside your network. Allow them when your mail server runs on this machine or your LAN.'),
    ]),
    card('Google and Microsoft sign-in', 'Let users connect Gmail and Microsoft 365 / Outlook.com accounts through the provider’s sign-in (OAuth 2.0, XOAUTH2), without app passwords.', [
      h('p.small', 'Register Ferrite as a web application with the provider and give it this redirect URI:'),
      codeBlock(data.redirect_uri, 'Redirect URI'),
      h('p.small.muted', 'Google: Google Cloud console → APIs & Services → Credentials (scope https://mail.google.com/). Microsoft: Entra ID → App registrations (delegated permissions IMAP.AccessAsUser.All, SMTP.Send, offline_access).'),
      providerForm('google', 'Google', s.oauth.google, data.settings.google_secret_set),
      providerForm('microsoft', 'Microsoft', s.oauth.microsoft, data.settings.microsoft_secret_set),
    ]),
    card('Spam reporting', '“Report spam” and “Not spam” forward the message as an attachment to these addresses (e.g. your spam filter’s training mailboxes) and move it. Leave empty to only move it.', [
      h('div.set-grid', field('Spam reports to', spam), field('“Not spam” reports to', ham)),
      saveRow(() => patch({ spam_report_address: spam.value.trim(), ham_report_address: ham.value.trim() })),
    ]),
    card('Sending', 'Limits and alerts for outgoing mail.', [
      field('Attachment size limit (MB)', attach, 'Larger files are sent as share links. Empty: no limit.'),
      h('label.check', abuseOn, h('span', 'Alert administrators about suspicious sending')),
      h('div.set-grid', field('Recipients per message', maxRcpt), field('Messages per 15 minutes', per15)),
      h('div.set-grid', field('Messages per hour', perHour), field('Messages per day', perDay)),
      h('p.small.muted', 'Exceeding a threshold is logged and notifies every administrator (once per period); the mail is still sent. Empty or 0 turns a rule off.'),
      saveRow(async () => {
        const limit = attach.value.trim() === '' ? null : Math.round(Number(attach.value) * MB);
        await patch({
          attachment_size_limit: limit,
          abuse: { enabled: abuseOn.checked, max_recipients: Number(maxRcpt.value) || 0, per_15m: Number(per15.value) || 0, per_hour: Number(perHour.value) || 0, per_day: Number(perDay.value) || 0 },
        });
      }),
      h('h3.set-subhead', 'Recent alerts'),
      alertsHost,
    ]),
    card('Background sync', 'Every account is synced at this interval, also when its owner is offline (inboxes of people online update live).', [
      field('Interval (minutes)', interval),
      h('div.set-actions',
        h('span.grow', syncReport),
        h('button.btn', {
          type: 'button',
          onclick: async (e) => {
            e.currentTarget.disabled = true;
            syncReport.textContent = 'Syncing…';
            try {
              const { report } = await api.post('/api/admin/mail/sync');
              syncReport.textContent = `${report.accounts} account${report.accounts === 1 ? '' : 's'} synced, ${report.new_messages} new message${report.new_messages === 1 ? '' : 's'}${report.failed ? `, ${report.failed} failed` : ''}.`;
            } catch (ex) { syncReport.textContent = ex.message; }
            e.currentTarget.disabled = false;
          },
        }, icon('refresh', 'sm'), 'Sync all now'),
        h('button.btn.primary', { type: 'button', onclick: () => patch({ sync_interval: Math.round(Number(interval.value) * 60) }).catch(errorToast) }, 'Save')),
    ]),
    card('AI and privacy', null, [h('div.set-mail-switches', llm.el, importance.el, gravatar.el),
      h('p.small.muted.set-help', 'Gravatar receives a hash of the sender’s address for each avatar it is asked for.')]),
    card('Defaults for users', 'Until people choose for themselves.', [
      h('div.set-grid', field('Layout', layout), field('Order', sort)),
      h('label.check', threaded, h('span', 'Group messages into conversations')),
      h('label.check', compact, h('span', 'Compact list (no avatars or previews)')),
      saveRow(() => patch({ ui_defaults: { layout: layout.value, sort: sort.value, threaded: threaded.checked, compact: compact.checked } })),
    ]),
    card('Provisioning', 'Templates that give users a mail account automatically, by email domain or group. Users can change only the sender name and signature of these accounts.', [provHost],
      h('div.row', { style: { gap: '6px' } },
        h('button.btn.sm', {
          type: 'button',
          onclick: async () => {
            try {
              const { report } = await api.post('/api/admin/mail/provision');
              toast(`${report.created} created, ${report.updated} updated, ${report.removed} removed${report.pending ? `, ${report.pending} waiting for a sign-in` : ''}`, 'ok');
              loadProvisionings();
            } catch (e) { errorToast(e); }
          },
        }, icon('refresh', 'sm'), h('span.fl-hide-sm', 'Provision all')),
        h('button.btn.primary.sm', { type: 'button', onclick: async () => { try { const { groups } = await api.get('/api/admin/groups'); templateDialog(null, groups); } catch (e) { errorToast(e); } } }, icon('plus', 'sm'), 'New template'))),
    card('Shared accounts', 'Mail accounts people shared with others. Revoke access here if needed.', [delegHost]));
  await Promise.all([loadAlerts(), loadProvisionings(), loadDelegations()]);
}
