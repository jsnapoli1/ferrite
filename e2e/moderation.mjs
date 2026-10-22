// Browser test of Talk moderation and breakout rooms: the lobby (waiting screen, who waits,
// live opening), the room password for guests, call moderation (raised-hands queue, ask to
// mute, lower a hand, mute everyone, remove and rejoin, who may start calls) and breakout
// rooms (manual assignment, live moves of chat and call for users and a guest, broadcast,
// ask for help, stop and return, readable breakout chat afterwards). Takes light, dark and
// phone-width screenshots.
//
// Run against a server with the SFU enabled and a fresh data dir:
//   FERRITE_URL=http://127.0.0.1:8831 SHOTS=/some/dir node moderation.mjs
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8831';
const shots = process.env.SHOTS || '/tmp';
const T = 20000;
const errors = [];
const failed = []; // HTTP responses >= 400, to explain tolerated 'Failed to load resource' errors
let currentStep = '';
const step = (s) => { currentStep = s; console.log(`- ${s}`); };
const H = { 'X-Ferrite-Request': '1' };

const browser = await chromium.launch({
  args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'],
});

const logs = []; // warnings and toasts, printed when the test fails
function watch(page, who) {
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(`${who}: ${m.text()}`);
    else if (m.type() === 'warning' || m.text().startsWith('toast:')) logs.push(`${who}: ${m.text()}`);
  });
  page.addInitScript(() => {
    new MutationObserver((records) => {
      for (const r of records) for (const n of r.addedNodes) if (n.classList?.contains('toast')) console.log(`toast: ${n.textContent}`);
    }).observe(document, { childList: true, subtree: true });
  });
  page.on('pageerror', (e) => errors.push(`${who}: pageerror ${e.message}`));
  page.on('response', (r) => { if (r.status() >= 400) failed.push(`${who}: ${r.status()} ${r.request().method()} ${new URL(r.url()).pathname}`); });
}

async function api(ctx, method, path, data) {
  const r = await ctx.request.fetch(base + path, { method, headers: H, data });
  if (!r.ok()) throw new Error(`${method} ${path}: ${r.status()} ${await r.text()}`);
  return r.json();
}

async function userPage(name, viewport = { width: 1280, height: 820 }) {
  const ctx = await browser.newContext({ viewport });
  await ctx.grantPermissions(['camera', 'microphone'], { origin: base });
  const page = await ctx.newPage();
  watch(page, name);
  await api(ctx, 'POST', '/api/auth/login', { username: name, password: `${name}-password` });
  return { ctx, page };
}

const msg = (page, text) => page.locator('.msg', { hasText: text }).last();
/** A breakout room's card in the panel, by its exact name. */
const boRoom = (page, name) => page.locator('.bo-room[data-token]', { has: page.locator('.bo-room-head strong', { hasText: new RegExp(`^${name}$`) }) });
const callOf = (page) => page.evaluate(() => {
  const c = window.__ferriteCall;
  return c && c.session ? { roomId: c.roomId, session: c.session, members: c.members.size, audio: c.state.audio, hand: c.state.hand } : null;
});
async function waitFor(page, fn, arg, timeout = T) {
  await page.waitForFunction(fn, arg, { timeout, polling: 200 });
}
/** Waits until the page is in a call of `roomId` with `n` others. */
async function inCall(page, roomId, n) {
  await waitFor(page, ([r, k]) => {
    const c = window.__ferriteCall;
    return c && c.session && c.roomId === r && c.members.size === k;
  }, [roomId, n]);
}
async function shot(page, name, { dark = false } = {}) {
  if (dark) await page.emulateMedia({ colorScheme: 'dark' });
  await page.screenshot({ path: `${shots}/${name}.png` });
  if (dark) await page.emulateMedia({ colorScheme: 'light' });
}
async function noOverflow(page, what) {
  if (await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth)) throw new Error(`horizontal overflow: ${what}`);
}

