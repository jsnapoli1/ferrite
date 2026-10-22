// Call quality probe: N people in one call; reports, per sender, the camera's capture rate and
// what the encoder actually sends (fps, resolution, bitrate, and WebRTC's own reason when it
// holds back), and, per receiver, the decoded frame rate of each remote participant.
//
//   FERRITE_URL=http://127.0.0.1:8850 [PEOPLE=3] [SECONDS=6] [CAMERA=/path/cam.y4m] [MIN_FPS=25] node callfps.mjs
//
// CAMERA feeds Chromium a Y4M file as the webcam (e.g. 720p60 from ffmpeg's testsrc2); without
// it Chromium's built-in fake camera is used. With MIN_FPS the run fails if any receiver
// decodes fewer frames per second than that from any sender.
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8850';
const people = Number(process.env.PEOPLE || 3);
const seconds = Number(process.env.SECONDS || 6);
const minFps = process.env.MIN_FPS ? Number(process.env.MIN_FPS) : null;
const H = { 'X-Ferrite-Request': '1' };
const T = 30000;
const errors = [];

const args = ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream'];
if (process.env.CAMERA) args.push(`--use-file-for-fake-video-capture=${process.env.CAMERA}`);
const browser = await chromium.launch({ args });

async function api(ctx, method, path, data) {
  const r = await ctx.request.fetch(base + path, { method, headers: H, data });
  if (!r.ok()) throw new Error(`${method} ${path}: ${r.status()} ${await r.text()}`);
  return r.json();
}

/** Sender and receiver statistics of this page's call, summed over its connections. */
function snapshot(page) {
  return page.evaluate(async () => {
    const c = window.__ferriteCall;
    const pcs = [...new Set([...c.transport.peers.values()].map((p) => p.pc))];
    const out = { transport: c.transport.constructor.name, outbound: [], inbound: {}, capture: null, available: null };
    const track = c.stream && c.stream.getVideoTracks()[0];
    if (track) out.capture = track.getSettings();
    for (const pc of pcs) {
      const stats = await pc.getStats();
      stats.forEach((s) => {
        if (s.type === 'outbound-rtp' && s.kind === 'video') {
          out.outbound.push({ frames: s.framesEncoded || 0, sent: s.framesSent || 0, bytes: s.bytesSent || 0, w: s.frameWidth, h: s.frameHeight, reason: s.qualityLimitationReason, target: s.targetBitrate, encoder: s.encoderImplementation, rid: s.rid });
        }
        if (s.type === 'candidate-pair' && s.nominated && s.availableOutgoingBitrate) out.available = s.availableOutgoingBitrate;
      });
      for (const [session, p] of c.transport.peers) {
        if (p.pc !== pc) continue;
        const v = p.stream.getVideoTracks()[0];
        stats.forEach((s) => {
          if (s.type === 'inbound-rtp' && s.kind === 'video' && v && s.trackIdentifier === v.id) {
            out.inbound[session] = { frames: s.framesDecoded || 0, w: s.frameWidth, h: s.frameHeight, dropped: s.framesDropped || 0, codec: s.codecId };
          }
        });
      }
    }
    return out;
  });
}

