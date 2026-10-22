// Browser test of Web Push: the service worker and manifest, Settings → Notifications
// (permission handling, devices, categories, test push), real pushes from the server to a
// mock push service (decrypted here with Node's own crypto), and push handling in the service
// worker: pushes are delivered over the DevTools protocol (headless Chromium has no push
// service), shown or suppressed, and notification clicks navigate the app.
//
// Run against a server with a fresh data dir and local endpoints allowed:
//   FERRITE_PUSH_ALLOW_INSECURE=1 FERRITE_SFU=0 FERRITE_DATA_DIR=$(mktemp -d) \
//     FERRITE_LISTEN=127.0.0.1:8834 FERRITE_PUBLIC_URL=http://127.0.0.1:8834 ferrite serve &
//   FERRITE_URL=http://127.0.0.1:8834 SHOTS=/some/dir node push.mjs
import { chromium, request } from 'playwright';
import crypto from 'node:crypto';
import http from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8834';
const shots = process.env.SHOTS || '/tmp';
const T = 15000;
const errors = [];
const failed = [];
const step = (s) => console.log(`- ${s}`);
const H = { 'X-Ferrite-Request': '1' };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function assert(cond, msg) {
  if (!cond) throw new Error(`assertion failed: ${msg}`);
}

async function waitFor(fn, what, timeout = T) {
  const end = Date.now() + timeout;
  for (;;) {
    const v = await fn();
    if (v) return v;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(100);
  }
}

// ---- a mock push service and subscription keys --------------------------------------------
const b64u = (buf) => Buffer.from(buf).toString('base64url');
const pushes = [];
const mock = http.createServer((req, res) => {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    const device = req.url.split('/').pop();
    pushes.push({ device, headers: req.headers, body: Buffer.concat(chunks) });
    if (device.startsWith('gone')) { res.writeHead(410); res.end(); return; }
    res.writeHead(201, { Location: `/m/${device}` });
    res.end();
  });
});
await new Promise((r) => mock.listen(0, '127.0.0.1', r));
const mockOrigin = `http://127.0.0.1:${mock.address().port}`;

function newDevice(name) {
  const ecdh = crypto.createECDH('prime256v1');
  ecdh.generateKeys();
  const auth = crypto.randomBytes(16);
  return {
    name, ecdh, auth, pub: ecdh.getPublicKey(),
    endpoint: `${mockOrigin}/push/${name}`,
    json() { return { endpoint: this.endpoint, expirationTime: null, keys: { p256dh: b64u(this.pub), auth: b64u(this.auth) } }; },
  };
}

/** RFC 8291 / RFC 8188 decryption, independent of the server's implementation. */
function decrypt(body, dev) {
  const salt = body.subarray(0, 16);
  const idlen = body[20];
  const asPub = body.subarray(21, 21 + idlen);
  const ct = body.subarray(21 + idlen);
  assert(body.readUInt32BE(16) >= ct.length, 'single record');
  const secret = dev.ecdh.computeSecret(asPub);
  const keyInfo = Buffer.concat([Buffer.from('WebPush: info\0'), dev.pub, asPub]);
  const ikm = Buffer.from(crypto.hkdfSync('sha256', secret, dev.auth, keyInfo, 32));
  const cek = Buffer.from(crypto.hkdfSync('sha256', ikm, salt, Buffer.from('Content-Encoding: aes128gcm\0'), 16));
  const nonce = Buffer.from(crypto.hkdfSync('sha256', ikm, salt, Buffer.from('Content-Encoding: nonce\0'), 12));
  const d = crypto.createDecipheriv('aes-128-gcm', cek, nonce);
  d.setAuthTag(ct.subarray(ct.length - 16));
  const plain = Buffer.concat([d.update(ct.subarray(0, ct.length - 16)), d.final()]);
  let end = plain.length - 1;
  while (end >= 0 && plain[end] === 0) end--;
  assert(plain[end] === 2, 'padding delimiter');
  return JSON.parse(plain.subarray(0, end).toString());
}

