// A small SMTP server for the mail browser tests, so they need no system Postfix (one
// shared service; several isolated Dovecot instances can each get their own relay). It
// accepts every message (AUTH PLAIN or LOGIN with any password), keeps it for the test to
// inspect, and delivers a copy for each recipient with a test mailbox (alice, bob and
// $EXTRA_USERS @example.test; other addresses are only kept) into the test Dovecot with
// `mail-server.sh deliver` (as if it arrived from outside, Sieve included). A test can make
// the next messages fail with an SMTP error.
//
//   node smtp-relay.mjs PORT [MAIL_TEST_DIR]      # standalone, e.g. for mail.mjs
//   import { startSmtpRelay } from './smtp-relay.mjs'
import { createServer } from 'node:net';
import { execFile } from 'node:child_process';
import { writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

function deliverTo(user, data, dir) {
  return new Promise((resolve, reject) => {
    const tmp = mkdtempSync(join(tmpdir(), 'relay-'));
    const file = join(tmp, 'msg.eml');
    writeFileSync(file, data);
    execFile('bash', [join(here, 'mail-server.sh'), 'deliver', user, file, dir], (err, _out, stderr) => {
      rmSync(tmp, { recursive: true, force: true });
      if (err) reject(new Error(`deliver to ${user}: ${stderr || err.message}`)); else resolve();
    });
  });
}

/**
 * Starts the relay. Returns { port, messages, waitFor(pred, ms), failNext(n, reply), close() }.
 * messages: [{ from, to: [addresses], data (CRLF text) }].
 */
export function startSmtpRelay({
  port = 0, dir = process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-server', deliver = true,
  mailboxes = ['alice', 'bob', ...(process.env.EXTRA_USERS || '').split(/\s+/).filter(Boolean)],
} = {}) {
  const local = new Set(mailboxes.map((u) => `${u}@example.test`));
  const messages = [];
  const waiters = [];
  let failures = 0;
  let failReply = '451 4.3.0 Try again later';
  const sockets = new Set();
  const server = createServer((sock) => {
    sockets.add(sock);
    sock.on('close', () => sockets.delete(sock));
    sock.setEncoding('utf8');
    let buf = '';
    let inData = false;
    let data = [];
    let msg = { from: '', to: [] };
    let authStep = null;
    const reply = (s) => sock.write(`${s}\r\n`);
    reply('220 relay.example.test ESMTP');
    sock.on('data', (chunk) => {
      buf += chunk;
      let i;
      while ((i = buf.indexOf('\r\n')) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 2);
        if (inData) {
          if (line === '.') {
            inData = false;
            const m = { ...msg, data: `${data.join('\r\n')}\r\n` };
            data = [];
            msg = { from: '', to: [] };
            if (failures > 0) { failures--; reply(failReply); continue; }
            messages.push(m);
            const delivered = deliver
              ? Promise.all(m.to.filter((t) => local.has(t.toLowerCase())).map((t) => deliverTo(t, m.data, dir)))
              : Promise.resolve();
            delivered.then(() => {
              reply('250 2.0.0 queued');
              for (const w of [...waiters]) if (w.pred(m)) { waiters.splice(waiters.indexOf(w), 1); w.resolve(m); }
            }, (e) => { console.error(e.message); reply('451 4.3.0 local delivery failed'); });
          } else data.push(line.startsWith('.') ? line.slice(1) : line);
          continue;
        }
        if (authStep === 'plain') { authStep = null; reply('235 2.7.0 Authenticated'); continue; }
        if (authStep === 'user') { authStep = 'pass'; reply('334 UGFzc3dvcmQ6'); continue; }
        if (authStep === 'pass') { authStep = null; reply('235 2.7.0 Authenticated'); continue; }
        const up = line.toUpperCase();
        if (up.startsWith('EHLO')) sock.write('250-relay.example.test\r\n250-8BITMIME\r\n250-SMTPUTF8\r\n250-AUTH PLAIN LOGIN\r\n250 SIZE 52428800\r\n');
        else if (up.startsWith('HELO')) reply('250 relay.example.test');
        else if (up.startsWith('AUTH PLAIN')) { if (line.trim().split(/\s+/).length > 2) reply('235 2.7.0 Authenticated'); else { authStep = 'plain'; reply('334 '); } } else if (up.startsWith('AUTH LOGIN')) { authStep = 'user'; reply('334 VXNlcm5hbWU6'); } else if (up.startsWith('MAIL FROM:')) { msg.from = line.slice(10).trim().replace(/^<|>.*$/g, ''); reply('250 2.1.0 OK'); } else if (up.startsWith('RCPT TO:')) { msg.to.push(line.slice(8).trim().replace(/^<|>.*$/g, '')); reply('250 2.1.5 OK'); } else if (up === 'DATA') { inData = true; reply('354 End data with <CR><LF>.<CR><LF>'); } else if (up === 'QUIT') { reply('221 2.0.0 Bye'); sock.end(); } else if (up === 'RSET') { msg = { from: '', to: [] }; reply('250 OK'); } else reply('250 OK');
      }
    });
    sock.on('error', () => {});
  });
  const ready = new Promise((resolve) => server.listen(port, '127.0.0.1', resolve));
  return {
    ready,
    get port() { return server.address()?.port; },
    messages,
    /** Resolves with the first message (already received or arriving) matching `pred`. */
    waitFor(pred, ms = 30000) {
      const found = messages.find(pred);
      if (found) return Promise.resolve(found);
      return new Promise((resolve, reject) => {
        const w = { pred, resolve };
        waiters.push(w);
        setTimeout(() => { const i = waiters.indexOf(w); if (i >= 0) { waiters.splice(i, 1); reject(new Error('timed out waiting for an SMTP message')); } }, ms);
      });
    },
    /** The next `n` messages are refused with `reply`. */
    failNext(n = 1, replyText = '451 4.3.0 Try again later') { failures = n; failReply = replyText; },
    /** Stops listening and drops open connections (clients keep pooled ones open). */
    close() {
      for (const s of sockets) s.destroy();
      return new Promise((resolve) => server.close(() => resolve()));
    },
  };
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  const port = Number(process.argv[2] || 3587);
  const relay = startSmtpRelay({ port, dir: process.argv[3] || process.env.MAIL_TEST_DIR });
  await relay.ready;
  console.log(`SMTP relay on 127.0.0.1:${port}, delivering into ${process.argv[3] || process.env.MAIL_TEST_DIR || '/tmp/ferrite-mail-server'}`);
}
