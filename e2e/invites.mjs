// Browser test of calendar sharing and invitations, run against a fresh server whose [smtp]
// points at the SMTP sink this script runs on 127.0.0.1:$SMTP_PORT (default 8836):
//   FERRITE_URL=http://127.0.0.1:8835 node invites.mjs
// Two users (Alice, Bob) and an external guest (Carol): Alice shares a calendar read-only,
// creates a public link (fetches and revokes it), invites Bob and Carol from the event form
// (suggestions for Ferrite users, typed addresses for others); Bob sees the invitation live,
// answers from the event dialog and Alice sees his answer; Carol answers on the public
// invitation page from her email link; Alice upgrades the share and Bob adds an event to her
// calendar. Also: the "can't email" note without outgoing mail, light + dark themes and a
// 390px phone. Screenshots go to $SHOTS. Prints {ok, errors}; any console error fails it.
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';
import net from 'node:net';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8835';
const smtpPort = Number(process.env.SMTP_PORT || 8836);
const shots = process.env.SHOTS || '/tmp/ferrite-shots/invites';
mkdirSync(shots, { recursive: true });
const H = { 'X-Ferrite-Request': '1' };
const ALICE = { username: 'alice', display_name: 'Alice Archer', password: 'alice-password', email: 'alice@example.org' };
const BOB = { username: 'bob', display_name: 'Bob Baker', password: 'bob-password', email: 'bob@example.org' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

async function waitFor(fn, what, timeout = 10000) {
  const end = Date.now() + timeout;
  let last;
  while (Date.now() < end) {
    try {
      last = await fn();
      if (last) return last;
    } catch (e) { last = e; }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`Timed out waiting for ${what} (last: ${last instanceof Error ? last.message : JSON.stringify(last)})`);
}

// ---- a minimal SMTP sink ----------------------------------------------------------------
const mails = [];
const smtp = net.createServer((sock) => {
  let buf = '';
  let inData = false;
  let msg = { to: [], data: '' };
  sock.write('220 sink ESMTP\r\n');
  sock.on('data', (chunk) => {
    buf += chunk.toString('utf8');
    let i;
    while ((i = buf.indexOf('\r\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 2);
      if (inData) {
        if (line === '.') {
          inData = false;
          mails.push(msg);
          msg = { to: [], data: '' };
          sock.write('250 queued\r\n');
        } else {
          msg.data += `${line.startsWith('.') ? line.slice(1) : line}\r\n`;
        }
        continue;
      }
      const up = line.toUpperCase();
      if (up.startsWith('EHLO') || up.startsWith('HELO')) sock.write('250-sink\r\n250 8BITMIME\r\n');
      else if (up.startsWith('RCPT TO:')) { msg.to.push(line.slice(8).trim().replace(/[<>]/g, '')); sock.write('250 OK\r\n'); }
      else if (up === 'DATA') { inData = true; sock.write('354 go ahead\r\n'); }
      else if (up === 'QUIT') { sock.end('221 bye\r\n'); }
      else sock.write('250 OK\r\n');
    }
  });
  sock.on('error', () => {});
});
await new Promise((r) => smtp.listen(smtpPort, '127.0.0.1', r));

/** Decodes quoted-printable soft breaks and escapes (enough for links). */
const unQP = (s) => s.replace(/=\r\n/g, '').replace(/=([0-9A-F]{2})/g, (_, x) => String.fromCharCode(parseInt(x, 16)));

// ---- browsers ---------------------------------------------------------------------------
const browser = await chromium.launch();
const errors = [];

async function newPage(name, viewport = { width: 1360, height: 860 }) {
  const ctx = await browser.newContext({ viewport });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  page.api = async (method, path, data) => {
    const r = await page.request.fetch(`${base}${path}`, { method, data, headers: H });
    const type = r.headers()['content-type'] || '';
    const body = type.includes('json') ? await r.json() : await r.text();
    if (!r.ok()) throw new Error(`${method} ${path} -> ${r.status()} ${JSON.stringify(body)}`);
    return body;
  };
  return page;
}

const shot = async (page, name) => {
  for (const scheme of ['light', 'dark']) {
    await page.emulateMedia({ colorScheme: scheme });
    await page.waitForTimeout(120);
    await page.screenshot({ path: `${shots}/${name}${scheme === 'dark' ? '-dark' : ''}.png` });
  }
  await page.emulateMedia({ colorScheme: 'light' });
};

async function noHorizontalOverflow(page, what) {
  const o = await page.evaluate(() => {
    const doc = document.scrollingElement;
    const modalEl = document.querySelector('.modal');
    return { doc: doc.scrollWidth - doc.clientWidth, modal: modalEl ? modalEl.scrollWidth - modalEl.clientWidth : 0 };
  });
  assert(o.doc <= 1 && o.modal <= 1, `${what}: horizontal overflow ${JSON.stringify(o)}`);
}

const modal = (page) => page.locator('.modal');
const primary = (page) => page.locator('.modal .modal-foot .btn.primary');
const closeModal = async (page) => {
  await page.keyboard.press('Escape');
  await page.locator('.modal').waitFor({ state: 'detached' });
};
/** Opens an event's dialog until `ready` (a locator inside it) shows: live refreshes land a moment after notifications. */
async function openUntil(page, chip, ready, what) {
  await waitFor(async () => {
    await chip.click();
    await modal(page).locator('.cal-info').waitFor();
    if (await ready().count()) return true;
    await closeModal(page);
    return false;
  }, what);
}
const guestStatus = (page, name, status) => () => modal(page).locator('.cal-person', { hasText: name }).locator('.pill', { hasText: status });

let a;
let b;
let carol;
try {
  a = await newPage('alice');
  const st = await a.api('GET', '/api/status');
  assert(st.setup_needed, 'run against a fresh server');
  await a.api('POST', '/api/setup', ALICE);
  await a.api('POST', '/api/admin/users', BOB);
  b = await newPage('bob');
  await b.api('POST', '/api/auth/login', { username: BOB.username, password: BOB.password });
  const { calendar: team } = await a.api('POST', '/api/calendar/calendars', { name: 'Team', color: '#0f766e' });

  await Promise.all([a.goto(`${base}/calendar`), b.goto(`${base}/calendar`)]);
  await a.locator('.cal-cal', { hasText: 'Team' }).waitFor();
  await b.locator('.cal-cal', { hasText: 'Personal' }).waitFor();
  log('signed in');

  // ---- share read-only, public link ---------------------------------------------------
  const teamRow = a.locator('.cal-cal', { hasText: 'Team' });
  await teamRow.getByRole('button', { name: 'Options for Team' }).click();
  await a.locator('.menu button', { hasText: 'Share…' }).click();
  await modal(a).getByText('Share “Team”').waitFor();
  await modal(a).getByText('Only you can see this calendar').waitFor();
  await modal(a).getByLabel('Person to share with').fill('bob');
  await modal(a).getByRole('button', { name: 'Share', exact: true }).click();
  await modal(a).locator('.cal-share-row[data-target="bob"]').waitFor();
  assert(await modal(a).locator('.cal-share-row select').inputValue() === 'read', 'read-only by default');
  await modal(a).getByRole('button', { name: 'Create public link' }).click();
  const linkInput = modal(a).getByLabel('Public subscription link');
  await linkInput.waitFor();
  const feedUrl = await linkInput.inputValue();
  assert(feedUrl.startsWith(`${base}/remote.php/dav/public-calendars/`), `feed URL ${feedUrl}`);
  await shot(a, 'share-dialog');
  const feed = await a.request.get(feedUrl);
  assert(feed.ok() && (await feed.text()).includes('X-WR-CALNAME:Team'), 'feed serves the calendar');
  await modal(a).getByRole('button', { name: 'Revoke link' }).click();
  await a.locator('.modal .modal-foot .btn.danger.solid').click();
  await modal(a).getByRole('button', { name: 'Create public link' }).waitFor();
  assert((await a.request.get(feedUrl)).status() === 404, 'revoked feed is gone');
  await primary(a).click();
  await modal(a).waitFor({ state: 'detached' });
  await teamRow.locator('.cal-cal-shared').waitFor();
  log('shared read-only; public link created, fetched and revoked');

  // Bob's list follows live, with the owner's name.
  const shared = b.locator('.cal-cal', { hasText: 'Team' });
  await shared.waitFor({ timeout: 8000 });
  assert((await b.locator('.cal-cals-sub').textContent()) === 'Shared with me', 'shared section');
  assert((await shared.locator('.cal-cal-owner').textContent()) === 'Alice Archer · view only', 'owner and permission shown');

  // ---- the "can't email" note without outgoing mail ------------------------------------
  await a.route('**/api/calendar/calendars', async (route) => {
    const resp = await route.fetch();
    const json = await resp.json();
    await route.fulfill({ response: resp, json: { ...json, can_email: false } });
  });
  await a.reload();
  await a.locator('.cal-cal', { hasText: 'Team' }).waitFor();
  await a.locator('.cal-new').click();
  await modal(a).locator('.cal-guests-note').waitFor();
  assert((await modal(a).locator('.cal-guests-note').textContent()).includes('can’t be emailed'), 'note without mail');
  await closeModal(a);
  await a.unroute('**/api/calendar/calendars');
  await a.reload();
  await a.locator('.cal-cal', { hasText: 'Team' }).waitFor();

  // ---- invite from the event form -----------------------------------------------------
  await a.locator('.cal-new').click();
  await modal(a).getByText('New event').waitFor();
  assert(await modal(a).locator('.cal-guests-note').count() === 0, 'mail is configured: no note');
  await modal(a).getByLabel('Title').fill('Design kickoff');
  await modal(a).locator('select[id^="ev-cal-"]').selectOption(team.uri);
  await modal(a).getByLabel('Location', { exact: true }).fill('Room 2');
  // Today 10:00–11:00, whatever the time now, so the event stays in this month's views.
  const now = new Date();
  const today = `${now.getFullYear()}-${String(now.getMonth() + 1).padStart(2, '0')}-${String(now.getDate()).padStart(2, '0')}`;
  await modal(a).getByLabel('Start date').fill(today);
  await modal(a).getByLabel('End date').fill(today);
  await modal(a).getByLabel('Start time').fill('10:00');
  await modal(a).getByLabel('End time').fill('11:00');
  const guests = modal(a).locator('.cal-guests-input');
  await guests.fill('bak');
  const opt = a.locator('.cal-guests-pop .cal-guests-opt', { hasText: 'Bob Baker' });
  await opt.waitFor();
  assert((await opt.textContent()).includes('Ferrite user'), 'users are marked');
  await guests.press('Enter');
  await modal(a).locator('.cal-guest[data-email="bob@example.org"]').waitFor();
  await guests.fill('Carol Chen <carol@example.net>');
  await a.locator('.cal-guests-pop .cal-guests-opt', { hasText: 'Invite carol@example.net' }).waitFor();
  await guests.press('Enter');
  await modal(a).locator('.cal-guest[data-email="carol@example.net"]').waitFor();
  await guests.fill('zed@example.net');
  await guests.press('Backspace');
  await guests.fill('');
  await guests.press('Backspace');
  assert(await modal(a).locator('.cal-guest').count() === 1, 'backspace on an empty field removes the last guest');
  await guests.fill('Carol Chen <carol@example.net>');
  await guests.press('Enter');
  assert(await modal(a).locator('.cal-guest').count() === 2, 'two guests');
  await shot(a, 'event-form-guests');
  await primary(a).click();
  await modal(a).waitFor({ state: 'detached' });
  const aChip = a.locator('.cal-chip', { hasText: 'Design kickoff' });
  await aChip.waitFor();
  log('invited Bob (user) and Carol (email) from the form');

  // Carol's email: one iMIP invitation, Reply-To Alice, with response links.
  const invite = await waitFor(() => mails.find((m) => m.to.includes('carol@example.net')), 'invitation mail');
  const raw = unQP(invite.data);
  assert(/method=REQUEST/i.test(raw) && raw.includes('METHOD:REQUEST'), 'iMIP request');
  assert(/^Reply-To: .*alice@example\.org/mi.test(raw), 'reply-to the organizer');
  const link = raw.match(/http:\/\/127\.0\.0\.1:\d+\/invitation\/[A-Za-z0-9_~-]+\?response=accepted/);
  assert(link, 'accept link in the mail');

  // ---- Bob answers --------------------------------------------------------------------
  const bChip = b.locator('.cal-chip.pending', { hasText: 'Design kickoff' });
  await bChip.waitFor({ timeout: 8000 });
  await b.locator('.cal-chip:not(.pending)', { hasText: 'Design kickoff' }).waitFor();
  await waitFor(async () => (await b.locator('#notif-badge').textContent()) !== '' && !(await b.locator('#notif-badge').isHidden()), 'notification badge');
  await shot(b, 'invitation-month');
  await bChip.click();
  await modal(b).locator('.cal-rsvp').waitFor();
  assert((await modal(b).locator('.cal-rsvp-q').textContent()).includes('Alice Archer invited you'), 'invited by Alice');
  assert(await modal(b).getByRole('button', { name: 'Edit' }).count() === 0, 'guests cannot edit');
  assert(await modal(b).getByRole('button', { name: 'Remove' }).count() === 1, 'invitations can be removed');
  await modal(b).locator('.cal-person', { hasText: 'Alice Archer' }).getByText('Organizer').waitFor();
  await shot(b, 'invitation-dialog');
  await modal(b).locator('.cal-rsvp-btn[data-response="accepted"]').click();
  await modal(b).locator('.cal-rsvp-btn[data-response="accepted"][aria-pressed="true"]').waitFor();
  await modal(b).locator('.cal-person', { hasText: 'Bob Baker' }).locator('.pill', { hasText: 'Going' }).waitFor();
  await closeModal(b);
  await b.locator('.cal-chip:not(.pending)', { hasText: 'Design kickoff' }).nth(1).waitFor();
  log('Bob accepted from the dialog');

  // Alice is told, and sees the answer.
  await a.locator('.toast', { hasText: 'Bob Baker accepted' }).waitFor({ timeout: 8000 });
  await openUntil(a, aChip, guestStatus(a, 'Bob Baker', 'Going'), 'Bob going in Alice’s view');
  await modal(a).locator('.cal-person', { hasText: 'Carol Chen' }).locator('.pill', { hasText: 'Awaiting reply' }).waitFor();
  assert(await modal(a).getByRole('button', { name: 'Edit' }).count() === 1, 'the organizer edits');
  await closeModal(a);

  // ---- Carol answers on the public page --------------------------------------------------
  const carolCtx = await browser.newContext({ viewport: { width: 390, height: 844 } });
  carol = await carolCtx.newPage();
  carol.on('console', (m) => { if (m.type() === 'error') errors.push(`carol: ${m.text()}`); });
  carol.on('pageerror', (e) => errors.push(`carol: ${e.message}`));
  await carol.goto(link[0].replace(/^http:\/\/127\.0\.0\.1:\d+/, base));
  await carol.locator('.inv-title', { hasText: 'Design kickoff' }).waitFor();
  assert((await carol.locator('.inv-kicker').textContent()).includes('Alice Archer invited Carol Chen'), 'who invited whom');
  assert(await carol.locator('.inv-answer.primary[data-response="accepted"]').count() === 1, 'the link preselects the answer');
  assert((await carol.locator('.inv-status').textContent()).includes('Press an answer'), 'nothing recorded before a click');
  await noHorizontalOverflow(carol, 'invitation page at 390px');
  await shot(carol, 'invitation-page-phone');
  await carol.locator('.inv-answer[data-response="accepted"]').click();
  await carol.locator('.inv-status', { hasText: 'You accepted' }).waitFor();
  await carol.setViewportSize({ width: 1100, height: 760 });
  await shot(carol, 'invitation-page');
  await a.locator('.toast', { hasText: 'Carol Chen accepted' }).waitFor({ timeout: 8000 });
  await openUntil(a, aChip, guestStatus(a, 'Carol Chen', 'Going'), 'Carol going in Alice’s view');
  await closeModal(a);
  log('Carol accepted on the public page');

  // ---- read-only, then read-write sharee ----------------------------------------------
  const sharedChip = b.locator('.cal-chip:not(.pending)', { hasText: 'Design kickoff' }).first();
  await sharedChip.click();
  await modal(b).locator('.cal-info').waitFor();
  assert(await modal(b).getByRole('button', { name: 'Edit' }).count() === 0 && await modal(b).getByRole('button', { name: 'Delete' }).count() === 0, 'view only');
  await closeModal(b);
  const { shares } = await a.api('GET', `/api/calendar/calendars/${team.uri}/shares`);
  await a.api('PATCH', `/api/calendar/calendars/${team.uri}/shares/${shares[0].id}`, { writable: true });
  await waitFor(async () => (await shared.locator('.cal-cal-owner').textContent()) === 'Alice Archer', 'permission follows live');
  await b.locator('.cal-new').click();
  await modal(b).getByText('New event').waitFor();
  await modal(b).getByLabel('Title').fill('Bob’s demo');
  const sel = modal(b).locator('select[id^="ev-cal-"]');
  assert((await sel.locator('option').allTextContents()).includes('Team (Alice Archer)'), 'shared calendar offered');
  await sel.selectOption(`${team.uri}_shared_by_alice`);
  await primary(b).click();
  await modal(b).waitFor({ state: 'detached' });
  const demo = a.locator('.cal-chip', { hasText: 'Bob’s demo' });
  await demo.waitFor({ timeout: 8000 });
  await demo.click();
  await modal(a).getByText('Added by Bob Baker').waitFor();
  await closeModal(a);
  log('read-write sharee added an event, attributed to him');

  // ---- phone layout -------------------------------------------------------------------
  await b.setViewportSize({ width: 390, height: 844 });
  await b.goto(`${base}/calendar?view=list`);
  await b.locator('.cal-item', { hasText: 'Design kickoff' }).first().waitFor();
  await noHorizontalOverflow(b, 'list at 390px');
  await b.locator('.cal-side-toggle').click();
  await b.locator('.cal-cals-sub').waitFor();
  await shot(b, 'calendars-phone');
  await b.mouse.click(375, 420); // the scrim, right of the panel
  await b.locator('.cal-page.side-open').waitFor({ state: 'detached' });
  await b.locator('.cal-item', { hasText: 'Design kickoff' }).first().click();
  await modal(b).locator('.cal-info').waitFor();
  await noHorizontalOverflow(b, 'event dialog at 390px');
  await shot(b, 'event-dialog-phone');
  await closeModal(b);
  assert(mails.filter((m) => m.to.includes('carol@example.net')).length === 1, `Carol got exactly one mail (${mails.length} total)`);
  log('phone layout ok');
} catch (e) {
  errors.push(`test: ${e.stack || e.message}`);
  for (const [name, p] of [['alice', a], ['bob', b], ['carol', carol]]) {
    if (p) await p.screenshot({ path: `${shots}/failure-${name}.png` }).catch(() => {});
  }
} finally {
  await browser.close();
  smtp.close();
}
console.log(JSON.stringify({ ok: errors.length === 0, errors }));
process.exit(errors.length ? 1 : 0);
