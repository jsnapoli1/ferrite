// Settings → Security: two-factor authentication (set up, recovery codes, turn off) and the
// single sign-on identities linked to this account.

import { api } from '../api.js';
import { h, clear, icon, toast, modal, relTime, dateTime, spinner } from '../ui.js';
import { card, field, pill } from './settings-common.js';
import { codeInput, enrollPanel, recoveryCodesPanel } from '../twofactor.js';

export async function securityTab(host) {
  const tfHost = h('div', spinner());
  const ssoHost = h('div');
  host.append(
    card('Two-factor authentication', 'A second step when you sign in with your password: a code from an authenticator app on your phone. Someone who learns your password still can’t get in.', tfHost),
    ssoHost);

  async function load() {
    const sec = await api.get('/api/me/security');
    renderTwoFactor(sec.two_factor);
    clear(ssoHost);
    if (sec.providers.length || sec.identities.length) ssoHost.appendChild(identitiesCard(sec));
  }

  function renderTwoFactor(tf) {
    if (!tf.enabled) {
      clear(tfHost).append(
        h('div.tf-status', pill('Off'), tf.required ? pill('Required on this server', 'warn') : null,
          h('span.small.muted.grow', 'Only your password protects this account.')),
        h('div.set-actions', h('button.btn.primary', { type: 'button', onclick: beginSetup }, icon('lock', 'sm'), 'Set up two-factor authentication')));
      return;
    }
    const few = tf.recovery_codes_left <= 3;
    clear(tfHost).append(
      h('div.tf-status', pill('On', 'ok'), tf.required ? pill('Required on this server', 'info') : null,
        h('span.small.muted.grow', tf.enabled_at ? `Turned on ${relTime(tf.enabled_at)}` : '')),
      h('dl.set-dl.tf-dl',
        h('dt', 'Authenticator app'), h('dd', 'Asked for a code each time you sign in with your password'),
        h('dt', 'Recovery codes'), h('dd', few ? h('span.set-warn', icon('key', 'sm'), `${tf.recovery_codes_left} left: create new ones soon`) : `${tf.recovery_codes_left} of 10 left`),
        h('dt', 'App tokens'), h('dd', 'Sync and WebDAV clients sign in with an app token instead of your password')),
      h('div.set-actions',
        h('button.btn', { type: 'button', onclick: newCodes }, icon('refresh', 'sm'), 'New recovery codes'),
        tf.required ? null : h('button.btn.danger', { type: 'button', onclick: turnOff }, 'Turn off')));
  }

  async function beginSetup() {
    const e = await api.post('/api/me/2fa/totp', {});
    const input = codeInput();
    const err = h('div.error-text', { role: 'alert' });
    const btn = h('button.btn.primary', { type: 'submit' }, 'Turn on');
    const form = h('form', {
      onsubmit: async (ev) => {
        ev.preventDefault();
        err.textContent = '';
        btn.disabled = true;
        try {
          const { recovery_codes: codes } = await api.post('/api/me/2fa/totp/confirm', { code: input.value.trim() });
          showCodes(codes, 'Two-factor authentication is on. Save your recovery codes now.');
        } catch (ex) {
          err.textContent = ex.message;
          input.select();
        } finally {
          btn.disabled = false;
        }
      },
    },
    enrollPanel(e),
    h('div.tf-confirm', field('Code from the app', input), err),
    h('div.set-actions', h('button.btn', { type: 'button', onclick: load }, 'Cancel'), btn));
    clear(tfHost).appendChild(form);
    input.focus();
  }

  function showCodes(codes, message) {
    const done = h('button.btn.primary', { type: 'button', onclick: () => { toast('Two-factor authentication is on', 'ok'); load(); } }, 'I’ve saved them');
    clear(tfHost).append(h('p.tf-done', icon('check', 'sm'), message), recoveryCodesPanel(codes), h('div.set-actions', done));
    done.focus();
  }

  async function newCodes() {
    const password = h('input', { type: 'password', autocomplete: 'current-password', required: true });
    const codes = await modal({
      title: 'New recovery codes',
      body: h('form', { onsubmit: (ev) => { ev.preventDefault(); ev.target.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
        h('p', 'Your current recovery codes stop working and ten new ones are created.'),
        field('Your password', password), h('button', { type: 'submit', hidden: true })),
      actions: [{ label: 'Cancel', value: null }, {
        label: 'Create new codes', kind: 'primary',
        run: async () => (await api.post('/api/me/2fa/recovery-codes', { password: password.value })).recovery_codes,
      }],
    });
    if (codes) {
      await modal({ title: 'Your new recovery codes', body: recoveryCodesPanel(codes), actions: [{ label: 'Done', kind: 'primary', value: true }] });
      load();
    }
  }

  async function turnOff() {
    const password = h('input', { type: 'password', autocomplete: 'current-password', required: true });
    const code = codeInput();
    const ok = await modal({
      title: 'Turn off two-factor authentication?',
      body: h('form', { onsubmit: (ev) => { ev.preventDefault(); ev.target.closest('.modal').querySelector('.modal-foot .btn.danger').click(); } },
        h('p', 'Your password alone will sign you in again. Confirm with your password and a code from your app (or a recovery code).'),
        field('Your password', password), field('Code', code), h('button', { type: 'submit', hidden: true })),
      actions: [{ label: 'Cancel', value: null }, {
        label: 'Turn off', kind: 'danger.solid',
        run: async () => { await api.post('/api/me/2fa/disable', { password: password.value, code: code.value.trim() }); return true; },
      }],
    });
    if (ok) {
      toast('Two-factor authentication is off', 'ok');
      load();
    }
  }

  function identitiesCard(sec) {
    const list = sec.identities.length
      ? h('div.list.panel', sec.identities.map((i) => h('div.list-item',
        h('span.set-provider-icon', icon('key', 'sm')),
        h('div.grow',
          h('strong', i.provider_name),
          h('div.small.muted.ellipsis', [i.email, `linked ${dateTime(i.created_at)}`, i.last_login_at ? `last used ${relTime(i.last_login_at)}` : null].filter(Boolean).join(' · '))),
        pill('Linked', 'ok'))))
      : h('p.muted.small.set-empty', 'No sign-in from another provider is linked to this account yet.');
    const names = sec.providers.map((p) => p.name).join(', ');
    return card('Single sign-on', `Sign in through your organization’s identity provider${names ? ` (${names})` : ''}. Its own security checks apply; Ferrite doesn’t ask for a second code.`, list);
  }

  await load();
}
