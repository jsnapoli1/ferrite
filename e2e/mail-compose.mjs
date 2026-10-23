// Browser test of composing and sending mail against a REAL IMAP server (Dovecot, see
// mail-server.sh; no Postfix needed) and a Ferrite server with a fresh data dir. Outgoing
// mail goes to an SMTP relay run by this script (smtp-relay.mjs), which keeps every message
// and delivers copies into the test mailboxes; each sent message is also checked with
// Python's email package, independently of Ferrite's own parser.
//
//   MAIL_TEST_DIR=/tmp/ferrite-mail-compose IMAP_PORT=3321 LMTP_PORT=3322 AUTH_PORT=3323 SIEVE_PORT=3324 \
//     POSTFIX=0 EXTRA_USERS=carol bash mail-server.sh start /tmp/ferrite-mail-compose
//   FERRITE_SFU=0 FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8862 FERRITE_PUBLIC_URL=http://127.0.0.1:8862 ../target/debug/ferrite serve &
//   MAIL_TEST_DIR=/tmp/ferrite-mail-compose IMAP_PORT=3321 EXTRA_USERS=carol FERRITE_URL=http://127.0.0.1:8862 node mail-compose.mjs
//
// Covers: Mail settings (rich text default, rich signature below the quote, undo delay,
// an alias with its own signature, a text block shared with another user); the rich editor
// (bold with Ctrl+B, lists, a link with Ctrl+K, an inline image, a text block with "!",
// an @-mention adding a recipient), group recipients expanding to members, the recipient
// info pane, switching From to an alias (signature swapped), a read-receipt request, undo
// send (the message comes back into the composer) and sending; the MIME structure
// (alternative/related, format=flowed, encoded words, cid images); reading it as the
// recipient (HTML with the inline image), the read-receipt question and the RFC 8098 report
// ($MDNSent remembered); replying with HTML quoting, switching to plain text; send later,
// the Outbox (scheduled, edit, send now), a failed send retried from the Outbox;
// minimizing with autosave of drafts and insertIntoComposer; forward as attachment and the
// attached message shown inline; an .eml attached by upload; a large attachment shared as a
// link by choice and over the admin's limit; the print view; light/dark/390px screenshots. Prints
// {ok, errors}; any console error fails the run.
import { chromium } from 'playwright';
import { mkdirSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { startSmtpRelay } from './smtp-relay.mjs';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8862';
const shots = process.env.SHOTS || '/tmp/ferrite-mail-compose-shots';
const DIR = process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-compose';
const IMAP = { host: '127.0.0.1', port: Number(process.env.IMAP_PORT || 3321) };
const here = dirname(fileURLToPath(import.meta.url));
mkdirSync(shots, { recursive: true });
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const work = mkdtempSync(join(tmpdir(), 'mail-compose-'));

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

// Python's email package reads each message independently of Ferrite.
const PY = String.raw`
import email, json, sys
from email import policy
m = email.message_from_bytes(open(sys.argv[1], 'rb').read(), policy=policy.default)
def walk(p):
    kids = list(p.iter_parts()) if p.is_multipart() else []
    if p.get_content_type() == 'message/rfc822':
        kids = [p.get_payload(0)]
    return {'type': p.get_content_type(), 'params': {k: v for k, v in (p.get_params() or [])[1:]}, 'cid': p.get('Content-ID'),
            'disp': p.get_content_disposition(), 'filename': p.get_filename(), 'cte': p.get('Content-Transfer-Encoding'), 'parts': [walk(x) for x in kids]}
texts = {}
for part in m.walk():
    ct = part.get_content_type()
    if ct in ('text/plain', 'text/html'):
        texts.setdefault(ct, []).append(part.get_payload(decode=True).decode(part.get_content_charset() or 'utf-8', 'replace'))
    elif ct == 'message/disposition-notification':
        texts.setdefault(ct, []).append(part.as_string())
print(json.dumps({'subject': str(m['subject']), 'from': str(m['from']), 'to': str(m['to'] or ''), 'cc': str(m['cc'] or ''),
    'dnt': str(m['Disposition-Notification-To']) if m['Disposition-Notification-To'] else None, 'in_reply_to': m['In-Reply-To'],
    'message_id': m['Message-ID'], 'auto': m['Auto-Submitted'], 'raw_subject': m.get('Subject', failobj='').__class__.__name__,
    'tree': walk(m), 'texts': texts, 'defects': [repr(d) for d in m.defects]}))
`;
let pyN = 0;
function pyParse(data) {
  const f = join(work, `m${++pyN}.eml`);
  writeFileSync(f, data);
  return JSON.parse(execFileSync('python3', ['-c', PY, f]).toString());
}
const flat = (t) => [t.type, ...t.parts.flatMap(flat)];
const find = (t, type) => (t.type === type ? t : t.parts.map((p) => find(p, type)).find(Boolean));

const relay = startSmtpRelay({ dir: DIR });
await relay.ready;

const browser = await chromium.launch();
const errors = [];
let exitCode = 0;
let current = null;

async function newPage(name, viewport = { width: 1440, height: 900 }, colorScheme = 'light') {
  const ctx = await browser.newContext({ viewport, colorScheme });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  page.on('response', (r) => {
    if (r.status() >= 400 && !page.expected?.some((x) => r.url().includes(x))) errors.push(`${name}: HTTP ${r.status()} ${r.request().method()} ${r.url()}`);
  });
  page.api = async (method, path, data) => {
    const r = await page.request.fetch(`${base}${path}`, { method, data, headers: H });
    const body = (r.headers()['content-type'] || '').includes('json') ? await r.json() : await r.text();
    if (!r.ok()) throw new Error(`${method} ${path} -> ${r.status()} ${JSON.stringify(body)}`);
    return body;
  };
  page.apiStatus = async (method, path, data) => (await page.request.fetch(`${base}${path}`, { method, data, headers: H })).status();
  return page;
}

const exact = (text) => new RegExp(`^${text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`);
const row = (page, subject) => page.locator('.ml-row', { has: page.locator('.ml-subject', { hasText: exact(subject) }) });
const folderBtn = (page, role) => page.locator(`.ml-folder[data-role="${role}"]`);
const composer = (page) => page.locator('.modal.ml-composer');
const area = (page) => page.locator('.ml-composer .ed-area');
const PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAgAAAAICAIAAABLbSncAAAAGElEQVR4nGP4z8DwHwYYYYIMDEMYgg8BAKm/Bf8V3Y3GAAAAAElFTkSuQmCC', 'base64');

/** A screenshot once animations (the dialog fading in) are over. */
async function shot(page, name) {
  await page.waitForTimeout(350);
  await page.screenshot({ path: `${shots}/${name}.png` });
}

async function placeCaretAtEnd(page) {
  await area(page).evaluate((el) => {
    const first = el.firstElementChild;
    const r = document.createRange();
    r.selectNodeContents(first || el);
    r.collapse(false);
    const sel = getSelection();
    sel.removeAllRanges();
    sel.addRange(r);
    el.focus();
  });
}

/** Delivers a message into a test mailbox as if it arrived from outside. */
function deliver(user, data) {
  const f = join(work, `in-${Date.now()}.eml`);
  writeFileSync(f, data.replace(/\r?\n/g, '\r\n'));
  execFileSync('bash', [join(here, 'mail-server.sh'), 'deliver', user, f, DIR]);
}

async function account(page, email, password) {
  return (await page.api('POST', '/api/mail/accounts', {
    email, name: email === 'alice@example.test' ? 'Alice Archer' : 'Bob Builder', password,
    imap_host: IMAP.host, imap_port: IMAP.port, imap_security: 'none',
    smtp_host: '127.0.0.1', smtp_port: relay.port, smtp_security: 'none', smtp_auth: true, signature: '',
  })).account;
}

try {
  log('empty the test mailboxes, seed mail');
  execFileSync('bash', [join(here, 'mail-server.sh'), 'reset', DIR], { stdio: 'inherit' });
  const date = new Date().toUTCString();
  deliver('alice', `From: Ada Lovelace <ada@example.org>\nTo: Alice <alice@example.test>\nSubject: Engine notes\nMessage-ID: <engine@e2e.test>\nDate: ${date}\nMIME-Version: 1.0\nContent-Type: multipart/related; boundary="R"\n\n--R\nContent-Type: text/html; charset=utf-8\n\n<p>The <b>analytical engine</b> needs a mill.</p><p><img src="cid:dot@e2e" alt="diagram"></p>\n--R\nContent-Type: image/png\nContent-ID: <dot@e2e>\nContent-Disposition: inline; filename="dot.png"\nContent-Transfer-Encoding: base64\n\n${PNG.toString('base64')}\n--R--\n`);

  const page = await newPage('alice');
  current = page;
  await page.api('POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password', email: 'alice@example.test' });
  for (const [u, n] of [['bob', 'Bob Builder'], ['carol', 'Carol Chen']]) {
    await page.api('POST', '/api/admin/users', { username: u, display_name: n, email: `${u}@example.test`, password: `${u}-password` });
  }
  const { group } = await page.api('POST', '/api/admin/groups', { name: 'Team' });
  for (const u of ['bob', 'carol']) await page.api('POST', `/api/admin/groups/${group.id}/members`, { username: u });
  await page.api('POST', '/api/contacts', { name: 'Bob Builder', emails: ['bob@example.test'], phones: ['+49 30 1234567'], org: 'Builder & Sons', note: 'Prefers short mails.' });
  // Servers on 127.0.0.1 need the admin's consent (mail settings of the [accounts] work).
  page.expected = ['/api/admin/mail/settings'];
  await page.apiStatus('PATCH', '/api/admin/mail/settings', { allow_local_servers: true });
  page.expected = [];
  const acc = await account(page, 'alice@example.test', 'alice-mail-pw');

  log('Mail settings: rich text, signature, undo delay, alias, shared text block');
  await page.goto(`${base}/mail`);
  await folderBtn(page, 'inbox').waitFor({ timeout: 20000 });
  await page.getByRole('button', { name: 'Mail settings' }).click();
  await page.getByRole('button', { name: 'Composing, signatures and text blocks…' }).click();
  const settings = page.getByRole('dialog', { name: 'Composing' });
  await settings.waitFor();
  await settings.locator('#ml-undo').selectOption('5');
  await page.locator('.toast', { hasText: 'Saved' }).first().waitFor();
  await settings.locator('input[value="richtext"]').check();
  const sigArea = settings.locator('.ed-area');
  await sigArea.click();
  await page.keyboard.type('Alice ');
  await page.keyboard.press('Control+b');
  await page.keyboard.type('Archer');
  await page.keyboard.press('Control+b');
  await settings.locator('label.check', { hasText: 'above the quoted message' }).locator('input').uncheck();
  await settings.getByRole('button', { name: 'Save', exact: true }).click();
  await page.locator('.toast', { hasText: 'Composer settings saved' }).waitFor();
  const ctxAfter = await page.api('GET', '/api/mail/compose/context');
  const prefs = ctxAfter.accounts[0].prefs;
  assert(prefs.editor_mode === 'richtext' && !prefs.signature_above_quote, 'composer settings stored');
  assert(prefs.signature_html.includes('<b>Archer</b>') && prefs.signature_text === 'Alice *Archer*', `rich signature with its text rendering: ${JSON.stringify(prefs)}`);
  assert(ctxAfter.send.undo_send_seconds === 5, 'undo delay');

  await settings.getByRole('tab', { name: 'Aliases' }).click();
  await settings.getByRole('button', { name: 'Add alias' }).click();
  const aliasDlg = page.locator('.modal', { hasText: 'New alias for alice@example.test' });
  await aliasDlg.locator('#ml-alias-name').fill('Alice Support');
  await aliasDlg.locator('#ml-alias-email').fill('support@example.test');
  await aliasDlg.locator('label.check', { hasText: 'own signature' }).locator('input').check();
  await aliasDlg.locator('.ed-area').click();
  await page.keyboard.type('Support team');
  await aliasDlg.getByRole('button', { name: 'Add alias' }).click();
  await settings.locator('.ml-set-row', { hasText: 'support@example.test' }).waitFor();

  await settings.getByRole('tab', { name: 'Text blocks' }).click();
  await settings.getByRole('button', { name: 'New text block' }).click();
  const blockDlg = page.locator('.modal', { hasText: 'New text block' });
  await blockDlg.locator('#ml-block-title').fill('Greeting');
  await blockDlg.locator('.ed-area').click();
  await page.keyboard.type('Thanks for reaching out, ');
  await page.keyboard.press('Control+i');
  await page.keyboard.type('we are on it');
  await page.keyboard.press('Control+i');
  await blockDlg.getByRole('button', { name: 'Create' }).click();
  const blockRow = settings.locator('.ml-set-row', { hasText: 'Greeting' });
  await blockRow.waitFor();
  await blockRow.getByRole('button', { name: 'Share' }).click();
  const shareDlg = page.locator('.modal', { hasText: 'Share “Greeting”' });
  await shareDlg.locator('input[list]').fill('bob');
  await shareDlg.getByRole('button', { name: 'Share', exact: true }).click();
  await shareDlg.locator('.ml-set-row', { hasText: 'Bob Builder' }).waitFor();
  await shareDlg.getByRole('button', { name: 'Done' }).click();
  await settings.locator('.ml-set-row', { hasText: 'Shared with Bob Builder' }).waitFor();
  await shot(page, 'compose-settings');
  await settings.getByRole('button', { name: 'Close', exact: true }).last().click();
  // Back in Mail settings, which opened the composer settings.
  await page.getByRole('dialog', { name: 'Mail settings' }).getByRole('button', { name: 'Cancel' }).click();
  await page.locator('.modal').waitFor({ state: 'detached' });

  log('compose rich text: group, formatting, link, image, text block, mention, alias, receipt');
  await page.locator('.ml-list-body').focus();
  await page.keyboard.press('c');
  await composer(page).waitFor();
  await area(page).waitFor();
  assert((await area(page).innerText()).includes('Alice Archer'), 'the signature is in a new message');
  await page.keyboard.type('Team');
  await page.locator('.talk-suggest-item', { hasText: 'Group' }).click();
  await composer(page).locator('.ml-chip.group', { hasText: 'Team' }).waitFor();
  await page.locator('.ml-subject-input').fill('Grüße zum Plan');
  await placeCaretAtEnd(page);
  await page.keyboard.type('Hi team, here are the ');
  await page.keyboard.press('Control+b');
  await page.keyboard.type('bold plans');
  await page.keyboard.press('Control+b');
  await page.keyboard.press('Enter');
  await page.getByRole('button', { name: 'Bulleted list (Ctrl+Shift+8)' }).click();
  await page.keyboard.type('first item');
  await page.keyboard.press('Enter');
  await page.keyboard.type('second item');
  await page.keyboard.press('Enter');
  await page.keyboard.press('Enter');
  await page.keyboard.type('Docs: ');
  await page.keyboard.press('Control+k');
  const linkDlg = page.locator('.modal', { hasText: 'Insert link' });
  await linkDlg.locator('input').fill('https://example.org/plans');
  await linkDlg.getByRole('button', { name: 'Insert' }).click();
  await area(page).locator('a[href="https://example.org/plans"]').waitFor();
  await page.keyboard.press('Enter');
  await composer(page).locator('.ed input[type=file]').setInputFiles({ name: 'chart.png', mimeType: 'image/png', buffer: PNG });
  await area(page).locator('img[src^="/api/mail/uploads/"]').waitFor();
  await page.waitForFunction(() => document.querySelector('.ml-composer .ed-area img')?.naturalWidth > 0);
  await placeCaretAtEnd(page);
  await page.keyboard.press('Enter');
  await page.keyboard.type('!gre');
  await page.locator('.ed-picker-item', { hasText: 'Greeting' }).waitFor();
  await page.keyboard.press('Enter');
  await area(page).locator('i', { hasText: 'we are on it' }).waitFor();
  await page.keyboard.type(' Ask @Bob');
  await page.locator('.ed-picker-item', { hasText: 'bob@example.test' }).waitFor();
  await page.keyboard.press('Enter');
  await composer(page).locator('.ml-chip', { hasText: 'Bob Builder' }).waitFor();
  assert((await area(page).innerText()).includes('@Bob Builder'), 'mention text');
  await page.getByRole('button', { name: 'Recipient info' }).click();
  const pane = composer(page).locator('.ml-rcpt-pane');
  await pane.locator('.ml-rcpt', { hasText: 'Bob Builder' }).locator('text=Builder & Sons').waitFor();
  assert(await pane.locator('a[href^="tel:"]').count() === 1, 'phone in the info pane');
  await composer(page).locator('select[aria-label="From"]').selectOption({ label: 'Alice Support <support@example.test>' });
  await area(page).locator('.ferrite-signature', { hasText: 'Support team' }).waitFor();
  assert(!(await area(page).innerText()).includes('Archer'), 'the account signature was swapped for the alias one');
  await composer(page).locator('label.check', { hasText: 'Request read receipt' }).locator('input').check();
  await shot(page, 'compose-rich');

  log('send, undo, send again');
  await composer(page).getByRole('button', { name: 'Send', exact: true }).click();
  const undoToast = page.locator('.toast', { hasText: /Sending in \d+ s/ });
  await undoToast.waitFor();
  await undoToast.getByRole('button', { name: 'Undo' }).click();
  await composer(page).locator('h2', { hasText: 'Queued message' }).waitFor();
  await area(page).locator('b', { hasText: 'bold plans' }).waitFor();
  await area(page).locator('img[src*="/api/mail/outbox/"]').waitFor();
  assert(await composer(page).locator('.ml-chip.group', { hasText: 'Team' }).count() === 1, 'the group came back');
  assert(relay.messages.length === 0, 'nothing was sent before the undo');
  await composer(page).getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Message sent' }).waitFor({ timeout: 30000 });
  const sent = await relay.waitFor((m) => m.data.includes('Disposition-Notification-To'));
  assert(sent.to.includes('bob@example.test') && sent.to.includes('carol@example.test'), `group expanded: ${sent.to}`);
  assert(sent.from === 'support@example.test', `envelope sender is the alias: ${sent.from}`);
  const p1 = pyParse(sent.data);
  assert(!p1.defects.length, `no MIME defects: ${p1.defects}`);
  assert(p1.subject === 'Grüße zum Plan' && /=\?utf-8\?/i.test(sent.data.match(/^Subject:.*$/m)[0]), 'encoded-word subject');
  assert(p1.from === 'Alice Support <support@example.test>', `From: ${p1.from}`);
  assert(p1.dnt && p1.dnt.includes('support@example.test'), 'read receipt requested');
  const kinds = flat(p1.tree);
  assert(JSON.stringify(kinds) === JSON.stringify(['multipart/alternative', 'text/plain', 'multipart/related', 'text/html', 'image/png']), `structure ${kinds}`);
  const textPart = find(p1.tree, 'text/plain');
  assert(textPart.params.format === 'flowed', 'format=flowed');
  const plain = p1.texts['text/plain'][0].replace(/\r\n/g, '\n');
  for (const want of ['*bold plans*', '- first item', '- second item', 'https://example.org/plans', '/we are on it/', '@Bob Builder', '-- \nSupport team']) {
    assert(plain.includes(want), `plain text has ${JSON.stringify(want)}: ${plain}`);
  }
  const img = find(p1.tree, 'image/png');
  const cid = img.cid.replace(/[<>]/g, '');
  const html = p1.texts['text/html'][0];
  assert(html.includes(`cid:${cid}`) && html.includes('<b>bold plans</b>') && html.includes('<ul>') && !html.includes('/api/mail/'), 'HTML uses the cid image');

  log('the recipient reads it: HTML with the inline image, read receipt');
  const bob = await newPage('bob');
  await bob.api('POST', '/api/auth/login', { username: 'bob', password: 'bob-password' });
  await account(bob, 'bob@example.test', 'bob-mail-pw');
  await bob.goto(`${base}/mail`);
  await row(bob, 'Grüße zum Plan').waitFor({ timeout: 20000 });
  await row(bob, 'Grüße zum Plan').click();
  await bob.waitForFunction(() => {
    const img = document.querySelector('.ml-card.expanded iframe')?.contentDocument?.querySelector('img');
    return img && img.complete && img.naturalWidth > 0;
  }, null, { timeout: 15000 });
  const frameHtml = await bob.evaluate(() => document.querySelector('.ml-card.expanded iframe').contentDocument.body.innerHTML);
  assert(frameHtml.includes('<b>bold plans</b>') && frameHtml.includes('/cid/'), 'rendered with the cid image');
  const banner = bob.locator('.ml-mdn', { hasText: 'asked for a read receipt' });
  await banner.waitFor();
  await shot(bob, 'compose-read-receipt');
  await banner.getByRole('button', { name: 'Send receipt' }).click();
  await bob.locator('.ml-mdn', { hasText: 'Read receipt sent' }).waitFor();
  const mdn = await relay.waitFor((m) => m.data.includes('disposition-notification'));
  assert(mdn.to.includes('support@example.test'), 'the receipt goes to the requester');
  const pm = pyParse(mdn.data);
  assert(pm.tree.type === 'multipart/report' && pm.tree.params['report-type'] === 'disposition-notification', `report: ${JSON.stringify(pm.tree)}`);
  assert(JSON.stringify(flat(pm.tree)).includes('message/disposition-notification'), 'disposition part');
  const disp = pm.texts['message/disposition-notification'][0];
  assert(disp.includes('Disposition: manual-action/MDN-sent-manually; displayed') && disp.includes(`Original-Message-ID: ${p1.message_id}`), disp);
  assert(pm.auto === 'auto-replied' && pm.subject === 'Read: Grüße zum Plan', 'receipt headers');
  const bobAcc = (await bob.api('GET', '/api/mail/accounts')).accounts[0];
  const bobInbox = (await bob.api('GET', `/api/mail/accounts/${bobAcc.id}/folders`)).folders.find((f) => f.role === 'inbox');
  const bobMsg = (await bob.api('GET', `/api/mail/folders/${bobInbox.id}/messages`)).messages.find((m) => m.subject === 'Grüße zum Plan');
  assert((await bob.api('GET', `/api/mail/messages/${bobMsg.id}/mdn`)).mdn.handled, '$MDNSent is set on the server');
  await bob.reload();
  await bob.locator('.ml-card.expanded iframe').waitFor();
  await sleep(800);
  assert(await bob.locator('.ml-mdn:not([hidden])').count() === 0, 'not asked again');
  // The text block shared with Bob is his to use, read-only.
  const bobBlocks = (await bob.api('GET', '/api/mail/text-blocks')).blocks;
  assert(bobBlocks.length === 1 && bobBlocks[0].title === 'Greeting' && !bobBlocks[0].editable, 'shared text block, read-only');
  bob.expected = ['/api/mail/text-blocks/'];
  assert(await bob.apiStatus('PATCH', `/api/mail/text-blocks/${bobBlocks[0].id}`, { title: 'Mine now' }) === 404, 'recipients cannot change it');
  await bob.context().close();

  log('reply with HTML quoting, switch to plain text, send later, the Outbox');
  await folderBtn(page, 'inbox').click();
  await row(page, 'Engine notes').click();
  await page.locator('.ml-card.expanded iframe').waitFor();
  await page.locator('.ml-list-body').focus();
  await page.keyboard.press('r');
  await composer(page).locator('h2', { hasText: 'Reply' }).waitFor();
  const quote = area(page).locator('.ferrite-quote blockquote');
  await quote.locator('b', { hasText: 'analytical engine' }).waitFor();
  await page.waitForFunction(() => document.querySelector('.ml-composer .ferrite-quote img')?.naturalWidth > 0);
  const order = await area(page).evaluate((el) => [...el.children].map((c) => c.className || c.localName));
  assert(order.indexOf('ferrite-quote') < order.indexOf('ferrite-signature'), `signature below the quote: ${order}`);
  await shot(page, 'compose-reply-html');
  await composer(page).getByRole('button', { name: 'Plain text' }).click();
  const ta = composer(page).locator('textarea.ml-body-input');
  await ta.waitFor();
  const plainReply = await ta.inputValue();
  assert(plainReply.includes('> The *analytical engine* needs a mill.') && plainReply.includes('wrote:') && /-- \nAlice \*Archer\*\s*$/.test(plainReply), `plain reply: ${plainReply}`);
  await ta.press('Control+Home');
  await ta.type('Sounds right to me.');
  await composer(page).getByRole('button', { name: 'Send later' }).click();
  await page.locator('.menu button', { hasText: 'Tomorrow morning' }).click();
  await page.locator('.toast', { hasText: 'Scheduled for' }).waitFor();
  await folderBtn(page, 'outbox').waitFor();
  await folderBtn(page, 'outbox').click();
  const entry = page.locator('.ml-outbox-item', { hasText: 'Re: Engine notes' });
  await entry.locator('.ml-outbox-status', { hasText: 'Scheduled for' }).waitFor();
  await shot(page, 'compose-outbox');
  await entry.getByRole('button', { name: 'Edit' }).click();
  await composer(page).locator('textarea.ml-body-input').waitFor();
  assert((await composer(page).locator('textarea.ml-body-input').inputValue()).startsWith('Sounds right to me.'), 'edited text kept');
  await page.locator('.ml-subject-input').fill('Re: Engine notes (mill)');
  await composer(page).getByRole('button', { name: 'Close', exact: true }).click();
  await page.locator('.toast', { hasText: 'Still scheduled for' }).waitFor();
  const entry2 = page.locator('.ml-outbox-item', { hasText: 'Re: Engine notes (mill)' });
  await entry2.locator('.ml-outbox-status', { hasText: 'Scheduled for' }).waitFor();
  assert(relay.messages.length === 2, 'the scheduled message has not gone out');
  await entry2.getByRole('button', { name: 'Send now' }).click();
  const reply = await relay.waitFor((m) => m.data.includes('Engine notes (mill)'));
  const pr = pyParse(reply.data);
  assert(pr.tree.type === 'text/plain' && pr.tree.params.format === 'flowed', `plain reply is one flowed part: ${JSON.stringify(pr.tree)}`);
  assert(pr.in_reply_to === '<engine@e2e.test>', 'threaded');
  assert(pr.texts['text/plain'][0].replace(/\r\n/g, '\n').includes('> The *analytical engine* needs a mill.'), 'quoted with >');
  await page.locator('.ml-outbox .empty', { hasText: 'The Outbox is empty' }).waitFor({ timeout: 20000 });

  log('a failed send is retried from the Outbox');
  relay.failNext(1, '451 4.3.0 Mailbox busy, try again');
  await page.keyboard.press('c');
  await composer(page).waitFor();
  await page.keyboard.type('bob@example.test,');
  await page.locator('.ml-subject-input').fill('Retry me');
  await placeCaretAtEnd(page);
  await page.keyboard.type('Second attempt works.');
  await composer(page).getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Sending “Retry me” failed' }).waitFor({ timeout: 30000 });
  await folderBtn(page, 'outbox').click();
  const failed = page.locator('.ml-outbox-item', { hasText: 'Retry me' });
  await failed.locator('.ml-outbox-status.danger', { hasText: 'Mailbox busy' }).waitFor();
  await shot(page, 'compose-outbox-failed');
  await failed.getByRole('button', { name: 'Send now' }).click();
  await relay.waitFor((m) => m.data.includes('Subject: Retry me'));
  await page.locator('.ml-outbox .empty').waitFor({ timeout: 20000 });

  log('minimize, autosave, insertIntoComposer, resume');
  await folderBtn(page, 'inbox').click();
  await page.getByRole('button', { name: 'Compose' }).click();
  await composer(page).waitFor();
  await page.keyboard.type('carol@example.test,');
  await page.locator('.ml-subject-input').fill('Autosaved idea');
  await placeCaretAtEnd(page);
  await page.keyboard.type('An idea worth keeping.');
  await composer(page).locator('.ml-compose-status', { hasText: 'Draft saved' }).waitFor({ timeout: 15000 });
  const draftsId = (await page.api('GET', `/api/mail/accounts/${acc.id}/folders`)).folders.find((f) => f.role === 'drafts').id;
  const drafts = (await page.api('GET', `/api/mail/folders/${draftsId}/messages`)).messages;
  assert(drafts.some((d) => d.subject === 'Autosaved idea'), 'the draft was saved on the IMAP server while typing');
  await composer(page).getByRole('button', { name: 'Minimize' }).click();
  const dock = page.locator('.ml-compose-dock');
  await dock.waitFor();
  assert(await composer(page).count() === 0, 'the composer is out of the way');
  await folderBtn(page, 'sent').click();
  await row(page, 'Re: Engine notes (mill)').waitFor({ timeout: 15000 });
  await shot(page, 'compose-minimized');
  const inserted = await page.evaluate(async () => (await import('/js/views/mail-compose.js')).insertIntoComposer('Smart reply: count me in.'));
  assert(inserted, 'insertIntoComposer found the composer');
  await composer(page).waitFor();
  assert(!(await dock.isVisible()), 'resumed');
  const body = await area(page).innerText();
  assert(body.includes('Smart reply: count me in.') && body.includes('An idea worth keeping.'), `content intact: ${body}`);
  await composer(page).getByRole('button', { name: 'Close', exact: true }).click();
  await page.locator('.toast', { hasText: 'Saved to Drafts' }).waitFor({ timeout: 15000 });

  log('forward as attachment; the attached message shown inline');
  await folderBtn(page, 'inbox').click();
  await row(page, 'Engine notes').click();
  await page.locator('.ml-card.expanded iframe').waitFor();
  await page.locator('.ml-actions').getByRole('button', { name: 'More actions' }).click();
  await page.locator('.menu button', { hasText: 'Forward as attachment' }).click();
  await composer(page).locator('.ml-file-chip.eml', { hasText: 'Engine notes.eml' }).waitFor();
  await page.keyboard.type('alice@example.test,');
  await composer(page).getByRole('button', { name: 'Send', exact: true }).click();
  const fwd = await relay.waitFor((m) => m.data.includes('Subject: Fwd: Engine notes'));
  const pf = pyParse(fwd.data);
  const inner = find(pf.tree, 'message/rfc822');
  assert(inner && inner.disp === 'attachment' && inner.cte !== 'base64', `message/rfc822 attachment: ${JSON.stringify(pf.tree)}`);
  assert(flat(inner).includes('image/png'), `the original travels whole: ${JSON.stringify(pf.tree)}`);
  await row(page, 'Fwd: Engine notes').waitFor({ timeout: 20000 });
  await row(page, 'Fwd: Engine notes').click();
  const att = page.locator('.ml-attachment', { hasText: 'Engine notes.eml' });
  await att.getByRole('button', { name: 'View' }).click();
  const nested = page.locator('.ml-nested');
  await nested.locator('.ml-nested-subject', { hasText: 'Engine notes' }).waitFor();
  await page.waitForFunction(() => {
    const img = document.querySelector('.ml-nested iframe')?.contentDocument?.querySelector('img');
    return img && img.complete && img.naturalWidth > 0;
  }, null, { timeout: 15000 });
  await shot(page, 'compose-nested-eml');

  log('an .eml attached by upload; a large attachment shared as a link');
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Compose' }).click();
  await composer(page).waitFor();
  await composer(page).locator('.ml-compose-tools input[type=file]').setInputFiles({ name: 'old thread.eml', mimeType: 'message/rfc822', buffer: Buffer.from('From: x@example.org\r\nSubject: Old thread\r\n\r\nHello\r\n') });
  await composer(page).locator('.ml-file-chip.eml', { hasText: 'old thread.eml' }).locator('button').waitFor();
  await composer(page).getByRole('button', { name: 'Discard' }).click();

  // By choice: "Share link" uploads to Files and puts a link in the message.
  await page.getByRole('button', { name: 'Compose' }).click();
  await composer(page).waitFor();
  await page.keyboard.type('bob@example.test,');
  await page.locator('.ml-subject-input').fill('Linked file');
  const [chooser] = await Promise.all([
    page.waitForEvent('filechooser'),
    (async () => {
      await composer(page).getByRole('button', { name: 'Share link' }).click();
      await page.locator('.menu button', { hasText: 'Upload a file and share a link' }).click();
    })(),
  ]);
  await chooser.setFiles({ name: 'slides.pdf', mimeType: 'application/pdf', buffer: Buffer.from('%PDF-1.4\n') });
  const choice = area(page).locator('a[href*="/s/"]', { hasText: 'slides.pdf' });
  await choice.waitFor({ timeout: 15000 });
  assert((await page.request.get(await choice.getAttribute('href'))).ok(), 'the link works');
  await composer(page).getByRole('button', { name: 'Send', exact: true }).click();
  const linked = await relay.waitFor((m) => m.data.includes('Subject: Linked file'));
  assert(!linked.data.includes('application/pdf'), 'shared as a link, not attached');

  // Over the admin's limit (mail settings of the [accounts] work): always a link, and the
  // server refuses such attachments.
  page.expected = ['/api/admin/mail/settings'];
  const limitSet = await page.apiStatus('PATCH', '/api/admin/mail/settings', { attachment_size_limit: 2048 });
  page.expected = [];
  if (limitSet === 200) {
    await page.getByRole('button', { name: 'Compose' }).click();
    await composer(page).waitFor();
    await page.keyboard.type('bob@example.test,');
    await page.locator('.ml-subject-input').fill('Big file');
    await composer(page).locator('.ml-compose-tools input[type=file]').setInputFiles({ name: 'report.csv', mimeType: 'text/csv', buffer: Buffer.alloc(6000, 'a,b,c\n') });
    const big = page.locator('.modal', { hasText: 'Large attachment' });
    await big.waitFor();
    await big.getByRole('button', { name: 'Share as link' }).click();
    const link = area(page).locator('a[href*="/s/"]', { hasText: 'report.csv' });
    await link.waitFor({ timeout: 15000 });
    const href = await link.getAttribute('href');
    assert((await page.request.get(href)).ok(), 'the link works');
    await composer(page).getByRole('button', { name: 'Send', exact: true }).click();
    const bigMail = await relay.waitFor((m) => m.data.includes('Subject: Big file'));
    assert(pyParse(bigMail.data).texts['text/plain'][0].includes(href) && !bigMail.data.includes('report.csv"'), 'sent as a link, not attached');
    const { upload } = await (await page.request.post(`${base}/api/mail/uploads?name=big.bin`, { headers: { ...H, 'Content-Type': 'application/octet-stream' }, data: Buffer.alloc(5000) })).json();
    page.expected = ['/api/mail/outbox'];
    assert(await page.apiStatus('POST', '/api/mail/outbox', { account_id: acc.id, compose: { to: ['bob@example.test'], subject: 'x', text: 'x', attachments: [{ kind: 'upload', id: upload.id }] } }) === 400, 'over the limit is refused');
    page.expected = [];
    await page.api('PATCH', '/api/admin/mail/settings', { attachment_size_limit: null });
  } else {
    log('(no admin mail settings API on this server: the attachment-limit steps are covered by tests/mail_compose.rs)');
  }

  log('print view');
  await row(page, 'Engine notes').click();
  await page.locator('.ml-card.expanded iframe').waitFor();
  await page.locator('.ml-actions').getByRole('button', { name: 'More actions' }).click();
  await page.locator('.menu button', { hasText: 'Print' }).click();
  await page.locator('iframe.ml-print-frame').waitFor({ state: 'attached' });
  const printSrc = await page.locator('iframe.ml-print-frame').getAttribute('src');
  const printPage = await newPage('print', { width: 800, height: 1000 });
  await printPage.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await printPage.emulateMedia({ media: 'print' });
  await printPage.goto(`${base}${printSrc}`);
  const printed = await printPage.locator('body').innerText();
  assert(printed.includes('Engine notes') && printed.includes('Ada Lovelace') && printed.includes('analytical engine'), 'headers and body');
  await shot(printPage, 'compose-print');
  await printPage.context().close();

  log('dark theme');
  const dark = await newPage('dark', { width: 1440, height: 900 }, 'dark');
  await dark.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await dark.goto(`${base}/mail`);
  await folderBtn(dark, 'inbox').waitFor();
  await dark.getByRole('button', { name: 'Compose' }).click();
  await area(dark).waitFor();
  await placeCaretAtEnd(dark);
  await dark.keyboard.type('Dark mode text with ');
  await dark.keyboard.press('Control+b');
  await dark.keyboard.type('bold');
  await shot(dark, 'compose-dark');
  await composer(dark).getByRole('button', { name: 'Discard' }).click();
  await dark.getByRole('button', { name: 'Mail settings' }).click();
  await dark.getByRole('button', { name: 'Composing, signatures and text blocks…' }).click();
  await dark.locator('#ml-undo').waitFor();
  await shot(dark, 'compose-settings-dark');
  await dark.context().close();

  log('phone: full-screen composer and the dock');
  const phone = await newPage('phone', { width: 390, height: 844 });
  await phone.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await phone.goto(`${base}/mail`);
  await row(phone, 'Engine notes').waitFor();
  await phone.locator('.ml-list-body').focus();
  await phone.keyboard.press('c');
  await area(phone).waitFor();
  const box = await composer(phone).boundingBox();
  assert(box.width === 390, 'full width');
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll');
  await shot(phone, 'compose-phone');
  await composer(phone).getByRole('button', { name: 'Minimize' }).click();
  await phone.locator('.ml-compose-dock').waitFor();
  await shot(phone, 'compose-phone-dock');
  await phone.locator('.ml-compose-dock').getByRole('button', { name: 'Resume' }).click();
  await composer(phone).getByRole('button', { name: 'Discard' }).click();
  await phone.context().close();

  if (errors.length) throw new Error(`console errors: ${errors.join(' | ')}`);
  console.log(JSON.stringify({ ok: true, errors, sent: relay.messages.length }));
} catch (e) {
  console.error(e);
  if (current) await shot(current, 'failure').catch(() => {});
  console.log(JSON.stringify({ ok: false, errors }));
  exitCode = 1;
} finally {
  await browser.close();
  await relay.close();
  rmSync(work, { recursive: true, force: true });
  process.exit(exitCode);
}
