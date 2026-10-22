// Browser test of Files, public links, Settings and the Assistant page.
//
// Needs a server with a FRESH data directory (the test creates the administrator itself):
//   FERRITE_URL=http://127.0.0.1:8821 node files.mjs
// Screenshots go to $SHOTS (default /tmp).
import { chromium } from 'playwright';
import http from 'node:http';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8821';
const shots = process.env.SHOTS || '/tmp';
const T = 15000;

/**
 * A tiny OpenAI-compatible model: the first turn calls files_list, the next answers.
 * Lets the test drive the assistant end to end without a real model.
 */
const fakeModel = http.createServer((req, res) => {
  let body = '';
  req.on('data', (c) => { body += c; });
  req.on('end', () => {
    res.setHeader('Content-Type', 'application/json');
    if (req.url.endsWith('/models')) return res.end(JSON.stringify({ data: [{ id: 'fake-1' }, { id: 'fake-2' }] }));
    const { messages } = JSON.parse(body || '{}');
    const last = messages[messages.length - 1];
    const message = last.role === 'tool'
      ? { role: 'assistant', content: '**Done.** Your root has a folder called `Projects`.' }
      : { role: 'assistant', content: 'Let me look at your files.', tool_calls: [{ id: 'c1', type: 'function', function: { name: 'files_list', arguments: '{"path":"/"}' } }] };
    setTimeout(() => res.end(JSON.stringify({ choices: [{ message, finish_reason: message.tool_calls ? 'tool_calls' : 'stop' }] })), 400);
  });
});
await new Promise((ok) => fakeModel.listen(0, '127.0.0.1', ok));
const fakeUrl = `http://127.0.0.1:${fakeModel.address().port}/v1`;

const browser = await chromium.launch();
const errors = [];
const foreign = [];
const steps = [];
const pages = [];

/** Collects console errors and page errors; 4xx responses we provoke on purpose are expected. */
function watch(page, who) {
  pages.push([who, page]);
  // The shell probes /api/me (and /api/config) to find out whether someone is signed in;
  // signed out, those answer 401. The missing public link answers 404 on purpose.
  const expected = (u) => /\/api\/public\/does-not-exist/.test(u) || /\/api\/(me|config)$/.test(u);
  page.on('console', (m) => {
    if (m.type() !== 'error') return;
    const loc = m.location()?.url || '';
    if (/Failed to load resource/.test(m.text()) && expected(loc)) return;
    const entry = `[${who}] ${m.text()} @ ${loc} (page ${page.url()})`;
    // The live editor (/edit/:id) is another engineer's view; report its errors separately.
    if (new URL(page.url()).pathname.startsWith('/edit/')) foreign.push(entry); else errors.push(entry);
  });
  page.on('pageerror', (e) => {
    const entry = `[${who}] ${e.message} (page ${page.url()})`;
    if (new URL(page.url()).pathname.startsWith('/edit/')) foreign.push(entry); else errors.push(entry);
  });
}

function step(name) {
  steps.push(name);
  console.error(`· ${name}`);
}

const rowOf = (page, name) => page.locator('tr.fl-row', { hasText: name }).first();

async function rowAction(page, name, action) {
  await rowOf(page, name).getByRole('button', { name: `Actions for ${name}` }).click();
  await page.getByRole('menuitem', { name: action, exact: true }).click();
}

async function dialog(page, title) {
  const d = page.getByRole('dialog', { name: title });
  await d.waitFor({ timeout: T });
  return d;
}

// A 1x1 PNG.
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');

