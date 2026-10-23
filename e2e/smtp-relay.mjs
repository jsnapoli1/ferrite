// A tiny SMTP server for the browser tests that delivers into Dovecot over LMTP, for test
// instances run without Postfix (mail-server.sh with POSTFIX=0). It accepts any login and
// delivers mail for @example.test (the test mailboxes) through Dovecot's LMTP listener;
// mail for anyone else is accepted and dropped. Plain connections on localhost only.
//
//   node smtp-relay.mjs SMTP_PORT LMTP_PORT        # e.g. 3315 3312
import { createServer, connect } from 'node:net';

const [smtpPort, lmtpPort] = process.argv.slice(2).map(Number);
if (!smtpPort || !lmtpPort) {
  console.error('usage: node smtp-relay.mjs SMTP_PORT LMTP_PORT');
  process.exit(2);
}
const LOCAL = /@example\.test$/i;

/** Hands a message to Dovecot over LMTP; resolves when it answered for every recipient. */
function lmtpDeliver(from, rcpts, data) {
  return new Promise((resolve, reject) => {
    const sock = connect(lmtpPort, '127.0.0.1');
    const script = ['LHLO relay.test', `MAIL FROM:<${from}>`, ...rcpts.map((r) => `RCPT TO:<${r}>`), 'DATA'];
    let step = -1;
    let buf = '';
    let replies = 0;
    let sentData = false;
    sock.setEncoding('utf8');
    sock.on('data', (chunk) => {
      buf += chunk;
      for (;;) {
        const m = buf.match(/^(\d{3}) .*\r\n/m);
        if (!m) return;
        buf = buf.slice(buf.indexOf(m[0]) + m[0].length);
        const code = Number(m[1]);
        if (code >= 400) { sock.destroy(); reject(new Error(`LMTP ${m[0].trim()}`)); return; }
        if (sentData) {
          // One reply per recipient after the data.
          if (++replies === rcpts.length) { sock.end('QUIT\r\n'); resolve(); return; }
          continue;
        }
        step++;
        if (step < script.length) sock.write(`${script[step]}\r\n`);
        else {
          sentData = true;
          const body = data.replace(/\r?\n/g, '\r\n').replace(/^\./gm, '..');
          sock.write(`${body}${body.endsWith('\r\n') ? '' : '\r\n'}.\r\n`);
        }
      }
    });
    sock.on('error', reject);
  });
}

createServer((sock) => {
  sock.setEncoding('utf8');
  let buf = '';
  let mode = 'cmd';
  let from = '';
  let rcpts = [];
  let data = [];
  const reply = (s) => sock.write(`${s}\r\n`);
  reply('220 relay.test ESMTP test relay');
  // Lines are handled in order, one delivery at a time.
  let chain = Promise.resolve();
  sock.on('data', (chunk) => {
    buf += chunk;
    chain = chain.then(handle);
  });
  async function handle() {
    let i;
    while ((i = buf.indexOf('\r\n')) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 2);
      if (mode === 'data') {
        if (line === '.') {
          mode = 'cmd';
          const local = rcpts.filter((r) => LOCAL.test(r));
          const message = data.join('\r\n');
          data = [];
          try {
            if (local.length) await lmtpDeliver(from, local, message);
            reply('250 2.0.0 queued');
          } catch (e) {
            reply(`451 4.3.0 ${e.message}`);
          }
        } else data.push(line.startsWith('..') ? line.slice(1) : line);
        continue;
      }
      if (mode === 'auth-user') { mode = 'auth-pass'; reply('334 UGFzc3dvcmQ6'); continue; }
      if (mode === 'auth-pass') { mode = 'cmd'; reply('235 2.7.0 Authentication successful'); continue; }
      const cmd = line.slice(0, 4).toUpperCase();
      if (cmd === 'EHLO') { sock.write('250-relay.test\r\n250-AUTH PLAIN LOGIN\r\n250-8BITMIME\r\n250 SIZE 52428800\r\n'); }
      else if (cmd === 'HELO') reply('250 relay.test');
      else if (cmd === 'AUTH') {
        const [, mech, initial] = line.split(' ');
        if (mech.toUpperCase() === 'LOGIN') { mode = 'auth-user'; reply('334 VXNlcm5hbWU6'); } else if (initial) reply('235 2.7.0 Authentication successful');
        else { mode = 'auth-pass'; reply('334 '); }
      } else if (cmd === 'MAIL') { from = (line.match(/<([^>]*)>/) || [])[1] || ''; rcpts = []; reply('250 2.1.0 Ok'); }
      else if (cmd === 'RCPT') { rcpts.push((line.match(/<([^>]*)>/) || [])[1] || ''); reply('250 2.1.5 Ok'); }
      else if (cmd === 'DATA') { mode = 'data'; data = []; reply('354 End data with <CR><LF>.<CR><LF>'); }
      else if (cmd === 'RSET') { from = ''; rcpts = []; reply('250 2.0.0 Ok'); }
      else if (cmd === 'NOOP') reply('250 2.0.0 Ok');
      else if (cmd === 'QUIT') { reply('221 2.0.0 Bye'); sock.end(); }
      else reply('502 5.5.2 Command not recognized');
    }
  }
  sock.on('error', () => {});
}).listen(smtpPort, '127.0.0.1', () => console.log(`SMTP relay on 127.0.0.1:${smtpPort} -> LMTP 127.0.0.1:${lmtpPort}`));
