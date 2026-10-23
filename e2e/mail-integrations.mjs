// Browser test of the mail integrations against a REAL Dovecot with Sieve (mail-server.sh
// with SIEVE=1 POSTFIX=0) and a Ferrite server with a fresh data dir:
//
//   MAIL_TEST_DIR=/tmp/ferrite-mail-integrations IMAP_PORT=3341 LMTP_PORT=3342 AUTH_PORT=3343 SIEVE_PORT=3344 \
//     POSTFIX=0 SIEVE=1 EXTRA_USERS="carol" bash mail-server.sh start /tmp/ferrite-mail-integrations
//   FERRITE_SFU=0 FERRITE_MAIL_ALLOW_PRIVATE_FETCH=1 FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8864 \
//     FERRITE_PUBLIC_URL=http://127.0.0.1:8864 ../target/debug/ferrite serve &
//   MAIL_TEST_DIR=/tmp/ferrite-mail-integrations FERRITE_URL=http://127.0.0.1:8864 node mail-integrations.mjs
//
// The mailbox is emptied first (mail-server.sh reset). Mail arrives through Dovecot's
// delivery agent (mail-server.sh deliver), so Sieve really filters it and the autoresponder
// really answers (its replies land in $MAIL_TEST_DIR/outgoing). The script runs an SMTP sink
// (the account's outgoing server), a mock OpenAI-compatible model and a mock mailing-list
// server. Covers: turning on Sieve, a visual filter that files delivered mail, a filter made
// from a message, the autoresponder replying, the Sieve script with the server's error, the
// absence in Settings driving the autoresponder, an invitation card with a conflict answered
// by mail, a flight reservation added to the calendar, one-click unsubscribe, adding the
// sender to contacts, events and tasks from a message (with AI suggestions), the AI summary,
// reply suggestions inserted into the composer, translation, follow-up reminders, and dark
// and 390px layouts. Prints {ok, errors}; any console error fails the run.
import { chromium } from 'playwright';
import { mkdirSync, writeFileSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import http from 'node:http';
import net from 'node:net';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8864';
const DIR = process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-integrations';
const IMAP_PORT = Number(process.env.IMAP_PORT || 3341);
const SIEVE_PORT = Number(process.env.SIEVE_PORT || 3344);
const SINK_PORT = Number(process.env.SINK_PORT || 18975);
const LLM_PORT = Number(process.env.LLM_PORT || 18976);
const LIST_PORT = Number(process.env.LIST_PORT || 18977);
const shots = process.env.SHOTS || '/tmp/ferrite-shots/mail-integrations';
mkdirSync(shots, { recursive: true });
const here = dirname(fileURLToPath(import.meta.url));
const fixtures = join(here, '../crates/ferrite-server/tests/fixtures/imip');
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

async function waitFor(fn, what, timeout = 15000) {
  const end = Date.now() + timeout;
  for (;;) {
    if (await fn()) return;
    if (Date.now() > end) throw new Error(`timed out waiting for ${what}`);
    await sleep(200);
  }
}

// ---- mail in and out ----------------------------------------------------------------------
let seq = 0;
/** Delivers a message as arriving mail (Dovecot LDA: Sieve runs). */
function deliver(raw) {
  const file = join(DIR, `e2e-${process.pid}-${++seq}.eml`);
  writeFileSync(file, raw.replace(/\r?\n/g, '\r\n'));
  execFileSync('bash', [join(here, 'mail-server.sh'), 'deliver', 'alice', file, DIR], { stdio: 'inherit' });
  rmSync(file);
}
/** Into a folder without Sieve (dovecot-lda -m). */
function deliverTo(folder, raw) {
  const file = join(DIR, `e2e-${process.pid}-${++seq}.eml`);
  writeFileSync(file, raw.replace(/\r?\n/g, '\r\n'));
  execFileSync('/usr/lib/dovecot/dovecot-lda', ['-c', join(DIR, 'dovecot.conf'), '-d', 'alice@example.test', '-m', folder, '-f', 'alice@example.test', '-p', file]);
  rmSync(file);
}
const outgoing = () => readdirSync(join(DIR, 'outgoing')).filter((f) => f.endsWith('.eml')).map((f) => readFileSync(join(DIR, 'outgoing', f), 'utf8'));
const plain = (from, subject, id, extra, body) => `From: ${from}\nTo: Alice <alice@example.test>\nSubject: ${subject}\nDate: ${new Date().toUTCString()}\nMessage-ID: <${id}>\nMIME-Version: 1.0\n${extra}Content-Type: text/plain; charset=utf-8\n\n${body}\n`;
const fill = (name, vars) => Object.entries(vars).reduce((s, [k, v]) => s.split(`{${k}}`).join(v), readFileSync(join(fixtures, name), 'utf8'));

// The account's SMTP server: a sink that keeps what Ferrite sends.
const sent = [];
const sink = net.createServer((sock) => {
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
        if (line === '.') { inData = false; sent.push(msg); msg = { to: [], data: '' }; sock.write('250 queued\r\n'); } else msg.data += `${line.startsWith('.') ? line.slice(1) : line}\r\n`;
        continue;
      }
      const up = line.toUpperCase();
      if (up.startsWith('EHLO') || up.startsWith('HELO')) sock.write('250-sink\r\n250 8BITMIME\r\n');
      else if (up.startsWith('RCPT TO:')) { msg.to.push(line.slice(8).trim().replace(/[<>]/g, '')); sock.write('250 OK\r\n'); }
      else if (up === 'DATA') { inData = true; sock.write('354 go ahead\r\n'); }
      else if (up === 'QUIT') sock.end('221 bye\r\n');
      else sock.write('250 OK\r\n');
    }
  });
  sock.on('error', () => {});
});
await new Promise((r) => sink.listen(SINK_PORT, '127.0.0.1', r));

