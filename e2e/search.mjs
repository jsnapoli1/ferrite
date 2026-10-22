// Browser test of the search page: hits for files, events and contacts render with the right
// labels and open the right view.
//
//   FERRITE_URL=http://127.0.0.1:8826 node search.mjs   (fresh data dir)
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8826';
const H = { 'X-Ferrite-Request': '1' };
const errors = [];
const step = (s) => console.log(`- ${s}`);
const browser = await chromium.launch();
let exitCode = 0;
try {
  const ctx = await browser.newContext({ viewport: { width: 1200, height: 800 } });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
  page.on('pageerror', (e) => errors.push(`pageerror ${e.message}`));
  const post = async (path, data) => {
    const r = await ctx.request.post(base + path, { headers: H, data });
    if (!r.ok()) throw new Error(`${path}: ${r.status()} ${await r.text()}`);
    return r.json();
  };

  step('setup: admin, a file, an event and a contact that all mention "Zephyr"');
  await post('/api/setup', { username: 'alice', display_name: 'Alice', password: 'alice-password' });
  const me = await (await ctx.request.get(`${base}/api/me`)).json();
  const up = await ctx.request.put(`${base}/api/files/${me.root_id}/upload?name=zephyr-plan.md`, { headers: H, data: 'Project Zephyr launch plan' });
  if (!up.ok()) throw new Error(`upload ${up.status()}`);
  await post('/api/calendar/events', { summary: 'Zephyr kickoff', start: '2026-11-18T10:00:00Z', end: '2026-11-18T11:00:00Z' });
  const { contact } = await post('/api/contacts', { name: 'Zephyr Vendor', emails: ['sales@zephyr.example'] });

  step('search shows each kind with its own label');
  await page.goto(`${base}/search?q=zephyr`);
  const item = (label) => page.locator('a.list-item', { has: page.locator('span.pill', { hasText: label }) });
  for (const label of ['File', 'Event', 'Contact']) await item(label).first().waitFor({ timeout: 10000 });
  if (await item('Card').count()) throw new Error('an event or contact was labelled as a card');

  step('an event hit opens the calendar on its week');
  await item('Event').first().click();
  await page.waitForURL(/\/calendar\?view=week&date=2026-11-18/, { timeout: 10000 });
  await page.getByText('Zephyr kickoff').first().waitFor({ timeout: 10000 });

  step('a contact hit opens that contact');
  await page.goto(`${base}/search?q=zephyr&kinds=contact`);
  await item('Contact').first().click();
  await page.waitForURL(new RegExp(`/contacts\\?c=${contact.id}`), { timeout: 10000 });
  await page.getByText('sales@zephyr.example').first().waitFor({ timeout: 10000 });

  if (errors.length) throw new Error(`console errors: ${errors.join(' | ')}`);
  console.log(JSON.stringify({ ok: true, errors }));
} catch (e) {
  console.error(e);
  console.log(JSON.stringify({ ok: false, errors }));
  exitCode = 1;
} finally {
  await browser.close();
  process.exit(exitCode);
}
