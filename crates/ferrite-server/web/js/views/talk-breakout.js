// Breakout rooms in a conversation pane: the banner under the header (state, countdown,
// "go to your room", "ask for help"), the management panel for moderators (set up, assign,
// start/stop with a timer, broadcast, help requests), and the live moves: when breakout rooms
// start, change or stop, participants' clients follow (leaving one call and joining the next).
//
// The same component serves the main conversation and its breakout rooms; in a breakout room
// it shows the room's own banner and follows the main conversation's state.

import { live } from '../ws.js';
import { h, clear, icon, avatar, confirmDialog, toast, errorToast, spinner } from '../ui.js';

const MODES = { auto: 'Automatic', manual: 'Manual', free: 'Free choice' };
const MODE_HINTS = {
  auto: 'Participants are spread evenly; you can still move people.',
  manual: 'You assign people to rooms.',
  free: 'Participants pick a room themselves once the rooms start.',
};

/** "12:05" (or "1:02:05") until `ts`. */
function remaining(ts) {
  const s = Math.max(0, Math.round((new Date(ts).getTime() - Date.now()) / 1000));
  const hh = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, '0');
  return hh ? `${hh}:${String(mm).padStart(2, '0')}:${ss}` : `${mm}:${ss}`;
}

/**
 * opts: {
 *   room,                 // RoomView of the pane (main conversation or breakout room)
 *   self: {id, kind, name}, guest, tapi, base,
 *   isMod(), inCall(),    // the pane's live state
 *   onOpenRoom(token, {call, room}),   // move to another conversation
 *   onChange(),           // the panel or banner changed size or content
 * }
 * Returns {banner, paintPanel(container, close), load(), callEnded(reason), destroy()}.
 */
