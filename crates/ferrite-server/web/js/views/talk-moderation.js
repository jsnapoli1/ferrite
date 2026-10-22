// Talk moderation in the web UI: the moderation settings dialog (lobby with an opening time,
// public room password, who can start calls), the lobby waiting screen, and call moderation
// on the call stage (raised-hands queue, per-tile moderator menu, mute everyone, and handling
// what moderators do to our own call session). The server enforces all of it; this only
// shows the state and sends requests.

import { live } from '../ws.js';
import { h, clear, icon, avatar, modal, menu, toast, errorToast, timeOnly, dateTime } from '../ui.js';

/** Whether the lobby holds people back now (a passed opening time counts as open). */
export function lobbyActive(room) {
  return !!room.lobby && (!room.lobby_timer || new Date(room.lobby_timer).getTime() > Date.now());
}

/** "Today at 14:00" style text for a lobby opening time. */
export function openingText(ts) {
  const d = new Date(ts);
  const today = new Date();
  return d.toDateString() === today.toDateString() ? `today at ${timeOnly(ts)}` : dateTime(ts);
}

/** A datetime-local value (local time, minutes precision) for a Date. */
function localInput(d) {
  const pad = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * The moderation settings of a group or public room. Resolves with the updated room, or null
 * when cancelled. opts: {room, tapi, base}
 */
export async function moderationDialog({ room, tapi, base }) {
  const lobby = h('input', { type: 'checkbox', id: 'mod-lobby', checked: !!room.lobby });
  const timed = h('input', { type: 'checkbox', id: 'mod-lobby-timed', checked: !!room.lobby_timer });
  const soon = new Date(Date.now() + 15 * 60 * 1000);
  const when = h('input', {
    type: 'datetime-local', id: 'mod-lobby-at', 'aria-label': 'Lobby opens at',
    value: localInput(room.lobby_timer ? new Date(room.lobby_timer) : soon),
  });
  const whenRow = h('div.mod-indent', h('label.check', timed, 'Open the lobby automatically at'), when);
  const sync = () => {
    whenRow.hidden = !lobby.checked;
    when.disabled = !timed.checked;
  };
  lobby.addEventListener('change', sync);
  timed.addEventListener('change', sync);
  sync();

  const callPerm = h('select', { id: 'mod-call', 'aria-label': 'Who can start a call' },
    h('option', { value: 'everyone' }, 'Everyone'),
    h('option', { value: 'moderators' }, 'Moderators only'));
  callPerm.value = room.call_permission || 'everyone';

  let pwSection = '';
  let pwInput = null;
  let pwRemove = null;
  if (room.kind === 'public') {
    pwInput = h('input', { type: 'password', id: 'mod-password', autocomplete: 'new-password', placeholder: room.has_password ? 'Leave empty to keep the current password' : 'No password', 'aria-label': 'Conversation password' });
    pwRemove = h('input', { type: 'checkbox', id: 'mod-password-remove' });
    pwRemove.addEventListener('change', () => { pwInput.disabled = pwRemove.checked; });
    pwSection = h('section.mod-section',
      h('h3', icon('key', 'sm'), 'Password'),
      h('p.small.muted', room.has_password
        ? 'Guests and people joining by link must enter the password.'
        : 'Set a password that guests and people joining by link must enter.'),
      pwInput,
      room.has_password ? h('label.check', pwRemove, 'Remove the password') : '');
  }

  const body = h('form.mod-dialog', { onsubmit: (e) => e.preventDefault() },
    h('section.mod-section',
      h('h3', icon('lock', 'sm'), 'Lobby'),
      h('p.small.muted', 'While the lobby is on, only moderators can read and write messages or join the call. Everyone else waits until it opens.'),
      h('label.check', lobby, 'Hold participants in the lobby'),
      whenRow),
    pwSection,
    h('section.mod-section',
      h('h3', icon('phone', 'sm'), 'Calls'),
      h('label', { for: 'mod-call' }, 'Who can start a call'), callPerm));

  return modal({
    title: 'Moderation',
    body,
    actions: [{ label: 'Cancel', value: null }, {
      label: 'Save', kind: 'primary', run: async () => {
        const patch = { lobby: lobby.checked, call_permission: callPerm.value };
        if (lobby.checked && timed.checked) {
          const at = new Date(when.value);
          if (Number.isNaN(at.getTime()) || at.getTime() <= Date.now()) { toast('Pick an opening time in the future', 'warn'); return undefined; }
          patch.lobby_timer = at.toISOString();
        } else patch.lobby_timer = null;
        if (pwInput) {
          if (pwRemove && pwRemove.checked) patch.password = null;
          else if (pwInput.value) {
            if (pwInput.value.length < 4) { toast('The password needs at least 4 characters', 'warn'); return undefined; }
            patch.password = pwInput.value;
          }
        }
        const { room: r } = await tapi.patch(`${base}/moderation`, patch);
        return r;
      },
    }],
  });
}

/** The screen shown to someone waiting in the lobby. */
export function waitingView(room) {
  const when = room.lobby_timer ? h('p.lobby-when', icon('clock', 'sm'), `The conversation opens ${openingText(room.lobby_timer)}.`) : '';
  return h('div.lobby-wait', { role: 'status' },
    h('div.lobby-icon', icon('lock', 'icon-lg')),
    h('h2', 'You are in the lobby'),
    h('p.muted', 'A moderator has not let participants in yet. This page updates by itself as soon as the conversation opens.'),
    when,
    room.description ? h('p.lobby-desc.small', room.description) : '',
    h('div.lobby-dots', h('span.talk-dots', h('i'), h('i'), h('i')), h('span.small.muted', 'Waiting for a moderator')));
}

/**
 * Adds moderation to a call stage: a raised-hands queue for everyone, and for moderators a
 * menu on each remote tile and a "mute everyone" button. Also applies what moderators do to
 * our own session (mute request, lowered hand). opts: {isMod(), tapi, base}
 */
export function attachCallModeration(call, { isMod, tapi, base }) {
  const hands = h('div.call-hands', { hidden: true, 'aria-label': 'Raised hands', role: 'list' });
  call.el.insertBefore(hands, call.grid);
  const muteAll = h('button.call-btn.call-mute-all', {
    type: 'button', title: 'Mute everyone', 'aria-label': 'Mute everyone',
    onclick: () => moderate('mute_all').then((r) => r && toast(r.affected ? 'Everyone was asked to mute' : 'Nobody else is in the call', 'ok', { timeout: 2500 })),
  }, icon('mic-off'));
  const controls = call.el.querySelector('.call-controls');
  controls.insertBefore(muteAll, controls.lastElementChild);

  async function moderate(action, session) {
    try { return await tapi.post(`${base}/call/moderate`, { action, session }); } catch (e) { errorToast(e); return null; }
  }

  /** Every member with a raised hand, oldest first (ourselves included). */
  function queue() {
    const list = [...call.members.values()].filter((m) => m.hand);
    if (call.state.hand && call.session) list.push({ session: call.session, actor: { name: `${call.self.name} (you)` }, hand_raised_at: call.handSince, self: true });
    return list.sort((a, b) => new Date(a.hand_raised_at || 0) - new Date(b.hand_raised_at || 0));
  }

  function paintHands() {
    const q = queue();
    hands.hidden = !q.length;
    clear(hands).append(h('span.call-hands-title', icon('hand', 'sm'), 'Raised hands'),
      ...q.map((m, i) => h('span.call-hand', { role: 'listitem', dataset: { session: m.session } },
        h('span.call-hand-n', String(i + 1)), h('span.ellipsis', m.actor.name),
        isMod() && !m.self ? h('button', { type: 'button', 'aria-label': `Lower ${m.actor.name}'s hand`, title: 'Lower hand', onclick: () => moderate('lower_hand', m.session) }, icon('x', 'sm')) : '')));
  }

  function tileMenu(session, t) {
    let btn = t.el.querySelector('.tile-mod');
    if (!session || !isMod()) { btn?.remove(); return; }
    if (!btn) {
      btn = h('button.tile-mod', { type: 'button', title: 'Moderate' }, icon('more', 'sm'));
      btn.addEventListener('click', (e) => {
        const m = call.members.get(session);
        if (!m) return;
        menu(e.currentTarget, [
          m.audio && { label: 'Ask to mute', icon: 'mic-off', run: () => moderate('mute', session).then((r) => r && toast(`${m.actor.name} was asked to mute`, 'ok', { timeout: 2500 })) },
          m.hand && { label: 'Lower hand', icon: 'hand', run: () => moderate('lower_hand', session) },
          { label: 'Remove from call', icon: 'phone-off', danger: true, run: () => moderate('remove', session) },
        ]);
      });
      t.el.appendChild(btn);
    }
    const name = call.members.get(session)?.actor.name || 'participant';
    btn.setAttribute('aria-label', `Moderate ${name}`);
  }

  call.onTilePaint = (session, t) => {
    tileMenu(session, t);
    paintHands();
  };
  const paintMod = () => {
    muteAll.hidden = !isMod();
    for (const [key, t] of call.tiles) tileMenu(key === 'local' ? null : key, t);
    paintHands();
  };
  paintMod();

  // Our own hand: remember when it went up so the queue order matches everyone else's.
  const toggle = call.toggleHand.bind(call);
  call.toggleHand = () => {
    toggle();
    call.handSince = call.state.hand ? new Date().toISOString() : null;
    paintHands();
  };

  const offs = [
    live.on('call.moderated', (ev) => {
      if (ev.room_id !== call.roomId || ev.session !== call.session) return;
      const by = ev.by ? ev.by.name : 'A moderator';
      if (ev.action === 'mute') {
        if (call.state.audio) call.toggleMic();
        toast(`${by} muted your microphone`, 'info');
      } else if (ev.action === 'lower_hand') {
        call.state.hand = false;
        call.handSince = null;
        call.paintControls();
        call.paintTile(null);
        toast(`${by} lowered your hand`, 'info');
      }
    }),
    // Members leaving (or other hands going down) change the queue.
    live.on('call.left', (ev) => { if (ev.room_id === call.roomId) queueMicrotask(paintHands); }),
    live.on('call.state', (ev) => { if (ev.room_id === call.roomId) queueMicrotask(paintHands); }),
  ];
  const end = call.onEnd;
  call.onEnd = () => { offs.forEach((off) => off()); end(); };
  return { refresh: paintMod };
}
