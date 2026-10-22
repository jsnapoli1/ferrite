// Browser test of Mail against a REAL mail server (Dovecot IMAP + Postfix SMTP, see
// mail-server.sh) and a Ferrite server with a fresh data dir:
//
//   sudo ./mail-server.sh start                                  # once
//   FERRITE_SFU=0 FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8832 FERRITE_PUBLIC_URL=http://127.0.0.1:8832 ../target/debug/ferrite serve &
//   FERRITE_URL=http://127.0.0.1:8832 node mail.mjs
//
// The test empties alice@example.test's mailbox first (mail-server.sh reset; set
// MAIL_RESET=0 to skip), then delivers seed mail over SMTP. Covers: account setup dialog
// (provider lookup, connection test), folders with unread counts, the three panes, threads, sanitized HTML in
// a sandboxed iframe with remote images blocked until asked, attachments (download, save to
// Files), keyboard shortcuts (j/k, r, c, e, #), reply with quoting, compose with contact
// autocomplete and attachments uploaded and from Files, sending and live arrival (IMAP IDLE)
// with a notification, multi-select archive, delete to Trash, drafts (closing keeps a draft,
// editing it), global search hits for mail, light/dark/390px screenshots. Prints {ok, errors};
// any console error fails the run.
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { connect } from 'node:net';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8832';
const shots = process.env.SHOTS || '/tmp/ferrite-mail-shots';
const IMAP = { host: '127.0.0.1', port: Number(process.env.IMAP_PORT || 3143) };
const SMTP = { host: '127.0.0.1', port: Number(process.env.SMTP_PORT || 3587) };
const ALICE = { email: 'alice@example.test', password: 'alice-mail-pw' };
mkdirSync(shots, { recursive: true });
const here = dirname(fileURLToPath(import.meta.url));
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

/** Delivers a message over SMTP (AUTH PLAIN as Alice), like any mail client would. */
function smtpSend(from, to, data) {
  return new Promise((resolve, reject) => {
    const sock = connect(SMTP.port, SMTP.host);
    const auth = Buffer.from(`\0${ALICE.email}\0${ALICE.password}`).toString('base64');
    const script = ['EHLO e2e.test', `AUTH PLAIN ${auth}`, `MAIL FROM:<${from}>`, `RCPT TO:<${to}>`, 'DATA',
      `${data.replace(/\r?\n/g, '\r\n').replace(/^\./gm, '..')}\r\n.`, 'QUIT'];
    let buf = '';
    let step = -1;
    sock.setEncoding('utf8');
    sock.on('data', (chunk) => {
      buf += chunk;
      // Act once per complete reply (the last line has a space after the code).
      for (;;) {
        const m = buf.match(/^(\d{3}) .*\r\n/m);
        if (!m) return;
        const end = buf.indexOf(m[0]) + m[0].length;
        const code = Number(m[1]);
        buf = buf.slice(end);
        if (code >= 400) { sock.destroy(); reject(new Error(`SMTP ${m[0].trim()} after ${script[step] || 'greeting'}`)); return; }
        step++;
        if (step < script.length) sock.write(`${script[step]}\r\n`);
        else { sock.end(); resolve(); return; }
      }
    });
    sock.on('error', reject);
  });
}

const msg = (from, subject, id, headers, body) => `From: ${from}\nTo: Alice <${ALICE.email}>\nSubject: ${subject}\nMessage-ID: <${id}>\nDate: ${new Date().toUTCString()}\nMIME-Version: 1.0\n${headers}\n${body}`;

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

async function login(page) {
  await page.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
}

const exact = (text) => new RegExp(`^${text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`);
const row = (page, subject) => page.locator('.ml-row', { has: page.locator('.ml-subject', { hasText: exact(subject) }) });
const folderBtn = (page, role) => page.locator(`.ml-folder[data-role="${role}"]`);
const frameText = async (page) => {
  const f = page.frameLocator('.ml-card.expanded iframe.ml-frame').first();
  return f.locator('body').innerText();
};

