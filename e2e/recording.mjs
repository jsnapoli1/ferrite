// Browser test of server-side call recording through the SFU: only moderators can record
// (the server refuses others), everyone sees the indicator, stopping saves one WebM file per
// person into the moderator's Files and shares them into the chat, the files hold real VP8
// video and Opus audio of the right length (checked with ffprobe), and a recording stops by
// itself when the call ends.
//
//   FERRITE_URL=http://127.0.0.1:8852 [CAMERA=/path/cam.y4m] node recording.mjs   (fresh data dir, SFU on)
import { chromium } from 'playwright';
import { execFileSync } from 'node:child_process';
import { writeFileSync, mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8852';
const H = { 'X-Ferrite-Request': '1' };
const T = 20000;
const errors = [];
const step = (s) => console.log(`- ${s}`);
const args = ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'];
if (process.env.CAMERA) args.push(`--use-file-for-fake-video-capture=${process.env.CAMERA}`);
const browser = await chromium.launch({ args });

async function api(ctx, method, path, data) {
  const r = await ctx.request.fetch(base + path, { method, headers: H, data });
  if (!r.ok()) throw new Error(`${method} ${path}: ${r.status()} ${await r.text()}`);
  return r.json();
}

async function person(name) {
  const ctx = await browser.newContext({ viewport: { width: 1200, height: 800 } });
  await ctx.grantPermissions(['camera', 'microphone'], { origin: base });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  await api(ctx, 'POST', '/api/auth/login', { username: name, password: `${name}-password` });
  return { ctx, page, name };
}

const inCall = (page, others) => page.waitForFunction((n) => {
  const c = window.__ferriteCall;
  const peers = c && c.transport ? [...c.transport.peers.values()] : [];
  return peers.length === n && peers.every((p) => p.pc.connectionState === 'connected')
    && [...document.querySelectorAll('.call-stage video')].filter((v) => v.videoWidth > 0).length >= n;
}, others, { timeout: T, polling: 250 });

function probe(buf) {
  const dir = mkdtempSync(join(tmpdir(), 'ferrite-rec-'));
  const file = join(dir, 'r.webm');
  writeFileSync(file, buf);
  const out = execFileSync('ffprobe', ['-v', 'error', '-count_frames', '-show_entries', 'stream=codec_name,nb_read_frames,width,height:format=duration', '-of', 'json', file]);
  // The whole file must decode without errors, and seeking must work.
  execFileSync('ffmpeg', ['-v', 'error', '-i', file, '-f', 'null', '-']);
  execFileSync('ffmpeg', ['-v', 'error', '-ss', '2', '-i', file, '-frames:v', '3', '-f', 'null', '-']);
  return JSON.parse(out.toString());
}

let exitCode = 0;
try {
  step('setup: alice (admin, owner of the room) and bob (member)');
  const admin = await browser.newContext();
  await api(admin, 'POST', '/api/setup', { username: 'alice', display_name: 'Alice Archer', password: 'alice-password' });
  await admin.request.post(`${base}/api/admin/users`, { headers: H, data: { username: 'bob', display_name: 'Bob Builder', password: 'bob-password' } });
  const { room } = await api(admin, 'POST', '/api/talk/rooms', { kind: 'group', name: 'Weekly sync', invite: ['bob'] });
  const alice = await person('alice');
  const bob = await person('bob');

  step('both join the call through the SFU');
  for (const p of [alice, bob]) await p.page.goto(`${base}/talk/${room.token}?call=1`);
  for (const p of [alice, bob]) await inCall(p.page, 1);
  if (await alice.page.evaluate(() => window.__ferriteCall.transport.constructor.name) !== 'SfuTransport') throw new Error('not using the SFU');

  step('only the moderator sees the record button; the server refuses bob');
  const recBtn = (p) => p.page.locator('.call-controls button[aria-label="Start recording"], .call-controls button[aria-label="Stop recording"]');
  if (!(await recBtn(alice).isVisible())) throw new Error('alice has no record button');
  if (await recBtn(bob).isVisible()) throw new Error('bob should not see the record button');
  const refused = await bob.page.evaluate(async () => {
    const { live } = await import('/js/ws.js');
    try {
      await live.request({ type: 'call.record', room_id: window.__ferriteCall.roomId, action: 'start' }, 'call.recording', 5000);
      return 'started';
    } catch (e) {
      return e.message;
    }
  });
  if (refused === 'started') throw new Error('bob could start a recording');
  console.log(`    bob's attempt: ${refused}`);

  step('alice starts recording: both see the indicator and the chat notice');
  await recBtn(alice).click();
  for (const p of [alice, bob]) {
    await p.page.locator('.call-rec', { hasText: 'Recording' }).waitFor({ state: 'visible', timeout: T });
    await p.page.locator('.msg', { hasText: 'Alice Archer started recording the call' }).first().waitFor({ timeout: T });
  }
  const seconds = 6;
  await new Promise((r) => setTimeout(r, seconds * 1000));

  step('alice stops: the indicator goes away and one file per person is shared into the chat');
  await alice.page.locator('.call-controls button[aria-label="Stop recording"]').click();
  for (const p of [alice, bob]) {
    await p.page.locator('.call-rec').waitFor({ state: 'hidden', timeout: T });
    await p.page.locator('.msg', { hasText: /stopped the recording; 2 files saved/ }).first().waitFor({ timeout: 60000 });
  }
  const cards = bob.page.locator('.file-card', { hasText: '.webm' });
  await cards.nth(1).waitFor({ timeout: T });
  const msgs = await api(alice.ctx, 'GET', `/api/talk/rooms/${room.token}/messages?limit=50`);
  const files = msgs.messages.filter((m) => m.meta && /\.webm$/.test(m.meta.name || '')).map((m) => m.meta);
  if (files.length !== 2) throw new Error(`expected 2 recording files, got ${JSON.stringify(files)}`);

  step('the files are in alice\'s Files under Talk/Recordings and play back');
  const folder = await api(alice.ctx, 'GET', `/api/files/resolve?path=${encodeURIComponent('/Talk/Recordings')}`);
  const sessions = await api(alice.ctx, 'GET', `/api/files/${folder.entry.id}/children`);
  const recFolder = sessions.entries.find((c) => c.name.startsWith('Weekly sync'));
  if (!recFolder) throw new Error('no recording folder');
  const inFolder = await api(alice.ctx, 'GET', `/api/files/${recFolder.id}/children`);
  const names = inFolder.entries.map((c) => c.name).sort();
  console.log(`    ${recFolder.name}: ${names.join(', ')}`);
  if (names.join('|') !== 'Alice Archer.webm|Bob Builder.webm') throw new Error(`unexpected files ${names}`);
  for (const f of inFolder.entries) {
    // Bob can open it too, through the conversation share.
    const r = await bob.ctx.request.get(`${base}/api/files/${f.id}/download`);
    if (!r.ok()) throw new Error(`bob cannot download ${f.name}: ${r.status()}`);
    const info = probe(await r.body());
    const v = info.streams.find((s) => s.codec_name === 'vp8');
    const a = info.streams.find((s) => s.codec_name === 'opus');
    const duration = Number(info.format.duration);
    console.log(`    ${f.name}: ${duration.toFixed(1)} s, vp8 ${v && v.width}x${v && v.height} ${v && v.nb_read_frames} frames, opus ${a && a.nb_read_frames} packets`);
    if (!v || Number(v.nb_read_frames) < seconds * 10) throw new Error(`${f.name}: too little video`);
    if (!a || Number(a.nb_read_frames) < seconds * 40) throw new Error(`${f.name}: too little audio`);
    if (Math.abs(duration - seconds) > 2.5) throw new Error(`${f.name}: duration ${duration} for a ${seconds} s recording`);
  }

  step('a recording stops and saves by itself when everyone leaves');
  await recBtn(alice).click();
  await alice.page.locator('.call-rec').waitFor({ state: 'visible', timeout: T });
  await new Promise((r) => setTimeout(r, 3000));
  for (const p of [bob, alice]) await p.page.evaluate(() => window.__ferriteCall.leave());
  await alice.page.locator('.msg', { hasText: /The recording stopped; 2 files saved/ }).first().waitFor({ timeout: 60000 });
  const stats = await api(admin, 'GET', '/api/admin/stats');
  if (stats.sfu.recordings !== 0) throw new Error(`SFU still recording: ${JSON.stringify(stats.sfu)}`);

  await alice.page.screenshot({ path: `${process.env.SHOTS || '/tmp'}/recording-chat.png` });
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
