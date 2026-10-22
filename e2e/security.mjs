// Browser test of account security: two-factor authentication (set up in Settings → Security
// with a TOTP computed here, sign in with a code, sign in with a recovery code, admin view)
// and single sign-on through a mock OpenID Connect provider that this script serves itself.
//
// Run against a server with a fresh data dir and this provider configured:
//
//   [[oidc]]
//   id = "mock"
//   display_name = "Mock IdP"
//   issuer = "http://127.0.0.1:18833"       # $IDP_PORT
//   client_id = "ferrite-e2e"
//   client_secret = "e2e-secret"
//   auto_create = true
//
//   FERRITE_URL=http://127.0.0.1:8833 node security.mjs
//
// Screenshots (light, dark, 390px phone) go to $SHOTS. Prints {ok, errors}; any console
// error fails the run.
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';
import http from 'node:http';
import crypto from 'node:crypto';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8833';
const idpPort = Number(process.env.IDP_PORT || 18833);
const issuer = `http://127.0.0.1:${idpPort}`;
const shots = process.env.SHOTS || '/tmp/ferrite-security-shots';
mkdirSync(shots, { recursive: true });
const ADMIN = { username: 'admin', display_name: 'Ada Admin', password: 'admin-password' };
const CLIENT = { id: 'ferrite-e2e', secret: 'e2e-secret' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

// ---- TOTP (RFC 6238 over RFC 4226 HOTP: HMAC-SHA-1, 6 digits, 30 s) ----------------------

function base32Decode(s) {
  const alphabet = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';
  let bits = 0;
  let value = 0;
  const out = [];
  for (const c of s.replace(/[\s=]/g, '').toUpperCase()) {
    const v = alphabet.indexOf(c);
    if (v < 0) throw new Error(`bad base32 ${c}`);
    value = (value << 5) | v;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      out.push((value >>> bits) & 0xff);
    }
  }
  return Buffer.from(out);
}

function hotp(key, counter) {
  const msg = Buffer.alloc(8);
  msg.writeBigUInt64BE(BigInt(counter));
  const mac = crypto.createHmac('sha1', key).update(msg).digest();
  const off = mac[mac.length - 1] & 0x0f;
  const bin = ((mac[off] & 0x7f) << 24) | (mac[off + 1] << 16) | (mac[off + 2] << 8) | mac[off + 3];
  return String(bin % 1_000_000).padStart(6, '0');
}

// Self-check against RFC 4226 Appendix D before trusting it.
assert(hotp(Buffer.from('12345678901234567890'), 0) === '755224' && hotp(Buffer.from('12345678901234567890'), 9) === '520489', 'HOTP test vectors');

/** An authenticator: codes for strictly increasing steps (the server accepts each step once). */
function authenticator(secret) {
  const key = base32Decode(secret);
  let last = null;
  return async () => {
    for (;;) {
      const cur = Math.floor(Date.now() / 1000 / 30);
      const step = last === null ? cur - 1 : Math.max(cur - 1, last + 1);
      if (step <= cur + 1) {
        last = step;
        return hotp(key, step);
      }
      await new Promise((r) => setTimeout(r, 1000));
    }
  };
}

// ---- mock OpenID Connect provider -------------------------------------------------------

const { publicKey, privateKey } = crypto.generateKeyPairSync('rsa', { modulusLength: 2048 });
const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'e2e-1', use: 'sig', alg: 'RS256' };
const b64url = (b) => Buffer.from(b).toString('base64url');
const pending = new Map(); // authorization requests waiting for the user's choice
const codes = new Map(); // issued codes → request
const idpLog = [];

function signJwt(claims) {
  const input = `${b64url(JSON.stringify({ alg: 'RS256', kid: jwk.kid, typ: 'JWT' }))}.${b64url(JSON.stringify(claims))}`;
  return `${input}.${b64url(crypto.sign('sha256', Buffer.from(input), privateKey))}`;
}

const ADA = { sub: 'mock-ada-1', preferred_username: 'ada', name: 'Ada Lovelace', email: 'ada@example.org', email_verified: true };

