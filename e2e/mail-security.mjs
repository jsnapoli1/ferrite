// Browser test of Mail security and privacy against a REAL IMAP server (an isolated Dovecot
// from mail-server.sh; no Postfix needed) and a Ferrite server with a fresh data dir:
//
//   MAIL_TEST_DIR=/tmp/ferrite-mail-security IMAP_PORT=3331 LMTP_PORT=3332 AUTH_PORT=3333 \
//     SIEVE_PORT=3334 POSTFIX=0 bash mail-server.sh start /tmp/ferrite-mail-security
//   FERRITE_SFU=0 FERRITE_DATA_DIR=$(mktemp -d) FERRITE_LISTEN=127.0.0.1:8863 \
//     FERRITE_PUBLIC_URL=http://127.0.0.1:8863 FERRITE_MAIL_DNS=127.0.0.1:18853 \
//     FERRITE_MAIL_PROXY_ALLOW_PRIVATE=1 ../target/debug/ferrite serve &
//   MAIL_TEST_DIR=/tmp/ferrite-mail-security IMAP_PORT=3331 FERRITE_URL=http://127.0.0.1:8863 node mail-security.mjs
//
// The script serves what the server reaches out to: DNS with DKIM keys it generates
// (UDP 127.0.0.1:$DNS_PORT, 18853), remote images (HTTP $IMG_PORT, 18866; the server runs with
// FERRITE_MAIL_PROXY_ALLOW_PRIVATE=1 only so its image proxy may fetch from localhost), and
// an SMTP sink for the account ($SMTP_PORT, 18865). Mail arrives through Dovecot's delivery
// agent (mail-server.sh deliver), DKIM-signed by this script, S/MIME made by OpenSSL (the
// fixtures in crates/ferrite-core/src/mail/testdata/smime).
//
// Covered: DKIM chips (pass with Ed25519 and RSA, failure), phishing hints with reasons,
// external senders, remote images blocked and shown through the proxy, "always from this
// domain" and the trusted-senders list, sender photos from contacts, the settings dialog
// (internal addresses, S/MIME import of a PKCS#12 and a CA, account certificate), reading
// encrypted mail and checking signatures, view source, attachments as a ZIP, the composer's
// internal-address check, S/MIME sign+encrypt (refused for recipients without a certificate,
// before anything is sent), PGP without Mailvelope (notice and armored text) and with a mock
// of Mailvelope's documented client API (display container and encrypting editor), light,
// dark and 390px screenshots. Prints {ok, errors}; any console error fails the run.
import { chromium } from 'playwright';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { createServer as createTcp } from 'node:net';
import { createServer as createHttp } from 'node:http';
import { createSocket } from 'node:dgram';
import { createHash, generateKeyPairSync, sign as cryptoSign } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { deflateSync } from 'node:zlib';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8863';
const shots = process.env.SHOTS || '/tmp/ferrite-mail-security-shots';
const mailDir = process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-security';
const IMAP_PORT = Number(process.env.IMAP_PORT || 3331);
const DNS_PORT = Number(process.env.DNS_PORT || 18853);
const IMG_PORT = Number(process.env.IMG_PORT || 18866);
const SMTP_PORT = Number(process.env.SMTP_PORT || 18865);
const here = dirname(fileURLToPath(import.meta.url));
const fixtures = join(here, '../crates/ferrite-core/src/mail/testdata/smime');
mkdirSync(shots, { recursive: true });
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

// ---- DKIM (RFC 6376 relaxed/relaxed; RFC 8463 Ed25519) ----------------------------------------
const relaxedBody = (body) => {
  const lines = body.replace(/\r?\n/g, '\r\n').split('\r\n').map((l) => l.replace(/[ \t]+/g, ' ').replace(/ $/, ''));
  while (lines.length && lines[lines.length - 1] === '') lines.pop();
  return lines.length ? `${lines.join('\r\n')}\r\n` : '';
};
const relaxedHeader = (name, value) => `${name.trim().toLowerCase()}:${value.replace(/\r\n(?=[ \t])/g, '').replace(/[ \t]+/g, ' ').trim()}`;

