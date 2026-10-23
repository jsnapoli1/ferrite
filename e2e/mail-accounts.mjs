// Browser test of Mail accounts, delegation, provisioning and administration against REAL
// Dovecot servers, with a mock OAuth provider and an SMTP sink run by this script.
//
//   # Dovecot with quota (mailboxes alice, bob, carol, dave @example.test):
//   MAIL_TEST_DIR=/tmp/ferrite-mail-accounts IMAP_PORT=3351 LMTP_PORT=3352 AUTH_PORT=3353 SIEVE_PORT=3354 POSTFIX=0 QUOTA=1 \
//     EXTRA_USERS="carol dave" bash mail-server.sh start /tmp/ferrite-mail-accounts
//   # Dovecot that accepts OAuth bearer tokens, checked against this script's provider on :3365:
//   MAIL_TEST_DIR=/tmp/ferrite-mail-accounts-oauth IMAP_PORT=3361 LMTP_PORT=3362 AUTH_PORT=3363 SIEVE_PORT=3364 POSTFIX=0 \
//     EXTRA_USERS="carol dave" OAUTH2_INTROSPECTION_URL=http://127.0.0.1:3365/introspect bash mail-server.sh start /tmp/ferrite-mail-accounts-oauth
//   FERRITE_SFU=0 FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8865 FERRITE_PUBLIC_URL=http://127.0.0.1:8865 ../target/debug/ferrite serve &
//   FERRITE_URL=http://127.0.0.1:8865 node mail-accounts.mjs
//
// Covers: Settings → Mail admin (allow local servers, spam report address, Google sign-in
// client, a provisioning template with the password trade-off explained), adding an account
// with the connection test, quota in the folder list, account order by drag and drop,
// sharing an account (the delegate sees it marked as shared, reads it, and can't remove it),
// a provisioned account appearing after the user signs in, report spam (forwarded as an
// attachment and moved to Junk), connecting a Google account through the provider's consent
// screen (IMAP over XOAUTH2), light/dark/390px screenshots. Prints {ok, errors}; any console
// error or unexpected HTTP error fails the run.
import { chromium } from 'playwright';
import { mkdirSync, writeFileSync, mkdtempSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { createServer } from 'node:http';
import { createServer as createTcpServer } from 'node:net';
import { createHash, randomBytes } from 'node:crypto';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8865';
const shots = process.env.SHOTS || '/tmp/ferrite-mail-accounts-shots';
const IMAP = { host: '127.0.0.1', port: Number(process.env.IMAP_PORT || 3351), dir: process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-accounts' };
const OAUTH_IMAP = { port: Number(process.env.OAUTH_IMAP_PORT || 3361), dir: process.env.OAUTH_MAIL_DIR || '/tmp/ferrite-mail-accounts-oauth' };
const PROVIDER_PORT = Number(process.env.PROVIDER_PORT || 3365);
const SMTP_PORT = Number(process.env.SINK_PORT || 3366);
mkdirSync(shots, { recursive: true });
const here = dirname(fileURLToPath(import.meta.url));
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

async function until(what, fn, timeout = 15000) {
  const end = Date.now() + timeout;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(200);
  }
}

// ---- an SMTP sink that accepts AUTH PLAIN / XOAUTH2 -------------------------------------------
const received = [];
const smtp = createTcpServer((sock) => {
  sock.setEncoding('utf8');
  let buf = '';
  let data = null;
  let msg = { to: [] };
  sock.write('220 sink ESMTP\r\n');
  sock.on('data', (chunk) => {
    buf += chunk;
    let i;
    while ((i = buf.indexOf('\r\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 2);
      if (data !== null) {
        if (line === '.') { received.push({ ...msg, data }); data = null; msg = { to: [] }; sock.write('250 queued\r\n'); } else data += `${line.replace(/^\./, '')}\r\n`;
        continue;
      }
      const up = line.toUpperCase();
      if (up.startsWith('EHLO')) sock.write('250-sink\r\n250-AUTH PLAIN XOAUTH2\r\n250 8BITMIME\r\n');
      else if (up.startsWith('AUTH ')) { msg.auth = line.split(' ')[1]; sock.write('235 ok\r\n'); } else if (up.startsWith('RCPT TO:')) { msg.to.push(line.slice(8).trim().replace(/[<>]/g, '')); sock.write('250 OK\r\n'); } else if (up === 'DATA') { data = ''; sock.write('354 go ahead\r\n'); } else if (up === 'QUIT') { sock.end('221 bye\r\n'); } else sock.write('250 OK\r\n');
    }
  });
  sock.on('error', () => {});
});
await new Promise((r) => smtp.listen(SMTP_PORT, '127.0.0.1', r));

// ---- a mock OAuth 2.0 provider (consent page, token and introspection endpoints) ---------------
const codes = new Map(); // code -> {challenge, redirect, email}
const tokens = new Map(); // access token -> email
const refreshTokens = new Map();
const form = async (req) => Object.fromEntries(new URLSearchParams(await new Promise((r) => { let b = ''; req.on('data', (c) => { b += c; }); req.on('end', () => r(b)); })));
const json = (res, status, v) => { res.writeHead(status, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(v)); };
const esc = (s) => String(s).replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
const provider = createServer(async (req, res) => {
  const url = new URL(req.url, `http://127.0.0.1:${PROVIDER_PORT}`);
  if (url.pathname === '/authorize') {
    const p = Object.fromEntries(url.searchParams);
    if (p.client_id !== 'e2e-client' || p.code_challenge_method !== 'S256') { res.writeHead(400); res.end('bad request'); return; }
    res.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8' });
    res.end(`<!doctype html><title>Mock Google</title><h1>Mock Google</h1><p>Ferrite wants to read and send mail as <b id="who">${esc(p.login_hint)}</b>.</p>
      <form method="post" action="/approve">${['redirect_uri', 'state', 'code_challenge', 'login_hint'].map((k) => `<input type="hidden" name="${k}" value="${esc(p[k])}">`).join('')}<button>Allow</button></form>`);
    return;
  }
  if (url.pathname === '/approve' && req.method === 'POST') {
    const f = await form(req);
    const code = randomBytes(12).toString('hex');
    codes.set(code, { challenge: f.code_challenge, redirect: f.redirect_uri, email: f.login_hint });
    res.writeHead(302, { Location: `${f.redirect_uri}?code=${code}&state=${encodeURIComponent(f.state)}` });
    res.end();
    return;
  }
  if (url.pathname === '/token' && req.method === 'POST') {
    const f = await form(req);
    if (f.client_id !== 'e2e-client' || f.client_secret !== 'e2e-secret') return json(res, 401, { error: 'invalid_client' });
    let email;
    if (f.grant_type === 'authorization_code') {
      const c = codes.get(f.code);
      codes.delete(f.code);
      const ok = c && c.redirect === f.redirect_uri && createHash('sha256').update(f.code_verifier || '').digest('base64url') === c.challenge;
      if (!ok) return json(res, 400, { error: 'invalid_grant' });
      email = c.email;
    } else if (f.grant_type === 'refresh_token' && refreshTokens.has(f.refresh_token)) {
      email = refreshTokens.get(f.refresh_token);
    } else return json(res, 400, { error: 'invalid_grant' });
    const access = randomBytes(16).toString('hex');
    const refresh = randomBytes(16).toString('hex');
    tokens.set(access, email);
    refreshTokens.set(refresh, email);
    return json(res, 200, { access_token: access, refresh_token: refresh, expires_in: 3600, token_type: 'Bearer' });
  }
  if (url.pathname === '/introspect' && req.method === 'POST') {
    const f = await form(req);
    const email = tokens.get(f.token);
    return json(res, 200, email ? { active: true, username: email } : { active: false });
  }
  res.writeHead(404); res.end();
});
await new Promise((r) => provider.listen(PROVIDER_PORT, '127.0.0.1', r));
const mock = `http://127.0.0.1:${PROVIDER_PORT}`;

const browser = await chromium.launch();
const errors = [];
let exitCode = 0;
let current = null;

async function newPage(name, viewport = { width: 1440, height: 900 }, colorScheme = 'light') {
  const ctx = await browser.newContext({ viewport, colorScheme });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  page.on('response', (r) => { if (r.status() >= 400) errors.push(`${name}: HTTP ${r.status()} ${r.request().method()} ${r.url()}`); });
  page.api = async (method, path, data) => {
    const r = await page.request.fetch(`${base}${path}`, { method, data, headers: H });
    const body = (r.headers()['content-type'] || '').includes('json') ? await r.json() : await r.text();
    if (!r.ok()) throw new Error(`${method} ${path} -> ${r.status()} ${JSON.stringify(body)}`);
    return body;
  };
  return page;
}

function deliver(dir, user, subject, id, body) {
  const tmp = mkdtempSync(join(tmpdir(), 'fe-mail-'));
  const file = join(tmp, 'msg.eml');
  writeFileSync(file, `From: Ada <ada@example.org>\nTo: ${user}@example.test\nSubject: ${subject}\nMessage-ID: <${id}>\nDate: ${new Date().toUTCString()}\nMIME-Version: 1.0\nContent-Type: text/plain\n\n${body}\n`);
  execFileSync('bash', [join(here, 'mail-server.sh'), 'deliver', user, file, dir], { stdio: 'inherit' });
}

const exact = (text) => new RegExp(`^${text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`);
const row = (page, subject) => page.locator('.ml-row', { has: page.locator('.ml-subject', { hasText: exact(subject) }) });
const accountHead = (page, email) => page.locator('.ml-account', { has: page.locator('.ml-account-name', { hasText: exact(email) }) });
const accountMenu = async (page, email, item) => {
  await page.getByRole('button', { name: `Account menu for ${email}` }).click();
  await page.getByRole('menuitem', { name: item }).click();
};

try {
  log('empty the test mailboxes, deliver seed mail');
  execFileSync('bash', [join(here, 'mail-server.sh'), 'reset', IMAP.dir], { stdio: 'inherit' });
  execFileSync('bash', [join(here, 'mail-server.sh'), 'reset', OAUTH_IMAP.dir], { stdio: 'inherit' });
  deliver(IMAP.dir, 'alice', 'Team plans', 'plans@e2e.test', 'Shared with the team.');
  deliver(IMAP.dir, 'alice', 'Win a prize', 'spam@e2e.test', 'Click here to claim your prize.');
  deliver(IMAP.dir, 'carol', 'Welcome Carol', 'welcome@e2e.test', 'Your provisioned mailbox works.');
  deliver(OAUTH_IMAP.dir, 'bob', 'Hello from Google', 'google@e2e.test', 'Read over XOAUTH2.');

  const page = await newPage('alice');
  current = page;
  await page.api('POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  await page.api('POST', '/api/admin/users', { username: 'bob', display_name: 'Bob Builder', password: 'bob-password' });
  await page.api('POST', '/api/admin/users', { username: 'carol', display_name: 'Carol Chen', password: 'carol-mail-pw', email: 'carol@example.test' });

  log('Settings → Mail admin');
  await page.goto(`${base}/settings/admin-mail`);
  await page.getByRole('heading', { name: 'Connection security' }).waitFor();
  const localSwitch = page.getByRole('switch', { name: 'Allow mail servers on local and private networks' });
  assert(!(await localSwitch.isChecked()), 'local servers are refused by default');
  assert(await page.getByRole('switch', { name: 'Verify the mail servers’ TLS certificates' }).isChecked(), 'TLS verification is on by default');
  await page.locator('label.set-switch', { hasText: 'Allow mail servers on local and private networks' }).click();
  await page.getByText('Saved').first().waitFor();
  await page.getByLabel('Spam reports to').fill('spam-reports@example.test');
  await page.locator('.set-card', { hasText: 'Spam reporting' }).getByRole('button', { name: 'Save' }).click();
  const google = page.locator('fieldset[data-provider="google"]');
  await google.getByLabel('Client ID').fill('e2e-client');
  await google.getByLabel('Client secret').fill('e2e-secret');
  await google.locator('summary', { hasText: 'Endpoints and servers' }).click();
  await google.getByLabel('Authorization URL').fill(`${mock}/authorize`);
  await google.getByLabel('Token URL').fill(`${mock}/token`);
  await google.getByLabel('IMAP server').fill('127.0.0.1');
  await google.getByLabel('Port').first().fill(String(OAUTH_IMAP.port));
  await google.getByLabel('Security').first().selectOption('none');
  await google.getByLabel('SMTP server').fill('127.0.0.1');
  await google.getByLabel('Port').nth(1).fill(String(SMTP_PORT));
  await google.getByLabel('Security').nth(1).selectOption('none');
  await google.getByRole('button', { name: 'Save Google' }).click();
  await page.getByText('Google sign-in saved').waitFor();
  await until('the Google client to be configured', async () => (await page.api('GET', '/api/admin/mail/settings')).settings.google_secret_set);
  const admin = (await page.api('GET', '/api/admin/mail/settings')).settings;
  assert(admin.allow_local_servers && admin.spam_report_address === 'spam-reports@example.test', JSON.stringify(admin));
  assert(admin.oauth.google.client_secret === '', 'the client secret is not sent back');

  log('a provisioning template (login password mode)');
  await page.getByRole('button', { name: 'New template' }).click();
  const dlg = page.locator('.modal');
  await dlg.getByLabel('Name', { exact: true }).fill('Example staff');
  await dlg.getByLabel('Domain of the user’s email').fill('example.test');
  await dlg.locator('fieldset', { hasText: 'Incoming mail (IMAP)' }).getByLabel('Server').fill('127.0.0.1');
  await dlg.locator('fieldset', { hasText: 'Incoming mail (IMAP)' }).getByLabel('Port').fill(String(IMAP.port));
  await dlg.locator('fieldset', { hasText: 'Incoming mail (IMAP)' }).getByLabel('Security').selectOption('none');
  await dlg.locator('fieldset', { hasText: 'Outgoing mail (SMTP)' }).getByLabel('Server').fill('127.0.0.1');
  await dlg.locator('fieldset', { hasText: 'Outgoing mail (SMTP)' }).getByLabel('Port').fill(String(SMTP_PORT));
  await dlg.locator('fieldset', { hasText: 'Outgoing mail (SMTP)' }).getByLabel('Security').selectOption('none');
  await dlg.getByText('Each user’s sign-in password').click();
  assert(await dlg.getByText('which people may reuse elsewhere').isVisible(), 'the trade-off is explained');
  assert(!(await dlg.getByLabel('Master password', { exact: true }).isVisible()), 'no master password in login mode');
  await page.screenshot({ path: `${shots}/admin-template.png` });
  await dlg.getByRole('button', { name: 'Create and provision' }).click();
  await page.getByText('1 waiting').first().waitFor();
  await page.locator('.set-mail-template', { hasText: 'Example staff' }).getByText('1 waiting for a password sign-in').waitFor();
  // The settings page scrolls inside the app shell: a tall viewport shows all of it.
  await page.setViewportSize({ width: 1440, height: 4200 });
  await page.screenshot({ path: `${shots}/admin-mail.png` });
  await page.setViewportSize({ width: 1440, height: 900 });

  log('add an account with the connection test');
  await page.goto(`${base}/mail`);
  await page.getByRole('button', { name: 'Add mail account' }).click();
  await page.locator('#ml-email').fill('alice@example.test');
  await page.locator('#ml-password').fill('alice-mail-pw');
  await page.locator('#ml-name').fill('Alice Archer');
  await page.getByText('No published settings found').waitFor({ timeout: 20000 });
  await page.locator('#imap-host').fill(IMAP.host);
  await page.locator('#imap-port').fill(String(IMAP.port));
  await page.locator('#imap-security').selectOption('none');
  await page.locator('#imap-port').fill(String(IMAP.port));
  await page.locator('#smtp-host').fill('127.0.0.1');
  await page.locator('#smtp-security').selectOption('none');
  await page.locator('#smtp-port').fill(String(SMTP_PORT));
  await page.getByRole('button', { name: 'Test servers' }).click();
  await page.locator('.ml-test-line.ok', { hasText: 'IMAP: signed in' }).waitFor();
  await page.locator('.ml-test-line.ok', { hasText: 'SMTP: signed in' }).waitFor();
  assert(await page.getByRole('button', { name: 'Sign in with Google' }).isVisible(), 'the Google button shows when configured');
  await page.screenshot({ path: `${shots}/account-setup.png` });
  await page.getByRole('button', { name: 'Connect' }).click();
  await row(page, 'Team plans').waitFor({ timeout: 20000 });
  const accounts = (await page.api('GET', '/api/mail/accounts')).accounts;
  const acc = accounts[0];
  await page.locator('.ml-quota', { hasText: 'of 10 MB used' }).waitFor();

  log('a second account, reordered by drag and drop');
  const second = (await page.api('POST', '/api/mail/accounts', {
    email: 'dave@example.test', name: 'Alice (Dave box)', password: 'dave-mail-pw', imap_host: IMAP.host, imap_port: IMAP.port, imap_security: 'none',
    smtp_host: '127.0.0.1', smtp_port: SMTP_PORT, smtp_security: 'none',
  })).account;
  await accountHead(page, 'dave@example.test').waitFor();
  await accountHead(page, 'dave@example.test').dragTo(accountHead(page, 'alice@example.test'));
  await until('the new order', async () => (await page.api('GET', '/api/mail/accounts')).accounts[0].id === second.id);
  assert((await page.locator('.ml-account-name').first().textContent()) === 'dave@example.test', 'dave first in the list');
  await accountMenu(page, 'dave@example.test', 'Move down');
  await until('the order back', async () => (await page.api('GET', '/api/mail/accounts')).accounts[0].id === acc.id);

  log('share the account with Bob');
  await accountMenu(page, 'alice@example.test', 'Share…');
  await page.locator('#ml-share-user').fill('bob');
  await page.locator('.ml-share-form').getByRole('button', { name: 'Share' }).click();
  await page.locator('.ml-share-row[data-user="bob"]').waitFor();
  await page.screenshot({ path: `${shots}/share-dialog.png` });
  await page.getByRole('button', { name: 'Done' }).click();

  log('report spam: forwarded as an attachment, moved to Junk');
  await row(page, 'Win a prize').click();
  await page.locator('.ml-card.expanded .ml-text').waitFor();
  await page.locator('.ml-actions').getByRole('button', { name: 'More actions' }).click();
  await page.getByRole('menuitem', { name: 'Report spam' }).click();
  await page.getByText('Reported as spam: 1 message').waitFor();
  const report = await until('the spam report', () => received.find((m) => m.to.includes('spam-reports@example.test')));
  assert(/message\/rfc822/.test(report.data) && report.data.includes('Click here to claim your prize.'), 'the original is attached');
  const junk = (await page.api('GET', `/api/mail/accounts/${acc.id}/folders`)).folders.find((f) => f.role === 'junk');
  await page.locator(`.ml-folder[data-id="${junk.id}"]`).click();
  await row(page, 'Win a prize').waitFor({ timeout: 20000 });

  log('Bob sees the shared account');
  const bob = await newPage('bob');
  current = bob;
  await bob.api('POST', '/api/auth/login', { username: 'bob', password: 'bob-password' });
  await bob.goto(`${base}/mail`);
  await accountHead(bob, 'alice@example.test').waitFor();
  await bob.locator('.ml-account-owner', { hasText: 'Shared by Alice Archer' }).waitFor();
  await row(bob, 'Team plans').click();
  await bob.locator('.ml-card.expanded .ml-text', { hasText: 'Shared with the team.' }).waitFor();
  await bob.getByRole('button', { name: 'Account menu for alice@example.test' }).click();
  assert(!(await bob.getByRole('menuitem', { name: 'Remove account' }).count()), 'delegates cannot remove the account');
  assert(!(await bob.getByRole('menuitem', { name: 'Share…' }).count()), 'delegates cannot share it further');
  await bob.getByRole('menuitem', { name: 'Edit account' }).click();
  await bob.getByText('shared this account with you').waitFor();
  assert(!(await bob.locator('#ml-password').count()), 'no password field for delegates');
  await bob.screenshot({ path: `${shots}/delegate-edit.png` });
  await bob.getByRole('button', { name: 'Cancel' }).click();
  await bob.screenshot({ path: `${shots}/delegate-mail.png` });

  log('Bob connects a Google account through the consent screen');
  await bob.getByRole('button', { name: 'Add account' }).click();
  await bob.locator('#ml-email').fill('bob@example.test');
  await bob.getByRole('button', { name: 'Sign in with Google' }).click();
  await bob.getByRole('heading', { name: 'Mock Google' }).waitFor();
  assert((await bob.locator('#who').textContent()) === 'bob@example.test', 'login hint passed');
  await bob.getByRole('button', { name: 'Allow' }).click();
  await bob.waitForURL(/\/mail/);
  await bob.getByText('Account connected').waitFor();
  await accountHead(bob, 'bob@example.test').waitFor();
  const bobAccounts = (await bob.api('GET', '/api/mail/accounts')).accounts;
  const g = bobAccounts.find((a) => a.email === 'bob@example.test');
  assert(g && g.auth_method === 'xoauth2' && g.oauth_provider === 'google', JSON.stringify(bobAccounts));
  const gInbox = (await bob.api('GET', `/api/mail/accounts/${g.id}/folders`)).folders.find((f) => f.role === 'inbox');
  await bob.locator(`.ml-folder[data-id="${gInbox.id}"]`).click();
  await row(bob, 'Hello from Google').waitFor({ timeout: 20000 });
  assert(tokens.size >= 1, 'an access token was issued');

  log('Carol signs in and finds her provisioned account');
  const carol = await newPage('carol');
  current = carol;
  await carol.goto(`${base}/`);
  await carol.getByLabel('Username').fill('carol');
  await carol.getByLabel('Password').fill('carol-mail-pw');
  await carol.getByRole('button', { name: /sign in/i }).click();
  await carol.locator('a[data-nav="settings"]').waitFor();
  await carol.goto(`${base}/mail`);
  await row(carol, 'Welcome Carol').waitFor({ timeout: 20000 });
  assert(await carol.locator('.ml-account-badge[title="Set up by your administrator"]').count(), 'provisioned marker');
  await carol.getByRole('button', { name: 'Account menu for carol@example.test' }).click();
  assert(!(await carol.getByRole('menuitem', { name: 'Remove account' }).count()), 'provisioned accounts cannot be removed');
  await carol.getByRole('menuitem', { name: 'Edit account' }).click();
  await carol.getByText('Your administrator set up this account').waitFor();
  await carol.locator('#ml-signature').fill('Carol C.');
  await carol.getByRole('button', { name: 'Save' }).click();
  await carol.locator('.modal').waitFor({ state: 'detached' });
  const ca = (await carol.api('GET', '/api/mail/accounts')).accounts[0];
  assert(ca.signature === 'Carol C.' && ca.provisioning_id, JSON.stringify(ca));
  await carol.context().close();

  log('dark and phone layouts');
  const dark = await newPage('dark', { width: 1280, height: 860 }, 'dark');
  current = dark;
  await dark.api('POST', '/api/auth/login', { username: 'bob', password: 'bob-password' });
  await dark.goto(`${base}/mail`);
  await row(dark, 'Team plans').waitFor();
  await dark.screenshot({ path: `${shots}/mail-accounts-dark.png` });
  await dark.context().close();
  const adminDark = await newPage('admin-dark', { width: 1280, height: 4200 }, 'dark');
  current = adminDark;
  await adminDark.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await adminDark.goto(`${base}/settings/admin-mail`);
  await adminDark.locator('.set-mail-template').first().waitFor();
  await adminDark.screenshot({ path: `${shots}/admin-mail-dark.png` });
  await adminDark.context().close();
  const phone = await newPage('phone', { width: 390, height: 844 });
  current = phone;
  await phone.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await phone.goto(`${base}/settings/admin-mail`);
  await phone.locator('.set-mail-template').first().waitFor();
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll on the admin page');
  await phone.setViewportSize({ width: 390, height: 7400 });
  await phone.screenshot({ path: `${shots}/admin-mail-phone.png` });
  await phone.setViewportSize({ width: 390, height: 844 });
  await phone.goto(`${base}/mail`);
  await row(phone, 'Team plans').waitFor();
  await phone.getByRole('button', { name: 'Folders' }).click();
  await phone.waitForTimeout(400);
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll in Mail');
  await phone.screenshot({ path: `${shots}/mail-accounts-phone.png` });
  await phone.context().close();
  await bob.context().close();

  if (errors.length) throw new Error(`console errors: ${errors.join(' | ')}`);
  console.log(JSON.stringify({ ok: true, errors }));
} catch (e) {
  console.error(e);
  if (current) await current.screenshot({ path: `${shots}/failure.png` }).catch(() => {});
  console.log(JSON.stringify({ ok: false, errors }));
  exitCode = 1;
} finally {
  await browser.close();
  smtp.close();
  provider.close();
  process.exit(exitCode);
}