/** Checks the VAPID header as a push service would (RFC 8292). */
function checkVapid(p, publicKey) {
  const m = /^vapid t=([^,]+), k=(.+)$/.exec(p.headers.authorization || '');
  assert(m, `vapid authorization header: ${p.headers.authorization}`);
  const [h, c, s] = m[1].split('.');
  assert(m[2] === publicKey, 'k is the published key');
  const k = Buffer.from(m[2], 'base64url');
  const key = crypto.createPublicKey({ key: { kty: 'EC', crv: 'P-256', x: b64u(k.subarray(1, 33)), y: b64u(k.subarray(33)) }, format: 'jwk' });
  assert(crypto.verify('sha256', Buffer.from(`${h}.${c}`), { key, dsaEncoding: 'ieee-p1363' }, Buffer.from(s, 'base64url')), 'JWT signature verifies');
  const claims = JSON.parse(Buffer.from(c, 'base64url'));
  assert(claims.aud === mockOrigin, `aud is the endpoint origin (${claims.aud})`);
  const now = Date.now() / 1000;
  assert(claims.exp > now && claims.exp <= now + 86400, 'exp within 24h');
  assert(p.headers['content-encoding'] === 'aes128gcm', 'aes128gcm');
}

async function nextPush(device, after) {
  return waitFor(() => pushes.slice(after).find((p) => p.device === device), `a push to ${device}`);
}

// ---- browser and accounts -------------------------------------------------------------------
function watch(p, who) {
  p.on('console', (m) => { if (m.type() === 'error') errors.push(`${who}: ${m.text()}`); });
  p.on('pageerror', (e) => errors.push(`${who}: pageerror ${e.message}`));
  p.on('response', (r) => { if (r.status() >= 400) failed.push(`${who}: ${r.status()} ${r.request().method()} ${new URL(r.url()).pathname}`); });
}

const profile = mkdtempSync(join(tmpdir(), 'fe-push-'));
// A persistent profile: Chromium refuses the Push API outright in incognito contexts. The new
// headless mode (channel "chromium") is needed for notifications: the headless shell reports
// them as denied whatever is granted. The "blocked" state is checked in a separate context.
const context = await chromium.launchPersistentContext(profile, { channel: 'chromium', viewport: { width: 1280, height: 860 }, permissions: ['notifications'] });
context.on('weberror', (e) => errors.push(`weberror ${e.error().message}`));
const page = context.pages()[0] || await context.newPage();
watch(page, 'ada');

async function api(ctx, method, path, data) {
  const r = await ctx.fetch(base + path, { method, headers: H, data });
  if (!r.ok()) throw new Error(`${method} ${path}: ${r.status()} ${await r.text()}`);
  return r.json();
}