function dkimSign(raw, { domain, selector, key, algo }) {
  const norm = raw.replace(/\r?\n/g, '\r\n');
  const split = norm.indexOf('\r\n\r\n');
  const head = norm.slice(0, split);
  const body = norm.slice(split + 4);
  const fields = [];
  for (const line of head.split('\r\n')) {
    if (/^[ \t]/.test(line) && fields.length) fields[fields.length - 1][1] += `\r\n${line}`;
    else { const i = line.indexOf(':'); fields.push([line.slice(0, i), line.slice(i + 1)]); }
  }
  const names = ['from', 'to', 'subject', 'date', 'message-id'];
  const bh = createHash('sha256').update(relaxedBody(body)).digest('base64');
  const value = ` v=1; a=${algo}; c=relaxed/relaxed; d=${domain}; s=${selector}; t=${Math.floor(Date.now() / 1000)}; h=${names.join(':')}; bh=${bh}; b=`;
  let data = '';
  for (const n of names) {
    const f = [...fields].reverse().find(([k]) => k.trim().toLowerCase() === n);
    if (f) data += `${relaxedHeader(f[0], f[1])}\r\n`;
  }
  data += relaxedHeader('DKIM-Signature', value);
  const b = algo === 'rsa-sha256'
    ? cryptoSign('sha256', Buffer.from(data), key).toString('base64')
    : cryptoSign(null, createHash('sha256').update(data).digest(), key).toString('base64');
  return `DKIM-Signature:${value}${b}\r\n${norm}`;
}

const ed = generateKeyPairSync('ed25519');
const rsa = generateKeyPairSync('rsa', { modulusLength: 2048 });
const txt = {
  'ed._domainkey.signed.example': `v=DKIM1; k=ed25519; p=${ed.publicKey.export({ format: 'der', type: 'spki' }).subarray(-32).toString('base64')}`,
  'rsa._domainkey.signed.example': `v=DKIM1; k=rsa; p=${rsa.publicKey.export({ format: 'der', type: 'spki' }).toString('base64')}`,
};

// ---- the network the server reaches ------------------------------------------------------------
const dns = createSocket('udp4');
dns.on('message', (q, peer) => {
  if (q.length < 17) return;
  let i = 12;
  const labels = [];
  while (i < q.length && q[i] !== 0) { labels.push(q.subarray(i + 1, i + 1 + q[i]).toString().toLowerCase()); i += 1 + q[i]; }
  const qtype = q.readUInt16BE(i + 1);
  const answer = qtype === 16 ? txt[labels.join('.')] : undefined;
  const head = Buffer.from([q[0], q[1], 0x81, answer ? 0x80 : 0x83, 0, 1, 0, answer ? 1 : 0, 0, 0, 0, 0]);
  const parts = [head, q.subarray(12, i + 5)];
  if (answer) {
    const chunks = [];
    for (let p = 0; p < answer.length; p += 255) { const c = Buffer.from(answer.slice(p, p + 255)); chunks.push(Buffer.from([c.length]), c); }
    const rdata = Buffer.concat(chunks);
    const rr = Buffer.alloc(12);
    rr.writeUInt16BE(0xc00c, 0); rr.writeUInt16BE(16, 2); rr.writeUInt16BE(1, 4); rr.writeUInt32BE(60, 6); rr.writeUInt16BE(rdata.length, 10);
    parts.push(rr, rdata);
  }
  dns.send(Buffer.concat(parts), peer.port, peer.address);
});
await new Promise((r) => dns.bind(DNS_PORT, '127.0.0.1', r));

/** A solid-colour PNG (w×h, [r, g, b]), built here so the test needs no image files. */
function solidPng(w, h, [r, g, b]) {
  const crcTable = Array.from({ length: 256 }, (_, n) => { let c = n; for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1; return c >>> 0; });
  const crc = (buf) => { let c = 0xffffffff; for (const x of buf) c = crcTable[(c ^ x) & 0xff] ^ (c >>> 8); return (c ^ 0xffffffff) >>> 0; };
  const chunk = (type, data) => {
    const len = Buffer.alloc(4); len.writeUInt32BE(data.length);
    const td = Buffer.concat([Buffer.from(type), data]);
    const c = Buffer.alloc(4); c.writeUInt32BE(crc(td));
    return Buffer.concat([len, td, c]);
  };
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0); ihdr.writeUInt32BE(h, 4); ihdr[8] = 8; ihdr[9] = 2;
  const rowBytes = Buffer.concat([Buffer.from([0]), Buffer.from(Array.from({ length: w }, () => [r, g, b]).flat())]);
  const raw = Buffer.concat(Array.from({ length: h }, () => rowBytes));
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', ihdr), chunk('IDAT', deflateSync(raw)), chunk('IEND', Buffer.alloc(0))]);
}
const BANNER = solidPng(320, 120, [15, 118, 110]);
const PHOTO = solidPng(48, 48, [124, 58, 237]);
const imageRequests = [];
const img = createHttp((req, res) => {
  imageRequests.push({ url: req.url, headers: req.headers });
  if (req.url.startsWith('/banner.png') || req.url.startsWith('/pixel.png')) {
    res.writeHead(200, { 'Content-Type': 'image/png', 'Set-Cookie': 'tracker=1' });
    res.end(BANNER);
  } else { res.writeHead(404); res.end(); }
});
await new Promise((r) => img.listen(IMG_PORT, '127.0.0.1', r));

