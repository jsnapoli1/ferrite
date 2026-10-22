// Browser test of Talk: chat between two users (live delivery, unread/mentions, reply,
// reactions, edit, delete, typing, polls, files, read markers), a WebRTC call with fake
// devices, and the guest join flow.
//
// Run against a server with a fresh data dir:
//   FERRITE_URL=http://127.0.0.1:8822 SHOTS=/some/dir node talk.mjs
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8822';
const shots = process.env.SHOTS || '/tmp';
const T = 15000;
const notes = [];
const errors = [];
const failed = []; // HTTP responses >= 400, to explain any 'Failed to load resource' console errors
const step = (s) => console.log(`- ${s}`);

const browser = await chromium.launch({
  args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'],
});

function watch(page, who) {
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${who}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${who}: pageerror ${e.message}`));
  page.on('response', (r) => { if (r.status() >= 400) failed.push(`${who}: ${r.status()} ${r.request().method()} ${new URL(r.url()).pathname}`); });
}

async function newUserPage(who) {
  const ctx = await browser.newContext({ viewport: { width: 1280, height: 820 } });
  await ctx.grantPermissions(['camera', 'microphone'], { origin: base });
  const page = await ctx.newPage();
  watch(page, who);
  return { ctx, page };
}

const H = { 'X-Ferrite-Request': '1' };
async function post(ctx, path, data) {
  const r = await ctx.request.post(base + path, { headers: H, data });
  if (!r.ok()) throw new Error(`${path}: ${r.status()} ${await r.text()}`);
  return r.json();
}

const msg = (page, text) => page.locator('.msg', { hasText: text }).last();
async function send(page, text) {
  const input = page.getByLabel('Write a message');
  await input.fill(text);
  await input.press('Enter');
  await msg(page, text).waitFor({ timeout: T });
}
async function waitFor(page, fn, arg, timeout = T) {
  await page.waitForFunction(fn, arg, { timeout, polling: 200 });
}

let exitCode = 0;
try {
  // ---- accounts --------------------------------------------------------------------------
  step('setup: alice (admin) and bob');
  const A = await newUserPage('alice');
  const B = await newUserPage('bob');
  const status = await (await A.ctx.request.get(`${base}/api/status`)).json();
  if (status.setup_needed) {
    await post(A.ctx, '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  } else {
    await post(A.ctx, '/api/auth/login', { username: 'alice', password: 'alice-password' });
  }
  await A.ctx.request.post(`${base}/api/admin/users`, { headers: H, data: { username: 'bob', display_name: 'Bob Builder', password: 'bob-password' } });
  await post(B.ctx, '/api/auth/login', { username: 'bob', password: 'bob-password' });

  const alice = A.page;
  const bob = B.page;
  await alice.goto(`${base}/talk`);
  await bob.goto(`${base}/talk`);
  await alice.getByRole('heading', { name: 'Talk' }).waitFor();
  await bob.getByRole('heading', { name: 'Talk' }).waitFor();

  // ---- create a group room inviting bob ------------------------------------------------------
  step('alice creates a group conversation inviting bob');
  await alice.getByRole('button', { name: 'New', exact: true }).click();
  await alice.getByRole('menuitem', { name: 'Group conversation' }).click();
  await alice.getByLabel('Conversation name').fill('Project Fe');
  const invite = alice.getByLabel('Invite people');
  await invite.fill('bob');
  await alice.locator('.talk-suggest-item', { hasText: 'Bob Builder' }).waitFor();
  await invite.press('ArrowDown');
  await invite.press('ArrowUp');
  await invite.press('Enter');
  await alice.locator('.talk-chip', { hasText: 'Bob Builder' }).waitFor();
  await alice.getByRole('button', { name: 'Create', exact: true }).click();
  await alice.locator('.talk-room-title', { hasText: 'Project Fe' }).waitFor();
  const token = alice.url().split('/talk/')[1];
  if (!token) throw new Error(`no room token in ${alice.url()}`);

  step('bob sees the new conversation in his list live');
  const bobItem = bob.locator(`.talk-item[data-token="${token}"]`);
  await bobItem.waitFor({ timeout: T });

  step('alice mentions bob → unread mention badge in bob\'s list');
  await send(alice, 'Hello @bob, welcome to the project!');
  await bobItem.locator('.badge.talk-mention').waitFor({ timeout: T });

  step('bob opens the room: mention highlighted, badge cleared by mark-read');
  await bob.bringToFront();
  await bobItem.click();
  await msg(bob, 'welcome to the project').waitFor();
  if (!(await msg(bob, 'welcome to the project').evaluate((el) => el.classList.contains('mentioned')))) throw new Error('mention not highlighted');
  if (!(await msg(bob, 'welcome to the project').locator('.mention.me').count())) throw new Error('mention span not marked as me');
  await bobItem.locator('.badge').waitFor({ state: 'detached', timeout: T });

  step('live delivery into the open room');
  await send(alice, 'Second message, delivered live');
  await msg(bob, 'Second message, delivered live').waitFor({ timeout: T });

  step('typing indicator');
  await bob.getByLabel('Write a message').pressSequentially('Thanks', { delay: 30 });
  await alice.locator('.talk-typing', { hasText: 'Bob Builder is typing' }).waitFor({ timeout: T });

  step('bob replies to alice\'s message');
  await bob.getByLabel('Write a message').fill('');
  await msg(bob, 'welcome to the project').hover();
  await msg(bob, 'welcome to the project').getByRole('button', { name: 'Reply' }).click();
  await bob.locator('.talk-reply-chip', { hasText: 'Alice Archer' }).waitFor();
  await send(bob, 'Thanks Alice, glad to be here');
  await msg(alice, 'glad to be here').locator('.msg-quote', { hasText: 'welcome to the project' }).waitFor({ timeout: T });
  // Clicking the quote jumps to the original.
  await msg(alice, 'glad to be here').locator('.msg-quote').click();
  await alice.locator('.msg.highlight', { hasText: 'welcome to the project' }).waitFor({ timeout: 3000 });

  step('alice reacts with 👍; bob sees the count');
  await msg(alice, 'glad to be here').hover();
  await msg(alice, 'glad to be here').getByRole('button', { name: 'Add reaction' }).first().click();
  await alice.getByRole('button', { name: 'React with 👍' }).click();
  await msg(bob, 'glad to be here').locator('.reaction', { hasText: '👍' }).locator('.reaction-count', { hasText: '1' }).waitFor({ timeout: T });
  // Bob adds the same reaction by clicking the pill: count 2 for alice.
  await msg(bob, 'glad to be here').locator('.reaction', { hasText: '👍' }).click();
  await msg(alice, 'glad to be here').locator('.reaction', { hasText: '👍' }).locator('.reaction-count', { hasText: '2' }).waitFor({ timeout: T });

  step('alice edits her last message (ArrowUp) → bob sees the edit');
  await alice.getByLabel('Write a message').click();
  await alice.getByLabel('Write a message').press('ArrowUp');
  const editBox = alice.getByLabel('Edit message');
  await editBox.waitFor();
  await editBox.fill('Second message, now edited');
  await editBox.press('Enter');
  await msg(bob, 'Second message, now edited').locator('.msg-edited').waitFor({ timeout: T });

  step('alice deletes a message → bob sees the placeholder');
  await send(alice, 'Oops, wrong room');
  await msg(bob, 'Oops, wrong room').waitFor({ timeout: T });
  await msg(alice, 'Oops, wrong room').hover();
  await msg(alice, 'Oops, wrong room').getByRole('button', { name: 'More actions' }).click();
  await alice.getByRole('menuitem', { name: 'Delete' }).click();
  await alice.locator('.modal').getByRole('button', { name: 'Delete' }).click();
  await waitFor(bob, () => !document.body.innerText.includes('Oops, wrong room'));
  await bob.locator('.msg-deleted').first().waitFor({ timeout: T });

  step('poll: alice creates, bob votes, alice sees the result live');
  await alice.getByRole('button', { name: 'Attach' }).click();
  await alice.getByRole('menuitem', { name: 'Create poll' }).click();
  await alice.getByLabel('Question').fill('Where should we meet?');
  await alice.getByLabel('Option 1').fill('Office');
  await alice.getByLabel('Option 2').fill('Café');
  await alice.getByRole('button', { name: 'Add option' }).click();
  await alice.getByLabel('Option 3').fill('Online');
  await alice.getByRole('button', { name: 'Create poll' }).click();
  const bobPoll = bob.locator('.poll', { hasText: 'Where should we meet?' });
  await bobPoll.waitFor({ timeout: T });
  await bobPoll.getByRole('button', { name: /^Café/ }).click();
  await alice.locator('.poll', { hasText: 'Where should we meet?' }).locator('.poll-option', { hasText: 'Café' }).locator('.poll-count', { hasText: '1 · 100%' }).waitFor({ timeout: T });
  await alice.locator('.poll', { hasText: 'Where should we meet?' }).getByRole('button', { name: /^Online/ }).click();
  await bobPoll.locator('.poll-option', { hasText: 'Online' }).locator('.poll-count', { hasText: '1 · 50%' }).waitFor({ timeout: T });

  step('file attach: upload into Talk folder and share → file card for bob');
  // A 2x2 PNG (to check the image thumbnail through the room share) and a live document.
  const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAFklEQVR4nGP8z8DAwMDAxMDAwMDAAAANHQEDasKb6QAAAABJRU5ErkJggg==', 'base64');
  await alice.locator('.talk-composer input[type=file]').setInputFiles([
    { name: 'pixel.png', mimeType: 'image/png', buffer: png },
    { name: 'meeting-notes.md', mimeType: 'text/markdown', buffer: Buffer.from('# Notes\n') },
  ]);
  await bob.locator('.file-card', { hasText: 'pixel.png' }).waitFor({ timeout: T });
  await bob.locator('.file-card', { hasText: 'meeting-notes.md' }).waitFor({ timeout: T });
  const editHref = await bob.locator('.file-card', { hasText: 'meeting-notes.md' }).locator('a.file-name').getAttribute('href');
  if (!/^\/edit\//.test(editHref)) throw new Error(`live doc link is ${editHref}`);
  await waitFor(bob, () => { const i = document.querySelector('.file-thumb img'); return i && i.complete && i.naturalWidth > 0; });
  const talkFolder = await (await A.ctx.request.get(`${base}/api/me`)).json();
  const rootKids = await (await A.ctx.request.get(`${base}/api/files/${talkFolder.root_id}/children`)).json();
  if (!rootKids.entries.some((e) => e.name === 'Talk' && e.kind === 'dir')) throw new Error('Talk folder missing');

  step('mark-read clears the badge (bob away, then back)');
  await bob.locator('.talk-item').first().waitFor();
  // Alice writes while bob has another (no) room open: badge appears, then clears on open.
  await bob.getByRole('heading', { name: 'Talk' }).waitFor();
  await bob.evaluate(() => { history.pushState({}, '', '/talk'); dispatchEvent(new PopStateEvent('popstate')); });
  await bob.locator('.talk-main .empty', { hasText: 'Pick a conversation' }).waitFor();
  await send(alice, 'Are you still there?');
  await bobItem.locator('.badge').waitFor({ timeout: T });
  if (await bobItem.locator('.badge.talk-mention').count()) throw new Error('plain message counted as mention');
  await bobItem.click();
  await msg(bob, 'Are you still there?').waitFor();
  await bob.locator('.talk-unread').waitFor({ timeout: T });
  await bobItem.locator('.badge').waitFor({ state: 'detached', timeout: T });
  await alice.screenshot({ path: `${shots}/talk-chat-light.png` });
  await alice.emulateMedia({ colorScheme: 'dark' });
  await alice.screenshot({ path: `${shots}/talk-chat-dark.png` });
  await alice.emulateMedia({ colorScheme: 'light' });

  // ---- call -----------------------------------------------------------------------------------
  step('call: alice starts a video call, bob joins from the banner');
  await alice.getByRole('button', { name: 'Start call' }).click();
  await alice.getByRole('menuitem', { name: 'Video call' }).click();
  await alice.locator('.call-tile.local').waitFor({ timeout: T });
  await bob.locator('.talk-call-banner', { hasText: 'Call in progress' }).waitFor({ timeout: T });
  await bob.locator('.talk-call-banner').getByRole('button', { name: 'Join' }).click();
  const connected = () => {
    const c = window.__ferriteCall;
    if (!c || !c.peers.size) return false;
    const ok = [...c.peers.values()].every((p) => p.pc.connectionState === 'connected');
    const vids = [...document.querySelectorAll('.call-tile:not(.local) video')];
    return ok && vids.some((v) => v.videoWidth > 0);
  };
  await waitFor(alice, connected, null, 30000);
  await waitFor(bob, connected, null, 30000);
  const states = await Promise.all([alice, bob].map((p) => p.evaluate(() => [...window.__ferriteCall.peers.values()].map((x) => x.pc.connectionState))));
  step(`  peer connection states: alice=${states[0]} bob=${states[1]}`);

  step('mute indicator shows on the other side');
  await alice.getByRole('button', { name: 'Mute microphone' }).click();
  await bob.locator('.call-tile:not(.local) [data-flag="muted"]').waitFor({ timeout: T });
  await alice.getByRole('button', { name: 'Unmute microphone' }).click();
  await bob.locator('.call-tile:not(.local) [data-flag="muted"]').waitFor({ state: 'detached', timeout: T });
  step('raise hand shows on the other side');
  await bob.getByRole('button', { name: 'Raise hand' }).click();
  await alice.locator('.call-tile:not(.local) [data-flag="hand"]').waitFor({ timeout: T });

  step('camera off → avatar placeholder on the other side');
  await alice.getByRole('button', { name: 'Turn camera off' }).click();
  await bob.locator('.call-tile.no-video:not(.local)').waitFor({ timeout: T });
  await alice.getByRole('button', { name: 'Turn camera on' }).click();
  await bob.locator('.call-tile:not(.local):not(.no-video)').waitFor({ timeout: T });

  step('screen share (if the headless browser supports getDisplayMedia)');
  const hasDisplay = await alice.evaluate(() => !!(navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia));
  if (hasDisplay) {
    await alice.getByRole('button', { name: 'Share screen' }).click();
    try {
      await bob.locator('.call-tile:not(.local) [data-flag="screen"]').waitFor({ timeout: 4000 });
      step('  screen share started and indicated on the other side');
      await alice.getByRole('button', { name: 'Stop sharing screen' }).click();
      await bob.locator('.call-tile:not(.local) [data-flag="screen"]').waitFor({ state: 'detached', timeout: T });
    } catch (_) {
      notes.push('Screen share skipped: getDisplayMedia exists but did not produce a capture in headless Chromium.');
    }
  } else {
    notes.push('Screen share skipped: getDisplayMedia unavailable in this browser.');
  }
  await alice.screenshot({ path: `${shots}/talk-call-light.png` });
  await bob.emulateMedia({ colorScheme: 'dark' });
  await bob.screenshot({ path: `${shots}/talk-call-dark.png` });
  await bob.emulateMedia({ colorScheme: 'light' });

  step('bob leaves → his tile disappears for alice; the list shows the active call');
  await bob.getByRole('button', { name: 'Leave call' }).click();
  await alice.locator('.call-tile:not(.local)').waitFor({ state: 'detached', timeout: T });
  await bob.locator('.call-stage').waitFor({ state: 'detached' });
  await bobItem.locator('.talk-item-call').waitFor({ timeout: T });
  await alice.getByRole('button', { name: 'Leave call' }).click();
  await bobItem.locator('.talk-item-call').waitFor({ state: 'detached', timeout: T });
  await bob.locator('.talk-call-banner').waitFor({ state: 'hidden', timeout: T });

  // ---- guest ----------------------------------------------------------------------------------
  step('guest: alice creates a public room, a signed-out visitor joins by link');
  await alice.getByRole('button', { name: 'New', exact: true }).click();
  await alice.getByRole('menuitem', { name: 'Public conversation' }).click();
  await alice.getByLabel('Conversation name').fill('Open House');
  await alice.getByRole('button', { name: 'Create', exact: true }).click();
  await alice.locator('.talk-room-title', { hasText: 'Open House' }).waitFor();
  const pubToken = alice.url().split('/talk/')[1];
  const G = await browser.newContext({ viewport: { width: 390, height: 780 } });
  await G.grantPermissions(['camera', 'microphone'], { origin: base });
  const guest = await G.newPage();
  watch(guest, 'guest');
  await guest.goto(`${base}/join/${pubToken}`);
  await guest.getByLabel('Your name').fill('Gail');
  await guest.screenshot({ path: `${shots}/talk-guest-join.png` });
  await guest.getByRole('button', { name: 'Join conversation' }).click();
  await guest.getByLabel('Write a message').waitFor();
  await msg(alice, 'Gail (guest) joined').waitFor({ timeout: T });
  await send(guest, 'Hi from a guest!');
  await msg(alice, 'Hi from a guest!').locator('.pill', { hasText: 'Guest' }).waitFor({ timeout: T });
  await send(alice, 'Welcome, Gail');
  await msg(guest, 'Welcome, Gail').waitFor({ timeout: T });
  // Guests can react; file sharing and room admin are hidden.
  await msg(guest, 'Welcome, Gail').hover();
  await msg(guest, 'Welcome, Gail').getByRole('button', { name: 'Add reaction' }).first().click();
  await guest.getByRole('button', { name: 'React with 🎉' }).click();
  await msg(alice, 'Welcome, Gail').locator('.reaction', { hasText: '🎉' }).waitFor({ timeout: T });
  if (await guest.getByRole('button', { name: 'Conversation menu' }).count()) throw new Error('guest sees the room admin menu');
  if (await guest.getByRole('button', { name: 'Attach' }).count()) throw new Error('guest sees file attach');
  await guest.getByRole('button', { name: 'Create poll' }).waitFor();
  // The guest survives a reload (secret in sessionStorage).
  await guest.reload();
  await msg(guest, 'Hi from a guest!').waitFor({ timeout: T });
  await guest.screenshot({ path: `${shots}/talk-guest-mobile.png` });
  await guest.emulateMedia({ colorScheme: 'dark' });
  await guest.screenshot({ path: `${shots}/talk-guest-mobile-dark.png` });

  step('mention autocomplete with the keyboard');
  await alice.goto(`${base}/talk/${token}`);
  const ai = alice.getByLabel('Write a message');
  await ai.waitFor();
  await ai.pressSequentially('ping @bo');
  await alice.locator('.talk-suggest-item.active', { hasText: 'Bob Builder' }).waitFor();
  await ai.press('Enter');
  if ((await ai.inputValue()) !== 'ping @bob ') throw new Error(`autocomplete produced "${await ai.inputValue()}"`);
  await ai.press('Enter');
  await msg(bob, 'ping @bob').waitFor({ timeout: T });

  step('deep links: ?message= highlights, ?call=1 auto-joins');
  const firstId = await msg(bob, 'welcome to the project').getAttribute('data-id');
  await bob.goto(`${base}/talk/${token}?message=${firstId}`);
  await bob.locator(`.msg.highlight[data-id="${firstId}"]`).waitFor({ timeout: T });
  await bob.goto(`${base}/talk/${token}?call=1`);
  await bob.locator('.call-tile.local').waitFor({ timeout: T });
  await alice.locator('.talk-call-banner', { hasText: 'Bob Builder' }).waitFor({ timeout: T });
  await bob.getByRole('button', { name: 'Leave call' }).click();
  await alice.locator('.talk-call-banner').waitFor({ state: 'hidden', timeout: T });

  step('narrow (380px) layouts');
  await alice.setViewportSize({ width: 380, height: 760 });
  await alice.waitForTimeout(400); // the shell's sidebar slides out
  await alice.screenshot({ path: `${shots}/talk-narrow-room.png` });
  await alice.getByRole('button', { name: 'Back to conversations' }).click();
  await alice.locator('.talk-list').waitFor();
  await alice.waitForTimeout(300);
  await alice.screenshot({ path: `${shots}/talk-narrow-list.png` });
  const overflow = await alice.evaluate(() => document.documentElement.scrollWidth > window.innerWidth);
  if (overflow) throw new Error('horizontal overflow at 380px');
  await alice.setViewportSize({ width: 1280, height: 820 });
  await alice.goto(`${base}/talk/${token}`);
  await alice.emulateMedia({ colorScheme: 'dark' });
  await alice.getByRole('button', { name: 'Participants' }).click();
  await alice.locator('.talk-person', { hasText: 'Bob Builder' }).waitFor();
  await alice.screenshot({ path: `${shots}/talk-participants-dark.png` });
} catch (e) {
  exitCode = 1;
  errors.push(`FAILED: ${e.message.split('\n').slice(0, 6).join('\n')}`);
  for (const [i, p] of browser.contexts().flatMap((c) => c.pages()).entries()) {
    await p.screenshot({ path: `${shots}/talk-failure-${i}.png` }).catch(() => {});
  }
}

// The only tolerated console errors: on the signed-out guest page, app.js (the shell, not
// Talk) probes the session with GET /api/me, which answers 401 and Chromium logs as
// "Failed to load resource". Each tolerated error must match such a 401 /api/me response.
// Chromium's fake media devices log no errors.
let allowance = failed.filter((f) => f === 'guest: 401 GET /api/me').length;
const ignored = [];
const real = errors.filter((e) => {
  if (allowance > 0 && /^guest: Failed to load resource: .*status of 401/.test(e)) { allowance--; ignored.push(e); return false; }
  return true;
});
const ok = exitCode === 0 && real.length === 0;
console.log(JSON.stringify({ ok, errors: real, ignored: ignored.length ? `${ignored.length}x 401 from app.js GET /api/me on the signed-out guest page` : 'none', failedRequests: failed, notes }, null, 2));
await browser.close();
process.exit(ok ? 0 : 1);
