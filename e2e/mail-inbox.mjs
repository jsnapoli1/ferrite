// Browser test of Mail's inbox management against a REAL mail server (Dovecot, see
// mail-server.sh) and a Ferrite server with a fresh data dir. Without Postfix, smtp-relay.mjs
// stands in for SMTP (it delivers into Dovecot with mail-server.sh deliver):
//
//   export MAIL_TEST_DIR=/tmp/ferrite-mail-inbox IMAP_PORT=3311 LMTP_PORT=3312 AUTH_PORT=3313 SIEVE_PORT=3314
//   POSTFIX=0 SHARED=1 EXTRA_USERS=carol bash mail-server.sh start $MAIL_TEST_DIR   # once
//   EXTRA_USERS=carol node smtp-relay.mjs 3315 $MAIL_TEST_DIR &
//   FERRITE_SFU=0 FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8861 FERRITE_PUBLIC_URL=http://127.0.0.1:8861 ../target/debug/ferrite serve &
//   FERRITE_URL=http://127.0.0.1:8861 SMTP_PORT=3315 node mail-inbox.mjs
//
// The test empties the test mailboxes first (mail-server.sh reset), delivers seed mail, and
// checks results on the IMAP server itself. Covers: layouts (side by side, stacked, list
// only), compact rows, one row per conversation, oldest first, favourites on top and the
// Favourites view, preferences kept on the server and admin defaults, All inboxes across two
// accounts, the priority inbox, tags (IMAP keywords: apply, create, recolour, tag view),
// one-click row actions, snooze and unsnooze, quick actions, the move dialog and drag and
// drop, folders (create, subfolder, rename, move, statistics, mark all read, unsubscribe and
// showing unsubscribed folders, empty, delete), a shared folder, inbox settings, junk,
// advanced search, deep links after a move, the mailto: handler, Home's mail widgets, and
// light/dark/390px screenshots. Prints {ok, errors}; any console error fails the run.
import { chromium } from 'playwright';
import { mkdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { connect } from 'node:net';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8861';
const shots = process.env.SHOTS || '/tmp/ferrite-mail-inbox-shots';
const mailDir = process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-inbox';
const IMAP = { host: '127.0.0.1', port: Number(process.env.IMAP_PORT || 3311) };
const SMTP = { host: '127.0.0.1', port: Number(process.env.SMTP_PORT || 3315) };
const ALICE = { email: 'alice@example.test', password: 'alice-mail-pw' };
const CAROL = { email: 'carol@example.test', password: 'carol-mail-pw' };
mkdirSync(shots, { recursive: true });
const here = dirname(fileURLToPath(import.meta.url));
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

/** Delivers a message over SMTP, like any mail client would. */
function smtpSend(from, to, data) {
  return new Promise((resolve, reject) => {
    const sock = connect(SMTP.port, SMTP.host);
    const script = ['EHLO e2e.test', `MAIL FROM:<${from}>`, `RCPT TO:<${to}>`, 'DATA', `${data.replace(/\r?\n/g, '\r\n').replace(/^\./gm, '..')}\r\n.`, 'QUIT'];
    let buf = '';
    let step = -1;
    sock.setEncoding('utf8');
    sock.on('data', (chunk) => {
      buf += chunk;
      for (;;) {
        const m = buf.match(/^(\d{3}) .*\r\n/m);
        if (!m) return;
        buf = buf.slice(buf.indexOf(m[0]) + m[0].length);
        if (Number(m[1]) >= 400) { sock.destroy(); reject(new Error(`SMTP ${m[0].trim()}`)); return; }
        step++;
        if (step < script.length) sock.write(`${script[step]}\r\n`);
        else { sock.end(); resolve(); return; }
      }
    });
    sock.on('error', reject);
  });
}

/** Runs IMAP commands as a user and returns the untagged responses of the last one. */
function imap(user, commands) {
  return new Promise((resolve, reject) => {
    const sock = connect(IMAP.port, IMAP.host);
    const all = [`LOGIN "${user.email}" "${user.password}"`, ...commands, 'LOGOUT'];
    let buf = '';
    let step = -1;
    let untagged = [];
    let last = [];
    sock.setEncoding('utf8');
    const next = () => {
      step++;
      if (step < all.length) sock.write(`t${step} ${all[step]}\r\n`);
    };
    sock.on('data', (chunk) => {
      buf += chunk;
      let i;
      while ((i = buf.indexOf('\r\n')) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 2);
        if (step === -1 && line.startsWith('* OK')) { next(); continue; }
        if (line.startsWith('* ')) { untagged.push(line); continue; }
        const m = line.match(/^t(\d+) (OK|NO|BAD)/);
        if (!m) continue;
        if (m[2] !== 'OK' && Number(m[1]) < all.length - 1) { sock.destroy(); reject(new Error(`IMAP ${line} after ${all[step]}`)); return; }
        if (Number(m[1]) === all.length - 2) last = untagged;
        untagged = [];
        if (Number(m[1]) === all.length - 1) { sock.end(); resolve(last); return; }
        next();
      }
    });
    sock.on('error', reject);
  });
}