const idp = http.createServer(async (req, res) => {
  const url = new URL(req.url, issuer);
  const send = (status, body, headers = {}) => {
    res.writeHead(status, { 'content-type': typeof body === 'string' ? 'text/html; charset=utf-8' : 'application/json', ...headers });
    res.end(typeof body === 'string' ? body : JSON.stringify(body));
  };
  const redirect = (location) => { res.writeHead(303, { location }); res.end(); };
  idpLog.push(`${req.method} ${url.pathname}`);
  if (url.pathname === '/.well-known/openid-configuration') {
    return send(200, {
      issuer, authorization_endpoint: `${issuer}/authorize`, token_endpoint: `${issuer}/token`, jwks_uri: `${issuer}/jwks`,
      response_types_supported: ['code'], subject_types_supported: ['public'], id_token_signing_alg_values_supported: ['RS256'],
      code_challenge_methods_supported: ['S256'], token_endpoint_auth_methods_supported: ['client_secret_basic'],
    });
  }
  if (url.pathname === '/jwks') return send(200, { keys: [jwk] });
  if (url.pathname === '/favicon.ico') { res.writeHead(204); return res.end(); }
  if (url.pathname === '/authorize') {
    const q = Object.fromEntries(url.searchParams);
    if (q.client_id !== CLIENT.id || q.response_type !== 'code' || q.code_challenge_method !== 'S256' || !q.nonce || !q.state || !q.scope.split(' ').includes('openid')) {
      return send(400, '<!doctype html><title>Bad request</title><p>Invalid authorization request');
    }
    const id = crypto.randomBytes(12).toString('hex');
    pending.set(id, q);
    return send(200, `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Mock IdP</title><link rel="icon" href="data:,">
      <meta name="viewport" content="width=device-width, initial-scale=1"></head>
      <body style="font-family:sans-serif;max-width:360px;margin:60px auto;padding:0 16px">
      <h1>Mock IdP</h1><p>Ferrite wants you to sign in.</p>
      <form method="post" action="/decide"><input type="hidden" name="id" value="${id}">
      <button name="choice" value="allow">Continue as Ada Lovelace</button>
      <button name="choice" value="deny">Cancel</button></form></body></html>`);
  }
  if (url.pathname === '/decide' && req.method === 'POST') {
    const form = new URLSearchParams(await body(req));
    const q = pending.get(form.get('id'));
    pending.delete(form.get('id'));
    if (!q) return send(400, '<!doctype html><title>Expired</title><p>Expired');
    const back = new URL(q.redirect_uri);
    back.searchParams.set('state', q.state);
    if (form.get('choice') === 'deny') {
      back.searchParams.set('error', 'access_denied');
      back.searchParams.set('error_description', 'you cancelled the sign-in');
    } else {
      const code = crypto.randomBytes(16).toString('base64url');
      codes.set(code, q);
      back.searchParams.set('code', code);
    }
    return redirect(back.toString());
  }
  if (url.pathname === '/token' && req.method === 'POST') {
    const form = new URLSearchParams(await body(req));
    const [id, secret] = Buffer.from((req.headers.authorization || '').replace(/^Basic /, ''), 'base64').toString().split(':').map(decodeURIComponent);
    if (id !== CLIENT.id || secret !== CLIENT.secret) return send(401, { error: 'invalid_client' });
    const q = codes.get(form.get('code'));
    codes.delete(form.get('code'));
    if (!q || form.get('grant_type') !== 'authorization_code' || form.get('redirect_uri') !== q.redirect_uri) return send(400, { error: 'invalid_grant' });
    // PKCE S256 (RFC 7636 §4.6).
    const challenge = crypto.createHash('sha256').update(form.get('code_verifier') || '').digest('base64url');
    if (challenge !== q.code_challenge) return send(400, { error: 'invalid_grant', error_description: 'PKCE verification failed' });
    const now = Math.floor(Date.now() / 1000);
    const idToken = signJwt({ ...ADA, iss: issuer, aud: CLIENT.id, nonce: q.nonce, iat: now, exp: now + 300 });
    return send(200, { access_token: 'mock-access', token_type: 'Bearer', expires_in: 300, id_token: idToken });
  }
  send(404, { error: 'not found' });
});

function body(req) {
  return new Promise((resolve) => {
    let data = '';
    req.on('data', (c) => { data += c; });
    req.on('end', () => resolve(data));
  });
}

await new Promise((r) => idp.listen(idpPort, '127.0.0.1', r));

// ---- browser ------------------------------------------------------------------------------

const browser = await chromium.launch();
const errors = [];

async function newPage(name, { viewport = { width: 1280, height: 820 }, colorScheme = 'light' } = {}) {
  const ctx = await browser.newContext({ viewport, colorScheme });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  return page;
}