try {
  // ---- setup as admin -----------------------------------------------------------------
  const status = await (await fetch(`${base}/api/status`)).json();
  if (!status.setup_needed) throw new Error('the server must start with a fresh data directory');
  const adminCtx = await browser.newContext({ viewport: { width: 1360, height: 860 } });
  const page = await adminCtx.newPage();
  watch(page, 'admin');
  step('setup');
  await page.goto(base);
  await page.getByLabel('Username').fill('admin');
  await page.getByLabel('Your name').fill('Ada Admin');
  await page.getByLabel('Password (8+ characters)').fill('admin-password');
  await page.getByRole('button', { name: 'Create administrator' }).click();
  await page.getByRole('heading', { name: 'Hello, Ada' }).waitFor({ timeout: T });
  const h = { 'X-Ferrite-Request': '1' };
  const r = await page.request.post(`${base}/api/admin/users`, { headers: h, data: { username: 'bob', display_name: 'Bob Builder', password: 'bob-password' } });
  if (!r.ok()) throw new Error(`creating bob: ${r.status()}`);

  // ---- files: empty folder, new folder, upload ----------------------------------------
  step('files: empty state');
  await page.goto(`${base}/files`);
  await page.getByRole('heading', { name: 'This folder is empty' }).waitFor({ timeout: T });
  await page.screenshot({ path: `${shots}/files-empty.png` });

  step('files: new folder');
  await page.getByRole('button', { name: 'New folder' }).click();
  let d = await dialog(page, 'New folder');
  await d.getByRole('textbox').fill('Projects');
  await d.getByRole('textbox').press('Enter');
  await rowOf(page, 'Projects').waitFor({ timeout: T });

  step('files: upload');
  await page.locator('input[type=file]').setInputFiles([
    { name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('first draft\n') },
    { name: 'photo.png', mimeType: 'image/png', buffer: PNG },
    { name: 'data.bin', mimeType: 'application/octet-stream', buffer: Buffer.alloc(2048, 7) },
  ]);
  for (const n of ['notes.txt', 'photo.png', 'data.bin']) await rowOf(page, n).waitFor({ timeout: T });

  step('files: image preview');
  await rowOf(page, 'photo.png').click();
  d = await dialog(page, 'photo.png');
  await d.locator('img').waitFor({ timeout: T });
  await page.keyboard.press('Escape');
  await d.waitFor({ state: 'detached' });

  step('files: rename');
  await rowAction(page, 'data.bin', 'Rename');
  d = await dialog(page, 'Rename file');
  await d.getByRole('textbox').fill('data-renamed.bin');
  await d.getByRole('button', { name: 'Rename' }).click();
  await rowOf(page, 'data-renamed.bin').waitFor({ timeout: T });

  step('files: sort by size');
  await page.getByRole('button', { name: 'Size' }).click();
  await page.getByRole('button', { name: 'Size' }).click();

  step('files: versions');
  await page.locator('input[type=file]').setInputFiles([{ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('second draft, longer\n') }]);
  await page.waitForFunction(() => [...document.querySelectorAll('tr.fl-row')].some((r) => r.textContent.includes('notes.txt') && r.textContent.includes('21 B')), null, { timeout: T });
  await rowAction(page, 'notes.txt', 'Versions');
  d = await dialog(page, 'Versions of “notes.txt”');
  await d.getByRole('button', { name: 'Restore' }).first().click();
  await page.getByText('Version restored').waitFor({ timeout: T });
  await d.getByRole('button', { name: 'Close' }).last().click();
  await page.waitForFunction(() => [...document.querySelectorAll('tr.fl-row')].some((r) => r.textContent.includes('notes.txt') && r.textContent.includes('12 B')), null, { timeout: T });

  step('files: navigate into folder and upload');
  await rowOf(page, 'Projects').click();
  await page.waitForURL(/\/files\/[0-9a-f-]+$/);
  await page.getByRole('navigation', { name: 'Breadcrumbs' }).getByText('Projects').waitFor();
  await page.locator('input[type=file]').setInputFiles([{ name: 'plan.md', mimeType: 'text/markdown', buffer: Buffer.from('# Plan\n') }]);
  await rowOf(page, 'plan.md').waitFor({ timeout: T });
  const projectsUrl = page.url();

  step('files: share with bob and create a public link');
  await page.getByRole('navigation', { name: 'Breadcrumbs' }).getByRole('link', { name: 'All files' }).click();
  await rowOf(page, 'Projects').waitFor({ timeout: T });
  await rowAction(page, 'Projects', 'Share');
  d = await dialog(page, 'Share “Projects”');
  await d.getByLabel('User to share with').fill('bob');
  await d.getByRole('button', { name: 'Share', exact: true }).click();
  await d.locator('.fl-share-row', { hasText: 'Bob Builder' }).waitFor({ timeout: T });
  await d.getByRole('tab', { name: 'Public link' }).click();
  await d.getByLabel('Password', { exact: true }).fill('link-secret');
  await d.getByLabel('Permission', { exact: true }).selectOption({ label: 'Can view and upload' });
  await d.getByRole('button', { name: 'Create link' }).click();
  const linkRow = d.locator('.fl-share-row', { hasText: 'Public link' });
  await linkRow.waitFor({ timeout: T });
  const linkUrl = (await linkRow.locator('.mono').textContent()).trim();
  await page.screenshot({ path: `${shots}/files-share.png` });
  await d.getByRole('button', { name: 'Done' }).click();

  step('files: shared by me');
  await page.getByRole('link', { name: 'Shared by me' }).click();
  await page.locator('tr.fl-row', { hasText: 'Bob Builder' }).waitFor({ timeout: T });

  // ---- bob sees the share ---------------------------------------------------------------
  step('bob: shared with me');
  const bobCtx = await browser.newContext({ viewport: { width: 1280, height: 800 } });
  const bob = await bobCtx.newPage();
  watch(bob, 'bob');
  await bob.goto(base);
  await bob.getByLabel('Username').fill('bob');
  await bob.getByLabel('Password').fill('bob-password');
  await bob.getByRole('button', { name: 'Sign in' }).click();
  await bob.getByRole('heading', { name: 'Hello, Bob' }).waitFor({ timeout: T });
  await bob.goto(`${base}/files?view=shared`);
  await rowOf(bob, 'Projects').waitFor({ timeout: T });
  await rowOf(bob, 'Projects').getByText('Shared by Ada Admin').waitFor();
  await bob.screenshot({ path: `${shots}/files-shared-with-me.png` });
  await rowOf(bob, 'Projects').click();
  await rowOf(bob, 'plan.md').waitFor({ timeout: T });

  // ---- public link, signed out -------------------------------------------------------------
  step('public: password, listing, upload');
  const anonCtx = await browser.newContext({ viewport: { width: 1100, height: 760 } });
  const anon = await anonCtx.newPage();
  watch(anon, 'anon');
  await anon.goto(linkUrl);
  await anon.getByLabel('Password').fill('link-secret');
  await anon.screenshot({ path: `${shots}/public-locked.png` });
  await anon.getByRole('button', { name: 'Open' }).click();
  await anon.getByText('plan.md').waitFor({ timeout: T });
  // The owner watches the folder: the visitor's upload must appear live, without a reload.
  await page.goto(projectsUrl);
  await rowOf(page, 'plan.md').waitFor({ timeout: T });
  await anon.locator('input[type=file]').setInputFiles([{ name: 'dropped.txt', mimeType: 'text/plain', buffer: Buffer.from('from a visitor') }]);
  await anon.locator('.pub-row', { hasText: 'dropped.txt' }).waitFor({ timeout: T });
  await anon.screenshot({ path: `${shots}/public-folder.png` });
  await anon.emulateMedia({ colorScheme: 'dark' });
  await anon.screenshot({ path: `${shots}/public-folder-dark.png` });
  step('public: missing link');
  await anon.goto(`${base}/s/does-not-exist`);
  await anon.getByRole('heading', { name: 'This link doesn’t work anymore' }).waitFor({ timeout: T });
  await anon.screenshot({ path: `${shots}/public-404-dark.png` });

  step('files: live refresh shows the visitor upload');
  await rowOf(page, 'dropped.txt').waitFor({ timeout: T });

  step('bob: notification link /files?node=');
  await bob.goto(`${base}/files?node=${projectsUrl.split('/').pop()}`);
  await bob.waitForURL(projectsUrl, { timeout: T });
  await rowOf(bob, 'dropped.txt').waitFor({ timeout: T });

  // ---- trash ----------------------------------------------------------------------------
  step('trash: delete and restore');
  await page.goto(`${base}/files`);
  await rowAction(page, 'photo.png', 'Delete');
  await rowOf(page, 'photo.png').waitFor({ state: 'detached', timeout: T });
  await page.getByRole('link', { name: 'Trash' }).click();
  await rowOf(page, 'photo.png').waitFor({ timeout: T });
  await page.screenshot({ path: `${shots}/files-trash.png` });
  await page.getByRole('button', { name: 'Restore photo.png' }).click();
  await rowOf(page, 'photo.png').waitFor({ state: 'detached', timeout: T });
  await page.getByRole('link', { name: 'All files' }).click();
  await rowOf(page, 'photo.png').waitFor({ timeout: T });

  step('files: open with');
  await page.locator('input[type=file]').setInputFiles([{ name: 'report.docx', mimeType: 'application/octet-stream', buffer: Buffer.from('PK fake docx') }]);
  await rowOf(page, 'report.docx').waitFor({ timeout: T });
  await rowAction(page, 'report.docx', 'Open with…');
  d = await dialog(page, 'Open “report.docx” with');
  await d.getByText('WordCraft').first().waitFor({ timeout: T });
  await d.getByText('On your computer').waitFor();
  await page.waitForTimeout(300);
  await page.screenshot({ path: `${shots}/files-open-with.png` });
  await page.keyboard.press('Escape');

  step('files: light and dark screenshots');
  await page.screenshot({ path: `${shots}/files-light.png` });
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.screenshot({ path: `${shots}/files-dark.png` });
  await page.setViewportSize({ width: 390, height: 780 });
  await page.waitForTimeout(400);
  await page.screenshot({ path: `${shots}/files-mobile-dark.png` });
  await page.emulateMedia({ colorScheme: 'light' });
  await page.screenshot({ path: `${shots}/files-mobile.png` });
  await page.setViewportSize({ width: 1360, height: 860 });

  step('files: new mind map');
  await page.getByRole('button', { name: 'New document' }).click();
  await page.getByRole('menuitem', { name: 'Mind map' }).click();
  d = await dialog(page, 'New mind map');
  await d.getByRole('textbox').fill('Ideas');
  await d.getByRole('button', { name: 'Create' }).click();
  await page.waitForURL(/\/edit\/[0-9a-f-]+$/, { timeout: T });

  // ---- settings ---------------------------------------------------------------------------
  step('settings: token');
  await page.goto(`${base}/settings/tokens`);
  await page.getByLabel('Name', { exact: true }).fill('Laptop sync');
  await page.getByRole('button', { name: 'Create token' }).click();
  d = await dialog(page, 'Your new app token');
  await d.getByRole('button', { name: 'Done' }).click();
  await page.locator('table.set-table', { hasText: 'Laptop sync' }).waitFor({ timeout: T });
  await page.getByText('Bearer ').first().waitFor();
  await page.screenshot({ path: `${shots}/settings-tokens.png`, fullPage: true });

  step('settings: AI provider');
  await page.goto(`${base}/settings/ai`);
  await page.getByRole('button', { name: 'Ollama', exact: true }).click();
  await page.getByRole('button', { name: 'Add provider' }).click();
  await page.locator('.set-provider', { hasText: 'http://127.0.0.1:11434/v1' }).waitFor({ timeout: T });
  await page.locator('.set-role').first().waitFor();
  await page.screenshot({ path: `${shots}/settings-ai.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.screenshot({ path: `${shots}/settings-ai-dark.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'light' });

  step('settings: other tabs');
  await page.goto(`${base}/settings/profile`);
  await page.getByRole('radio', { name: 'Dark' }).click();
  if (await page.evaluate(() => document.documentElement.dataset.theme) !== 'dark') throw new Error('theme toggle did not apply');
  await page.getByRole('radio', { name: 'System' }).click();
  for (const [tab, text] of [['profile', 'Change password'], ['mcp', 'Connect a server'], ['craft', 'GridCraft'], ['admin', 'Saved by deduplication'], ['about', 'WebDAV']]) {
    await page.goto(`${base}/settings/${tab}`);
    await page.getByText(text).first().waitFor({ timeout: T });
    await page.screenshot({ path: `${shots}/settings-${tab}.png`, fullPage: true });
  }

  step('assistant: not configured');
  await page.goto(`${base}/assistant`);
  await page.getByRole('heading', { name: 'The assistant isn’t set up yet' }).waitFor({ timeout: T });
  await page.screenshot({ path: `${shots}/assistant-setup.png` });
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.screenshot({ path: `${shots}/assistant-setup-dark.png` });
  await page.emulateMedia({ colorScheme: 'light' });

  step('settings: connect a (fake) model and assign the chat role');
  await page.goto(`${base}/settings/ai`);
  await page.getByLabel('Name', { exact: true }).fill('Test model');
  await page.getByLabel('Base URL').fill(fakeUrl);
  await page.getByRole('button', { name: 'Add provider' }).click();
  const prov = page.locator('.set-provider', { hasText: 'Test model' });
  await prov.waitFor({ timeout: T });
  await prov.getByRole('button', { name: 'Refresh models' }).click();
  await prov.getByText('2 models').waitFor({ timeout: T });
  await page.getByLabel('Provider for chat').selectOption({ label: 'Test model' });
  await page.getByLabel('Model for chat').fill('fake-1');
  await page.getByRole('button', { name: 'Save chat role' }).click();
  await page.locator('form.set-role', { has: page.getByLabel('Model for chat') }).getByText('set', { exact: true }).waitFor({ timeout: T });

  step('assistant: consent and chat');
  await page.goto(`${base}/assistant`);
  await page.getByRole('button', { name: 'Turn on the assistant' }).click();
  await page.getByRole('heading', { name: 'How can I help, Ada?' }).waitFor({ timeout: T });
  await page.screenshot({ path: `${shots}/assistant-welcome.png` });
  await page.getByLabel('Message the assistant').fill('What is in my files?');
  await page.getByLabel('Message the assistant').press('Enter');
  await page.locator('.ai-msg.pending .ai-step').waitFor({ timeout: T });
  await page.getByText('Done.').waitFor({ timeout: 2 * T });
  await page.getByText('Used 1 tool').click();
  await page.locator('.ai-step summary', { hasText: 'files_list' }).click();
  await page.locator('.ai-step-body pre').last().waitFor();
  await page.screenshot({ path: `${shots}/assistant-chat.png` });
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.screenshot({ path: `${shots}/assistant-chat-dark.png` });
  await page.reload();
  await page.getByText('Done.').waitFor({ timeout: T });
  await page.getByRole('button', { name: 'New chat' }).click();
  await page.getByRole('heading', { name: 'How can I help, Ada?' }).waitFor({ timeout: T });

  step('mobile layouts');
  await page.setViewportSize({ width: 390, height: 780 });
  await page.emulateMedia({ colorScheme: 'light' });
  for (const [path, name] of [['/files', 'files'], ['/assistant', 'assistant'], ['/settings/tokens', 'settings-tokens'], ['/settings/admin', 'settings-admin'], ['/files?view=trash', 'files-trash']]) {
    await page.goto(`${base}${path}`);
    await page.waitForTimeout(600);
    const overflow = await page.evaluate(() => { const m = document.querySelector('.main') || document.scrollingElement; return m.scrollWidth - m.clientWidth; });
    if (overflow > 1) throw new Error(`${path} overflows horizontally by ${overflow}px at 390px`);
    await page.screenshot({ path: `${shots}/mobile-${name}.png` });
  }
  await anon.setViewportSize({ width: 390, height: 780 });
  await anon.goto(linkUrl);
  await anon.getByText('plan.md').waitFor({ timeout: T });
  await anon.screenshot({ path: `${shots}/mobile-public-dark.png` });

  // Let late events (e.g. live socket) settle before judging.
  await page.waitForTimeout(500);
  console.log(JSON.stringify({ ok: errors.length === 0, steps: steps.length, errors, foreign }, null, 1));
  await browser.close();
  fakeModel.close();
  process.exit(errors.length ? 1 : 0);
} catch (e) {
  // Leave a picture of every page for debugging.
  for (const [who, p] of pages) {
    await p.screenshot({ path: `${shots}/failure-${who}.png`, timeout: 5000 }).catch((x) => console.error(`screenshot ${who}: ${x.message.split('\n')[0]}`));
    if (process.env.DEBUG) console.error(who, await Promise.race([p.evaluate(() => document.body.innerText.slice(0, 1500)), new Promise((r) => setTimeout(() => r('(page not responding)'), 3000))]));
  }
  console.log(JSON.stringify({ ok: false, failedAt: steps[steps.length - 1], error: String(e.stack || e), errors, foreign }, null, 1));
  await browser.close();
  fakeModel.close();
  process.exit(1);
}
