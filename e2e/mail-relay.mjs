// A small SMTP submission server for test mail setups without Postfix: it authenticates
// (AUTH PLAIN/LOGIN, checked by logging in to the Dovecot instance's IMAP) and delivers into
// that Dovecot over LMTP, like the Postfix of mail-server.sh does. Plain TCP on localhost.
//
//   MAIL_TEST_DIR=/tmp/x IMAP_PORT=3331 LMTP_PORT=3332 POSTFIX=0 bash mail-server.sh start /tmp/x
//   SMTP_PORT=3335 IMAP_PORT=3331 LMTP_PORT=3332 node mail-relay.mjs &
//
// Also usable from a test script: `import { startRelay } from './mail-relay.mjs'`; the
// returned object has `close()` and `messages` (everything accepted, newest last).
import { createServer, connect } from 'node:net';
import { fileURLToPath } from 'node:url';

function lineReader(sock, onLine) {
  let buf = '';
  sock.setEncoding('binary');
  sock.on('data', (chunk) => {
    buf += chunk;
    for (;;) {
      const i = buf.indexOf('\r\n');
      if (i < 0) break;
      const line = buf.slice(0, i);
      buf = buf.slice(i + 2);
      onLine(line);
    }
  });
}

/** One request/response conversation with a line-based server; resolves with the replies. */
function converse(port, steps) {
  return new Promise((resolve, reject) => {
    const sock = connect(port, '127.0.0.1');
    const replies = [];
    let step = -1;
    let pending = '';
    sock.setEncoding('binary');
    sock.on('data', (chunk) => {
      pending += chunk;
      for (;;) {
        // Step -1 is the greeting; then each step's reply is complete when `until` says so.
        const done = step === -1 ? /\r\n$/.test(pending) : steps[step].until(pending);
        if (done) {
          replies.push(pending);
          pending = '';
          step++;
          if (step < steps.length) sock.write(steps[step].send);
          else { sock.end(); resolve(replies); return; }
        } else return;
      }
    });
    sock.on('error', reject);
    sock.setTimeout(20000, () => { sock.destroy(); reject(new Error('timeout')); });
  });
}

async function imapLogin(imapPort, user, pass) {
  const q = (s) => `"${s.replace(/["\\]/g, '\\$&')}"`;
  const replies = await converse(imapPort, [
    { send: `a LOGIN ${q(user)} ${q(pass)}\r\n`, until: (b) => /^a (OK|NO|BAD)/m.test(b) },
    { send: 'b LOGOUT\r\n', until: (b) => /^b /m.test(b) },
  ]);
  return /^a OK/m.test(replies[1]);
}

async function lmtpDeliver(lmtpPort, from, rcpts, data) {
  const body = data.replace(/\r?\n/g, '\r\n').replace(/^\./gm, '..');
  const steps = [
    { send: 'LHLO relay.test\r\n', until: (b) => /^250 /m.test(b) },
    { send: `MAIL FROM:<${from}>\r\n`, until: (b) => /^\d{3} /m.test(b) },
    ...rcpts.map((r) => ({ send: `RCPT TO:<${r}>\r\n`, until: (b) => /^\d{3} /m.test(b) })),
    { send: 'DATA\r\n', until: (b) => /^354 /m.test(b) },
    { send: `${body}${body.endsWith('\r\n') ? '' : '\r\n'}.\r\n`, until: (b) => (b.match(/^\d{3} /gm) || []).length >= rcpts.length },
    { send: 'QUIT\r\n', until: (b) => /^221 /m.test(b) },
  ];
  const replies = await converse(lmtpPort, steps);
  const final = replies[replies.length - 2] || '';
  if (/^[45]\d\d /m.test(final)) throw new Error(`LMTP: ${final.trim()}`);
}

export function startRelay({ port, lmtpPort, imapPort, host = '127.0.0.1' }) {
  const messages = [];
  const server = createServer((sock) => {
    let from = '';
    let rcpts = [];
    let authed = false;
    let mode = 'cmd';
    let data = [];
    let loginUser = null;
    const reply = (s) => sock.write(`${s}\r\n`);
    reply('220 relay.test ESMTP test relay');
    lineReader(sock, async (line) => {
      if (mode === 'data') {
        if (line === '.') {
          mode = 'cmd';
          const text = Buffer.from(data.join('\r\n') + '\r\n', 'binary').toString('utf8');
          data = [];
          messages.push({ from, to: rcpts.slice(), data: text });
          try {
            await lmtpDeliver(lmtpPort, from, rcpts, Buffer.from(text, 'utf8').toString('binary'));
            reply('250 2.0.0 delivered');
          } catch (e) {
            reply(`451 4.3.0 ${e.message}`);
          }
          rcpts = [];
        } else data.push(line.startsWith('..') ? line.slice(1) : line);
        return;
      }
      if (mode === 'login-user') { loginUser = Buffer.from(line, 'base64').toString(); mode = 'login-pass'; reply('334 UGFzc3dvcmQ6'); return; }
      if (mode === 'login-pass') {
        mode = 'cmd';
        authed = await imapLogin(imapPort, loginUser, Buffer.from(line, 'base64').toString()).catch(() => false);
        reply(authed ? '235 2.7.0 authenticated' : '535 5.7.8 authentication failed');
        return;
      }
      const upper = line.toUpperCase();
      if (upper.startsWith('EHLO') || upper.startsWith('HELO')) {
        sock.write('250-relay.test\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250-SIZE 52428800\r\n250 SMTPUTF8\r\n');
      } else if (upper.startsWith('AUTH PLAIN')) {
        const b64 = line.split(/\s+/)[2];
        const parts = Buffer.from(b64 || '', 'base64').toString().split('\0');
        authed = await imapLogin(imapPort, parts[1] || '', parts[2] || '').catch(() => false);
        reply(authed ? '235 2.7.0 authenticated' : '535 5.7.8 authentication failed');
      } else if (upper === 'AUTH LOGIN') {
        mode = 'login-user';
        reply('334 VXNlcm5hbWU6');
      } else if (upper.startsWith('MAIL FROM:')) {
        if (!authed) { reply('530 5.7.0 authentication required'); return; }
        from = line.slice(10).trim().replace(/^<|>.*$/g, '');
        reply('250 2.1.0 ok');
      } else if (upper.startsWith('RCPT TO:')) {
        rcpts.push(line.slice(8).trim().replace(/^<|>.*$/g, ''));
        reply('250 2.1.5 ok');
      } else if (upper === 'DATA') {
        mode = 'data';
        reply('354 go ahead');
      } else if (upper === 'RSET') {
        from = ''; rcpts = [];
        reply('250 ok');
      } else if (upper === 'NOOP') reply('250 ok');
      else if (upper === 'QUIT') { reply('221 bye'); sock.end(); }
      else reply('502 5.5.2 not implemented');
    });
    sock.on('error', () => {});
  });
  return new Promise((resolve) => server.listen(port, host, () => resolve({ messages, close: () => server.close() })));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const port = Number(process.env.SMTP_PORT || 3587);
  await startRelay({ port, lmtpPort: Number(process.env.LMTP_PORT || 3024), imapPort: Number(process.env.IMAP_PORT || 3143) });
  console.log(`SMTP relay on 127.0.0.1:${port} → LMTP ${process.env.LMTP_PORT || 3024}`);
}