// A mock OpenAI-compatible model that answers by what is asked.
const llm = http.createServer((req, res) => {
  let body = '';
  req.on('data', (c) => { body += c; });
  req.on('end', () => {
    const prompt = JSON.parse(body).messages.at(-1).content;
    let answer = '?';
    if (prompt.includes('Summarize the email conversation')) answer = 'The team agreed to release on Friday; Bob writes the release notes.';
    else if (prompt.includes('Suggest three very short replies')) answer = '{"reply1": "Sounds good!", "reply2": "Can we talk tomorrow?", "reply3": "Thanks, ___"}';
    else if (prompt.includes('Translate the e-mail')) answer = '{"subject": "Freigabe am Freitag", "text": "Wir veröffentlichen am Freitag."}';
    else if (prompt.includes('I am scheduling an event')) answer = '{"title": "Release planning", "agenda": "* Scope\\n* Dates", "start": "2027-03-15T14:00:00+01:00", "end": "2027-03-15T15:00:00+01:00"}';
    else if (prompt.includes('expect a reply')) answer = prompt.includes('Could you') ? '{"expectsReply": true}' : '{"expectsReply": false}';
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ choices: [{ finish_reason: 'stop', message: { role: 'assistant', content: answer } }] }));
  });
});
await new Promise((r) => llm.listen(LLM_PORT, '127.0.0.1', r));

// A mailing list's one-click unsubscribe endpoint.
const unsubscribed = [];
const list = http.createServer((req, res) => {
  let body = '';
  req.on('data', (c) => { body += c; });
  req.on('end', () => { unsubscribed.push(`${req.method} ${req.url} ${body}`); res.writeHead(200); res.end('ok'); });
});
await new Promise((r) => list.listen(LIST_PORT, '127.0.0.1', r));

// ---- browser ------------------------------------------------------------------------------
const browser = await chromium.launch();
const errors = [];
// Responses the test provokes on purpose.
const expected = [/PUT \S+\/sieve$/];
let exitCode = 0;
let current = null;

async function newPage(name, viewport = { width: 1440, height: 900 }, colorScheme = 'light') {
  const ctx = await browser.newContext({ viewport, colorScheme });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error' && !/status of 400/.test(m.text())) errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  page.on('response', (r) => {
    const what = `${r.request().method()} ${r.url()}`;
    if (r.status() >= 400 && !(r.status() === 400 && expected.some((x) => x.test(what)))) errors.push(`${name}: HTTP ${r.status()} ${what}`);
  });
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
const modalTitled = (page, title) => page.locator('.modal', { has: page.locator('.modal-head h2', { hasText: title }) });

/** A screenshot once dialogs and toasts have finished fading in. */
async function shot(page, name, opts = {}) {
  await page.waitForTimeout(350);
  await page.screenshot({ path: `${shots}/${name}.png`, ...opts });
}

async function openRow(page, subject) {
  await row(page, subject).first().waitFor({ timeout: 20000 });
  await row(page, subject).first().click();
  await page.locator('.ml-card.expanded .ml-card-body').first().waitFor();
}