let exitCode = 0;
try {
  step('setup: alice (owner), bob, carol and a public conversation');
  const setup = await browser.newContext();
  const status = await (await setup.request.get(`${base}/api/status`)).json();
  if (status.setup_needed) await api(setup, 'POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  else await api(setup, 'POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  for (const [u, n] of [['bob', 'Bob Builder'], ['carol', 'Carol Cooper']]) {
    await setup.request.post(`${base}/api/admin/users`, { headers: H, data: { username: u, display_name: n, password: `${u}-password` } });
  }
  const { room } = await api(setup, 'POST', '/api/talk/rooms', { kind: 'public', name: 'Workshop', invite: ['bob', 'carol'] });
  const token = room.token;
  const roomId = room.id;
  const A = await userPage('alice');
  const B = await userPage('bob');
  const C = await userPage('carol');
  const alice = A.page;
  const bob = B.page;
  const carol = C.page;
  for (const p of [alice, bob, carol]) await p.goto(`${base}/talk/${token}`);
  await alice.getByLabel('Write a message').waitFor();
  await bob.getByLabel('Write a message').waitFor();

  // ---- lobby and password -------------------------------------------------------------------
  step('alice turns the lobby on and sets a password in the moderation dialog');
  await alice.getByRole('button', { name: 'Conversation menu' }).click();
  await alice.getByRole('menuitem', { name: 'Moderation…' }).click();
  await alice.getByLabel('Hold participants in the lobby').check();
  await alice.getByLabel('Conversation password').fill('open sesame');
  await shot(alice, 'moderation-dialog-light');
  await shot(alice, 'moderation-dialog-dark', { dark: true });
  await alice.getByRole('button', { name: 'Save', exact: true }).click();
  await alice.locator('.talk-lobby-banner', { hasText: 'Lobby is on' }).waitFor();

  step('bob is moved to the waiting screen live; alice sees him waiting');
  await bob.getByRole('heading', { name: 'You are in the lobby' }).waitFor({ timeout: T });
  if (await bob.getByLabel('Write a message').isVisible()) throw new Error('waiting bob still sees the composer');
  if (await bob.locator('.msg').count()) throw new Error('waiting bob still sees messages');
  await alice.locator('.talk-lobby-banner', { hasText: '2 waiting' }).waitFor({ timeout: T });
  await alice.getByRole('button', { name: '2 waiting' }).click();
  await alice.locator('.talk-side-lobby .talk-person', { hasText: 'Bob Builder' }).waitFor();
  await shot(alice, 'lobby-moderator-light');
  await shot(bob, 'lobby-waiting-light');
  await shot(bob, 'lobby-waiting-dark', { dark: true });
  await alice.getByRole('button', { name: 'Close participants' }).click();

  step('a guest needs the password, then waits in the lobby too (phone width)');
  const G = await browser.newContext({ viewport: { width: 390, height: 780 } });
  await G.grantPermissions(['camera', 'microphone'], { origin: base });
  const guest = await G.newPage();
  watch(guest, 'guest');
  await guest.goto(`${base}/join/${token}`);
  await guest.getByLabel('Your name').fill('Gil');
  await guest.getByLabel('Conversation password').fill('wrong');
  await guest.getByRole('button', { name: 'Join conversation' }).click();
  await guest.locator('.error-text', { hasText: 'wrong password' }).waitFor({ timeout: T });
  await guest.getByLabel('Conversation password').fill('open sesame');
  await shot(guest, 'guest-password-mobile');
  await guest.getByRole('button', { name: 'Join conversation' }).click();
  await guest.getByRole('heading', { name: 'You are in the lobby' }).waitFor({ timeout: T });
  await noOverflow(guest, 'guest waiting screen');
  await shot(guest, 'lobby-waiting-mobile');
  // The guest survives a reload while waiting (the stored identity is checked, not the history).
  await guest.reload();
  await guest.getByRole('heading', { name: 'You are in the lobby' }).waitFor({ timeout: T });
  await alice.locator('.talk-lobby-banner', { hasText: '3 waiting' }).waitFor({ timeout: T });

  step('alice opens the lobby: everyone gets in live');
  await alice.getByRole('button', { name: 'Open lobby' }).click();
  await bob.getByLabel('Write a message').waitFor({ timeout: T });
  await guest.getByLabel('Write a message').waitFor({ timeout: T });
  await carol.getByLabel('Write a message').waitFor({ timeout: T });
  await msg(bob, 'opened the lobby').waitFor({ timeout: T });
  if (await alice.locator('.talk-lobby-banner').isVisible()) throw new Error('lobby banner still shown');

  // ---- call moderation ---------------------------------------------------------------------
  step('alice, bob and carol join the call');
  for (const p of [alice, bob, carol]) await p.goto(`${base}/talk/${token}?call=1`);
  for (const p of [alice, bob, carol]) await inCall(p, roomId, 2);

  step('bob and carol raise hands: everyone sees the queue in order');
  await bob.locator('.call-controls').getByRole('button', { name: 'Raise hand' }).click();
  await alice.locator('.call-hand', { hasText: 'Bob Builder' }).waitFor({ timeout: T });
  await carol.locator('.call-controls').getByRole('button', { name: 'Raise hand' }).click();
  await alice.locator('.call-hand', { hasText: 'Carol Cooper' }).waitFor({ timeout: T });
  const order = await alice.locator('.call-hand').allTextContents();
  if (!(order[0].includes('1') && order[0].includes('Bob') && order[1].includes('2') && order[1].includes('Carol'))) throw new Error(`hand queue order: ${order}`);
  await carol.locator('.call-hand', { hasText: 'Bob Builder' }).waitFor({ timeout: T });
  await shot(alice, 'call-moderation-light');
  await shot(alice, 'call-moderation-dark', { dark: true });

  step('alice lowers carol\'s hand from the queue');
  await alice.getByRole('button', { name: 'Lower Carol Cooper\'s hand' }).click();
  await waitFor(carol, () => window.__ferriteCall && !window.__ferriteCall.state.hand);
  await carol.locator('.call-controls').getByRole('button', { name: 'Raise hand' }).waitFor({ timeout: T });
  await alice.locator('.call-hand', { hasText: 'Carol Cooper' }).waitFor({ state: 'detached', timeout: T });

  step('alice asks bob to mute from his tile, then mutes everyone');
  const bobSession = (await callOf(bob)).session;
  const bobTile = alice.locator(`.call-tile[data-session="${bobSession}"]`);
  await bobTile.hover();
  await bobTile.getByRole('button', { name: 'Moderate Bob Builder' }).click();
  await alice.getByRole('menuitem', { name: 'Ask to mute' }).click();
  await waitFor(bob, () => window.__ferriteCall && !window.__ferriteCall.state.audio);
  await bob.locator('.toast', { hasText: 'Alice Archer muted your microphone' }).waitFor({ timeout: T });
  await bobTile.locator('[data-flag="muted"]').waitFor({ timeout: T });
  await alice.getByRole('button', { name: 'Mute everyone' }).click();
  await waitFor(carol, () => window.__ferriteCall && !window.__ferriteCall.state.audio);
  if (!(await callOf(alice)).audio) throw new Error('mute everyone muted the moderator too');

  step('alice removes carol from the call; carol rejoins');
  const carolSession = (await callOf(carol)).session;
  const carolTile = alice.locator(`.call-tile[data-session="${carolSession}"]`);
  await carolTile.hover();
  await carolTile.getByRole('button', { name: 'Moderate Carol Cooper' }).click();
  await alice.getByRole('menuitem', { name: 'Remove from call' }).click();
  const notice = carol.locator('.toast', { hasText: 'removed you from the call' });
  await notice.waitFor({ timeout: T });
  await carol.locator('.call-stage').waitFor({ state: 'detached', timeout: T });
  await carolTile.waitFor({ state: 'detached', timeout: T });
  await notice.getByRole('button', { name: 'Rejoin' }).click();
  await inCall(carol, roomId, 2);
  await inCall(alice, roomId, 2);

  // ---- breakout rooms ------------------------------------------------------------------------
  step('alice sets up two breakout rooms with manual assignment');
  await alice.getByRole('button', { name: 'Conversation menu' }).click();
  await alice.getByRole('menuitem', { name: 'Breakout rooms' }).click();
  await alice.getByLabel('Number of rooms').fill('2');
  await alice.getByLabel('Assignment').selectOption('manual');
  await alice.getByRole('button', { name: 'Set up breakout rooms' }).click();
  await boRoom(alice, 'Room 2').waitFor({ timeout: T });
  await alice.getByLabel('Room for Bob Builder').selectOption({ label: 'Room 1' });
  await boRoom(alice, 'Room 1').locator('.bo-member', { hasText: 'Bob Builder' }).waitFor({ timeout: T });
  await alice.getByLabel('Room for Carol Cooper').selectOption({ label: 'Room 2' });
  await boRoom(alice, 'Room 2').locator('.bo-member', { hasText: 'Carol Cooper' }).waitFor({ timeout: T });
  await alice.getByLabel('Room for Gil (guest)').selectOption({ label: 'Room 1' });
  await boRoom(alice, 'Room 1').locator('.bo-member', { hasText: 'Gil (guest)' }).waitFor({ timeout: T });
  await shot(alice, 'breakouts-setup-light');

  step('alice starts them with a timer: bob and carol leave the main call and join their rooms\' calls; the guest follows');
  const { breakouts: bo } = await api(A.ctx, 'GET', `/api/talk/rooms/${token}/breakouts`);
  const [r1, r2] = bo.rooms;
  await guest.getByLabel('Write a message').waitFor();
  await alice.getByLabel('Timer in minutes').fill('30');
  await alice.getByRole('button', { name: 'Start breakout rooms' }).click();
  await bob.waitForURL(`**/talk/${r1.token}`, { timeout: T });
  await carol.waitForURL(`**/talk/${r2.token}`, { timeout: T });
  await inCall(bob, r1.id, 0);
  await inCall(carol, r2.id, 0);
  await inCall(alice, roomId, 0);
  await bob.locator('.talk-breakout-banner', { hasText: 'Breakout room of Workshop' }).waitFor({ timeout: T });
  await bob.locator('.talk-breakout-banner .bo-timer').waitFor();
  await guest.locator('.talk-breakout-banner', { hasText: 'Room 1' }).waitFor({ timeout: T });
  await guest.getByLabel('Write a message').fill('Guest checking in from Room 1');
  await guest.getByLabel('Write a message').press('Enter');
  await msg(bob, 'Guest checking in from Room 1').waitFor({ timeout: T });
  if (await msg(carol, 'Guest checking in from Room 1').count()) throw new Error('room 2 sees room 1 chat');
  await alice.locator('.bo-status.on', { hasText: 'Running' }).waitFor();

  step('alice broadcasts to all rooms');
  await alice.getByLabel('Message to all breakout rooms').fill('Five minutes left!');
  await alice.getByRole('button', { name: 'Send to all rooms' }).click();
  for (const p of [bob, carol, guest]) await msg(p, 'Five minutes left!').locator('.pill', { hasText: 'To all rooms' }).waitFor({ timeout: T });
  await shot(alice, 'breakouts-running-light');
  await shot(alice, 'breakouts-running-dark', { dark: true });
  await shot(bob, 'breakout-room-light');
  await noOverflow(guest, 'guest in a breakout room');
  await shot(guest, 'breakout-room-mobile');
  await shot(guest, 'breakout-room-mobile-dark', { dark: true });

  step('bob asks for help; alice gets a notice with a link and visits Room 1');
  await bob.getByRole('button', { name: 'Ask for help' }).click();
  const help = alice.locator('.toast', { hasText: 'Bob Builder asks for help in Room 1' });
  await help.waitFor({ timeout: T });
  await boRoom(alice, 'Room 1').and(alice.locator('.bo-room.help')).waitFor({ timeout: T });
  await bob.getByRole('button', { name: 'Cancel help request' }).waitFor({ timeout: T });
  await help.getByRole('button', { name: 'Go to room' }).click();
  await alice.waitForURL(`**/talk/${r1.token}`, { timeout: T });
  await msg(alice, 'Guest checking in from Room 1').waitFor({ timeout: T });
  await alice.getByRole('button', { name: 'Dismiss help request' }).click();
  await bob.getByRole('button', { name: 'Ask for help' }).waitFor({ timeout: T });
  await alice.locator('.talk-breakout-banner').getByRole('button', { name: 'Main room' }).click();
  await alice.waitForURL(`**/talk/${token}`, { timeout: T });

  step('alice stops the breakout rooms: everyone comes back to the main room and its call');
  await alice.locator('.talk-breakout-banner').getByRole('button', { name: 'Stop' }).click();
  await alice.getByRole('dialog', { name: 'Stop breakout rooms' }).getByRole('button', { name: 'Stop' }).click();
  await bob.waitForURL(`**/talk/${token}`, { timeout: T });
  await carol.waitForURL(`**/talk/${token}`, { timeout: T });
  await guest.locator('.talk-breakout-banner').waitFor({ state: 'hidden', timeout: T });
  await msg(guest, 'stopped the breakout rooms').waitFor({ timeout: T });
  // Alice left the main call when she visited Room 1, so bob and carol meet each other.
  await inCall(bob, roomId, 1);
  await inCall(carol, roomId, 1);

  step('the breakout chat stays readable but closed');
  await bob.getByRole('button', { name: 'Conversation menu' }).click();
  await bob.getByRole('menuitem', { name: 'Breakout rooms' }).click();
  await boRoom(bob, 'Room 1').waitFor({ timeout: T });
  await bob.evaluate(() => window.__ferriteCall?.leave());
  await bob.goto(`${base}/talk/${r1.token}`);
  await msg(bob, 'Guest checking in from Room 1').waitFor({ timeout: T });
  await bob.locator('.talk-readonly', { hasText: 'This breakout room has ended' }).waitFor();
  await bob.locator('.talk-breakout-banner', { hasText: 'has ended' }).waitFor();

  step('only moderators may start calls once alice says so');
  for (const p of [alice, carol]) await p.evaluate(() => window.__ferriteCall?.leave());
  await bob.goto(`${base}/talk/${token}`);
  await bob.getByLabel('Write a message').waitFor();
  await alice.getByRole('button', { name: 'Conversation menu' }).click();
  await alice.getByRole('menuitem', { name: 'Moderation…' }).click();
  await alice.getByLabel('Who can start a call').selectOption('moderators');
  await alice.getByRole('button', { name: 'Save', exact: true }).click();
  await waitFor(bob, () => document.querySelector('.talk-call-btn')?.disabled === true);
  await alice.locator('.talk-call-btn').click();
  await alice.getByRole('menuitem', { name: 'Audio only' }).click();
  await inCall(alice, roomId, 0);
  await waitFor(bob, () => document.querySelector('.talk-call-btn')?.disabled === false);

  step('phone width: lobby banner and breakout panel fit');
  await alice.evaluate(() => window.__ferriteCall?.leave());
  await alice.setViewportSize({ width: 390, height: 800 });
  await alice.getByRole('button', { name: 'Conversation menu' }).click();
  await alice.getByRole('menuitem', { name: 'Moderation…' }).click();
  await alice.getByLabel('Hold participants in the lobby').check();
  await alice.getByRole('button', { name: 'Save', exact: true }).click();
  await alice.locator('.talk-lobby-banner', { hasText: 'Lobby is on' }).waitFor();
  await alice.getByRole('button', { name: 'Conversation menu' }).click();
  await alice.getByRole('menuitem', { name: 'Breakout rooms' }).click();
  await boRoom(alice, 'Room 2').waitFor();
  await noOverflow(alice, 'moderator at 390px');
  await shot(alice, 'moderator-mobile');
  await shot(alice, 'moderator-mobile-dark', { dark: true });
} catch (e) {
  exitCode = 1;
  errors.push(`FAILED in "${currentStep}": ${e.message.split('\n').slice(0, 6).join('\n')}`);
  for (const [i, p] of browser.contexts().flatMap((c) => c.pages()).entries()) {
    await p.screenshot({ path: `${shots}/moderation-failure-${i}.png` }).catch(() => {});
    const state = await p.evaluate(() => {
      const c = window.__ferriteCall;
      return { url: location.pathname, stage: !!document.querySelector(".call-stage"), msgs: document.querySelectorAll(".msg").length, main: (document.querySelector(".talk-main")?.innerText || "").slice(0, 300), call: c ? { session: c.session, members: [...c.members.values()].map((m) => m.actor.name) } : null, toasts: [...document.querySelectorAll('.toast')].map((t) => t.textContent) };
    }).catch(() => null);
    errors.push(`page ${i}: ${JSON.stringify(state)}`);
  }
}

// Tolerated console errors, each matched to the response that caused it: on the signed-out
// guest page app.js probes the session (GET /api/me → 401), and the deliberately wrong
// guest password answers 400.
const allow = {
  401: failed.filter((f) => f === 'guest: 401 GET /api/me').length,
  400: failed.filter((f) => /^guest: 400 POST \/api\/talk\/guest\//.test(f)).length,
};
const real = errors.filter((e) => {
  const m = /^guest: Failed to load resource: .*status of (40[01])/.exec(e);
  if (m && allow[m[1]] > 0) { allow[m[1]]--; return false; }
  return true;
});
const ok = exitCode === 0 && real.length === 0;
console.log(JSON.stringify({ ok, errors: real, failedRequests: failed, logs: ok ? undefined : logs }, null, 2));
await browser.close();
process.exit(ok ? 0 : 1);
