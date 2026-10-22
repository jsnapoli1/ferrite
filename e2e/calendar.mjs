// Browser test of the Calendar and Contacts views, run against a server with a fresh data dir:
//   FERRITE_URL=http://127.0.0.1:8825 node calendar.mjs
// Covers event create/edit/delete through the UI, month/week/list views, live refresh when
// another session of the same user changes data (groupware.changed), recurring and
// multi-day events, a second calendar with a visibility toggle, the CalDAV help popover,
// contacts create/search/delete, light + dark themes and a 390px-wide phone viewport.
// Screenshots go to $SHOTS. Prints {ok, errors}; any console error fails the run.
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8825';
const shots = process.env.SHOTS || '/tmp/claude-0/-home-user-ferrite/152dfe4c-0391-5ed5-8646-9b8a5fc86c2c/scratchpad/agent-cal/shots';
mkdirSync(shots, { recursive: true });
const H = { 'X-Ferrite-Request': '1' };
const USER = { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' };
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

const pad = (n) => String(n).padStart(2, '0');
const key = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;

const browser = await chromium.launch();
const errors = [];

/** A signed-in page in its own browser context (its own session cookie). */
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

async function login(page) {
  const status = await page.api('GET', '/api/status');
  if (status.setup_needed) await page.api('POST', '/api/setup', USER);
  else await page.api('POST', '/api/auth/login', { username: USER.username, password: USER.password });
}

const shot = async (page, name) => {
  for (const scheme of ['light', 'dark']) {
    await page.emulateMedia({ colorScheme: scheme });
    await page.waitForTimeout(120);
    await page.screenshot({ path: `${shots}/${name}${scheme === 'dark' ? '-dark' : ''}.png` });
  }
  await page.emulateMedia({ colorScheme: 'light' });
};

/** Fails if the page (or the app's main area) scrolls sideways. */
async function noHorizontalOverflow(page, what) {
  const o = await page.evaluate(() => {
    const doc = document.scrollingElement;
    const main = document.querySelector('.main');
    return { doc: doc.scrollWidth - doc.clientWidth, main: main ? main.scrollWidth - main.clientWidth : 0 };
  });
  assert(o.doc <= 1 && o.main <= 1, `${what}: horizontal overflow ${JSON.stringify(o)}`);
}

const modal = (page) => page.locator('.modal');
const primary = (page) => page.locator('.modal .modal-foot .btn.primary');
const dangerConfirm = (page) => page.locator('.modal .modal-foot .btn.danger.solid');

/** Center of a box lies inside another box. */
const inside = (inner, outer) => {
  const cx = inner.x + inner.width / 2;
  const cy = inner.y + inner.height / 2;
  return cx >= outer.x && cx <= outer.x + outer.width && cy >= outer.y && cy <= outer.y + outer.height;
};

let failed = null;
try {
  const a = await newPage('alice-A');
  await login(a);
  const b = await newPage('alice-B');
  await login(b);

  // Target days in the current month, so the default month view shows them.
  const now = new Date();
  const day15 = new Date(now.getFullYear(), now.getMonth(), 15);
  const k15 = key(day15);

  // ---- month view: create through the UI --------------------------------------------
  await a.goto(`${base}/calendar`);
  await a.locator('.cal-month').waitFor();
  await a.locator('.cal-cal', { hasText: 'Personal' }).waitFor();
  assert(await a.locator('.nav a[data-nav="calendar"].active').count() === 1, 'calendar nav entry active');
  const cell = a.locator(`.cal-cell[data-day="${k15}"]`);
  const cb = await cell.boundingBox();
  await a.mouse.click(cb.x + cb.width / 2, cb.y + cb.height - 8);
  await modal(a).getByText('New event').waitFor();
  await a.getByLabel('Title').fill('Design review');
  assert(await a.getByLabel('Start date').inputValue() === k15, 'form starts on the clicked day');
  await a.getByLabel('Start time').fill('14:00');
  assert(await a.getByLabel('End time').inputValue() === '15:00', 'moving the start keeps the duration');
  await a.getByLabel('End time').fill('15:30');
  await a.getByLabel('Location').fill('Room 4');
  await a.getByLabel('Description').fill('Bring the mockups');
  await primary(a).click();
  const chip = a.locator('.cal-chip', { hasText: 'Design review' });
  await chip.waitFor();
  assert(inside(await chip.boundingBox(), await cell.boundingBox()), 'event chip is drawn in the 15th');
  const { events: evs1 } = await a.api('GET', '/api/calendar/events');
  const created = evs1.find((e) => e.summary === 'Design review');
  assert(created, 'event stored');
  const expect = await a.evaluate((k) => { const [y, m, d] = k.split('-').map(Number); return [new Date(y, m - 1, d, 14).toISOString(), new Date(y, m - 1, d, 15, 30).toISOString()]; }, k15);
  assert(new Date(created.starts_at).toISOString() === expect[0] && new Date(created.ends_at).toISOString() === expect[1], `times stored as local 14:00-15:30 (${created.starts_at})`);
  assert(created.location === 'Room 4' && created.description === 'Bring the mockups', 'location and description stored');
  log('created through the UI');

  // ---- view and edit ----------------------------------------------------------------
  await chip.click();
  await modal(a).locator('.cal-info').waitFor();
  assert(await modal(a).getByText('Room 4').count() === 1, 'details show the location');
  await modal(a).getByRole('button', { name: 'Edit' }).click();
  await modal(a).getByText('Edit event').waitFor();
  await a.getByLabel('Title').fill('Design review v2');
  await primary(a).click();
  await a.locator('.cal-chip', { hasText: 'Design review v2' }).waitFor();
  await waitFor(async () => (await a.api('GET', '/api/calendar/events')).events.some((e) => e.id === created.id && e.summary === 'Design review v2'), 'title saved');
  log('edited');

  // ---- multi-day all-day and recurring events (from the API and CalDAV) ----------------
  const d17 = new Date(now.getFullYear(), now.getMonth(), 17);
  const utc = (d, n = 0) => new Date(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate() + n)).toISOString();
  await b.api('POST', '/api/calendar/events', { summary: 'Offsite', all_day: true, start: utc(d17), end: utc(d17, 3) });
  const ics = ['BEGIN:VCALENDAR', 'VERSION:2.0', 'PRODID:-//e2e//EN', 'BEGIN:VEVENT', 'UID:standup-e2e', 'DTSTAMP:20260101T000000Z',
    `DTSTART:${key(new Date(now.getFullYear(), now.getMonth(), 6)).replaceAll('-', '')}T080000Z`, 'DURATION:PT15M', 'SUMMARY:Standup', 'RRULE:FREQ=DAILY', 'END:VEVENT', 'END:VCALENDAR', ''].join('\r\n');
  const put = await b.request.fetch(`${base}/remote.php/dav/calendars/${USER.username}/personal/standup-e2e.ics`, {
    method: 'PUT', data: ics,
    headers: { 'Content-Type': 'text/calendar', Authorization: `Basic ${Buffer.from(`${USER.username}:${USER.password}`).toString('base64')}` },
  });
  assert(put.status() === 201, `CalDAV PUT status ${put.status()}`);
  const bar = a.locator('.cal-chip.bar', { hasText: 'Offsite' }).first();
  await bar.waitFor();
  const standup = a.locator('.cal-chip', { hasText: 'Standup' });
  await standup.waitFor();
  assert((await standup.getAttribute('aria-label')).includes('repeats'), 'recurring event is marked');
  // The bar covers the 17th–19th (it may wrap onto the next week row).
  const barBoxes = await a.locator('.cal-chip.bar', { hasText: 'Offsite' }).evaluateAll((els) => els.map((e) => e.getBoundingClientRect().width).reduce((x, y) => x + y, 0));
  const cellW = (await cell.boundingBox()).width;
  assert(barBoxes > cellW * 2.6 && barBoxes < cellW * 3.2, `all-day bar spans three days (${barBoxes} vs cell ${cellW})`);
  // A busy day collapses into "+N more", which lists the whole day.
  for (let i = 0; i < 5; i++) {
    const s = await a.evaluate(([y, m, h]) => new Date(y, m, 20, h).toISOString(), [now.getFullYear(), now.getMonth(), 8 + i]);
    await b.api('POST', '/api/calendar/events', { summary: `Busy ${i + 1}`, start: s });
  }
  const more = a.locator('.cal-more');
  await waitFor(async () => (await more.count()) === 1 && /^\+2 more$/.test(await more.textContent()), '"+2 more" on the busy day');
  assert(inside(await more.boundingBox(), await a.locator(`.cal-cell[data-day="${key(new Date(now.getFullYear(), now.getMonth(), 20))}"]`).boundingBox()), '"+N more" sits in its day');
  await shot(a, 'calendar-month');
  await more.click();
  await waitFor(async () => (await modal(a).locator('.cal-item').count()) === 5, 'day dialog lists all five');
  await a.keyboard.press('Escape');
  await modal(a).waitFor({ state: 'detached' });
  log('multi-day + recurring shown live in month view');

  // ---- CalDAV help popover ----------------------------------------------------------
  await a.getByRole('button', { name: 'Sync with CalDAV' }).click();
  const pop = a.locator('.sync-help-pop');
  await pop.waitFor();
  assert(await pop.locator('input').inputValue() === `${base}/remote.php/dav`, 'CalDAV URL shown');
  await a.screenshot({ path: `${shots}/calendar-sync-help.png` });
  await a.keyboard.press('Escape');
  await pop.waitFor({ state: 'detached' });

  // ---- week view + live update from another session ---------------------------------
  await a.locator('.cal-daynum', { hasText: /^15$/ }).click();
  await a.locator('.cal-week-view').waitFor();
  assert(await a.locator('.cal-views [data-view="week"][aria-pressed="true"]').count() === 1, 'week button pressed');
  await a.locator(`.cal-col[data-day="${k15}"] .cal-block`, { hasText: 'Design review v2' }).waitFor();
  await a.evaluate(() => { window.__noReload = true; });
  const liveStart = await a.evaluate((k) => { const [y, m, d] = k.split('-').map(Number); return [new Date(y, m - 1, d, 9).toISOString(), new Date(y, m - 1, d, 9, 45).toISOString()]; }, k15);
  const { event: liveEv } = await b.api('POST', '/api/calendar/events', { summary: 'Live sync check', start: liveStart[0], end: liveStart[1] });
  const liveBlock = a.locator(`.cal-col[data-day="${k15}"] .cal-block`, { hasText: 'Live sync check' });
  await liveBlock.waitFor({ timeout: 8000 });
  assert(await a.evaluate(() => window.__noReload === true), 'appeared without a reload');
  const [lb, rb] = [await liveBlock.boundingBox(), await a.locator('.cal-block', { hasText: 'Design review v2' }).boundingBox()];
  assert(lb.y < rb.y, 'the 9:00 event sits above the 14:00 one');
  await shot(a, 'calendar-week');
  log('live event appeared in week view');

  // ---- delete through the UI ----------------------------------------------------------
  await liveBlock.click();
  await modal(a).getByRole('button', { name: 'Delete' }).click();
  await modal(a).getByText('Delete event').waitFor();
  await dangerConfirm(a).click();
  await liveBlock.waitFor({ state: 'detached' });
  await waitFor(async () => !(await b.api('GET', '/api/calendar/events')).events.some((e) => e.id === liveEv.id), 'event deleted on the server');
  log('deleted');

  // ---- second calendar + visibility ------------------------------------------------------
  await a.getByRole('button', { name: 'New calendar' }).click();
  await a.getByLabel('Name').fill('Team');
  await modal(a).locator('.swatch').nth(2).click();
  await primary(a).click();
  const teamRow = a.locator('.cal-cal', { hasText: 'Team' });
  await teamRow.waitFor();
  const { calendars } = await b.api('GET', '/api/calendar/calendars');
  const team = calendars.find((c) => c.name === 'Team');
  assert(team && team.color === '#0f766e', `team calendar created with the picked color (${team && team.color})`);
  const teamStart = await a.evaluate((k) => { const [y, m, d] = k.split('-').map(Number); return new Date(y, m - 1, d, 11).toISOString(); }, k15);
  await b.api('POST', '/api/calendar/events', { summary: 'Team sync', start: teamStart, calendar: team.uri });
  const teamBlock = a.locator('.cal-block', { hasText: 'Team sync' });
  await teamBlock.waitFor();
  assert((await teamBlock.evaluate((el) => getComputedStyle(el).borderLeftColor)) === 'rgb(15, 118, 110)', 'event uses the calendar color');
  await teamRow.getByRole('checkbox').uncheck();
  await teamBlock.waitFor({ state: 'detached' });
  assert(await a.locator('.cal-block', { hasText: 'Design review v2' }).count() === 1, 'other calendars stay visible');
  await teamRow.getByRole('checkbox').check();
  await teamBlock.waitFor();
  // Rename through the row menu.
  await teamRow.getByRole('button', { name: 'Options for Team' }).click();
  await a.locator('.menu button', { hasText: 'Rename' }).click();
  await modal(a).locator('input').fill('Team Rocket');
  await primary(a).click();
  await a.locator('.cal-cal', { hasText: 'Team Rocket' }).waitFor();
  log('second calendar created, toggled and renamed');

  // ---- list view ----------------------------------------------------------------------
  await a.locator('.cal-views [data-view="list"]').click();
  await a.locator('.cal-list').waitFor();
  assert(new URL(a.url()).searchParams.get('view') === 'list', 'view is kept in the URL');
  await a.locator(`.cal-day[data-day="${k15}"] .cal-item`, { hasText: 'Design review v2' }).waitFor();
  assert(await a.locator('.cal-item', { hasText: 'Offsite' }).count() === 3, 'multi-day event listed on each of its days');
  await shot(a, 'calendar-list');
  await a.locator('.cal-views [data-view="month"]').click();
  await a.locator('.cal-month').waitFor();

  // ---- contacts --------------------------------------------------------------------------
  await a.locator('.nav a[data-nav="contacts"]').click();
  await a.locator('.ct-page').waitFor();
  await a.locator('.ct-detail .empty').waitFor();
  await a.getByRole('button', { name: 'New contact' }).first().click();
  await modal(a).getByText('New contact').waitFor();
  await a.getByLabel('Name').fill('Ada Lovelace');
  await modal(a).getByLabel('Email', { exact: true }).first().fill('ada@example.org');
  await modal(a).getByRole('button', { name: 'Add email' }).click();
  await modal(a).getByLabel('Email', { exact: true }).nth(1).fill('countess@example.org');
  await modal(a).getByLabel('Phone', { exact: true }).first().fill('+44 20 7946 0000');
  await a.getByLabel('Organization').fill('Analytical Engines');
  await a.getByLabel('Notes').fill('First programmer.');
  await primary(a).click();
  const card = a.locator('.ct-card');
  await card.locator('h2', { hasText: 'Ada Lovelace' }).waitFor();
  assert(await card.locator('a[href="mailto:countess@example.org"]').count() === 1, 'second email shown');
  assert(await card.locator('a[href="tel:+442079460000"]').count() === 1, 'phone link');
  assert(new URL(a.url()).searchParams.get('c'), 'selection in the URL');
  // Another session adds a contact: it shows up live.
  await b.api('POST', '/api/contacts', { name: 'Grace Hopper', emails: ['grace@navy.mil'], org: 'US Navy' });
  await a.locator('.ct-row', { hasText: 'Grace Hopper' }).waitFor();
  await shot(a, 'contacts');
  const searchBox = a.getByLabel('Search contacts');
  await searchBox.fill('analytical');
  await waitFor(async () => (await a.locator('.ct-row').count()) === 1, 'search narrows to one contact');
  assert(await a.locator('.ct-row', { hasText: 'Ada Lovelace' }).count() === 1, 'search finds by organization');
  await searchBox.fill('nobody-here');
  await a.locator('.ct-empty', { hasText: 'No contacts match' }).waitFor();
  await searchBox.fill('');
  await waitFor(async () => (await a.locator('.ct-row').count()) === 2, 'search cleared');
  await a.locator('.ct-row', { hasText: 'Ada Lovelace' }).click();
  await card.getByRole('button', { name: 'Delete' }).click();
  await modal(a).getByText('Delete contact').waitFor();
  await dangerConfirm(a).click();
  await a.locator('.ct-row', { hasText: 'Ada Lovelace' }).waitFor({ state: 'detached' });
  assert(!(await b.api('GET', '/api/contacts')).contacts.some((c) => c.name === 'Ada Lovelace'), 'contact deleted on the server');
  await a.getByRole('button', { name: 'Sync with CardDAV' }).click();
  assert(await a.locator('.sync-help-pop input').inputValue() === `${base}/remote.php/dav`, 'CardDAV URL shown');
  await a.keyboard.press('Escape');
  log('contacts ok');

  // ---- phone width -------------------------------------------------------------------------
  const p = await newPage('alice-phone', { width: 390, height: 844 });
  await login(p);
  await p.goto(`${base}/calendar`);
  await p.locator('.cal-list').waitFor();
  await noHorizontalOverflow(p, 'calendar list (default on phones)');
  await shot(p, 'phone-calendar-list');
  await p.locator('.cal-views [data-view="month"]').click();
  await p.locator('.cal-month').waitFor();
  await p.locator('.cal-chip', { hasText: 'Design review v2' }).waitFor();
  await noHorizontalOverflow(p, 'calendar month');
  await shot(p, 'phone-calendar-month');
  await p.locator('.cal-views [data-view="week"]').click();
  await p.locator('.cal-week-view').waitFor();
  await noHorizontalOverflow(p, 'calendar week');
  await shot(p, 'phone-calendar-week');
  await p.getByRole('button', { name: 'Show calendars' }).click();
  await p.locator('.cal-page.side-open .cal-cal', { hasText: 'Team Rocket' }).waitFor();
  await p.waitForTimeout(250);
  await noHorizontalOverflow(p, 'calendar drawer');
  await shot(p, 'phone-calendar-drawer');
  await p.locator('.cal-scrim').click({ position: { x: 370, y: 400 } });
  await p.locator('.cal-new').click();
  await modal(p).getByText('New event').waitFor();
  await noHorizontalOverflow(p, 'event form');
  await shot(p, 'phone-event-form');
  await p.keyboard.press('Escape');
  await p.goto(`${base}/contacts`);
  await p.locator('.ct-row', { hasText: 'Grace Hopper' }).click();
  await p.locator('.ct-card h2', { hasText: 'Grace Hopper' }).waitFor();
  await noHorizontalOverflow(p, 'contact detail');
  await shot(p, 'phone-contact');
  await p.getByRole('button', { name: 'All contacts' }).click();
  await p.locator('.ct-row', { hasText: 'Grace Hopper' }).waitFor();
  await noHorizontalOverflow(p, 'contact list');
  log('phone layout ok');
} catch (e) {
  failed = e;
  let i = 0;
  for (const c of browser.contexts()) for (const pg of c.pages()) await pg.screenshot({ path: `${shots}/failure-${i++}.png` }).catch(() => {});
}

await browser.close();
if (failed) errors.unshift(`failed: ${failed.message}`);
const ok = errors.length === 0;
console.log(JSON.stringify({ ok, errors }));
process.exit(ok ? 0 : 1);