let exitCode = 0;
try {
  const admin = await browser.newContext();
  const status = await (await admin.request.get(`${base}/api/status`)).json();
  if (status.setup_needed) await api(admin, 'POST', '/api/setup', { username: 'p0', display_name: 'P0', password: 'p0-password' });
  else await api(admin, 'POST', '/api/auth/login', { username: 'p0', password: 'p0-password' });
  const names = Array.from({ length: people }, (_, i) => `p${i}`);
  for (const n of names.slice(1)) {
    await admin.request.post(`${base}/api/admin/users`, { headers: H, data: { username: n, display_name: n.toUpperCase(), password: `${n}-password` } });
  }
  const { room } = await api(admin, 'POST', '/api/talk/rooms', { kind: 'group', name: 'FPS probe', invite: names.slice(1) });
  const pages = [];
  for (const n of names) {
    const ctx = await browser.newContext({ viewport: { width: 960, height: 640 } });
    await ctx.grantPermissions(['camera', 'microphone'], { origin: base });
    const page = await ctx.newPage();
    page.on('console', (m) => { if (m.type() === 'error') errors.push(`${n}: ${m.text()}`); });
    page.on('pageerror', (e) => errors.push(`${n}: ${e.message}`));
    await api(ctx, 'POST', '/api/auth/login', { username: n, password: `${n}-password` });
    await page.goto(`${base}/talk/${room.token}?call=1`);
    pages.push({ n, page });
  }
  for (const { page } of pages) {
    await page.waitForFunction((want) => {
      const c = window.__ferriteCall;
      const peers = c && c.transport ? [...c.transport.peers.values()] : [];
      return peers.length === want && peers.every((p) => p.pc.connectionState === 'connected')
        && [...document.querySelectorAll('.call-stage video')].filter((v) => v.videoWidth > 0).length >= want;
    }, people - 1, { timeout: T, polling: 250 });
  }
  // Let bandwidth estimation and encoders ramp up before measuring.
  await new Promise((r) => setTimeout(r, Number(process.env.WARMUP || 4) * 1000));
  const a = await Promise.all(pages.map((p) => snapshot(p.page)));
  await new Promise((r) => setTimeout(r, seconds * 1000));
  const b = await Promise.all(pages.map((p) => snapshot(p.page)));
  const sessions = await Promise.all(pages.map((p) => p.page.evaluate(() => window.__ferriteCall.session)));
  const nameOf = (s) => names[sessions.indexOf(s)] || s.slice(0, 6);
  const report = { transport: b[0].transport, people, seconds, camera: process.env.CAMERA ? 'file' : 'chromium-fake', senders: {}, receivers: {} };
  let worst = Infinity;
  pages.forEach(({ n }, i) => {
    // One entry per encoding: simulcast layers (rid) through the SFU, or one per peer in a mesh.
    const o = b[i].outbound;
    const layers = o.map((x, k) => {
      const before = a[i].outbound[k] || { frames: 0, bytes: 0 };
      return {
        layer: x.rid || `peer${k + 1}`,
        fps: +((x.frames - before.frames) / seconds).toFixed(1),
        kbps: Math.round(((x.bytes - before.bytes) * 8) / seconds / 1000),
        resolution: x.w ? `${x.w}x${x.h}` : 'paused',
      };
    }).sort((x, y) => x.layer.localeCompare(y.layer));
    report.senders[n] = {
      capture: b[i].capture ? `${b[i].capture.width}x${b[i].capture.height}@${Math.round(b[i].capture.frameRate)}` : null,
      layers,
      limited_by: [...new Set(o.map((x) => x.reason))].join(','),
      encoder: o[0] && o[0].encoder,
      available_kbps: b[i].available ? Math.round(b[i].available / 1000) : null,
    };
    report.receivers[n] = {};
    for (const [s, v] of Object.entries(b[i].inbound)) {
      const fps = (v.frames - ((a[i].inbound[s] && a[i].inbound[s].frames) || 0)) / seconds;
      worst = Math.min(worst, fps);
      report.receivers[n][nameOf(s)] = { fps: +fps.toFixed(1), resolution: `${v.w}x${v.h}`, dropped: v.dropped };
    }
  });
  report.worst_receiver_fps = +worst.toFixed(1);
  console.log(JSON.stringify(report, null, 2));
  if (minFps !== null && worst < minFps) throw new Error(`a receiver decoded only ${worst.toFixed(1)} fps (< ${minFps})`);
  if (errors.length) throw new Error(`console errors: ${errors.join(' | ')}`);
} catch (e) {
  console.error(e);
  exitCode = 1;
} finally {
  await browser.close();
  process.exit(exitCode);
}
