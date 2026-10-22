// Two-factor authentication UI shared by the sign-in screen and Settings → Security:
// the second sign-in step, the enrollment panel (QR code + secret) and recovery codes.

import { api } from './api.js';
import { h, clear, icon, copyText } from './ui.js';

/** An input for a 6-digit code (or, with `recovery`, a recovery code). */
export function codeInput({ recovery = false, id } = {}) {
  const input = h('input.tf-code', { type: 'text', id, required: true, spellcheck: 'false', autocapitalize: 'off' });
  setCodeMode(input, recovery);
  return input;
}

function setCodeMode(input, recovery) {
  input.value = '';
  input.classList.toggle('recovery', recovery);
  if (recovery) {
    input.removeAttribute('inputmode');
    input.removeAttribute('pattern');
    input.setAttribute('autocomplete', 'off');
    input.setAttribute('maxlength', '24');
    input.setAttribute('placeholder', 'xxxx-xxxx-xxxx-xxxx');
  } else {
    input.setAttribute('inputmode', 'numeric');
    input.setAttribute('pattern', '[0-9 ]{6,7}');
    input.setAttribute('autocomplete', 'one-time-code');
    input.setAttribute('maxlength', '7');
    input.setAttribute('placeholder', '123456');
  }
}

/** QR code, the secret for typing in, and what to do with them. */
export function enrollPanel({ qr, secret, otpauth_uri: uri }) {
  const grouped = secret.match(/.{1,4}/g).join(' ');
  return h('div.tf-enroll',
    h('a.tf-qr', { href: uri, title: 'Open in an authenticator app on this device', 'aria-label': 'QR code for your authenticator app' },
      h('img', { src: qr, alt: 'QR code to scan with an authenticator app', width: 168, height: 168 })),
    h('div.tf-enroll-text',
      h('ol.tf-steps',
        h('li', 'Open an authenticator app (such as Aegis, Google Authenticator, 1Password or Microsoft Authenticator).'),
        h('li', 'Scan the QR code, or enter this key by hand:'),
        h('div.tf-secret', h('code', { 'aria-label': 'Setup key' }, grouped),
          h('button.btn.sm.ghost.icon', { type: 'button', 'aria-label': 'Copy setup key', title: 'Copy key', onclick: () => copyText(secret, 'Setup key copied') }, icon('copy', 'sm'))),
        h('li', 'Enter the 6-digit code the app shows.'))));
}

/** The recovery codes, with copy and download. Shown once. */
export function recoveryCodesPanel(codes) {
  const text = `Ferrite recovery codes (${location.host})\nEach code works once.\n\n${codes.join('\n')}\n`;
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: 'text/plain' }));
    const a = h('a', { href: url, download: 'ferrite-recovery-codes.txt' });
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  return h('div.tf-recovery',
    h('p.small', 'If you lose your phone, each of these codes signs you in once instead of an app code. Keep them somewhere safe: they won’t be shown again.'),
    h('ol.tf-codes', { 'aria-label': 'Recovery codes' }, codes.map((c) => h('li', h('code', c)))),
    h('div.row.wrap.tf-code-actions',
      h('button.btn.sm', { type: 'button', onclick: () => copyText(codes.join('\n'), 'Recovery codes copied') }, icon('copy', 'sm'), 'Copy'),
      h('button.btn.sm', { type: 'button', onclick: download }, icon('download', 'sm'), 'Download')));
}

function authCard(...children) {
  const app = document.getElementById('app');
  const card = h('div.card.tf-card', ...children);
  clear(app).appendChild(h('div.auth', card));
  return card;
}

/**
 * The second sign-in step for a password sign-in that answered with a challenge.
 * `res` is that answer: {two_factor: 'totp'|'enroll', challenge, enrollment?}.
 * `onDone` runs once signed in; `onCancel` goes back to the password form.
 */
export function showSecondStep(res, { onDone, onCancel }) {
  const enrolling = res.two_factor === 'enroll';
  let recovery = false;
  const input = codeInput({ id: 'tf-code' });
  const label = h('label', { for: 'tf-code' }, 'Authentication code');
  const err = h('div.error-text', { role: 'alert' });
  const btn = h('button.btn.primary.tf-submit', { type: 'submit' }, enrolling ? 'Turn on and sign in' : 'Verify');
  const toggle = h('a', {
    href: '#',
    onclick: (e) => {
      e.preventDefault();
      recovery = !recovery;
      setCodeMode(input, recovery);
      label.textContent = recovery ? 'Recovery code' : 'Authentication code';
      hint.textContent = recovery ? 'Enter one of the recovery codes you saved when you set up two-factor authentication.' : 'Enter the 6-digit code from your authenticator app.';
      toggle.textContent = recovery ? 'Use your authenticator app instead' : 'Use a recovery code instead';
      err.textContent = '';
      input.focus();
    },
  }, 'Use a recovery code instead');
  const hint = h('p.muted', enrolling
    ? 'This server requires two-factor authentication. Set it up now to finish signing in.'
    : 'Enter the 6-digit code from your authenticator app.');
  const form = h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      btn.disabled = true;
      try {
        const done = await api.post('/api/auth/2fa', { challenge: res.challenge, code: input.value.trim() });
        if (done.recovery_codes) showSavedCodes(done.recovery_codes, onDone);
        else await onDone();
      } catch (ex) {
        err.textContent = ex.message;
        input.select();
      } finally {
        btn.disabled = false;
      }
    },
  },
  enrolling ? enrollPanel(res.enrollment) : null,
  h('div.field', label, input), err, btn);
  authCard(
    h('h1', h('span.brand-mark', 'Fe'), enrolling ? 'Set up two-factor authentication' : 'Two-factor authentication'),
    hint,
    form,
    h('p.small.muted.tf-links', enrolling ? null : [toggle, h('br')],
      h('a', { href: '#', onclick: (e) => { e.preventDefault(); onCancel(); } }, 'Back to sign in')));
  setTimeout(() => input.focus(), 0);
}

function showSavedCodes(codes, onDone) {
  const btn = h('button.btn.primary.tf-submit', { type: 'button', onclick: async () => { btn.disabled = true; await onDone(); } }, 'I’ve saved them, continue');
  authCard(
    h('h1', h('span.brand-mark', 'Fe'), 'Save your recovery codes'),
    h('p.muted', 'Two-factor authentication is on.'),
    recoveryCodesPanel(codes),
    btn);
  setTimeout(() => btn.focus(), 0);
}
