// Browser test of calls through the built-in SFU: three users join one call, and each must
// receive and decode live video from both others over a single peer connection to the
// server. Also checks that the server's forwarding counters grow, that a leaver's tile
// disappears for the rest, and simulcast: with small tiles a receiver gets a small layer, and
// when a tile grows the SFU switches it to a larger one without interrupting the video.
//
// Run against a server with the SFU enabled and a fresh data dir:
//   FERRITE_URL=http://127.0.0.1:8824 node sfu.mjs
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8824';
const T = 20000;
const errors = [];
const step = (s) => console.log(`- ${s}`);
const H = { 'X-Ferrite-Request': '1' };

const browser = await chromium.launch({
  args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'],
});

async function userPage(name) {
  const ctx = await browser.newContext({ viewport: { width: 1100, height: 760 } });
  await ctx.grantPermissions(['camera', 'microphone'], { origin: base });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: pageerror ${e.message}`));
  const r = await ctx.request.post(`${base}/api/auth/login`, { headers: H, data: { username: name, password: `${name}-password` } });
  if (!r.ok()) throw new Error(`login ${name}: ${r.status()}`);
  return { ctx, page, name };
}

async function api(ctx, method, path, data) {
  const r = await ctx.request.fetch(base + path, { method, headers: H, data });
  if (!r.ok()) throw new Error(`${method} ${path}: ${r.status()} ${await r.text()}`);
  return r.json();
}

/** Per remote session: {conn, framesDecoded, width} read from the transport and getStats. */
function callState(page) {
  return page.evaluate(async () => {
    const c = window.__ferriteCall;
    if (!c || !c.transport) return null;
    const out = { transport: c.transport.constructor.name, peers: {} };
    const pcs = new Set([...c.transport.peers.values()].map((p) => p.pc));
    const inbound = [];
    for (const pc of pcs) {
      const stats = await pc.getStats();
      stats.forEach((s) => { if (s.type === 'inbound-rtp' && s.kind === 'video') inbound.push(s); });
    }
    for (const [session, p] of c.transport.peers) {
      const video = p.stream.getVideoTracks()[0];
      const st = video && inbound.find((s) => s.trackIdentifier === video.id);
      out.peers[session] = { conn: p.pc.connectionState, framesDecoded: st ? st.framesDecoded || 0 : 0, width: st ? st.frameWidth || 0 : 0 };
    }
    out.tiles = [...document.querySelectorAll('.call-stage video')].filter((v) => v.videoWidth > 0).length;
    return out;
  });
}

let exitCode = 0;
try {
  step('setup: alice (admin), bob, carol and a group room');
  const setupCtx = await browser.newContext();
  const status = await (await setupCtx.request.get(`${base}/api/status`)).json();
  if (status.setup_needed) await api(setupCtx, 'POST', '/api/setup', { username: 'alice', display_name: 'Alice', password: 'alice-password' });
  else await api(setupCtx, 'POST', '/api/auth/login', { username: 'alice', password: 'alice-password' });
  for (const u of ['bob', 'carol']) {
    await setupCtx.request.post(`${base}/api/admin/users`, { headers: H, data: { username: u, display_name: u[0].toUpperCase() + u.slice(1), password: `${u}-password` } });
  }
  const room = await api(setupCtx, 'POST', '/api/talk/rooms', { kind: 'group', name: 'SFU check', invite: ['bob', 'carol'] });
  const token = room.token || room.room?.token;
  if (!token) throw new Error(`no room token in ${JSON.stringify(room)}`);

  const users = [];
  for (const n of ['alice', 'bob', 'carol']) users.push(await userPage(n));

  step('all three join the call with camera and microphone');
  for (const u of users) {
    await u.page.goto(`${base}/talk/${token}?call=1`);
    await u.page.waitForFunction(() => window.__ferriteCall && window.__ferriteCall.session, null, { timeout: T });
  }

  step('each participant uses the SFU transport and sees two remote peers connected');
  for (const u of users) {
    await u.page.waitForFunction(() => {
      const c = window.__ferriteCall;
      const peers = c && c.transport ? [...c.transport.peers.values()] : [];
      return peers.length === 2 && peers.every((p) => p.pc.connectionState === 'connected');
    }, null, { timeout: T, polling: 250 });
    const s = await callState(u.page);
    if (s.transport !== 'SfuTransport') throw new Error(`${u.name} used ${s.transport}`);
    const pcs = await u.page.evaluate(() => new Set([...window.__ferriteCall.transport.peers.values()].map((p) => p.pc)).size);
    if (pcs !== 1) throw new Error(`${u.name} has ${pcs} peer connections, expected 1`);
  }

  step('video from both others decodes continuously for every participant');
  for (const u of users) {
    await u.page.waitForFunction(() => [...document.querySelectorAll('.call-stage video')].filter((v) => v.videoWidth > 0).length >= 2, null, { timeout: T, polling: 250 });
  }
  const before = await Promise.all(users.map((u) => callState(u.page)));
  await new Promise((r) => setTimeout(r, 3000));
  const after = await Promise.all(users.map((u) => callState(u.page)));
  for (const [i, u] of users.entries()) {
    for (const [session, p] of Object.entries(after[i].peers)) {
      const gained = p.framesDecoded - (before[i].peers[session]?.framesDecoded || 0);
      console.log(`    ${u.name} ← ${session.slice(0, 8)}: ${gained} frames in 3s at ${p.width}px wide`);
      if (gained < 15) throw new Error(`${u.name} decoded only ${gained} frames from ${session} in 3s`);
    }
  }

  step('the server forwarded the media');
  const stats = await api(setupCtx, 'GET', '/api/admin/stats');
  console.log(`    sfu: ${JSON.stringify(stats.sfu)}`);
  if (!stats.sfu || stats.sfu.clients !== 3 || stats.sfu.packets_forwarded < 500) throw new Error(`unexpected SFU stats ${JSON.stringify(stats.sfu)}`);

  step('carol leaves: her tile disappears for alice and bob, who stay connected');
  await users[2].page.evaluate(() => window.__ferriteCall.leave());
  for (const u of users.slice(0, 2)) {
    await u.page.waitForFunction(() => window.__ferriteCall && window.__ferriteCall.transport.peers.size === 1, null, { timeout: T, polling: 250 });
  }
  const a1 = await callState(users[0].page);
  await new Promise((r) => setTimeout(r, 1500));
  const a2 = await callState(users[0].page);
  const [bobSession] = Object.keys(a2.peers);
  const gained = a2.peers[bobSession].framesDecoded - (a1.peers[bobSession]?.framesDecoded || 0);
  console.log(`    alice ← bob after carol left: ${gained} frames in 1.5s`);
  if (gained < 8) throw new Error('video stalled after a participant left');

  step('simulcast: alice enlarges her window, so bob\'s tile grows and the SFU switches her to a larger layer');
  const small = a2.peers[bobSession].width;
  await users[0].page.setViewportSize({ width: 1800, height: 1100 });
  await users[0].page.waitForFunction(([s, w]) => {
    const c = window.__ferriteCall;
    const v = c && c.transport.peers.get(s) && c.transport.peers.get(s).stream.getVideoTracks()[0];
    const tile = document.querySelector(`.call-tile[data-session="${s}"] video`);
    return v && tile && tile.videoWidth > w;
  }, [bobSession, small], { timeout: T, polling: 250 });
  const b1 = await callState(users[0].page);
  await new Promise((r) => setTimeout(r, 1500));
  const b2 = await callState(users[0].page);
  const grown = b2.peers[bobSession];
  const kept = grown.framesDecoded - b1.peers[bobSession].framesDecoded;
  console.log(`    alice ← bob: ${small}px wide in a small tile, ${grown.width}px after growing; ${kept} frames in 1.5s after the switch`);
  if (!(grown.width > small)) throw new Error(`layer did not switch up (${small} → ${grown.width})`);
  if (kept < 8) throw new Error('video stalled after the layer switch');
  const after2 = await api(setupCtx, 'GET', '/api/admin/stats');
  if (after2.sfu.clients !== 2) throw new Error(`expected 2 SFU clients after leave, got ${after2.sfu.clients}`);

  if (errors.length) throw new Error(`console errors: ${errors.join(' | ')}`);
  console.log(JSON.stringify({ ok: true, errors }, null, 2));
} catch (e) {
  console.error(e);
  console.log(JSON.stringify({ ok: false, errors }, null, 2));
  exitCode = 1;
} finally {
  await browser.close();
  process.exit(exitCode);
}