async function moreMenu(page, item) {
  await page.locator('.ml-actions').getByRole('button', { name: 'More actions' }).click();
  await page.locator('.menu button', { hasText: item }).click();
}

try {
  log('empty the test mailbox and its Sieve scripts');
  execFileSync('bash', [join(here, 'mail-server.sh'), 'reset', DIR], { stdio: 'inherit' });
  for (const f of readdirSync(join(DIR, 'outgoing'))) rmSync(join(DIR, 'outgoing', f));
  deliver(plain('Bob <bob@example.test>', 'Welcome', 'welcome@e2e.test', '', 'Hello Alice.'));

  const page = await newPage('desktop');
  current = page;
  await page.api('POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  await page.api('PATCH', '/api/me', { email: 'alice@example.test' });
  // The test servers are on 127.0.0.1, which the admin has to allow.
  await page.api('PATCH', '/api/admin/mail/settings', { allow_local_servers: true });
  const { account } = await page.api('POST', '/api/mail/accounts', {
    email: 'alice@example.test', name: 'Alice Archer', password: 'alice-mail-pw', imap_host: '127.0.0.1', imap_port: IMAP_PORT, imap_security: 'none',
    smtp_host: '127.0.0.1', smtp_port: SINK_PORT, smtp_security: 'none', smtp_auth: false,
  });
  await page.goto(`${base}/mail`);
  await row(page, 'Welcome').waitFor({ timeout: 20000 });

  log('turn on Sieve for the account');
  await page.getByRole('button', { name: 'Account menu for alice@example.test' }).click();
  await page.locator('.menu button', { hasText: 'Filters and autoresponder' }).click();
  const dlg = modalTitled(page, 'Filters and autoresponder');
  await dlg.locator('.mf-notice', { hasText: 'Turn on Sieve' }).waitFor();
  await dlg.locator('label.check', { hasText: 'Manage filters and the autoresponder' }).locator('input').check();
  await dlg.getByLabel('Port').fill(String(SIEVE_PORT));
  await dlg.getByLabel('Security').selectOption('none');
  await shot(page, 'filters-settings');
  await dlg.getByRole('button', { name: 'Save settings' }).click();
  await page.locator('.toast', { hasText: 'Settings saved' }).waitFor({ timeout: 15000 });

  log('a visual filter that files newsletters into Junk');
  await dlg.locator('.mf-tabs button', { hasText: 'Filters' }).click();
  await dlg.locator('.empty', { hasText: 'No filters yet' }).waitFor();
  await dlg.getByRole('button', { name: 'New filter' }).click();
  await dlg.getByLabel('Filter name').fill('Newsletters');
  await dlg.getByLabel('Value').fill('news@');
  await dlg.getByLabel('Folder').selectOption('Junk');
  await dlg.getByRole('button', { name: 'Add action' }).click();
  await dlg.getByLabel('Tag').fill('$label4');
  await shot(page, 'filters-editor');
  await dlg.getByRole('button', { name: 'Save filter' }).click();
  await dlg.locator('.mf-item', { hasText: 'Newsletters' }).waitFor();
  assert((await dlg.locator('.mf-item .mf-desc').innerText()).includes('Sender contains “news@”: move to Junk, tag To do'), 'filter summary');
  await shot(page, 'filters-list');
  deliver(plain('Ferrite News <news@example.org>', 'Monthly news', 'news-1@e2e.test', '', 'Read all about it.'));
  const folders = (await page.api('GET', `/api/mail/accounts/${account.id}/folders`)).folders;
  const junk = folders.find((f) => f.role === 'junk');
  await waitFor(async () => {
    await page.api('POST', `/api/mail/accounts/${account.id}/sync`, { folder_id: junk.id });
    return (await page.api('GET', `/api/mail/folders/${junk.id}/messages`)).messages.some((m) => m.subject === 'Monthly news');
  }, 'the newsletter in Junk');

  log('the Sieve script: hand-written part, the server reports errors');
  await dlg.locator('.mf-tabs button', { hasText: 'Sieve script' }).click();
  const script = dlg.locator('textarea.mf-script');
  await script.waitFor();
  assert((await script.inputValue()).includes('### Nextcloud Mail: Filters ### DON\'T EDIT ###'), 'generated section');
  const good = await script.inputValue();
  await script.fill(`${good}\nfileintoo "Junk";\n`);
  await dlg.getByRole('button', { name: 'Save script' }).click();
  await dlg.locator('.mf-error', { hasText: 'line' }).waitFor();
  await shot(page, 'filters-script-error');
  await script.fill(`${good}\n# my own rule\nif header :contains "X-Spam-Flag" "YES" {\n\tfileinto "Junk";\n}\n`);
  await dlg.getByRole('button', { name: 'Save script' }).click();
  await page.locator('.toast', { hasText: 'Script saved' }).waitFor();

  log('the autoresponder answers');
  await dlg.locator('.mf-tabs button', { hasText: 'Autoresponder' }).click();
  await dlg.locator('label.check', { hasText: 'Reply automatically' }).locator('input').check();
  await dlg.getByLabel('First day').fill(new Date(Date.now() - 3600000).toISOString().slice(0, 16));
  await dlg.getByLabel('Message').fill('I am away until Monday.');
  await dlg.getByRole('button', { name: 'Save autoresponder' }).click();
  await page.locator('.toast', { hasText: 'Autoresponder on' }).waitFor();
  deliver(plain('Dana <dana@example.org>', 'Are you there?', 'dana-1@e2e.test', '', 'Ping.'));
  await waitFor(() => outgoing().some((m) => m.includes('I am away until Monday.') && m.includes('Subject: Re: Are you there?')), 'the vacation reply');
  await dlg.locator('label.check', { hasText: 'Reply automatically' }).locator('input').uncheck();
  await dlg.getByRole('button', { name: 'Save autoresponder' }).click();
  await page.locator('.toast', { hasText: 'Autoresponder off' }).waitFor();
  // Filters survived the script edit and the autoresponder.
  await dlg.locator('.mf-tabs button', { hasText: 'Filters' }).click();
  await dlg.locator('.mf-item', { hasText: 'Newsletters' }).waitFor();
  await dlg.getByRole('button', { name: 'Close', exact: true }).last().click();

  log('a filter from a message');
  await openRow(page, 'Welcome');
  await moreMenu(page, 'Create filter from message');
  await dlg.getByLabel('Filter name').waitFor();
  assert((await dlg.getByLabel('Filter name').inputValue()) === 'From Bob', 'prefilled name');
  assert((await dlg.getByLabel('Value').inputValue()) === 'bob@example.test', 'prefilled sender');
  await dlg.getByRole('button', { name: 'Save filter' }).click();
  await dlg.locator('.mf-item', { hasText: 'From Bob' }).waitFor();
  assert((await dlg.locator('.mf-item').count()) === 2, 'two filters');
  await dlg.getByRole('button', { name: 'Close', exact: true }).last().click();

  log('the absence in Settings drives the autoresponder');
  await page.goto(`${base}/settings/profile`);
  const away = page.locator('.set-card', { has: page.locator('h2', { hasText: 'Out of office' }) });
  await away.waitFor();
  const today = new Date();
  const ymd = (d) => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
  await away.getByLabel('First day').fill(ymd(today));
  await away.getByLabel('Last day').fill(ymd(new Date(today.getTime() + 4 * 86400000)));
  await away.getByLabel('Status').fill('On vacation');
  await away.getByLabel('Message').fill('Back next week. Ask Carol for anything urgent.');
  await away.getByRole('button', { name: 'Save absence' }).click();
  await away.locator('.pill', { hasText: 'Away now' }).waitFor();
  await shot(page, 'settings-absence', { fullPage: true });
  await page.api('PATCH', `/api/mail/accounts/${account.id}/integrations`, { ooo_follows_absence: true });
  deliver(plain('Erin <erin@example.org>', 'Quick question', 'erin-1@e2e.test', '', 'Hi?'));
  await waitFor(() => outgoing().some((m) => m.includes('Back next week. Ask Carol') && m.includes('Subject: Re: Quick question')), 'the absence reply');
  assert((await page.api('GET', '/api/users/alice/absence')).absence.status === 'On vacation', 'others see the absence');
  await away.getByRole('button', { name: 'Remove absence' }).click();
  await away.locator('.pill', { hasText: 'Not set' }).waitFor();

  log('an invitation with a conflict, answered by mail');
  await page.api('POST', '/api/calendar/events', { summary: 'Dentist', start: '2027-03-10T09:30:00Z', end: '2027-03-10T10:30:00Z' });
  deliver(fill('thunderbird-request.eml', {
    UID: 'design-review-42@example.org', SUMMARY: 'Design review', SEQ: '0', START: '20270310T100000', END: '20270310T110000',
    DTSTAMP: '20261021T100000Z', MSGID: 'req-1@e2e.test', DAY: 'Wed Mar 10, 2027', DATE: new Date().toUTCString(),
  }));
  await page.goto(`${base}/mail`);
  await openRow(page, 'Invitation: Design review @ Wed Mar 10, 2027');
  const invite = page.locator('.mi-card', { has: page.locator('.mi-title', { hasText: 'Design review' }) });
  await invite.waitFor();
  assert((await invite.locator('.mi-conflicts').innerText()).includes('Dentist'), 'the conflict is shown');
  assert((await invite.locator('.mi-facts').innerText()).includes('Organizer One'), 'organizer');
  await shot(page, 'imip-invitation');
  await invite.getByLabel('Note for the organizer').fill('Looking forward to it');
  await invite.getByRole('button', { name: 'Accept', exact: true }).click();
  await invite.locator('.mi-answer', { hasText: 'Accepted' }).waitFor();
  await waitFor(() => sent.some((m) => m.to.includes('org@example.org') && m.data.includes('method=REPLY')), 'the REPLY by mail');
  await invite.locator('.mi-note', { hasText: 'In your calendar' }).waitFor();
  await shot(page, 'imip-accepted');

  log('a flight booking: the reservation goes into the calendar');
  const flight = `{"@context":"http://schema.org","@type":"FlightReservation","reservationNumber":"RXJ34P","underName":{"@type":"Person","name":"Alice Archer"},"reservationFor":{"@type":"Flight","flightNumber":"110","airline":{"@type":"Airline","name":"United","iataCode":"UA"},"departureAirport":{"@type":"Airport","name":"San Francisco Airport","iataCode":"SFO"},"departureTime":"2027-03-04T20:15:00-08:00","arrivalAirport":{"@type":"Airport","name":"John F. Kennedy International Airport","iataCode":"JFK"},"arrivalTime":"2027-03-05T06:30:00-05:00"},"airplaneSeat":"9A"}`;
  deliver(`From: United <receipts@united.example>\nTo: alice@example.test\nSubject: Your trip confirmation\nDate: ${new Date().toUTCString()}\nMessage-ID: <trip@e2e.test>\nMIME-Version: 1.0\nContent-Type: text/html; charset=utf-8\n\n<html><head><script type="application/ld+json">${flight}</script></head><body><h1>Thanks for booking</h1><p>Confirmation RXJ34P</p></body></html>\n`);
  await openRow(page, 'Your trip confirmation');
  const trip = page.locator('.mi-trip', { hasText: 'Flight UA 110 SFO → JFK' });
  await trip.waitFor();
  await shot(page, 'itinerary');
  await trip.getByRole('button', { name: 'Add to calendar' }).click();
  await trip.locator('.pill', { hasText: 'In your calendar' }).waitFor();
  const events = (await page.api('GET', '/api/calendar/events?from=2027-03-04T00:00:00Z&to=2027-03-06T00:00:00Z')).events;
  assert(events.some((e) => e.summary === 'Flight UA 110 SFO → JFK' && e.starts_at === '2027-03-05T04:15:00Z'), 'flight in the calendar');

  log('one-click unsubscribe');
  deliver(plain('Weekly <weekly@lists.example>', 'Weekly digest', 'digest@e2e.test',
    `List-Id: Weekly digest <weekly.lists.example>\nList-Unsubscribe: <mailto:unsub@lists.example?subject=stop>, <http://127.0.0.1:${LIST_PORT}/unsub/42>\nList-Unsubscribe-Post: List-Unsubscribe=One-Click\n`, 'This week…'));
  await openRow(page, 'Weekly digest');
  const unsub = page.locator('.mx-unsub');
  await unsub.waitFor();
  assert((await unsub.innerText()).includes('Weekly digest'), 'list name');
  await unsub.getByRole('button', { name: 'Unsubscribe' }).click();
  await page.locator('.mx-unsub.done', { hasText: 'Unsubscribed' }).waitFor();
  assert(unsubscribed[0] === 'POST /unsub/42 List-Unsubscribe=One-Click', `one-click POST: ${unsubscribed}`);

  log('the sender into contacts');
  deliver(plain('Carol Chen <carol@example.test>', 'Coffee?', 'coffee@e2e.test', '', 'Coffee on Thursday at 10?'));
  await openRow(page, 'Coffee?');
  const senderLine = page.locator('.ml-card.expanded .mx-sender');
  await senderLine.locator('text=Not in your contacts').waitFor();
  await senderLine.getByRole('button', { name: 'Add' }).click();
  const addDlg = modalTitled(page, 'Add carol@example.test to contacts');
  assert((await addDlg.getByLabel('Name').inputValue()) === 'Carol Chen', 'name prefilled');
  await addDlg.getByRole('button', { name: 'Add', exact: true }).click();
  await senderLine.locator('text=In your contacts as').waitFor();
  const info = (await page.api('GET', '/api/mail/contact-info?email=carol@example.test')).info;
  assert(info.contact?.name === 'Carol Chen' && info.received === 1, 'contact info');

  log('AI for mail');
  const { id: providerId } = await page.api('POST', '/api/ai/providers', { name: 'Local', kind: 'openai', base_url: `http://127.0.0.1:${LLM_PORT}/v1` });
  await page.api('PUT', '/api/ai/roles', { role: 'chat', provider_id: providerId, model: 'mock' });
  await page.api('PATCH', '/api/me', { ai_consent: true });
  await page.api('PATCH', '/api/admin/mail/settings', { llm_processing: true });
  deliver(plain('Bob <bob@example.test>', 'Release', 'rel-0@e2e.test', '', 'Shall we release on Friday?'));
  deliver(plain('Carol Chen <carol@example.test>', 'Re: Release', 'rel-1@e2e.test', 'In-Reply-To: <rel-0@e2e.test>\nReferences: <rel-0@e2e.test>\n', 'Yes, Friday works.'));
  deliver(plain('Bob <bob@example.test>', 'Re: Release', 'rel-2@e2e.test', 'In-Reply-To: <rel-1@e2e.test>\nReferences: <rel-0@e2e.test> <rel-1@e2e.test>\n', 'I will write the release notes.'));
  // A question Alice sent five days ago, still unanswered.
  const fiveDays = new Date(Date.now() - 5 * 86400000).toUTCString();
  deliverTo('Sent', `From: Alice Archer <alice@example.test>\nTo: Dan <dan@example.org>\nSubject: Budget numbers\nDate: ${fiveDays}\nMessage-ID: <budget@e2e.test>\nContent-Type: text/plain\n\nCould you send me the budget by Monday?\n`);
  await page.reload();
  await openRow(page, 'Re: Release');
  await page.locator('.ma-summary p', { hasText: 'release on Friday' }).waitFor({ timeout: 20000 });
  const chips = page.locator('.ma-chips .ma-chip');
  await chips.first().waitFor({ timeout: 20000 });
  assert((await chips.count()) === 3, 'three suggestions');
  await shot(page, 'ai-thread');
  await chips.filter({ hasText: 'Sounds good!' }).click();
  await page.locator('.modal .ml-compose').waitFor();
  await waitFor(async () => (await page.locator('.ml-body-input').inputValue()).startsWith('Sounds good!\n'), 'the suggestion in the composer');
  const replyText = await page.locator('.ml-body-input').inputValue();
  assert(/^Sounds good!\n[\s\S]*wrote:\n> /.test(replyText), `the suggestion above the quote: ${JSON.stringify(replyText)}`);
  await page.getByRole('button', { name: 'Discard' }).click();
  await moreMenu(page, 'Translate');
  await page.locator('.ma-translation .ma-tr-body', { hasText: 'Wir veröffentlichen am Freitag.' }).waitFor({ timeout: 20000 });
  await shot(page, 'ai-translation');

  log('an event and a task from a message (AI suggestions)');
  await moreMenu(page, 'Create event');
  const ev = modalTitled(page, 'Create event');
  await ev.locator('.mx-ai', { hasText: 'Suggested by AI' }).waitFor({ timeout: 20000 });
  assert((await ev.getByLabel('Title').inputValue()) === 'Release planning', 'AI title');
  await shot(page, 'event-from-mail');
  await ev.getByRole('button', { name: 'Create event' }).click();
  await page.locator('.toast', { hasText: 'Event created' }).waitFor();
  const planned = (await page.api('GET', '/api/calendar/events?from=2027-03-15T00:00:00Z&to=2027-03-16T00:00:00Z')).events;
  assert(planned.some((e) => e.summary === 'Release planning' && e.starts_at === '2027-03-15T13:00:00Z'), 'event at the suggested time');
  await moreMenu(page, 'Create task');
  const task = modalTitled(page, 'Create task');
  assert((await task.getByLabel('Title').inputValue()) === 'Release', 'task title from the subject');
  await task.getByRole('button', { name: 'Create task' }).click();
  await page.locator('.toast', { hasText: 'Task created' }).waitFor();

  log('follow-up reminders');
  const sentFolder = (await page.api('GET', `/api/mail/accounts/${account.id}/folders`)).folders.find((f) => f.role === 'sent');
  await page.api('POST', `/api/mail/accounts/${account.id}/sync`, { folder_id: sentFolder.id });
  await page.getByRole('button', { name: 'Account menu for alice@example.test' }).click();
  await page.locator('.menu button', { hasText: 'Filters and autoresponder' }).click();
  await dlg.locator('.mf-tabs button', { hasText: 'Settings' }).click();
  await dlg.getByRole('button', { name: 'Check sent mail now' }).click();
  await page.locator('.toast', { hasText: '1 waiting for an answer' }).waitFor({ timeout: 20000 });
  await dlg.getByRole('button', { name: 'Close', exact: true }).last().click();
  await page.reload();
  const fu = page.locator('.ma-followup-btn');
  await fu.waitFor({ timeout: 15000 });
  assert((await fu.locator('.badge').innerText()) === '1', 'one follow-up');
  await fu.click();
  const fuDlg = modalTitled(page, 'Follow up');
  await fuDlg.locator('.ma-followup', { hasText: 'Budget numbers' }).waitFor();
  await shot(page, 'follow-ups');
  await fuDlg.getByRole('button', { name: 'Follow up', exact: true }).click();
  await page.locator('.modal .ml-compose').waitFor();
  assert((await page.locator('.modal .ml-chip').first().innerText()).includes('Dan'), 'the follow-up goes to Dan');
  await page.getByRole('button', { name: 'Discard' }).click();

  log('dark theme');
  const dark = await newPage('dark', { width: 1440, height: 900 }, 'dark');
  current = dark;
  await dark.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await dark.goto(`${base}/mail`);
  await openRow(dark, 'Invitation: Design review @ Wed Mar 10, 2027');
  await dark.locator('.mi-card .mi-title').waitFor();
  await shot(dark, 'imip-dark');
  await openRow(dark, 'Re: Release');
  await dark.locator('.ma-summary p').waitFor({ timeout: 20000 });
  await dark.locator('.ma-chips .ma-chip').first().waitFor({ timeout: 20000 });
  await shot(dark, 'ai-dark');
  await dark.getByRole('button', { name: 'Account menu for alice@example.test' }).click();
  await dark.locator('.menu button', { hasText: 'Filters and autoresponder' }).click();
  await modalTitled(dark, 'Filters and autoresponder').locator('.mf-item', { hasText: 'Newsletters' }).waitFor();
  await shot(dark, 'filters-dark');
  await dark.context().close();

  log('phone');
  const phone = await newPage('phone', { width: 390, height: 844 });
  current = phone;
  await phone.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  await phone.goto(`${base}/mail`);
  await openRow(phone, 'Invitation: Design review @ Wed Mar 10, 2027');
  await phone.locator('.mi-card .mi-title').waitFor();
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll (invitation)');
  await shot(phone, 'imip-phone');
  await phone.getByRole('button', { name: 'Back to the list' }).click();
  await openRow(phone, 'Your trip confirmation');
  await phone.locator('.mi-trip').waitFor();
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll (reservation)');
  await shot(phone, 'itinerary-phone');
  await phone.getByRole('button', { name: 'Back to the list' }).click();
  await phone.getByRole('button', { name: 'Folders' }).click();
  await phone.waitForTimeout(400);
  await phone.getByRole('button', { name: 'Account menu for alice@example.test' }).click();
  await phone.locator('.menu button', { hasText: 'Filters and autoresponder' }).click();
  const pdlg = modalTitled(phone, 'Filters and autoresponder');
  await pdlg.locator('.mf-item', { hasText: 'Newsletters' }).waitFor();
  await pdlg.getByRole('button', { name: 'Edit Newsletters' }).click();
  await pdlg.getByLabel('Filter name').waitFor();
  await shot(phone, 'filters-phone');
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
  sink.close();
  llm.close();
  list.close();
  process.exit(exitCode);
}