async function shot(page, name) {
  await page.waitForTimeout(150);
  await page.screenshot({ path: `${shots}/${name}.png`, fullPage: false });
}

async function signOut(page) {
  await page.locator('.header > .btn').last().click();
  await page.getByRole('menuitem', { name: 'Sign out' }).click();
  await page.getByRole('button', { name: 'Sign in', exact: true }).waitFor();
}

async function passwordStep(page, { username, password }) {
  await page.getByLabel('Username').fill(username);
  await page.getByLabel('Password').fill(password);
  await page.getByRole('button', { name: 'Sign in', exact: true }).click();
}

let result;
try {
  // Setup: the first administrator.
  const page = await newPage('main');
  await page.goto(base);
  await page.getByLabel('Username').fill(ADMIN.username);
  await page.getByLabel('Your name').fill(ADMIN.display_name);
  await page.getByLabel('Password (8+ characters)').fill(ADMIN.password);
  await page.getByRole('button', { name: 'Create administrator' }).click();
  await page.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
  log('admin created');

  // Settings → Security: set up 2FA with a code computed here.
  await page.goto(`${base}/settings/security`);
  await page.getByRole('heading', { name: 'Two-factor authentication' }).waitFor();
  await page.getByRole('heading', { name: 'Single sign-on' }).waitFor();
  await page.getByRole('button', { name: 'Set up two-factor authentication' }).click();
  const secretText = await page.getByLabel('Setup key', { exact: true }).textContent();
  const qr = await page.getByRole('img', { name: /QR code/ }).evaluate((img) => ({ ok: img.complete && img.naturalWidth > 0, src: img.src.slice(0, 26) }));
  assert(qr.ok && qr.src === 'data:image/svg+xml;base64,', `QR code renders (${JSON.stringify(qr)})`);
  await shot(page, 'settings-security-enroll-light');
  const nextCode = authenticator(secretText);
  await page.getByLabel('Code from the app').fill(await nextCode());
  await page.getByRole('button', { name: 'Turn on' }).click();
  await page.getByRole('list', { name: 'Recovery codes' }).waitFor();
  const recovery = await page.getByRole('list', { name: 'Recovery codes' }).locator('code').allTextContents();
  assert(recovery.length === 10 && recovery.every((c) => /^[a-z2-9]{4}(-[a-z2-9]{4}){3}$/.test(c)), `ten recovery codes (${recovery})`);
  await shot(page, 'settings-security-codes-light');
  await page.getByRole('button', { name: 'I’ve saved them' }).click();
  await page.getByText('10 of 10 left').waitFor();
  await shot(page, 'settings-security-on-light');
  log('2FA enabled');

  // Sign out; the sign-in page offers SSO. Password → code step → signed in.
  await signOut(page);
  await page.getByRole('link', { name: 'Continue with Mock IdP' }).waitFor();
  await shot(page, 'login-light');
  await passwordStep(page, ADMIN);
  await page.getByRole('heading', { name: 'Two-factor authentication' }).waitFor();
  await shot(page, 'login-2fa-light');
  await page.getByLabel('Authentication code').fill(await nextCode());
  await page.getByRole('button', { name: 'Verify' }).click();
  await page.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
  log('signed in with a TOTP code');

  // Recovery code instead of the app.
  await signOut(page);
  await passwordStep(page, ADMIN);
  await page.getByRole('link', { name: 'Use a recovery code instead' }).click();
  await page.getByLabel('Recovery code').fill(recovery[0].toUpperCase());
  await shot(page, 'login-recovery-light');
  await page.getByRole('button', { name: 'Verify' }).click();
  await page.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
  // A used recovery code is refused (checked outside the page: a refusal is an HTTP 400).
  const ctxReq = page.context().request;
  const ch = await (await ctxReq.post(`${base}/api/auth/login`, { data: { username: ADMIN.username, password: ADMIN.password }, headers: { 'X-Ferrite-Request': '1' } })).json();
  const reuse = await ctxReq.post(`${base}/api/auth/2fa`, { data: { challenge: ch.challenge, code: recovery[0] }, headers: { 'X-Ferrite-Request': '1' } });
  assert(reuse.status() === 400, `a used recovery code is refused (${reuse.status()})`);
  await page.goto(`${base}/settings/security`);
  await page.getByText('9 of 10 left').waitFor();
  log('signed in with a recovery code');

  // The admin's user list shows 2FA.
  await page.goto(`${base}/settings/admin`);
  await page.getByRole('cell', { name: /Ada Admin/ }).getByText('2FA').waitFor();
  await shot(page, 'admin-users-light');
  await page.getByRole('button', { name: `Edit ${ADMIN.username}` }).click();
  await page.getByRole('dialog').getByRole('button', { name: 'Reset 2FA' }).waitFor();
  await shot(page, 'admin-edit-user-light');
  await page.keyboard.press('Escape');

  // SSO: cancel at the provider → a clear message on the sign-in page.
  await signOut(page);
  await page.getByRole('link', { name: 'Continue with Mock IdP' }).click();
  await page.getByRole('button', { name: 'Cancel' }).click();
  await page.getByRole('alert').filter({ hasText: 'you cancelled the sign-in' }).waitFor();
  assert(!page.url().includes('sso_error'), 'the error is taken out of the address bar');
  await shot(page, 'login-sso-error-light');

  // SSO: continue → back in Ferrite, signed in as an auto-created account; no 2FA step.
  await page.getByRole('link', { name: 'Continue with Mock IdP' }).click();
  await page.getByRole('button', { name: 'Continue as Ada Lovelace' }).click();
  await page.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
  const me = await page.evaluate(() => fetch('/api/me').then((r) => r.json()));
  assert(me.user.username === 'ada' && me.user.display_name === 'Ada Lovelace', `SSO user (${JSON.stringify(me.user)})`);
  await page.goto(`${base}/settings/security`);
  await page.getByText('Mock IdP', { exact: true }).waitFor();
  await shot(page, 'settings-security-sso-light');
  assert(idpLog.includes('POST /token') && idpLog.includes('GET /jwks'), `the provider was used (${idpLog})`);
  log('SSO sign-in');

  // Dark theme and a 390px phone: sign-in, code step, enrollment, security settings.
  for (const [label, opts] of [['dark', { colorScheme: 'dark' }], ['phone', { viewport: { width: 390, height: 844 } }], ['phone-dark', { viewport: { width: 390, height: 844 }, colorScheme: 'dark' }]]) {
    const p = await newPage(label, opts);
    await p.goto(base);
    await p.getByRole('link', { name: 'Continue with Mock IdP' }).waitFor();
    await shot(p, `login-${label}`);
    await passwordStep(p, ADMIN);
    await p.getByRole('heading', { name: 'Two-factor authentication' }).waitFor();
    await shot(p, `login-2fa-${label}`);
    await p.getByLabel('Authentication code').fill(await nextCode());
    await p.getByRole('button', { name: 'Verify' }).click();
    await p.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
    await p.goto(`${base}/settings/security`);
    await p.getByText('9 of 10 left').waitFor();
    await shot(p, `settings-security-on-${label}`);
    const overflow = await p.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
    assert(overflow <= 0, `no horizontal scroll on ${label} (${overflow}px)`);
    await p.context().close();
  }
  // Enrollment and recovery codes on a phone, as the SSO user.
  const phone = await newPage('phone-enroll', { viewport: { width: 390, height: 844 }, colorScheme: 'dark' });
  await phone.goto(base);
  await phone.getByRole('link', { name: 'Continue with Mock IdP' }).click();
  await phone.getByRole('button', { name: 'Continue as Ada Lovelace' }).click();
  await phone.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
  await phone.goto(`${base}/settings/security`);
  await phone.getByRole('button', { name: 'Set up two-factor authentication' }).click();
  await phone.getByLabel('Setup key', { exact: true }).waitFor();
  await shot(phone, 'settings-security-enroll-phone-dark');
  const adaCode = authenticator(await phone.getByLabel('Setup key', { exact: true }).textContent());
  await phone.getByLabel('Code from the app').fill(await adaCode());
  await phone.getByRole('button', { name: 'Turn on' }).click();
  await phone.getByRole('list', { name: 'Recovery codes' }).waitFor();
  await shot(phone, 'settings-security-codes-phone-dark');
  const overflow = await phone.evaluate(() => document.documentElement.scrollWidth - document.documentElement.clientWidth);
  assert(overflow <= 0, `no horizontal scroll while enrolling on a phone (${overflow}px)`);
  log('themes and phone');

  result = { ok: errors.length === 0, errors };
} catch (e) {
  result = { ok: false, errors: [...errors, e.stack || String(e)] };
}
console.log(JSON.stringify(result));
await browser.close();
idp.close();
process.exit(result.ok ? 0 : 1);