try {
  if (process.env.MAIL_RESET !== '0') {
    log('empty the test mailbox');
    execFileSync('bash', [join(here, 'mail-server.sh'), 'reset'], { stdio: 'inherit' });
  }

  log('deliver seed mail over SMTP');
  await smtpSend('bob@example.test', ALICE.email, msg('Bob Builder <bob@example.test>', 'Quarterly report', 'report@e2e.test',
    'Content-Type: multipart/mixed; boundary="B"\n',
    '--B\nContent-Type: text/plain; charset=utf-8\n\nHi Alice,\n\nthe quarterly numbers are attached.\nhttps://example.org/q3\n--B\nContent-Type: text/csv; name="q3.csv"\nContent-Disposition: attachment; filename="q3.csv"\n\nregion,revenue\nnorth,42\n--B--\n'));
  await smtpSend('news@example.test', ALICE.email, msg('Ferrite News <news@example.test>', 'Monthly newsletter', 'news@e2e.test',
    'Content-Type: text/html; charset=utf-8\n',
    `<html><head><style>.hero{color:#c2410c}</style></head><body><h1 class="hero">October edition</h1><p onclick="alert(1)">Hello <b>subscriber</b>!</p><img alt="logo" src="${base}/favicon.svg" width="32"><script>alert('x')</script></body></html>`));
  await smtpSend('carol@example.test', ALICE.email, msg('Carol <carol@example.test>', 'Lunch on Friday?', 'lunch@e2e.test', 'Content-Type: text/plain\n', 'Shall we try the new place?\n'));
  await smtpSend('dave@example.test', ALICE.email, msg('Dave <dave@example.test>', 'Re: Lunch on Friday?', 'lunch-re@e2e.test',
    'In-Reply-To: <lunch@e2e.test>\nReferences: <lunch@e2e.test>\nContent-Type: text/plain\n', 'Count me in!\n\n> Shall we try the new place?\n'));
  await smtpSend('erin@example.test', ALICE.email, msg('Erin <erin@example.test>', 'Old receipt', 'receipt@e2e.test', 'Content-Type: text/plain\n', 'Your receipt.\n'));
  await sleep(1500);

  const page = await newPage('desktop');
  current = page;
  await page.api('POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  await page.api('POST', '/api/contacts', { name: 'Bob Builder', emails: ['bob@example.test'] });
  const me = await page.api('GET', '/api/me');
  const up = await page.request.put(`${base}/api/files/${me.root_id}/upload?name=agenda.md`, { headers: H, data: '# Agenda\n' });
  assert(up.ok(), 'upload agenda.md');
  await page.api('POST', `/api/files/${me.root_id}/mkdir`, { name: 'Mail attachments' });

  log('empty state and the account dialog');
  await page.goto(`${base}/mail`);
  await page.getByRole('heading', { name: 'Connect your mail' }).waitFor();
  await page.screenshot({ path: `${shots}/mail-empty.png` });
  await page.getByRole('button', { name: 'Add mail account' }).click();
  await page.locator('#ml-email').fill(ALICE.email);
  await page.locator('#ml-password').fill(ALICE.password);
  await page.locator('#ml-name').fill('Alice Archer');
  // The lookup finds nothing for example.test and guesses; enter the real servers.
  await page.getByText('No published settings found').waitFor({ timeout: 15000 });
  await page.locator('#imap-host').fill(IMAP.host);
  await page.locator('#imap-security').selectOption('none');
  await page.locator('#imap-port').fill(String(IMAP.port));
  await page.locator('#smtp-host').fill(SMTP.host);
  await page.locator('#smtp-security').selectOption('none');
  await page.locator('#smtp-port').fill(String(SMTP.port));
  await page.locator('#ml-signature').fill('Alice A.');
  await page.screenshot({ path: `${shots}/mail-setup.png` });
  await page.getByRole('button', { name: 'Connect' }).click();
  await page.locator('.modal').waitFor({ state: 'detached', timeout: 30000 });

  log('folders, unread counts and the list');
  await folderBtn(page, 'inbox').waitFor();
  for (const role of ['inbox', 'drafts', 'sent', 'junk', 'trash']) await folderBtn(page, role).waitFor();
  await row(page, 'Quarterly report').waitFor({ timeout: 15000 });
  assert(await page.locator('.ml-row').count() === 5, 'five messages');
  assert((await folderBtn(page, 'inbox').locator('.badge').innerText()) === '5', 'five unread');
  assert(await row(page, 'Quarterly report').locator('.ml-icon').count() === 1, 'attachment icon');
  assert(await row(page, 'Re: Lunch on Friday?').locator('.ml-count').innerText() === '2', 'thread size');

  log('HTML mail: sanitized, sandboxed, remote images on request');
  await row(page, 'Monthly newsletter').click();
  await page.locator('.ml-remote').waitFor();
  const frame = page.locator('.ml-card.expanded iframe.ml-frame');
  assert((await frame.getAttribute('sandbox')) === 'allow-same-origin allow-popups allow-popups-to-escape-sandbox', 'sandboxed frame');
  await page.waitForFunction(() => document.querySelector('.ml-card.expanded iframe')?.contentDocument?.body?.innerText.includes('October edition'));
  const html = await page.evaluate(() => document.querySelector('.ml-card.expanded iframe').contentDocument.documentElement.outerHTML);
  assert(!html.includes('<script') && !html.includes('onclick') && !html.includes('favicon.svg'), 'no script, handlers or remote image');
  assert((await frameText(page)).includes('Hello subscriber!'), 'html text');
  await page.getByRole('button', { name: 'Show images' }).click();
  await page.waitForFunction(() => {
    const img = document.querySelector('.ml-card.expanded iframe')?.contentDocument?.querySelector('img');
    return img && img.complete && img.naturalWidth > 0;
  }, null, { timeout: 10000 });
  await page.screenshot({ path: `${shots}/mail-html.png` });
  assert(!(await row(page, 'Monthly newsletter').getAttribute('class')).includes('unread'), 'opening marks read');

  log('a conversation, keyboard navigation');
  await row(page, 'Re: Lunch on Friday?').click();
  await page.locator('.ml-card').nth(1).waitFor();
  assert(await page.locator('.ml-card').count() === 2, 'two messages in the thread');
  await page.locator('.ml-card.expanded .ml-text', { hasText: 'Count me in!' }).waitFor();
  await page.screenshot({ path: `${shots}/mail-desktop.png` });
  const subjects = await page.locator('.ml-row .ml-subject').allInnerTexts();
  const at = subjects.indexOf('Re: Lunch on Friday?');
  const [prev, next] = [subjects[at - 1], subjects[at + 1]];
  await page.locator('.ml-list-body').focus();
  if (next) {
    await page.keyboard.press('j');
    await page.locator('.ml-thread-head h2').filter({ hasText: exact(next) }).waitFor();
    await page.keyboard.press('k');
  } else {
    await page.keyboard.press('k');
    await page.locator('.ml-thread-head h2').filter({ hasText: exact(prev) }).waitFor();
    await page.keyboard.press('j');
  }
  await page.locator('.ml-thread-head h2').filter({ hasText: exact('Re: Lunch on Friday?') }).waitFor();

  log('reply with r: quoted, threaded, sent, filed in Sent');
  await page.keyboard.press('r');
  await page.locator('.modal .ml-compose').waitFor();
  assert((await page.locator('.modal .ml-chip').first().innerText()).includes('Dave'), 'reply goes to Dave');
  const quoted = await page.locator('.ml-body-input').inputValue();
  assert(quoted.includes('> Count me in!') && quoted.includes('-- \nAlice A.'), 'quote and signature');
  await page.locator('.ml-body-input').press('Control+Home');
  await page.locator('.ml-body-input').type('Friday works for me.');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Message sent' }).waitFor({ timeout: 20000 });
  await folderBtn(page, 'sent').click();
  await row(page, 'Re: Lunch on Friday?').waitFor({ timeout: 15000 });
  await row(page, 'Re: Lunch on Friday?').click();
  await page.locator('.ml-card').nth(2).waitFor();
  assert(await page.locator('.ml-card').count() === 3, 'the reply joined the conversation');

  log('compose with c: contact autocomplete, upload and Files attachments; arrives live');
  await folderBtn(page, 'inbox').click();
  await row(page, 'Quarterly report').waitFor();
  await page.locator('.ml-list-body').focus();
  await page.keyboard.press('c');
  await page.locator('.modal .ml-compose').waitFor();
  await page.keyboard.type('Bob');
  await page.locator('.talk-suggest-item', { hasText: 'bob@example.test' }).click();
  // Also to Alice herself, so the message comes back into the inbox.
  await page.keyboard.type('alice@example.test,');
  assert(await page.locator('.modal .ml-chip').count() === 2, 'two recipients');
  await page.locator('.ml-subject-input').fill('Planning notes');
  await page.locator('.ml-body-input').fill('See the attached agenda and notes.');
  await page.locator('.ml-compose input[type=file]').setInputFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('uploaded notes') });
  await page.locator('.ml-file-chip', { hasText: 'notes.txt' }).locator('button').waitFor();
  await page.getByRole('button', { name: 'From Files' }).click();
  await page.locator('.picker-list .list-item', { hasText: 'agenda.md' }).click();
  await page.getByRole('button', { name: 'Attach', exact: true }).click();
  await page.locator('.ml-file-chip', { hasText: 'agenda.md' }).waitFor();
  await page.screenshot({ path: `${shots}/mail-compose.png` });
  await page.keyboard.press('Control+Enter');
  await page.locator('.toast', { hasText: 'Message sent' }).waitFor({ timeout: 20000 });
  // IMAP IDLE brings it in without a reload, with a notification.
  await row(page, 'Planning notes').waitFor({ timeout: 20000 });
  await page.locator('.toast', { hasText: 'New mail from Alice Archer' }).waitFor({ timeout: 10000 });
  await row(page, 'Planning notes').click();
  await page.locator('.ml-attachment', { hasText: 'agenda.md' }).waitFor();
  assert(await page.locator('.ml-attachment').count() === 2, 'both attachments arrived');

  log('attachments: download and save to Files');
  await row(page, 'Quarterly report').click();
  const att = page.locator('.ml-attachment', { hasText: 'q3.csv' });
  await att.waitFor();
  const [download] = await Promise.all([page.waitForEvent('download'), att.locator('a').click()]);
  assert(download.suggestedFilename() === 'q3.csv', 'download name');
  await att.getByRole('button', { name: /Save q3.csv to Files/ }).click();
  await page.locator('.fl-picker-list button', { hasText: 'Mail attachments' }).click();
  await page.getByRole('button', { name: 'Save here' }).click();
  await page.locator('.toast', { hasText: 'Saved as q3.csv' }).waitFor();
  const { results } = await page.api('GET', '/api/files/resolve?path=/Mail%20attachments/q3.csv').then((n) => ({ results: [n] })).catch(() => ({ results: [] }));
  assert(results.length === 1, 'q3.csv is in Files');

  log('multi-select and archive; delete to Trash with #');
  await row(page, 'Old receipt').locator('.ml-check input').check();
  await row(page, 'Lunch on Friday?').locator('.ml-check input').check();
  await page.locator('.ml-selection', { hasText: '2 selected' }).waitFor();
  await page.locator('.ml-selection').getByRole('button', { name: 'Archive', exact: true }).click();
  await page.locator('.toast', { hasText: 'Archived 2 messages' }).waitFor({ timeout: 15000 });
  await folderBtn(page, 'archive').click();
  await row(page, 'Old receipt').waitFor({ timeout: 15000 });
  await folderBtn(page, 'inbox').click();
  await row(page, 'Monthly newsletter').click();
  await page.locator('.ml-list-body').focus();
  await page.keyboard.press('#');
  await page.locator('.toast', { hasText: 'Moved to Trash' }).waitFor({ timeout: 15000 });
  await folderBtn(page, 'trash').click();
  await row(page, 'Monthly newsletter').waitFor({ timeout: 15000 });

  log('drafts: closing a changed message keeps it; editing it');
  await folderBtn(page, 'inbox').click();
  await page.getByRole('button', { name: 'Compose' }).click();
  await page.locator('.modal .ml-compose').waitFor();
  await page.locator('.ml-subject-input').fill('Unfinished thoughts');
  await page.locator('.ml-body-input').fill('To be continued');
  await page.keyboard.press('Escape');
  await page.locator('.toast', { hasText: 'Saved to Drafts' }).waitFor({ timeout: 15000 });
  await folderBtn(page, 'drafts').click();
  await row(page, 'Unfinished thoughts').click();
  await page.getByRole('button', { name: 'Edit draft' }).click();
  assert((await page.locator('.ml-body-input').inputValue()).includes('To be continued'), 'draft text');
  await page.getByRole('button', { name: 'Discard' }).click();

  log('global search finds mail and opens it');
  await page.goto(`${base}/search?q=quarterly&kinds=mail`);
  const hit = page.locator('a.list-item', { has: page.locator('span.pill', { hasText: 'Mail' }) }).first();
  await hit.waitFor({ timeout: 10000 });
  await hit.click();
  await page.waitForURL(/\/mail\?m=/);
  await page.locator('.ml-thread-head h2', { hasText: 'Quarterly report' }).waitFor();
  await page.locator('.ml-folder.active[data-role="inbox"]').waitFor();
  await page.screenshot({ path: `${shots}/mail-light.png` });

  log('dark theme');
  const dark = await newPage('dark', { width: 1440, height: 900 }, 'dark');
  await login(dark);
  await dark.goto(`${base}/mail`);
  await row(dark, 'Quarterly report').click();
  await dark.locator('.ml-card.expanded .ml-text').waitFor();
  await dark.screenshot({ path: `${shots}/mail-dark.png` });
  await dark.context().close();

  log('phone: one pane at a time');
  const phone = await newPage('phone', { width: 390, height: 844 });
  await login(phone);
  await phone.goto(`${base}/mail`);
  await row(phone, 'Quarterly report').waitFor();
  assert(!(await phone.locator('.ml-reader').isVisible()), 'reader hidden on the list');
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll');
  await phone.screenshot({ path: `${shots}/mail-phone-list.png` });
  await row(phone, 'Planning notes').click();
  await phone.locator('.ml-card.expanded .ml-text').waitFor();
  assert(!(await phone.locator('.ml-list').isVisible()), 'list hidden while reading');
  await phone.screenshot({ path: `${shots}/mail-phone-reader.png` });
  await phone.getByRole('button', { name: 'Back to the list' }).click();
  await row(phone, 'Quarterly report').waitFor();
  await phone.getByRole('button', { name: 'Folders' }).click();
  await folderBtn(phone, 'sent').waitFor({ state: 'visible' });
  await phone.waitForTimeout(400); // the drawer slides in
  await phone.screenshot({ path: `${shots}/mail-phone-folders.png` });
  await folderBtn(phone, 'sent').click();
  await row(phone, 'Re: Lunch on Friday?').waitFor();
  await phone.context().close();

  if (errors.length) throw new Error(`console errors: ${errors.join(' | ')}`);
  console.log(JSON.stringify({ ok: true, errors }));
} catch (e) {
  console.error(e);
  if (current) await current.screenshot({ path: `${shots}/failure.png` }).catch(() => {});
  console.log(JSON.stringify({ ok: false, errors }));
  exitCode = 1;
} finally {
  await browser.close();
  process.exit(exitCode);
}