const sent = [];
const smtp = createTcp((sock) => {
  let data = null;
  let lines = [];
  let buf = '';
  sock.write('220 sink ESMTP\r\n');
  sock.on('data', (chunk) => {
    buf += chunk.toString('binary');
    let i;
    while ((i = buf.indexOf('\r\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 2);
      if (data !== null) {
        if (line === '.') { sent.push(Buffer.from(lines.join('\r\n'), 'binary').toString('utf8')); lines = []; data = null; sock.write('250 queued\r\n'); } else lines.push(line.startsWith('..') ? line.slice(1) : line);
        continue;
      }
      const u = line.toUpperCase();
      if (u.startsWith('EHLO') || u.startsWith('HELO')) sock.write('250-sink\r\n250 8BITMIME\r\n');
      else if (u === 'DATA') { data = ''; sock.write('354 go ahead\r\n'); } else if (u === 'QUIT') { sock.write('221 bye\r\n'); sock.end(); } else sock.write('250 OK\r\n');
    }
  });
  sock.on('error', () => {});
});
await new Promise((r) => smtp.listen(SMTP_PORT, '127.0.0.1', r));

// ---- mail -------------------------------------------------------------------------------------------
function deliver(raw) {
  const f = join(tmpdir(), `ferrite-sec-${process.pid}-${Math.random().toString(36).slice(2)}.eml`);
  writeFileSync(f, raw.replace(/\r?\n/g, '\r\n'));
  execFileSync('bash', [join(here, 'mail-server.sh'), 'deliver', 'alice', f, mailDir], { stdio: 'inherit' });
}
const msg = (from, subject, id, headers, body, date = new Date().toUTCString()) => `From: ${from}\r\nTo: Alice <alice@example.test>\r\nSubject: ${subject}\r\nDate: ${date}\r\nMessage-ID: <${id}>\r\nMIME-Version: 1.0\r\n${headers}\r\n${body}`;
const plain = 'Content-Type: text/plain; charset=utf-8\r\n';
const SECRET_PGP = 'Secret plans from Bob: meet at the lighthouse.';
const pgpArmor = (text) => `-----BEGIN PGP MESSAGE-----\nVersion: Mailvelope mock\n\n${Buffer.from(text).toString('base64')}\n-----END PGP MESSAGE-----\n`;

// The mock of Mailvelope's client API (https://mailvelope.github.io/mailvelope/): the
// extension injects window.mailvelope and fires a "mailvelope" event; containers are frames
// it renders into the given element. "Encryption" here is base64 inside the armor.
const MAILVELOPE_MOCK = `(() => {
  // The extension provides its API to the page only, not to the frames it renders.
  if (window.top !== window) return;
  const keys = new Set(['bob@example.test', 'alice@example.test']);
  const b64 = (s) => btoa(String.fromCharCode(...new TextEncoder().encode(s)));
  const unb64 = (s) => new TextDecoder().decode(Uint8Array.from(atob(s), (c) => c.charCodeAt(0)));
  const armor = (text) => '-----BEGIN PGP MESSAGE-----\\nVersion: Mailvelope mock\\n\\n' + b64(text) + '\\n-----END PGP MESSAGE-----\\n';
  const frame = (selector) => {
    const host = document.querySelector(selector);
    if (!host) throw new Error('container not found: ' + selector);
    const f = document.createElement('iframe');
    f.className = 'mailvelope-mock-frame';
    f.style.cssText = 'width:100%;height:100%;border:0';
    host.appendChild(f);
    return f;
  };
  const esc = (s) => s.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  window.mailvelopeCalls = [];
  window.mailvelope = {
    getVersion: async () => '6.0.0',
    getKeyring: async () => ({
      validKeyForAddress: async (recipients) => Object.fromEntries(recipients.map((r) => [r, keys.has(r) ? { keys: [{ fingerprint: 'MOCK', lastModified: new Date() }] } : false])),
    }),
    createDisplayContainer: async (selector, armored, keyring, options) => {
      window.mailvelopeCalls.push(['display', selector, options]);
      const m = armored.match(/\\n\\n([A-Za-z0-9+/=]+)\\n-----END PGP MESSAGE-----/);
      if (!m) return { error: { message: 'not a PGP message', code: 'DECRYPT_ERROR' } };
      const f = frame(selector);
      f.srcdoc = '<body style="font:14px sans-serif"><p class="mv-sender">From ' + esc(options?.senderAddress || '') + '</p><pre class="mv-plain">' + esc(unb64(m[1])) + '</pre></body>';
      return {};
    },
    createEditorContainer: async (selector, keyring, options) => {
      window.mailvelopeCalls.push(['editor', selector, options]);
      const f = frame(selector);
      f.srcdoc = '<body style="margin:0"><textarea class="mv-editor" style="width:100%;height:200px" aria-label="Mailvelope editor"></textarea></body>';
      await new Promise((r) => f.addEventListener('load', r, { once: true }));
      return {
        encrypt: async (recipients) => {
          const missing = recipients.filter((r) => !keys.has(r));
          if (missing.length) throw Object.assign(new Error('No key for ' + missing.join(', ')), { code: 'NO_KEY_FOR_RECIPIENT' });
          return armor(f.contentDocument.querySelector('textarea').value);
        },
        createDraft: async () => armor(f.contentDocument.querySelector('textarea').value),
      };
    },
  };
  setTimeout(() => window.dispatchEvent(new CustomEvent('mailvelope', { detail: window.mailvelope })), 0);
})();`;

const browser = await chromium.launch();
const errors = [];
let exitCode = 0;
let current = null;

async function newPage(name, viewport = { width: 1440, height: 900 }, colorScheme = 'light', init = null) {
  const ctx = await browser.newContext({ viewport, colorScheme, acceptDownloads: true });
  if (init) await ctx.addInitScript(init);
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

const login = (page) => page.api('POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
const exact = (text) => new RegExp(`^${text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`);
const row = (page, subject) => page.locator('.ml-row', { has: page.locator('.ml-subject', { hasText: exact(subject) }) });
const card = (page) => page.locator('.ml-card.expanded').first();
async function open(page, subject) {
  await row(page, subject).click();
  await page.locator('.ml-thread-head h2', { hasText: exact(subject) }).waitFor();
  await card(page).locator('.ml-card-body > .ml-content > *').first().waitFor();
}
const frameImages = (page) => page.evaluate(() => [...(document.querySelector('.ml-card.expanded iframe')?.contentDocument?.querySelectorAll('img') || [])]
  .map((i) => ({ src: i.getAttribute('src'), ok: i.complete && i.naturalWidth > 0 })));

try {
  log('empty the test mailbox and deliver mail');
  execFileSync('bash', [join(here, 'mail-server.sh'), 'reset', mailDir], { stdio: 'inherit' });
  const signed = (subject, id, text, selector, algo, key) => dkimSign(msg('Signer News <news@signed.example>', subject, id, plain, `${text}\r\n`),
    { domain: 'signed.example', selector, key, algo });
  deliver(signed('DKIM Ed25519', 'ed@signed.example', 'Signed with Ed25519.', 'ed', 'ed25519-sha256', ed.privateKey));
  deliver(signed('DKIM RSA', 'rsa@signed.example', 'Signed with RSA.', 'rsa', 'rsa-sha256', rsa.privateKey));
  deliver(signed('Payment details', 'tampered@signed.example', 'Pay 10 EUR to the usual account.', 'ed', 'ed25519-sha256', ed.privateKey).replace('Pay 10 EUR', 'Pay 9000 EUR'));
  deliver(msg('"Ada Lovelace" <ada.lovelace@freemail.example>', 'Urgent invoice', 'phish@freemail.example',
    'Content-Type: text/html; charset=utf-8\r\nReply-To: collect@attacker.example\r\n',
    '<p>Your invoice is overdue. Pay at <a href="https://attacker.example/pay">https://bank.example/pay</a> today.</p>\r\n', 'Fri, 1 Jan 2038 09:00:00 +0000'));
  deliver(msg('Shop Newsletter <news@shop.example>', 'Autumn sale', 'sale@shop.example', 'Content-Type: text/html; charset=utf-8\r\n',
    `<h2 style="color:#0f766e">Autumn sale</h2><p>Everything must go.</p><img alt="banner" width="320" height="120" src="http://127.0.0.1:${IMG_PORT}/banner.png"><p>See you soon.</p>\r\n`));
  deliver(msg('Ada Lovelace <ada@example.test>', 'Two files', 'files@example.test', 'Content-Type: multipart/mixed; boundary="B"\r\n',
    '--B\r\nContent-Type: text/plain\r\n\r\nBoth files attached.\r\n--B\r\nContent-Type: text/plain; name="notes.txt"\r\nContent-Disposition: attachment; filename="notes.txt"\r\n\r\nfirst\r\n--B\r\nContent-Type: text/csv; name="data.csv"\r\nContent-Disposition: attachment; filename="data.csv"\r\n\r\na,b\r\n--B--\r\n'));
  deliver(msg('Bob <bob@example.test>', 'PGP message', 'pgp@example.test', plain, pgpArmor(SECRET_PGP)));
  for (const f of ['encrypted-aes.eml', 'signed-detached.eml']) deliver(readFileSync(join(fixtures, f), 'utf8'));

  const page = await newPage('desktop');
  current = page;
  await page.api('POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  // Ada is a contact, with a photo (listing contacts creates the default address book).
  await page.api('GET', '/api/contacts');
  const photo = PHOTO.toString('base64');
  const vcard = `BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada-e2e\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.test\r\nPHOTO;ENCODING=b;TYPE=PNG:${photo}\r\nEND:VCARD\r\n`;
  const put = await page.request.put(`${base}/remote.php/dav/addressbooks/users/alice/contacts/ada-e2e.vcf`, {
    headers: { Authorization: `Basic ${Buffer.from('alice:alice-password').toString('base64')}`, 'Content-Type': 'text/vcard' }, data: vcard,
  });
  assert(put.ok(), `contact PUT ${put.status()}`);
  await page.api('PATCH', '/api/admin/mail/settings', { allow_local_servers: true });
  const { account } = await page.api('POST', '/api/mail/accounts', {
    email: 'alice@example.test', name: 'Alice Archer', password: 'alice-mail-pw', imap_host: '127.0.0.1', imap_port: IMAP_PORT, imap_security: 'none',
    smtp_host: '127.0.0.1', smtp_port: SMTP_PORT, smtp_security: 'none', smtp_auth: false,
  });
  assert(account.id, 'account created');

  await page.goto(`${base}/mail`);
  await row(page, 'Two files').waitFor({ timeout: 20000 });
  assert(await page.locator('.ml-row').count() === 9, `nine messages, got ${await page.locator('.ml-row').count()}`);

  log('sender photo from contacts; initials otherwise');
  await row(page, 'Two files').locator('.avatar.has-photo img').waitFor({ timeout: 15000 });
  assert(await row(page, 'Autumn sale').locator('.avatar.has-photo').count() === 0, 'no favicon reachable: initials');

  log('DKIM: verified, failed');
  await open(page, 'DKIM Ed25519');
  await card(page).locator('.ms-chip.ok', { hasText: 'Verified sender · signed.example' }).waitFor({ timeout: 20000 });
  await open(page, 'DKIM RSA');
  await card(page).locator('.ms-chip.ok', { hasText: 'Verified sender' }).waitFor({ timeout: 20000 });
  await open(page, 'Payment details');
  await card(page).locator('.ms-chip.warn', { hasText: 'Signature check failed' }).waitFor({ timeout: 20000 });
  await card(page).getByRole('button', { name: 'Details' }).click();
  await card(page).locator('.ms-details', { hasText: 'd=signed.example' }).waitFor();

  log('phishing hints with reasons');
  await open(page, 'Urgent invoice');
  const warn = card(page).locator('.ms-alert.warn');
  await warn.waitFor({ timeout: 20000 });
  const warnText = await warn.innerText();
  assert(warnText.includes('Take care with this message (4 reasons)'), warnText);
  for (const part of ['like one of your contacts', 'Replies go to collect@attacker.example', 'dated in the future', 'https://attacker.example/pay']) assert(warnText.includes(part), `${part} in ${warnText}`);
  await page.screenshot({ path: `${shots}/security-phishing.png` });

  log('remote images: blocked, then always shown for the domain, through the proxy');
  await open(page, 'Autumn sale');
  await card(page).locator('.ms-remote').waitFor();
  assert((await frameImages(page)).every((i) => !i.src), 'no remote image source while blocked');
  assert(imageRequests.length === 0, 'nothing fetched yet');
  await page.screenshot({ path: `${shots}/security-remote-blocked.png` });
  await card(page).getByRole('button', { name: 'Always from shop.example' }).click();
  await page.waitForFunction(() => {
    const i = document.querySelector('.ml-card.expanded iframe')?.contentDocument?.querySelector('img');
    return i && i.complete && i.naturalWidth > 0;
  }, null, { timeout: 15000 });
  const imgs = await frameImages(page);
  assert(imgs.length === 1 && imgs[0].src.startsWith('/api/mail/proxy?'), `proxied image: ${JSON.stringify(imgs)}`);
  const upstream = imageRequests[imageRequests.length - 1];
  assert(upstream.url === '/banner.png' && !upstream.headers.cookie && !upstream.headers.referer, `no cookies or referrer upstream: ${JSON.stringify(upstream.headers)}`);
  await open(page, 'Two files');
  await open(page, 'Autumn sale');
  await card(page).locator('.ms-chip', { hasText: 'Images allowed' }).waitFor({ timeout: 15000 });
  await page.waitForFunction(() => document.querySelector('.ml-card.expanded iframe')?.contentDocument?.querySelector('img')?.naturalWidth > 0, null, { timeout: 15000 });
  assert(await card(page).locator('.ms-remote').count() === 0, 'trusted: no banner');

  log('settings: trusted senders, internal addresses, certificates');
  await page.getByRole('button', { name: 'Privacy and security' }).click();
  const dlg = page.locator('.modal', { hasText: 'Privacy and security' });
  await dlg.locator('.ms-list .list-item', { hasText: 'Anyone at shop.example' }).waitFor();
  await page.screenshot({ path: `${shots}/security-settings-images.png` });
  await dlg.getByRole('tab', { name: 'Internal addresses' }).click();
  await dlg.locator('.set-switch', { hasText: 'Warn about external senders' }).click();
  await dlg.getByRole('textbox', { name: 'Add to internal addresses' }).fill('example.test');
  await dlg.getByRole('button', { name: 'Add', exact: true }).click();
  await dlg.locator('.ms-list .list-item', { hasText: 'Anyone at example.test' }).waitFor();
  await dlg.getByRole('tab', { name: 'Certificates' }).click();
  await dlg.locator('.ms-certs .empty').waitFor();
  await dlg.locator('input[aria-label="PKCS#12 file"]').setInputFiles(join(fixtures, 'alice.p12'));
  await dlg.locator('input[aria-label="Password of the file"]').fill('alice-p12');
  await dlg.getByRole('button', { name: 'Import' }).click();
  await dlg.locator('.ms-cert', { hasText: 'alice@example.test' }).waitFor();
  await page.locator('.toast', { hasText: 'Imported Alice' }).waitFor();
  await dlg.getByRole('radio', { name: 'PEM / DER' }).click();
  await dlg.locator('input[aria-label="Certificate file"]').setInputFiles(join(fixtures, 'ca.pem'));
  await dlg.getByRole('button', { name: 'Import' }).click();
  await dlg.locator('.ms-cert', { hasText: 'Trusted issuer' }).waitFor();
  const certSelect = dlg.locator('select[aria-label="Certificate for alice@example.test"]');
  await certSelect.waitFor();
  await certSelect.selectOption({ index: 1 });
  await page.locator('.toast', { hasText: 'Saved' }).last().waitFor();
  await dlg.locator('.ms-identity').getByText('Sign by default').click();
  await page.waitForTimeout(300);
  await page.screenshot({ path: `${shots}/security-settings-certificates.png` });
  await dlg.getByRole('button', { name: 'Done' }).click();
  await dlg.waitFor({ state: 'detached' });

  log('external sender; encrypted and signed S/MIME mail');
  await open(page, 'Urgent invoice');
  await card(page).locator('.ms-alert.info', { hasText: 'External sender' }).waitFor({ timeout: 15000 });
  await open(page, 'Encrypted AES');
  await card(page).locator('.ml-text', { hasText: 'The launch code is 3456.' }).waitFor({ timeout: 15000 });
  await card(page).locator('.ms-chip.ok', { hasText: 'Encrypted' }).waitFor({ timeout: 15000 });
  await open(page, 'Signed detached');
  await card(page).locator('.ms-chip.ok', { hasText: 'Signed by Bob' }).waitFor({ timeout: 15000 });
  assert(await card(page).locator('.ml-attachment').count() === 0, 'the signature is not an attachment');
  await page.screenshot({ path: `${shots}/security-smime.png` });

  log('view source and the ZIP of attachments');
  await page.getByRole('button', { name: 'More actions' }).click();
  await page.getByRole('menuitem', { name: 'View source' }).click();
  await page.locator('.ms-source', { hasText: 'Message-ID: <signed-detached@example.test>' }).waitFor();
  assert(await page.locator('.ms-source script, .ms-source b').count() === 0, 'source is text');
  await page.locator('.modal-foot').getByRole('button', { name: 'Close', exact: true }).click();
  await open(page, 'Two files');
  const zipLinkEl = card(page).locator('.ms-zip');
  await zipLinkEl.waitFor();
  const [download] = await Promise.all([page.waitForEvent('download'), zipLinkEl.click()]);
  assert(download.suggestedFilename() === 'Two files - attachments.zip', download.suggestedFilename());
  const zipBytes = readFileSync(await download.path());
  assert(zipBytes.subarray(0, 4).toString('hex') === '504b0304' && zipBytes.includes(Buffer.from('data.csv')), 'a ZIP with the files');

  log('PGP without Mailvelope: a notice and the armored text');
  await open(page, 'PGP message');
  await card(page).locator('.ms-alert.info', { hasText: 'This message is encrypted with PGP' }).waitFor({ timeout: 15000 });
  await card(page).locator('.ms-armored summary').click();
  await card(page).locator('.ms-armored pre', { hasText: 'BEGIN PGP MESSAGE' }).waitFor();

  log('composer: the internal-address check, S/MIME sign and encrypt');
  await page.getByRole('button', { name: 'Compose' }).click();
  const comp = page.locator('.modal .ml-compose');
  await comp.waitFor();
  await page.keyboard.type('eve@attacker.example,');
  await page.locator('.ml-subject-input').fill('Hello');
  await page.locator('.ml-body-input').fill('Hi Eve');
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.modal', { hasText: 'Send outside your organization?' }).waitFor();
  await page.screenshot({ path: `${shots}/security-external-recipient.png` });
  await page.locator('.modal', { hasText: 'Send outside your organization?' }).getByRole('button', { name: 'Cancel' }).click();
  assert(sent.length === 0, 'nothing was sent');
  await page.getByRole('button', { name: 'Discard' }).click();

  await page.getByRole('button', { name: 'Compose' }).click();
  await comp.waitFor();
  await page.keyboard.type('carol@example.test,');
  await page.locator('.ml-subject-input').fill('For Carol');
  await page.locator('.ml-body-input').fill('Secret');
  const signBtn = comp.getByRole('button', { name: 'Sign', exact: true });
  await signBtn.waitFor();
  assert(await signBtn.getAttribute('aria-pressed') === 'true', 'signing on by default');
  await comp.getByRole('button', { name: 'Encrypt', exact: true }).click();
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'No S/MIME certificate for carol@example.test' }).waitFor();
  assert(sent.length === 0, 'refused before sending');
  await page.getByRole('button', { name: 'Discard' }).click();

  await page.getByRole('button', { name: 'Compose' }).click();
  await comp.waitFor();
  await page.keyboard.type('bob@example.test,');
  await page.locator('.ml-subject-input').fill('Sealed');
  await page.locator('.ml-body-input').fill('Meet at the observatory.');
  await comp.getByRole('button', { name: 'Encrypt', exact: true }).click();
  await page.screenshot({ path: `${shots}/security-compose.png` });
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Message sent' }).waitFor({ timeout: 20000 });
  assert(sent.length === 1 && sent[0].includes('application/pkcs7-mime; smime-type=enveloped-data') && !sent[0].includes('observatory'), 'sent encrypted');

  log('Mailvelope (mock of its client API): reading and writing PGP');
  const mv = await newPage('mailvelope', { width: 1440, height: 900 }, 'light', MAILVELOPE_MOCK);
  current = mv;
  await login(mv);
  await mv.goto(`${base}/mail`);
  await open(mv, 'PGP message');
  const mvFrame = mv.frameLocator('.ml-card.expanded .ms-mailvelope iframe');
  await mvFrame.locator('.mv-plain', { hasText: SECRET_PGP }).waitFor({ timeout: 15000 });
  const calls = await mv.evaluate(() => window.mailvelopeCalls);
  assert(calls[0][0] === 'display' && calls[0][2].senderAddress === 'bob@example.test', JSON.stringify(calls));
  await card(mv).locator('.ms-chip', { hasText: 'PGP encrypted' }).waitFor();
  await mv.screenshot({ path: `${shots}/security-pgp-mailvelope.png` });
  await mv.getByRole('button', { name: 'Compose' }).click();
  await mv.locator('.modal .ml-compose').waitFor();
  await mv.keyboard.type('bob@example.test,');
  await mv.locator('.ml-subject-input').fill('Over PGP');
  const pgpBtn = mv.locator('.modal').getByRole('button', { name: 'PGP', exact: true });
  await pgpBtn.waitFor();
  await pgpBtn.click();
  const editor = mv.frameLocator('.modal .ms-editor iframe');
  await editor.locator('textarea').fill('Written in Mailvelope.');
  assert(!(await mv.locator('.ml-body-input').isVisible()), 'the plain editor makes way');
  await mv.getByRole('button', { name: 'Send', exact: true }).click();
  await mv.locator('.toast', { hasText: 'Message sent' }).waitFor({ timeout: 20000 });
  const pgpMail = sent[sent.length - 1];
  assert(pgpMail.includes('multipart/encrypted; protocol="application/pgp-encrypted"'), 'PGP/MIME');
  const armored = pgpMail.match(/\r\n\r\n([A-Za-z0-9+/=]+)\r\n-----END PGP MESSAGE-----/);
  assert(armored && Buffer.from(armored[1], 'base64').toString() === 'Written in Mailvelope.', 'the editor content, encrypted by Mailvelope');
  assert(!pgpMail.includes('Written in Mailvelope'), 'no plaintext on the wire');
  await mv.context().close();
  current = page;

  log('dark theme and phone');
  const dark = await newPage('dark', { width: 1440, height: 900 }, 'dark');
  await login(dark);
  await dark.goto(`${base}/mail`);
  await open(dark, 'Urgent invoice');
  await card(dark).locator('.ms-alert.warn').waitFor({ timeout: 15000 });
  await dark.screenshot({ path: `${shots}/security-dark.png` });
  await open(dark, 'Autumn sale');
  await card(dark).locator('.ms-chip', { hasText: 'Images allowed' }).waitFor({ timeout: 15000 });
  await dark.waitForFunction(() => {
    const f = document.querySelector('.ml-card.expanded iframe');
    const img = f?.contentDocument?.querySelector('img');
    return img?.complete && img.naturalWidth > 0 && f.offsetHeight >= f.contentDocument.documentElement.scrollHeight;
  }, null, { timeout: 15000 });
  await dark.screenshot({ path: `${shots}/security-dark-images.png` });
  await dark.getByRole('button', { name: 'Privacy and security' }).click();
  await dark.locator('.modal').getByRole('tab', { name: 'Certificates' }).click();
  await dark.locator('.ms-cert').first().waitFor();
  await dark.screenshot({ path: `${shots}/security-dark-settings.png` });
  await dark.context().close();

  const phone = await newPage('phone', { width: 390, height: 844 });
  await login(phone);
  await phone.goto(`${base}/mail`);
  await open(phone, 'Urgent invoice');
  await card(phone).locator('.ms-alert.warn').waitFor({ timeout: 15000 });
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll');
  await phone.screenshot({ path: `${shots}/security-phone.png` });
  await phone.getByRole('button', { name: 'Back to the list' }).click();
  await open(phone, 'Signed detached');
  await card(phone).locator('.ms-chip', { hasText: 'Signed by Bob' }).waitFor({ timeout: 15000 });
  await phone.screenshot({ path: `${shots}/security-phone-smime.png` });
  await phone.getByRole('button', { name: 'Back to the list' }).click();
  await phone.getByRole('button', { name: 'Folders' }).click();
  await phone.getByRole('button', { name: 'Privacy and security' }).click();
  await phone.locator('.modal').getByRole('tab', { name: 'Certificates' }).click();
  await phone.locator('.ms-cert').first().waitFor();
  assert(await phone.evaluate(() => document.documentElement.scrollWidth <= 390), 'no horizontal scroll in the dialog');
  assert(await phone.evaluate(() => { const r = document.querySelector('.modal').getBoundingClientRect(); return r.left >= 0 && r.right <= 390; }), 'the dialog fits the phone screen');
  await phone.screenshot({ path: `${shots}/security-phone-settings.png` });
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
  dns.close();
  img.close();
  smtp.close();
  process.exit(exitCode);
}