let exitCode = 1;
try {
  step('accounts');
  await api(context.request, 'POST', '/api/setup', { username: 'admin', password: 'admin-password', display_name: 'Ada Admin' });
  await api(context.request, 'POST', '/api/admin/users', { username: 'bob', password: 'bob-password', display_name: 'Bob Builder' });
  const bob = await request.newContext();
  await api(bob, 'POST', '/api/auth/login', { username: 'bob', password: 'bob-password' });
  const bobRoot = (await api(bob, 'GET', '/api/me')).root_id;
  async function bobShares(name) {
    const r = await bob.fetch(`${base}/api/files/${bobRoot}/upload?name=${name}`, { method: 'PUT', headers: H, data: Buffer.from('hello') });
    assert(r.ok(), 'upload');
    const node = (await r.json()).node;
    await api(bob, 'POST', `/api/files/${node.id}/shares`, { kind: 'user', target: 'admin' });
    return node;
  }

  step('service worker, manifest and icons');
  await page.goto(`${base}/settings/notifications`);
  await page.getByRole('heading', { name: 'This device' }).waitFor({ timeout: T });
  const reg = await page.evaluate(async () => {
    const r = await navigator.serviceWorker.ready;
    return { scope: r.scope, script: r.active && r.active.scriptURL };
  });
  assert(reg.scope === `${base}/` && reg.script === `${base}/sw.js`, `root-scoped /sw.js: ${JSON.stringify(reg)}`);
  const swResp = await context.request.get(`${base}/sw.js`);
  assert(/javascript/.test(swResp.headers()['content-type']), 'sw.js is JavaScript');
  assert(/script-src 'self'/.test(swResp.headers()['content-security-policy'] || ''), 'sw.js has its own CSP');
  const manifest = await (await context.request.get(`${base}/manifest.webmanifest`)).json();
  assert(manifest.display === 'standalone' && manifest.start_url === '/', 'installable manifest');
  for (const icon of manifest.icons) {
    const r = await context.request.get(base + icon.src);
    assert(r.ok() && /image\//.test(r.headers()['content-type']), `icon ${icon.src}`);
  }
  assert(manifest.icons.some((i) => i.purpose === 'maskable' && i.sizes === '512x512'), 'maskable icon');
  assert(await page.evaluate(() => !!document.querySelector('link[rel="manifest"]') && !!document.querySelector('link[rel="apple-touch-icon"]')), 'manifest linked');

  step('permission not granted: the UI explains instead of failing');
  const plain = await chromium.launch();
  const blockedCtx = await plain.newContext({ viewport: { width: 1280, height: 860 } });
  await api(blockedCtx.request, 'POST', '/api/auth/login', { username: 'admin', password: 'admin-password' });
  const blocked = await blockedCtx.newPage();
  watch(blocked, 'blocked');
  await blocked.goto(`${base}/settings/notifications`);
  await blocked.getByRole('heading', { name: 'This device' }).waitFor({ timeout: T });
  const blockedButton = blocked.getByRole('button', { name: 'Turn on push notifications' });
  if (await blocked.evaluate(async () => (await navigator.permissions.query({ name: 'notifications' })).state) === 'denied') {
    // Headless Chromium denies notifications unless granted.
    await blocked.getByText('Notifications are blocked for this site.').waitFor({ timeout: T });
    assert(!(await blockedButton.count()), 'no subscribe button while blocked');
  } else {
    await blockedButton.click();
    await blocked.locator('.toast.error').filter({ hasText: /blocked|not granted/ }).waitFor({ timeout: T });
  }
  await blocked.screenshot({ path: `${shots}/push-blocked.png` });
  await plain.close();

  step('permission granted: subscribe (or report a push service that cannot be reached)');
  const turnOn = page.getByRole('button', { name: 'Turn on push notifications' });
  await turnOn.click();
  const outcome = await Promise.race([
    page.locator('.toast.error').filter({ hasText: 'could not reach its push service' }).waitFor({ timeout: T }).then(() => 'no-push-service'),
    page.getByText('This device receives push notifications.').waitFor({ timeout: T }).then(() => 'subscribed'),
  ]);
  step(`  outcome: ${outcome}`);
  await page.locator('.toast .btn.ghost.icon').first().click();
  if (outcome === 'subscribed') {
    // The browser reached a real push service: this device is listed; turn it off again so
    // the rest of the test only talks to the mock push service.
    await page.locator('.set-push-device').filter({ hasText: 'This device' }).waitFor({ timeout: T });
    await page.getByRole('button', { name: 'Turn off on this device' }).click();
    await turnOn.waitFor({ timeout: T });
    assert((await api(context.request, 'GET', '/api/push')).subscriptions.length === 0, 'turned off server side');
    assert(!(await page.evaluate(async () => (await navigator.serviceWorker.ready).pushManager.getSubscription())), 'and in the browser');
  }

  step('a device (subscribed against the mock push service) and the device list');
  const overview = await api(context.request, 'GET', '/api/push');
  const publicKey = overview.public_key;
  const laptop = newDevice('laptop');
  const added = await page.evaluate(async (body) => {
    const r = await fetch('/api/push/subscriptions', { method: 'POST', headers: { 'Content-Type': 'application/json', 'X-Ferrite-Request': '1' }, body: JSON.stringify(body) });
    return r.json();
  }, laptop.json());
  assert(added.subscription.label === 'Chrome on Linux', `label from the user agent: ${added.subscription.label}`);
  await page.reload();
  const row = page.locator('.set-push-device').filter({ hasText: '127.0.0.1' });
  await row.waitFor({ timeout: T });
  assert((await row.textContent()).includes('Chrome on Linux'), 'device label shown');

  step('categories: files & shares off → no push; on → pushed and decrypted');
  const filesSwitch = page.locator('input[data-pref="files"]');
  assert(await filesSwitch.isChecked(), 'files on by default');
  let saved = page.waitForResponse((r) => r.url().endsWith('/api/push/settings') && r.request().method() === 'PUT');
  await page.locator('.set-push-pref').filter({ hasText: 'Files & shares' }).locator('label').click();
  assert((await (await saved).json()).settings.files === false, 'files saved off');
  let mark = pushes.length;
  await bobShares('muted.txt');
  await sleep(1500);
  assert(!pushes.slice(mark).some((p) => p.device === 'laptop'), 'no push while files is off');
  saved = page.waitForResponse((r) => r.url().endsWith('/api/push/settings') && r.request().method() === 'PUT');
  await page.locator('.set-push-pref').filter({ hasText: 'Files & shares' }).locator('label').click();
  assert((await (await saved).json()).settings.files === true, 'files saved on');
  // The master switch disables the categories in the UI.
  saved = page.waitForResponse((r) => r.url().endsWith('/api/push/settings') && r.request().method() === 'PUT');
  await page.locator('.set-push-master label').click();
  await saved;
  await waitFor(() => page.locator('input[data-pref="chat"]').isDisabled(), 'categories disabled while push is off');
  saved = page.waitForResponse((r) => r.url().endsWith('/api/push/settings') && r.request().method() === 'PUT');
  await page.locator('.set-push-master label').click();
  await saved;
  await waitFor(async () => !(await page.locator('input[data-pref="chat"]').isDisabled()), 'categories enabled again');
  mark = pushes.length;
  const shared = await bobShares('plan.txt');
  const sharePush = await nextPush('laptop', mark);
  checkVapid(sharePush, publicKey);
  assert(sharePush.headers.ttl === '86400' && sharePush.headers.urgency === 'normal', 'TTL and urgency');
  const share = decrypt(sharePush.body, laptop);
  assert(share.title === 'Bob Builder shared “plan.txt” with you', `share title: ${share.title}`);
  assert(share.url === `/files?node=${shared.id}`, 'share url');

  step('send test notification');
  mark = pushes.length;
  await page.getByRole('button', { name: 'Send test notification' }).click();
  await page.locator('.toast.ok').filter({ hasText: 'Test notification sent to 1 device' }).waitFor({ timeout: T });
  const test = decrypt((await nextPush('laptop', mark)).body, laptop);
  assert(test.kind === 'test' && test.title === 'Ferrite test notification', 'test payload');

  step('incoming call: Bob starts a call in Ops, Ada is rung');
  const ops = (await api(bob, 'POST', '/api/talk/rooms', { name: 'Ops', invite: ['admin'] })).room;
  const bobToken = (await api(bob, 'POST', '/api/me/tokens', { name: 'ws' })).secret;
  mark = pushes.length;
  const ws = new WebSocket(`${base.replace('http', 'ws')}/api/ws?token=${encodeURIComponent(bobToken)}`);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  ws.send(JSON.stringify({ type: 'call.join', room: ops.token, audio: true }));
  const callPush = await nextPush('laptop', mark);
  assert(callPush.headers.urgency === 'high' && callPush.headers.ttl === '60' && callPush.headers.topic, 'call urgency, TTL and topic');
  const call = decrypt(callPush.body, laptop);
  assert(call.kind === 'call' && call.title === 'Bob Builder is calling in Ops', `call title: ${call.title}`);
  assert(call.actions.map((a) => a.title).join() === 'Join,Dismiss', 'Join/Dismiss actions');
  ws.close();

  step('service worker: pushes delivered over the DevTools protocol');
  const sw = context.serviceWorkers().find((w) => w.url() === `${base}/sw.js`) || await context.waitForEvent('serviceworker');
  const cdp = await context.newCDPSession(page);
  const registrationId = await new Promise((resolve) => {
    cdp.on('ServiceWorker.workerRegistrationUpdated', ({ registrations }) => {
      const r = registrations.find((x) => x.scopeURL === `${base}/` && !x.isDeleted);
      if (r) resolve(r.registrationId);
    });
    cdp.send('ServiceWorker.enable');
  });
  const deliver = (payload) => cdp.send('ServiceWorker.deliverPushMessage', { origin: base, registrationId, data: JSON.stringify(payload) });
  const shown = () => sw.evaluate(async () => (await self.registration.getNotifications()).map((n) => ({
    title: n.title, body: n.body, tag: n.tag, data: n.data, requireInteraction: n.requireInteraction,
    actions: (n.actions || []).map((a) => `${a.action}:${a.title}`).join(','),
  })));
  const click = (tag, action) => sw.evaluate(async ([t, a]) => {
    const [n] = await self.registration.getNotifications({ tag: t });
    self.dispatchEvent(new NotificationEvent('notificationclick', { notification: n, action: a || '' }));
  }, [tag, action]);

  // A focused Ferrite window already has the in-app notification: nothing is shown…
  await deliver(share);
  await sleep(1000);
  assert(!(await shown()).some((n) => n.tag === share.tag), 'suppressed while a Ferrite window is focused');
  // …except test pushes.
  await deliver(test);
  await waitFor(async () => (await shown()).some((n) => n.title === 'Ferrite test notification'), 'test notification');
  await sw.evaluate(async () => (await self.registration.getNotifications()).forEach((n) => n.close()));

  // No Ferrite window: the share and the call are shown.
  await page.goto('about:blank');
  await deliver(share);
  await deliver(call);
  const list = await waitFor(async () => { const l = await shown(); return l.length >= 2 && l; }, 'two notifications');
  const shareN = list.find((n) => n.tag === share.tag);
  const callN = list.find((n) => n.tag === call.tag);
  assert(shareN && shareN.title === share.title && shareN.data.url === share.url, 'share notification');
  assert(callN && callN.requireInteraction && callN.actions === 'join:Join,dismiss:Dismiss', `call notification: ${JSON.stringify(callN)}`);

  step('notification clicks');
  const pagesBefore = context.pages().length;
  await click(call.tag, 'dismiss');
  await waitFor(async () => !(await shown()).some((n) => n.tag === call.tag), 'call notification dismissed');
  await sleep(500);
  assert(context.pages().length === pagesBefore, 'Dismiss opens nothing');
  // With a Ferrite window open, a click focuses it and routes in place (no reload).
  await page.goto(`${base}/settings/notifications`);
  await page.getByRole('heading', { name: 'This device' }).waitFor({ timeout: T });
  await page.evaluate(() => { window.__noReload = true; });
  await click(share.tag);
  await page.waitForURL(`${base}${share.url}`, { timeout: T });
  assert(await page.evaluate(() => window.__noReload === true), 'navigated without a reload');
  await page.locator('.nav a.active[data-nav="files"]').waitFor({ timeout: T });
  await page.locator(`tr[data-id="${shared.id}"]`).waitFor({ timeout: T }); // Files finished loading, with the shared file
  const notes = (await api(context.request, 'GET', '/api/notifications')).notifications;
  assert(notes.find((n) => n.id === share.notification_id)?.read === true, 'clicked notification marked read');

  step('410 Gone removes a device; screenshots; removing a device in the UI');
  const old = newDevice('gone-tablet');
  await api(context.request, 'POST', '/api/push/subscriptions', old.json());
  await page.goto(`${base}/settings/notifications`);
  await page.locator('.set-push-device').nth(1).waitFor({ timeout: T });
  await page.getByRole('button', { name: 'Send test notification' }).click();
  await page.locator('.toast.ok').filter({ hasText: 'Test notification sent to 1 device' }).waitFor({ timeout: T });
  await page.locator('.set-push-results .set-push-err').filter({ hasText: 'no longer knows this device' }).waitFor({ timeout: T });
  await waitFor(async () => (await page.locator('.set-push-device').count()) === 1, 'gone device removed from the list');
  assert((await api(context.request, 'GET', '/api/push')).subscriptions.length === 1, 'one device left');
  await page.locator('.toast .btn.ghost.icon').first().click();
  await page.screenshot({ path: `${shots}/push-settings.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.waitForTimeout(600); // let the layout settle after the resize
  await page.screenshot({ path: `${shots}/push-settings-phone-dark.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'light' });
  await page.setViewportSize({ width: 1280, height: 860 });
  await page.getByRole('button', { name: 'Remove Chrome on Linux' }).click();
  await page.locator('.modal').getByRole('button', { name: 'Remove' }).click();
  await page.getByText('No devices receive push notifications yet.').waitFor({ timeout: T });
  assert((await api(context.request, 'GET', '/api/push')).subscriptions.length === 0, 'device removed');

  await bob.dispose();
  const unexpected = failed.filter((f) => !/^4\d\d (GET|POST) \/api\/push\/test$/.test(f));
  console.log(JSON.stringify({ ok: errors.length === 0 && unexpected.length === 0, outcome, errors, failed: unexpected }));
  exitCode = errors.length || unexpected.length ? 1 : 0;
} catch (e) {
  console.error(e);
  console.log(JSON.stringify({ ok: false, errors, failed }));
  await page.screenshot({ path: `${shots}/push-failure.png` }).catch(() => {});
} finally {
  await context.close();
  mock.close();
  rmSync(profile, { recursive: true, force: true });
}
process.exit(exitCode);