/** UIDs matching an IMAP SEARCH in a folder. */
async function search(user, folder, criteria) {
  const lines = await imap(user, [`SELECT "${folder}"`, `UID SEARCH ${criteria}`]);
  const found = lines.find((l) => l.startsWith('* SEARCH'));
  return found ? found.slice(8).trim().split(/\s+/).filter(Boolean).map(Number) : [];
}

async function folderNames(user) {
  const lines = await imap(user, ['LIST "" "*"']);
  return lines.map((l) => (l.match(/"\/" "?(.*?)"?$/) || [])[1]).filter(Boolean);
}

async function waitFor(what, fn, timeout = 15000) {
  const end = Date.now() + timeout;
  for (;;) {
    if (await fn()) return;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(250);
  }
}

const day = 864e5;
const msg = (from, to, subject, id, { date = new Date(), headers = '', body = 'Hello.' } = {}) => `From: ${from}\nTo: ${to}\nSubject: ${subject}\nMessage-ID: <${id}>\nDate: ${date.toUTCString()}\nMIME-Version: 1.0\n${headers}Content-Type: text/plain; charset=utf-8\n\n${body}\n`;

const browser = await chromium.launch();
const errors = [];
let exitCode = 0;
let current = null;

async function newPage(name, viewport = { width: 1440, height: 900 }, colorScheme = 'light', extra = {}) {
  const ctx = await browser.newContext({ viewport, colorScheme, ...extra });
  await ctx.grantPermissions(['clipboard-read', 'clipboard-write'], { origin: base });
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

const exact = (text) => new RegExp(`^${text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`);
const row = (page, subject) => page.locator('.ml-row', { has: page.locator('.ml-subject', { hasText: exact(subject) }) });
const folderBtn = (page, role) => page.locator(`.ml-folder[data-role="${role}"]`);
const folderNamed = (page, name) => page.locator('.ml-folder', { has: page.locator('span.grow', { hasText: exact(name) }) });
const viewBtn = (page, view) => page.locator(`.ml-folder[data-view="${view}"]`);
const menuItem = (page, label) => page.locator('.menu button', { hasText: exact(label) });
const modal = (page) => page.locator('.modal');
const rowAction = async (page, subject, label) => {
  const r = row(page, subject).first();
  await r.hover();
  await r.locator('.ml-row-actions').getByRole('button', { name: label, exact: true }).click();
};
const moreAction = async (page, subject, label) => {
  await rowAction(page, subject, 'More actions');
  await menuItem(page, label).click();
};
const folderMenu = async (page, name, label) => {
  await folderNamed(page, name).click({ button: 'right' });
  await menuItem(page, label).click();
};
const prompt = async (page, value) => {
  const input = modal(page).locator('input').first();
  await input.fill(value);
  await modal(page).locator('.modal-foot .btn.primary').click();
  await modal(page).waitFor({ state: 'detached' });
};
const confirm = async (page) => {
  await modal(page).locator('.modal-foot .btn.danger, .modal-foot .btn.primary').click();
  await modal(page).waitFor({ state: 'detached' });
};
const subjects = (page) => page.locator('.ml-list-body .ml-row .ml-subject').allInnerTexts();

try {
  log('empty the test mailboxes and deliver seed mail');
  execFileSync('bash', [join(here, 'mail-server.sh'), 'reset'], { stdio: 'inherit', env: { ...process.env, MAIL_TEST_DIR: mailDir } });
  const now = Date.now();
  const seed = [
    ['Bob Builder <bob@example.test>', 'Quarterly report', 'report@e2e.test', { date: new Date(now - 1 * day), headers: 'X-Seed: 1\n', body: 'The numbers, attached.' }],
    ['Dana Doe <dana@example.org>', 'Team offsite', 'offsite@e2e.test', { date: new Date(now - 2 * day) }],
    ['Dana Doe <dana@example.org>', 'Re: Team offsite', 'offsite-re@e2e.test', { date: new Date(now - 1.5 * day), headers: 'In-Reply-To: <offsite@e2e.test>\nReferences: <offsite@e2e.test>\n' }],
    ['Shop <deals@shop.example>', 'Big sale this week', 'sale@e2e.test', { date: new Date(now - 3 * day) }],
    ['Erin <erin@example.org>', 'Old receipt', 'receipt@e2e.test', { date: new Date(now - 300 * day) }],
    ['Frank <frank@example.org>', 'Snooze me', 'snooze@e2e.test', { date: new Date(now - 4 * day) }],
    ['Gina <gina@example.org>', 'Quick action target', 'qa@e2e.test', { date: new Date(now - 5 * day) }],
    ['Hal <hal@example.org>', 'Move me', 'move@e2e.test', { date: new Date(now - 6 * day) }],
    ['Ivy <ivy@example.org>', 'Spammy offer', 'spam@e2e.test', { date: new Date(now - 7 * day) }],
  ];
  for (const [from, subject, id, opts] of seed) await smtpSend(from.match(/<(.*)>/)[1], ALICE.email, msg(from, `Alice <${ALICE.email}>`, subject, id, opts));
  await smtpSend('zed@example.org', CAROL.email, msg('Zed <zed@example.org>', `Carol <${CAROL.email}>`, 'Hello Carol', 'carol1@e2e.test', { date: new Date(now - 0.5 * day) }));
  // Carol shares her inbox with Alice.
  await waitFor('carol\'s inbox', async () => (await search(CAROL, 'INBOX', 'ALL')).length === 1);
  execFileSync('doveadm', ['-c', join(mailDir, 'dovecot.conf'), 'acl', 'set', '-u', CAROL.email, 'INBOX', `user=${ALICE.email}`, 'lookup', 'read', 'write-seen']);
  await waitFor('seed mail', async () => (await search(ALICE, 'INBOX', 'ALL')).length === seed.length);

  const page = await newPage('desktop');
  current = page;
  await page.api('POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  await page.api('PATCH', '/api/admin/mail/settings', { allow_local_servers: true });
  const smtp = { smtp_host: SMTP.host, smtp_port: SMTP.port, smtp_security: 'none', imap_host: IMAP.host, imap_port: IMAP.port, imap_security: 'none' };
  const { account: alice } = await page.api('POST', '/api/mail/accounts', { email: ALICE.email, name: 'Alice Archer', password: ALICE.password, ...smtp });
  const { account: carol } = await page.api('POST', '/api/mail/accounts', { email: CAROL.email, name: 'Carol', password: CAROL.password, ...smtp });

  log('two accounts: All inboxes');
  await page.goto(`${base}/mail`);
  await row(page, 'Quarterly report').waitFor({ timeout: 20000 });
  await viewBtn(page, 'all').click();
  await row(page, 'Hello Carol').waitFor({ timeout: 15000 });
  await row(page, 'Quarterly report').waitFor();
  assert(await page.locator('.ml-list-head h2', { hasText: 'All inboxes' }).count() === 1, 'All inboxes title');
  assert((await row(page, 'Hello Carol').locator('.ml-where').innerText()) === CAROL.email, 'unified rows name their account');
  await page.screenshot({ path: `${shots}/inbox-all.png` });

  log('shared folder from another user, with a marker');
  await page.locator(`.ml-account-name[title="${ALICE.email}"]`).waitFor();
  await page.getByRole('button', { name: `Account menu for ${ALICE.email}` }).click();
  await menuItem(page, 'Refresh').click();
  const shared = page.locator(`.ml-folder[data-name="shared/${CAROL.email}"]`);
  await shared.waitFor({ timeout: 20000 });
  await shared.locator('.ml-shared').waitFor();
  await shared.click();
  await row(page, 'Hello Carol').waitFor({ timeout: 15000 });

  log('one-click actions: important, favourite; priority inbox and Favourites');
  await folderBtn(page, 'inbox').first().click();
  await row(page, 'Quarterly report').waitFor();
  await rowAction(page, 'Quarterly report', 'Mark important');
  await row(page, 'Quarterly report').locator('.ml-important').waitFor();
  await waitFor('$label1 on the server', async () => (await search(ALICE, 'INBOX', 'KEYWORD $label1')).length === 1);
  await rowAction(page, 'Team offsite', 'Flag');
  await waitFor('\\Flagged on the server', async () => (await search(ALICE, 'INBOX', 'FLAGGED')).length === 1);
  // Favourites in their own section at the top.
  await page.locator('.ml-section', { hasText: 'Favourites' }).waitFor();
  assert((await subjects(page))[0] === 'Team offsite', 'the favourite comes first');
  await viewBtn(page, 'priority').click();
  await page.locator('.ml-section', { hasText: 'Important' }).waitFor();
  const firstImportant = page.locator('.ml-section', { hasText: 'Important' }).locator('xpath=following-sibling::div[1]');
  assert((await firstImportant.locator('.ml-subject').innerText()) === 'Quarterly report', 'Important section');
  await page.locator('.ml-section', { hasText: 'Others' }).waitFor();
  await page.screenshot({ path: `${shots}/inbox-priority.png` });
  await viewBtn(page, 'starred').click();
  await row(page, 'Team offsite').waitFor();
  assert(await page.locator('.ml-list-body .ml-row').count() === 1, 'only the favourite');

  log('tags: apply from a row, create and recolour, tag view');
  await folderBtn(page, 'inbox').first().click();
  await row(page, 'Big sale this week').waitFor();
  await moreAction(page, 'Big sale this week', 'Tags…');
  await modal(page).locator('label.check', { hasText: 'Work' }).locator('input').check();
  await waitFor('$label2 on the server', async () => (await search(ALICE, 'INBOX', 'KEYWORD $label2')).length === 1);
  await modal(page).getByRole('button', { name: 'New tag' }).click();
  await page.locator('#ml-tag-name').fill('Project X');
  await page.locator('.modal').last().locator('.modal-foot .btn.primary').click();
  await page.locator('.modal').last().locator('label.check', { hasText: 'Project X' }).locator('input').check();
  await waitFor('$project_x on the server', async () => (await search(ALICE, 'INBOX', 'KEYWORD $project_x')).length === 1);
  await page.getByRole('button', { name: 'Done' }).click();
  await row(page, 'Big sale this week').locator('.ml-tag', { hasText: 'Project X' }).waitFor();
  await row(page, 'Big sale this week').locator('.ml-tag', { hasText: 'Work' }).waitFor();
  await viewBtn(page, 'tag:$project_x').click();
  await row(page, 'Big sale this week').waitFor();
  assert(await page.locator('.ml-list-body .ml-row').count() === 1, 'tag view');
  // Recolour and rename in the settings' tag manager.
  await page.getByRole('button', { name: 'Mail settings' }).click();
  await page.getByRole('button', { name: 'Manage tags…' }).click();
  const tagName = page.locator('.ml-tag-edit input[aria-label="Name of Project X"]');
  await tagName.fill('Project Xenon');
  await tagName.press('Tab');
  await page.locator('.ml-tag-edit input[aria-label="Name of Project Xenon"]').waitFor();
  assert(await page.locator('.ml-tag-edit', { hasText: '$label1' }).getByRole('button', { name: /Delete/ }).count() === 0, 'default tags are not deletable');
  await page.locator('.modal').last().getByRole('button', { name: 'Done' }).click();
  await page.locator('.modal').getByRole('button', { name: 'Cancel' }).click();
  await viewBtn(page, 'tag:$project_x').locator('span.grow', { hasText: 'Project Xenon' }).waitFor();

  log('folders: create, subfolder, rename, move, statistics, mark read, unsubscribe, empty, delete');
  await page.getByRole('button', { name: `Account menu for ${ALICE.email}` }).click();
  await menuItem(page, 'New folder…').click();
  await prompt(page, 'Projects');
  await folderNamed(page, 'Projects').waitFor();
  await folderMenu(page, 'Projects', 'New subfolder…');
  await prompt(page, 'Entwürfe');
  await folderNamed(page, 'Entwürfe').waitFor();
  assert((await folderNames(ALICE)).includes('Projects/Entw&APw-rfe'), 'subfolder on the server (modified UTF-7)');
  await folderMenu(page, 'Entwürfe', 'Rename…');
  await prompt(page, 'Drafts 2026');
  await folderNamed(page, 'Drafts 2026').waitFor();
  assert((await folderNames(ALICE)).includes('Projects/Drafts 2026'), 'renamed on the server');
  await folderMenu(page, 'Drafts 2026', 'Move to…');
  await modal(page).locator('.ml-picker-item', { hasText: '(top level)' }).click();
  await waitFor('moved to the top', async () => (await folderNames(ALICE)).includes('Drafts 2026'));
  // Drag and drop a message onto a folder.
  await folderBtn(page, 'inbox').first().click();
  await row(page, 'Move me').waitFor();
  await row(page, 'Move me').dragTo(folderNamed(page, 'Drafts 2026'));
  await page.locator('.toast', { hasText: 'Moved 1 message' }).waitFor({ timeout: 15000 });
  await waitFor('the dropped message on the server', async () => (await search(ALICE, 'Drafts 2026', 'ALL')).length === 1);
  await folderMenu(page, 'Drafts 2026', 'Statistics');
  await modal(page).locator('.ml-stat', { hasText: 'Messages' }).locator('strong', { hasText: '1' }).waitFor();
  await modal(page).getByRole('button', { name: 'Close', exact: true }).last().click();
  await folderMenu(page, 'Drafts 2026', 'Mark all as read');
  await waitFor('read on the server', async () => (await search(ALICE, 'Drafts 2026', 'UNSEEN')).length === 0);
  await folderMenu(page, 'Drafts 2026', 'Unsubscribe');
  await folderNamed(page, 'Drafts 2026').waitFor({ state: 'detached' });
  await page.getByRole('button', { name: 'Mail settings' }).click();
  await page.locator('#ml-pref-show_unsubscribed').check();
  await modal(page).getByRole('button', { name: 'Save' }).click();
  await page.locator('.ml-folder-row.unsubscribed', { has: folderNamed(page, 'Drafts 2026') }).waitFor();
  await folderMenu(page, 'Drafts 2026', 'Subscribe');
  await page.locator('.ml-folder-row:not(.unsubscribed)', { has: folderNamed(page, 'Drafts 2026') }).waitFor();
  await folderMenu(page, 'Drafts 2026', 'Empty folder…');
  await confirm(page);
  await waitFor('emptied on the server', async () => (await search(ALICE, 'Drafts 2026', 'ALL')).length === 0);
  await folderMenu(page, 'Drafts 2026', 'Delete folder…');
  await confirm(page);
  await folderNamed(page, 'Drafts 2026').waitFor({ state: 'detached' });
  assert(!(await folderNames(ALICE)).includes('Drafts 2026'), 'deleted on the server');

  log('move dialog from the selection');
  await folderBtn(page, 'inbox').first().click();
  await row(page, 'Old receipt').locator('.ml-check input').check();
  await page.locator('.ml-selection').getByRole('button', { name: 'Move to…' }).click();
  await modal(page).locator('input[type=search]').fill('proj');
  await modal(page).locator('.ml-picker-item', { hasText: 'Projects' }).click();
  await page.locator('.toast', { hasText: 'Moved 1 message' }).waitFor({ timeout: 15000 });
  await waitFor('moved on the server', async () => (await search(ALICE, 'Projects', 'ALL')).length === 1);

  log('snooze and unsnooze');
  await rowAction(page, 'Snooze me', 'Snooze…');
  await modal(page).locator('.ml-snooze-preset', { hasText: 'Tomorrow' }).click();
  await page.locator('.toast', { hasText: 'Snoozed until' }).waitFor({ timeout: 15000 });
  await row(page, 'Snooze me').waitFor({ state: 'detached' });
  await waitFor('in Snoozed on the server', async () => (await search(ALICE, 'Snoozed', 'ALL')).length === 1);
  await folderBtn(page, 'snoozed').waitFor({ timeout: 15000 });
  await folderBtn(page, 'snoozed').click();
  await row(page, 'Snooze me').locator('.ml-until').waitFor({ timeout: 15000 });
  await page.screenshot({ path: `${shots}/inbox-snoozed.png` });
  await rowAction(page, 'Snooze me', 'Unsnooze');
  await page.locator('.toast', { hasText: 'Back where it was' }).waitFor({ timeout: 15000 });
  await waitFor('back in the inbox on the server', async () => (await search(ALICE, 'INBOX', 'HEADER Message-ID "<snooze@e2e.test>"')).length === 1);

  log('quick actions: define one, run it on a message');
  await page.getByRole('button', { name: `Account menu for ${ALICE.email}` }).click();
  await menuItem(page, 'Quick actions…').click();
  await modal(page).getByRole('button', { name: 'New quick action' }).click();
  const editor = page.locator('.modal').last();
  await editor.locator('#ml-qa-name').fill('File as to do');
  const stepSel = editor.getByRole('combobox', { name: 'Step to add' });
  await stepSel.selectOption('read');
  await editor.getByRole('button', { name: 'Add step' }).click();
  await stepSel.selectOption('tag');
  await editor.getByRole('combobox', { name: 'Tag' }).selectOption('$label4');
  await editor.getByRole('button', { name: 'Add step' }).click();
  await stepSel.selectOption('move');
  await editor.getByRole('combobox', { name: 'Folder' }).selectOption({ label: 'Projects' });
  await editor.getByRole('button', { name: 'Add step' }).click();
  assert(await editor.locator('.ml-qa-step').count() === 3, 'three steps');
  await editor.getByRole('button', { name: 'Save' }).click();
  await modal(page).locator('.ml-qa-item', { hasText: 'File as to do' }).waitFor();
  await modal(page).getByRole('button', { name: 'Done' }).click();
  await folderBtn(page, 'inbox').first().click();
  await moreAction(page, 'Quick action target', 'File as to do');
  await page.locator('.toast', { hasText: '“File as to do” done' }).waitFor({ timeout: 15000 });
  await waitFor('all steps on the server', async () => (await search(ALICE, 'Projects', 'SEEN KEYWORD $label4 HEADER Message-ID "<qa@e2e.test>"')).length === 1);

  log('report spam: moved to the Junk folder');
  await moreAction(page, 'Spammy offer', 'Report spam');
  await page.locator('.toast', { hasText: 'Reported as spam' }).waitFor({ timeout: 15000 });
  await waitFor('in Junk with $Junk', async () => (await search(ALICE, 'Junk', 'KEYWORD $Junk')).length === 1);

  log('inbox settings: trash retention and default folders');
  await page.getByRole('button', { name: `Account menu for ${ALICE.email}` }).click();
  await menuItem(page, 'Inbox settings…').click();
  await modal(page).locator('.ml-classifier-status').waitFor();
  await modal(page).locator('label.check', { hasText: 'Delete messages in Trash after' }).locator('input').check();
  await page.locator('#ml-retention').fill('14');
  await page.locator('#ml-role-archive').selectOption({ label: 'Projects' });
  await modal(page).getByRole('button', { name: 'Save' }).click();
  await page.locator('.toast', { hasText: 'Inbox settings saved' }).waitFor();
  const st = await page.api('GET', `/api/mail/accounts/${alice.id}/inbox-settings`);
  assert(st.settings.trash_retention_days === 14, 'retention saved');
  await page.locator('.ml-folder[data-role="archive"]', { hasText: 'Projects' }).waitFor({ timeout: 15000 });

  log('preferences: stacked, list-only, compact, conversations, oldest first (kept on the server)');
  const setPrefs = async (fn) => {
    await page.getByRole('button', { name: 'Mail settings' }).click();
    await fn(modal(page));
    await modal(page).getByRole('button', { name: 'Save' }).click();
    await modal(page).waitFor({ state: 'detached' });
  };
  await setPrefs(async (m) => {
    await m.locator('.ml-layout-opt', { hasText: 'Stacked' }).first().click();
    await m.locator('#ml-pref-compact').check();
    await m.locator('#ml-pref-threaded').check();
  });
  await page.locator('.ml.layout-horizontal.compact').waitFor();
  await waitFor('one row per conversation', async () => await row(page, 'Team offsite').count() + await row(page, 'Re: Team offsite').count() === 1);
  assert(!(await page.locator('.ml-row .avatar').first().isVisible()), 'compact rows have no avatars');
  await row(page, 'Quarterly report').click();
  await page.locator('.ml-thread-head h2', { hasText: 'Quarterly report' }).waitFor();
  const listBox = await page.locator('.ml-list').boundingBox();
  const readerBox = await page.locator('.ml-reader').boundingBox();
  assert(readerBox.y >= listBox.y + listBox.height - 2, 'the reader is below the list');
  await page.screenshot({ path: `${shots}/inbox-stacked-compact.png` });
  const prefs = (await page.api('GET', '/api/mail/preferences')).prefs;
  assert(prefs.layout === 'horizontal' && prefs.compact && prefs.threaded, 'stored on the server');
  await setPrefs(async (m) => {
    await m.locator('.ml-layout-opt', { hasText: 'List only' }).first().click();
    await m.locator('#ml-pref-sort').selectOption('oldest');
  });
  await page.locator('.ml.layout-list').waitFor();
  // The open message stays open; back to the list.
  await page.getByRole('button', { name: 'Back to the list' }).click();
  await waitFor('oldest first', async () => {
    const order = await subjects(page);
    return order.includes('Big sale this week') && order.indexOf('Big sale this week') < order.indexOf('Quarterly report');
  });
  await row(page, 'Big sale this week').click();
  await page.locator('.ml-thread-head h2', { hasText: 'Big sale this week' }).waitFor();
  assert(!(await page.locator('.ml-list').isVisible()), 'list only: the message replaces the list');
  await page.getByRole('button', { name: 'Back to the list' }).click();
  await row(page, 'Big sale this week').waitFor();
  await setPrefs(async (m) => {
    await m.locator('.ml-layout-opt', { hasText: 'Side by side' }).first().click();
    await m.locator('#ml-pref-compact').uncheck();
    await m.locator('#ml-pref-threaded').uncheck();
    await m.locator('#ml-pref-sort').selectOption('newest');
    // Administrators also set everyone's defaults.
    await m.locator('details.ml-admin-defaults summary').click();
    await m.locator('#ml-def-body_search').uncheck();
  });
  await page.locator('.ml.layout-vertical:not(.compact)').waitFor();
  const defaults = (await page.api('GET', '/api/mail/admin/defaults')).defaults;
  assert(defaults.body_search === false, 'admin default saved');
  const after = (await page.api('GET', '/api/mail/preferences')).prefs;
  assert(after.body_search === false, 'the user follows the default they did not change');

  log('advanced search');
  await page.getByRole('button', { name: 'Advanced search' }).click();
  assert(await page.locator('#ml-as-body').isDisabled(), 'body search is off');
  await page.locator('#ml-as-from').fill('dana');
  await modal(page).locator('label.check', { hasText: 'Favourite' }).locator('input').check();
  await modal(page).getByRole('button', { name: 'Search' }).click();
  await page.locator('.ml-filter-bar', { hasText: 'from “dana”' }).waitFor();
  await row(page, 'Team offsite').waitFor({ timeout: 15000 });
  assert(await page.locator('.ml-list-body .ml-row').count() === 1, 'favourite from dana only');
  await page.getByRole('button', { name: 'Clear search' }).click();
  await page.locator('.ml-search input').waitFor();

  log('deep link after the message moved');
  const { messages: inbox } = await page.api('GET', `/api/mail/folders/${(await page.api('GET', `/api/mail/accounts/${alice.id}/folders`)).folders.find((f) => f.role === 'inbox').id}/messages`);
  const report = inbox.find((m) => m.subject === 'Re: Team offsite');
  await page.api('POST', '/api/mail/actions', { ids: [report.id], action: 'archive' });
  await page.goto(`${base}/mail?m=${encodeURIComponent(report.id)}&mid=${encodeURIComponent(report.message_id)}`);
  await page.locator('.ml-thread-head h2', { hasText: 'Re: Team offsite' }).waitFor({ timeout: 15000 });
  await page.locator('.ml-folder.active[data-role="archive"]').waitFor();

  log('mailto: links open the composer');
  await page.getByRole('button', { name: 'Mail settings' }).click();
  await page.getByRole('button', { name: 'Open mailto: links in Ferrite' }).click();
  await page.locator('.toast', { hasText: /mailto: links|didn't allow/ }).waitFor();
  await modal(page).getByRole('button', { name: 'Cancel' }).click();
  await page.goto(`${base}/mail?mailto=${encodeURIComponent('mailto:bob@example.test?cc=carol@example.test&subject=Hello%20Bob&body=Line%20one')}`);
  await page.locator('.modal .ml-compose').waitFor({ timeout: 15000 });
  assert((await page.locator('.modal .ml-chip').first().innerText()).includes('bob@example.test'), 'mailto recipient');
  assert(await page.locator('.ml-subject-input').inputValue() === 'Hello Bob', 'mailto subject');
  assert((await page.locator('.ml-body-input').inputValue()).startsWith('Line one'), 'mailto body');
  await page.getByRole('button', { name: 'Discard' }).click();

  log('Home: unread and important mail');
  await page.goto(`${base}/`);
  await page.locator('h3', { hasText: 'Unread mail' }).waitFor({ timeout: 15000 });
  await page.locator('h3', { hasText: 'Important mail' }).waitFor();
  const card = page.locator('.card', { has: page.locator('h3', { hasText: 'Important mail' }) });
  await card.locator('a.list-item', { hasText: 'Quarterly report' }).click();
  await page.waitForURL(/\/mail\?m=/);
  await page.locator('.ml-thread-head h2', { hasText: 'Quarterly report' }).waitFor({ timeout: 15000 });
  await page.screenshot({ path: `${shots}/inbox-light.png` });
  await page.goto(`${base}/`);
  await page.locator('h3', { hasText: 'Important mail' }).waitFor({ timeout: 15000 });
  await page.screenshot({ path: `${shots}/inbox-home.png` });

  log('dark theme');
  const dark = await newPage('dark', { width: 1440, height: 900 }, 'dark');
  await dark.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await dark.goto(`${base}/mail?view=priority`);
  await dark.locator('.ml-section', { hasText: 'Others' }).waitFor({ timeout: 15000 });
  await row(dark, 'Big sale this week').hover();
  await dark.screenshot({ path: `${shots}/inbox-dark.png` });
  await row(dark, 'Big sale this week').click();
  await dark.locator('.ml-thread-tags .ml-tag').first().waitFor();
  await rowAction(dark, 'Big sale this week', 'More actions');
  await dark.waitForTimeout(200); // the menu fades in
  await dark.screenshot({ path: `${shots}/inbox-dark-menu.png` });
  await dark.keyboard.press('Escape');
  await dark.context().close();

  log('phone (390 px)');
  // A touch phone: no hover, so row actions sit behind a ⋯ button.
  const phone = await newPage('phone', { width: 390, height: 844 }, 'light', { isMobile: true, hasTouch: true });
  await phone.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await phone.goto(`${base}/mail`);
  await row(phone, 'Big sale this week').waitFor({ timeout: 15000 });
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll');
  await phone.screenshot({ path: `${shots}/inbox-phone-list.png` });
  await row(phone, 'Big sale this week').locator('.ml-row-actions').getByRole('button', { name: 'More actions' }).click();
  await menuItem(phone, 'Tags…').waitFor();
  await phone.waitForTimeout(200);
  await phone.screenshot({ path: `${shots}/inbox-phone-menu.png` });
  await phone.keyboard.press('Escape');
  await phone.locator('.ml-list-head h2').click();
  await phone.locator('.menu').waitFor({ state: 'detached' });
  await phone.getByRole('button', { name: 'Folders' }).click();
  await viewBtn(phone, 'priority').waitFor({ state: 'visible' });
  await phone.waitForTimeout(400);
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll (folders)');
  await phone.screenshot({ path: `${shots}/inbox-phone-folders.png` });
  await viewBtn(phone, 'priority').click();
  await phone.locator('.ml-section', { hasText: 'Others' }).waitFor({ timeout: 15000 });
  await phone.getByRole('button', { name: 'Advanced search' }).click();
  await phone.locator('#ml-as-from').waitFor();
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll (dialog)');
  await phone.waitForTimeout(300); // the dialog fades in
  await phone.screenshot({ path: `${shots}/inbox-phone-search.png` });
  await phone.context().close();

  // Unused accounts are fine; make sure the second one is still healthy.
  const accounts = (await page.api('GET', '/api/mail/accounts')).accounts;
  assert(accounts.some((a) => a.id === carol.id && !a.last_error), 'second account healthy');

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