export function createBreakouts(opts) {
  const { room, self, tapi, base } = opts;
  const isChild = room.kind === 'breakout';
  const parentId = isChild ? room.parent_id : room.id;
  let view = null;
  let loaded = false;
  let panelHost = null;
  let panelClose = null;
  // A call of ours the server ended to move us (we rejoin a call in the next room).
  let carryCall = false;
  const banner = h('div.talk-breakout-banner', { hidden: true, role: 'status' });

  const mineIn = (v) => v && v.rooms.find((r) => r.members.some((m) => m.kind === self.kind && m.id === self.id));
  const running = () => view && view.status === 'running';
  const timerText = () => (running() && view.ends_at ? `${remaining(view.ends_at)} left` : '');

  async function load() {
    try {
      ({ breakouts: view } = await tapi.get(`${base}/breakouts`));
    } catch (_) {
      view = null; // the lobby holds us back, or no access: nothing to show
    }
    loaded = true;
    paint();
  }

  function go(r, why) {
    const call = carryCall || opts.inCall();
    carryCall = false;
    if (why) toast(why, 'info', { timeout: 3500 });
    opts.onOpenRoom(r.token, { call, room: r });
  }

  /** Follows a live change: participants move into, between and out of breakout rooms. */
  function follow(prev, next) {
    const mine = mineIn(next);
    const parent = { token: next.token, id: next.room_id, kind: 'group', name: room.parent_name };
    if (!isChild) {
      if (opts.isMod() || next.status !== 'running' || !mine) return;
      const before = prev && prev.status === 'running' ? mineIn(prev) : null;
      if (!before || before.token !== mine.token) go({ ...mine, kind: 'breakout', parent_id: next.room_id, parent_token: next.token, parent_name: room.display_name }, `Moving you to ${mine.name}`);
      return;
    }
    const here = next.rooms.find((r) => r.id === room.id);
    if (next.status !== 'running' || !here) {
      if (prev && prev.status === 'running') go(parent, 'The breakout rooms ended: back to the main conversation');
      return;
    }
    if (opts.isMod()) return;
    if (mine && mine.id !== room.id) go({ ...mine, kind: 'breakout', parent_id: next.room_id, parent_token: next.token, parent_name: room.parent_name }, `Moving you to ${mine.name}`);
    else if (!mine) go(parent, 'You were moved back to the main conversation');
  }

  // ---- banner -----------------------------------------------------------------------------
  function paint() {
    paintBanner();
    if (panelHost) paintPanel(panelHost, panelClose);
    opts.onChange?.();
  }

  function paintBanner() {
    const parts = bannerParts();
    banner.hidden = !parts;
    if (parts) clear(banner).append(...parts);
  }

  function bannerParts() {
    const timer = () => h('span.bo-timer', timerText());
    if (isChild) {
      const back = h('button.btn.sm', { onclick: () => go({ token: room.parent_token, kind: 'group', name: room.parent_name }) }, icon('arrow-left', 'sm'), 'Main room');
      if (!view || !running()) {
        return [icon('canvas', 'sm'), h('div.grow.ellipsis', h('strong', room.display_name), ' · This breakout room has ended; its chat stays readable.'), back];
      }
      const here = view.rooms.find((r) => r.id === room.id);
      const helpOn = !!here?.help_requested_at;
      const help = opts.isMod()
        ? (helpOn ? h('button.btn.sm', { onclick: () => askHelp(room.token, false) }, icon('check', 'sm'), 'Dismiss help request') : '')
        : h(`button.btn.sm${helpOn ? '' : '.primary'}`, { onclick: () => askHelp(room.token, !helpOn), 'aria-pressed': String(helpOn) },
          icon('hand', 'sm'), helpOn ? 'Cancel help request' : 'Ask for help');
      return [h('span.bo-dot', icon('canvas', 'sm')),
        h('div.grow.ellipsis', h('strong', room.display_name), ` · Breakout room of ${room.parent_name || 'the main conversation'}`, view.ends_at ? ' · ' : '', view.ends_at ? timer() : '',
          helpOn ? h('span.pill.warn.bo-help', 'Help requested') : ''),
        help, back];
    }
    if (!view || !view.mode) return null;
    const mine = mineIn(view);
    if (opts.isMod()) {
      const helps = view.rooms.filter((r) => r.help_requested_at).length;
      const n = view.rooms.length;
      return [h('span.bo-dot', icon('canvas', 'sm')),
        h('div.grow.ellipsis', h('strong', running() ? 'Breakout rooms running' : `${n} breakout ${n === 1 ? 'room' : 'rooms'} ready`),
          ` · ${MODES[view.mode]}`, running() && view.ends_at ? ' · ' : '', running() && view.ends_at ? timer() : '',
          helps ? h('span.pill.warn.bo-help', `${helps} need${helps === 1 ? 's' : ''} help`) : ''),
        h('button.btn.sm', { onclick: () => opts.openPanel() }, 'Manage'),
        running() ? h('button.btn.sm.danger', { onclick: stop }, 'Stop') : h('button.btn.sm.primary', { onclick: () => start() }, 'Start')];
    }
    if (!running()) return null;
    if (mine) {
      return [h('span.bo-dot', icon('canvas', 'sm')), h('div.grow.ellipsis', h('strong', 'Breakout rooms are running'), ` · You are in ${mine.name}`, view.ends_at ? ' · ' : '', view.ends_at ? timer() : ''),
        h('button.btn.sm.primary', { onclick: () => go({ ...mine, kind: 'breakout', parent_id: view.room_id, parent_token: view.token, parent_name: room.display_name }) }, `Go to ${mine.name}`)];
    }
    if (view.mode === 'free') {
      return [h('span.bo-dot', icon('canvas', 'sm')), h('div.grow.ellipsis', h('strong', 'Breakout rooms are open'), ' · pick a room'),
        h('div.bo-choices', view.rooms.map((r) => h('button.btn.sm', { onclick: () => choose(r) }, `${r.name} (${r.members.length})`)))];
    }
    return [h('span.bo-dot', icon('canvas', 'sm')), h('div.grow.ellipsis', h('strong', 'Breakout rooms are running'), ' · You stay in the main room')];
  }

  // ---- actions ---------------------------------------------------------------------------------
  async function act(fn) {
    try {
      const res = await fn();
      if (res && res.breakouts) { view = res.breakouts; paint(); }
      return res;
    } catch (e) { errorToast(e); return null; }
  }

  const configure = (mode, count) => act(() => tapi.post(`${base}/breakouts`, { mode, count }));
  const assign = (m, roomToken) => act(() => tapi.post(`${base}/breakouts/assign`, { assignments: [{ kind: m.kind, id: m.id, room: roomToken || null }] }));
  const start = (minutes) => act(() => tapi.post(`${base}/breakouts/start`, { minutes: minutes || null }));
  async function stop() {
    if (!await confirmDialog('Stop breakout rooms', 'Bring everyone back to the main conversation? Breakout chats stay readable.', { confirm: 'Stop' })) return;
    act(() => tapi.post(`${base}/breakouts/stop`));
  }
  async function removeAll() {
    if (!await confirmDialog('Remove breakout rooms', 'Remove the breakout rooms and their chats for everyone?', { confirm: 'Remove', danger: true })) return;
    act(() => tapi.del(`${base}/breakouts`));
  }
  async function choose(r) {
    const res = await act(() => tapi.post(`${base}/breakouts/choose`, { room: r.token }));
    if (res) go({ ...r, kind: 'breakout', parent_id: view.room_id, parent_token: view.token, parent_name: room.display_name });
  }
  async function askHelp(childToken, on) {
    try {
      await tapi.post(`/api/talk/rooms/${encodeURIComponent(childToken)}/help`, { on });
      if (on && !opts.isMod()) toast('The moderators were asked to come by', 'ok', { timeout: 3000 });
    } catch (e) { errorToast(e); }
  }
  async function broadcast(ta) {
    const body = ta.value.trim();
    if (!body) { ta.focus(); return; }
    const res = await act(() => tapi.post(`${base}/breakouts/broadcast`, { body }));
    if (res) { ta.value = ''; delete drafts.broadcast; toast(`Sent to ${res.rooms} ${res.rooms === 1 ? 'room' : 'rooms'}`, 'ok', { timeout: 2500 }); }
  }

  // ---- panel ----------------------------------------------------------------------------------
  // The panel is rebuilt on every live change; what the moderator is typing survives that.
  const drafts = {};
  function kept(el, key) {
    el.dataset.keep = key;
    if (key in drafts) el.value = drafts[key];
    const save = () => { drafts[key] = el.value; };
    el.addEventListener('input', save);
    el.addEventListener('change', save);
    return el;
  }

  function setupForm(again) {
    const count = h('input', { type: 'number', min: 1, max: 20, value: view && view.rooms.length ? view.rooms.length : 2, id: 'bo-count', 'aria-label': 'Number of rooms' });
    const mode = h('select', { id: 'bo-mode', 'aria-label': 'Assignment' }, Object.entries(MODES).map(([v, label]) => h('option', { value: v }, label)));
    if (view && view.mode) mode.value = view.mode;
    kept(count, 'count');
    kept(mode, 'mode');
    const hint = h('p.small.muted', MODE_HINTS[mode.value]);
    mode.addEventListener('change', () => { hint.textContent = MODE_HINTS[mode.value]; });
    return h('form.bo-setup', {
      onsubmit: async (e) => {
        e.preventDefault();
        const n = Number(count.value);
        if (!(n >= 1 && n <= 20)) { toast('Between 1 and 20 rooms', 'warn'); return; }
        if (again && !await confirmDialog('Set up again', 'This replaces the current breakout rooms and their chats.', { confirm: 'Replace', danger: true })) return;
        delete drafts.count;
        delete drafts.mode;
        configure(mode.value, n);
      },
    },
    h('div.bo-setup-row', h('div', h('label', { for: 'bo-count' }, 'Rooms'), count), h('div.grow', h('label', { for: 'bo-mode' }, 'Assignment'), mode)),
    hint,
    h('button.btn.primary.sm', { type: 'submit' }, icon('canvas', 'sm'), again ? 'Set up again' : 'Set up breakout rooms'));
  }

  function memberRow(m, current, mod) {
    const pick = mod ? h('select.bo-move', { 'aria-label': `Room for ${m.name}`, onchange: (e) => assign(m, e.target.value) },
      h('option', { value: '' }, 'Main room'),
      view.rooms.map((r) => h('option', { value: r.token, selected: current === r.token }, r.name))) : '';
    return h('div.bo-member', avatar(m.name.replace(/\s*\(guest\)$/, ''), { size: 'sm', seed: m.id, online: m.online }),
      h('span.grow.ellipsis', m.name), pick);
  }

  function paintPanel(container, close) {
    const focused = container.contains(document.activeElement) ? document.activeElement.dataset.keep : null;
    paintPanelContent(container, close);
    if (focused) container.querySelector(`[data-keep="${focused}"]`)?.focus();
  }

  function paintPanelContent(container, close) {
    panelHost = container;
    panelClose = close;
    const mod = opts.isMod();
    const head = h('div.talk-side-head', h('h3.grow', 'Breakout rooms'),
      h('button.btn.ghost.icon.sm', { 'aria-label': 'Close breakout rooms', onclick: close }, icon('x')));
    const content = h('div.bo-panel');
    if (!loaded) {
      clear(container).append(head, spinner());
      return;
    }
    if (!view || !view.mode) {
      content.append(mod
        ? h('div', h('p.small.muted', 'Split this conversation into smaller rooms with their own chat and call, then bring everyone back.'), setupForm(false))
        : h('p.small.muted', 'There are no breakout rooms in this conversation.'));
      clear(container).append(head, content);
      return;
    }
    const status = running()
      ? h('div.bo-status.on', icon('canvas', 'sm'), h('strong', 'Running'), view.ends_at ? h('span.bo-timer', timerText()) : '', h('span.muted', `· ${MODES[view.mode]}`))
      : h('div.bo-status', icon('canvas', 'sm'), h('strong', 'Not started'), h('span.muted', `· ${MODES[view.mode]}`));
    content.append(status);
    if (mod && running()) {
      const ta = kept(h('textarea', { rows: 2, 'aria-label': 'Message to all breakout rooms', placeholder: 'Message to all rooms…' }), 'broadcast');
      ta.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); broadcast(ta); } });
      content.append(h('div.bo-broadcast', ta, h('button.btn.sm.primary', { onclick: () => broadcast(ta) }, icon('send', 'sm'), 'Send to all rooms')));
    }
    for (const r of view.rooms) {
      const help = r.help_requested_at;
      content.append(h(`section.bo-room${help ? '.help' : ''}`, { dataset: { token: r.token } },
        h('div.bo-room-head',
          h('strong.grow.ellipsis', r.name),
          r.call_active ? h('span.bo-call', { title: 'Call in progress' }, icon('phone', 'sm')) : '',
          help ? h('span.pill.warn', { title: 'Asked for help' }, icon('hand', 'sm'), 'Help') : '',
          h('span.small.muted', String(r.members.length)),
          mod || running() ? h('button.btn.sm', { onclick: () => go({ ...r, kind: 'breakout', parent_id: view.room_id, parent_token: view.token, parent_name: room.display_name }) }, mod ? 'Visit' : 'Open') : ''),
        help && mod ? h('div.bo-help-row.small', `Help requested at ${new Date(help).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}`,
          h('button.btn.ghost.sm', { onclick: () => askHelp(r.token, false) }, 'Dismiss')) : '',
        r.members.length ? r.members.map((m) => memberRow(m, r.token, mod)) : h('div.small.muted.bo-empty', 'Nobody yet')));
    }
    if (view.unassigned.length) {
      content.append(h('section.bo-room.bo-unassigned', h('div.bo-room-head', h('strong.grow', 'Main room'), h('span.small.muted', String(view.unassigned.length))),
        view.unassigned.map((m) => memberRow(m, '', mod))));
    }
    if (mod) {
      if (running()) {
        content.append(h('div.bo-actions', h('button.btn.danger.sm', { onclick: stop }, icon('phone-off', 'sm'), 'Stop breakout rooms')));
      } else {
        const minutes = kept(h('input', { type: 'number', min: 1, max: 1440, placeholder: 'No limit', id: 'bo-minutes', 'aria-label': 'Timer in minutes' }), 'minutes');
        content.append(
          h('div.bo-actions',
            h('div.bo-start', h('label', { for: 'bo-minutes' }, 'Timer (minutes)'), minutes),
            h('button.btn.primary.sm', { onclick: () => { delete drafts.minutes; start(Number(minutes.value) || null); } }, icon('play', 'sm'), 'Start breakout rooms')),
          h('details.bo-more', h('summary.small', 'Set up again or remove'), setupForm(true),
            h('button.btn.ghost.sm.danger', { onclick: removeAll }, icon('trash', 'sm'), 'Remove breakout rooms')));
      }
    }
    clear(container).append(head, content);
  }

  // ---- live ------------------------------------------------------------------------------------
  const ticker = setInterval(() => {
    if (!running() || !view.ends_at) return;
    const t = timerText();
    for (const el of document.querySelectorAll('.bo-timer')) if (banner.contains(el) || panelHost?.contains(el)) el.textContent = t;
  }, 1000);
  const offs = [
    live.on('talk.breakouts', (ev) => {
      if (ev.room_id !== parentId) return;
      const prev = view;
      view = ev.breakouts;
      paint();
      follow(prev, view);
    }),
  ];

  return {
    banner,
    load,
    /** Loads the state unless it already arrived (live events keep it current). */
    ensureLoaded() { if (!loaded) load(); },
    paintPanel,
    closePanel() { panelHost = null; panelClose = null; },
    /** The server ended our call to move us: rejoin a call wherever we go next. */
    callEnded() { carryCall = true; },
    destroy() {
      clearInterval(ticker);
      offs.forEach((off) => off());
    },
  };
}
