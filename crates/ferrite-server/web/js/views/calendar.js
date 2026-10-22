// Calendar: month (default), week (time grid) and list views of the user's calendars, with
// the calendar list (visibility, create, rename, recolor, delete) in a side panel and
// event create/view/edit/delete in modals. The same data is served over CalDAV, so the
// view refreshes on `groupware.changed` whichever client made the change.
//
// Route: /calendar?view=month|week|list&date=YYYY-MM-DD.
//
// The API does not expand recurrence rules: a recurring event comes back once, as its
// first occurrence, and is shown there with a "repeats" mark. All-day events carry UTC
// midnight dates, so their day is read from the UTC fields; timed events use local time.

import { api, q } from '../api.js';
import { live } from '../ws.js';
import { h, append, clear, icon, modal, menu, toast, errorToast, confirmDialog, promptDialog, debounce, copyText } from '../ui.js';

const DAY = 86400000;
const HOUR_PX = 48;
const MONTH_LANES = 4;
const WEEK_LANES = 3;
const HIDDEN_KEY = 'ferrite-cal-hidden';
const VIEWS = [['month', 'Month'], ['week', 'Week'], ['list', 'List']];
export const CAL_COLORS = ['#c2410c', '#3b82f6', '#0f766e', '#7c3aed', '#be185d', '#15803d', '#b45309', '#475569', '#0e7490', '#dc2626'];

// ---- dates ----------------------------------------------------------------------------

const pad = (n) => String(n).padStart(2, '0');
const dayKey = (d) => `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
const timeKey = (d) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
const addDays = (d, n) => new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
const startOfDay = (d) => new Date(d.getFullYear(), d.getMonth(), d.getDate());
const daysBetween = (a, b) => Math.round((startOfDay(b) - startOfDay(a)) / DAY);
const sameDay = (a, b) => dayKey(a) === dayKey(b);

function parseKey(k) {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(k || '');
  return m ? new Date(+m[1], +m[2] - 1, +m[3]) : null;
}

/** First day of the week for the browser's locale (0 = Sunday). */
function weekStartDay() {
  try {
    const loc = new Intl.Locale(navigator.language);
    const info = loc.getWeekInfo ? loc.getWeekInfo() : loc.weekInfo;
    return (info?.firstDay ?? 1) % 7;
  } catch (_) {
    return 1;
  }
}
const WEEK_START = weekStartDay();
const startOfWeek = (d) => addDays(startOfDay(d), -((d.getDay() - WEEK_START + 7) % 7));

const fmt = (d, opts) => d.toLocaleString([], opts);
const fmtTime = (d) => d.toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' });
const TIME_FMT = new Intl.DateTimeFormat([], { hour: 'numeric', minute: '2-digit' });
/** "9:00 – 9:45 AM" (one time when start and end are equal). */
const fmtRange = (s, e) => (+s === +e ? fmtTime(s) : TIME_FMT.formatRange ? TIME_FMT.formatRange(s, e) : `${fmtTime(s)} – ${fmtTime(e)}`);
/** Compact chip time: "2 PM", "2:30 PM". */
const fmtShort = (d) => d.toLocaleTimeString([], d.getMinutes() ? { hour: 'numeric', minute: '2-digit' } : { hour: 'numeric' });

/** "#rrggbb" for any color a client may have stored (#rgb, #rrggbbaa) or the accent. */
export function normColor(c) {
  const s = String(c || '').trim();
  let m = /^#([0-9a-f]{6})(?:[0-9a-f]{2})?$/i.exec(s);
  if (m) return `#${m[1].toLowerCase()}`;
  m = /^#([0-9a-f])([0-9a-f])([0-9a-f])$/i.exec(s);
  if (m) return `#${m[1]}${m[1]}${m[2]}${m[2]}${m[3]}${m[3]}`.toLowerCase();
  return CAL_COLORS[0];
}

/** Readable text color on a solid background of `hex`. */
function textOn(hex) {
  const n = parseInt(hex.slice(1), 16);
  const lin = (v) => { const c = v / 255; return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4; };
  const l = 0.2126 * lin(n >> 16) + 0.7152 * lin((n >> 8) & 255) + 0.0722 * lin(n & 255);
  return l > 0.36 ? '#16191f' : '#ffffff';
}

const colorVars = (el, color) => {
  const c = normColor(color);
  el.style.setProperty('--ev-color', c);
  el.style.setProperty('--ev-fg', textOn(c));
  return el;
};

/** Adds layout fields to an API event: s/e (Date, end exclusive), first/last day (inclusive). */
function prepare(ev) {
  let s;
  let e;
  if (ev.all_day) {
    const utcDay = (iso) => { const u = new Date(iso); return new Date(u.getUTCFullYear(), u.getUTCMonth(), u.getUTCDate()); };
    s = utcDay(ev.starts_at || ev.ends_at);
    e = ev.ends_at ? utcDay(ev.ends_at) : addDays(s, 1);
    if (e <= s) e = addDays(s, 1);
  } else {
    s = new Date(ev.starts_at || ev.ends_at);
    e = ev.ends_at ? new Date(ev.ends_at) : new Date(s);
    if (e < s) e = new Date(s);
  }
  const first = startOfDay(s);
  const last = e > s ? startOfDay(new Date(e - 1)) : first;
  // Multi-day and all-day events are drawn as bars; in the week view, so is anything 24h+.
  const span = ev.all_day || !sameDay(first, last);
  const long = ev.all_day || e - s >= DAY;
  return { ...ev, s, e, first, last, span, long };
}

function whenText(ev) {
  if (ev.all_day) {
    const days = daysBetween(ev.first, ev.last) + 1;
    const d1 = fmt(ev.first, { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric' });
    return days === 1 ? `${d1} · All day` : `${d1} – ${fmt(ev.last, { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric' })} · All day`;
  }
  const d1 = fmt(ev.s, { weekday: 'short', month: 'short', day: 'numeric', year: 'numeric' });
  if (sameDay(ev.s, ev.e) || +ev.s === +ev.e) return `${d1} · ${fmtRange(ev.s, ev.e)}`;
  return `${d1}, ${fmtTime(ev.s)} – ${fmt(ev.e, { weekday: 'short', month: 'short', day: 'numeric' })}, ${fmtTime(ev.e)}`;
}

// ---- hidden calendars (a per-browser preference) ---------------------------------------

function loadHidden() {
  try { return new Set(JSON.parse(localStorage.getItem(HIDDEN_KEY)) || []); } catch (_) { return new Set(); }
}
function saveHidden(set) {
  try { localStorage.setItem(HIDDEN_KEY, JSON.stringify([...set])); } catch (_) { /* private mode */ }
}

// ---- sync help (shared with the contacts view) -----------------------------------------

/** A button that opens a popover with the DAV URL and sign-in hints for sync clients. */
export function syncHelpButton(ctx, { what, apps }) {
  const url = `${ctx.config.public_url || location.origin}/remote.php/dav`;
  let pop = null;
  const btn = h('button.btn.ghost.sm.sync-help-btn', { type: 'button', 'aria-expanded': 'false', 'aria-haspopup': 'dialog' }, icon('refresh', 'sm'), `Sync with ${what}`);
  const close = () => {
    if (!pop) return;
    pop.remove();
    pop = null;
    btn.setAttribute('aria-expanded', 'false');
    document.removeEventListener('mousedown', away, true);
    document.removeEventListener('keydown', esc, true);
  };
  const away = (e) => { if (pop && !pop.contains(e.target) && !btn.contains(e.target)) close(); };
  const esc = (e) => { if (e.key === 'Escape') { e.stopPropagation(); close(); btn.focus(); } };
  btn.addEventListener('click', () => {
    if (pop) return close();
    const field = h('input.mono', { type: 'text', value: url, readonly: true, 'aria-label': `${what} URL`, onfocus: (e) => e.target.select() });
    pop = h('div.popover.sync-help-pop', { role: 'dialog', 'aria-label': `Sync with ${what}` },
      h('h3', `Sync with ${what}`),
      h('p.small', `Add a ${what} account in ${apps} with this server address:`),
      h('div.row', field, h('button.btn.sm', { type: 'button', onclick: () => copyText(url, 'Address copied') }, icon('copy', 'sm'), 'Copy')),
      h('p.small.muted', 'Sign in with your username ', h('code', ctx.me.user.username), ' and your password, or an app token with the groupware scope (',
        h('a', { href: '/settings/tokens', onclick: (e) => { e.preventDefault(); close(); ctx.navigate('/settings/tokens'); } }, 'Settings › Tokens & MCP'), ').'));
    document.body.appendChild(pop);
    const r = btn.getBoundingClientRect();
    const pr = pop.getBoundingClientRect();
    pop.style.position = 'fixed';
    pop.style.top = `${r.bottom + 6 + pr.height > window.innerHeight ? Math.max(8, r.top - pr.height - 6) : r.bottom + 6}px`;
    pop.style.left = `${Math.max(8, Math.min(r.left, window.innerWidth - pr.width - 8))}px`;
    btn.setAttribute('aria-expanded', 'true');
    field.focus();
    setTimeout(() => {
      document.addEventListener('mousedown', away, true);
      document.addEventListener('keydown', esc, true);
    }, 0);
  });
  btn.closePopover = close;
  return btn;
}

/** A row of round color buttons (radio group); calls onPick(color). */
function colorPicker(current, onPick) {
  const row = h('div.swatches', { role: 'radiogroup', 'aria-label': 'Color' });
  const colors = CAL_COLORS.includes(current) ? CAL_COLORS : [...CAL_COLORS, current];
  for (const c of colors) {
    const b = h('button.swatch', {
      type: 'button', role: 'radio', 'aria-checked': String(c === current), 'aria-label': c, title: c, style: { background: c },
      onclick: () => {
        row.querySelectorAll('.swatch').forEach((x) => x.setAttribute('aria-checked', 'false'));
        b.setAttribute('aria-checked', 'true');
        onPick(c);
      },
    });
    row.appendChild(b);
  }
  return row;
}

// ---- lanes ----------------------------------------------------------------------------

/**
 * Places events on a row of consecutive days (a month week or the week view's all-day row)
 * in lanes. Returns the placed segments and, per column, the events hidden past `maxLanes`
 * (the last visible lane then holds a "+N more" button where needed).
 */
function layoutLanes(days, events, maxLanes) {
  const d0 = days[0];
  const n = days.length;
  const segs = [];
  for (const ev of events) {
    const c0 = Math.max(0, daysBetween(d0, ev.first));
    const c1 = Math.min(n - 1, daysBetween(d0, ev.last));
    if (c1 < 0 || c0 > n - 1 || c1 < c0) continue;
    segs.push({ ev, c0, c1, contL: ev.first < d0, contR: daysBetween(d0, ev.last) > n - 1 });
  }
  segs.sort((a, b) => (b.ev.span - a.ev.span) || (a.c0 - b.c0) || ((b.c1 - b.c0) - (a.c1 - a.c0)) || (a.ev.s - b.ev.s) || a.ev.summary.localeCompare(b.ev.summary));
  const lanes = [];
  for (const sg of segs) {
    let lane = 0;
    while (lanes[lane] && lanes[lane].some((x) => x.c0 <= sg.c1 && sg.c0 <= x.c1)) lane++;
    (lanes[lane] ||= []).push(sg);
    sg.lane = lane;
  }
  // Days with events past the last lane give that lane to a "+N more" button, so a
  // last-lane segment touching such a day is hidden as well (and counted on every day).
  const deep = new Array(n).fill(false);
  for (const sg of segs) if (sg.lane >= maxLanes) for (let c = sg.c0; c <= sg.c1; c++) deep[c] = true;
  const hidden = segs.filter((sg) => sg.lane >= maxLanes || (sg.lane === maxLanes - 1 && deep.slice(sg.c0, sg.c1 + 1).some(Boolean)));
  const more = Array.from({ length: n }, () => []);
  for (const sg of hidden) for (let c = sg.c0; c <= sg.c1; c++) more[c].push(sg.ev);
  return { placed: segs.filter((sg) => !hidden.includes(sg)), more };
}

// ---- view ------------------------------------------------------------------------------

export default async function render(root, ctx) {
  ctx.setTitle('Calendar');
  const narrow = matchMedia('(max-width: 700px)');
  const state = {
    view: VIEWS.some(([k]) => k === ctx.query.view) ? ctx.query.view : (narrow.matches ? 'list' : 'month'),
    cursor: parseKey(ctx.query.date) || startOfDay(new Date()),
    calendars: [],
    events: [],
    hidden: loadHidden(),
  };
  const cleanups = [];
  let loadSeq = 0;
  let drawnPage = null;
  let nowTimer = null;

  const calList = h('div.cal-cals', { role: 'list', 'aria-label': 'Calendars' });
  const title = h('h1.cal-title', { 'aria-live': 'polite' });
  const body = h('div.cal-body');
  const viewBtns = VIEWS.map(([k, label]) => h('button.btn.sm', { type: 'button', dataset: { view: k }, 'aria-pressed': 'false', onclick: () => setView(k) }, label));
  const syncBtn = syncHelpButton(ctx, { what: 'CalDAV', apps: 'your calendar app (DAVx⁵, Apple Calendar, Thunderbird, GNOME, Evolution…)' });
  const page = h('div.page-full.cal-page');
  const side = h('aside.cal-side', { 'aria-label': 'Calendars' },
    h('div.cal-side-head', h('h2', 'Calendars'),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'New calendar', title: 'New calendar', onclick: newCalendar }, icon('plus'))),
    calList,
    h('div.cal-side-foot', syncBtn));
  const sideToggle = h('button.btn.sm.cal-side-toggle', { type: 'button', 'aria-label': 'Show calendars', onclick: () => page.classList.toggle('side-open') }, icon('calendar', 'sm'), 'Calendars');

  append(page, [
    side,
    h('div.cal-scrim', { onclick: () => page.classList.remove('side-open') }),
    h('section.cal-main',
      h('div.cal-toolbar',
        sideToggle,
        h('div.btn-group.cal-nav',
          h('button.btn.sm', { type: 'button', onclick: () => go(0), title: 'Today (T)' }, 'Today'),
          h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Previous', title: 'Previous (←)', onclick: () => go(-1) }, icon('chevron-left')),
          h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Next', title: 'Next (→)', onclick: () => go(1) }, icon('chevron-right'))),
        title,
        h('div.cal-toolbar-end',
          h('div.cal-views', { role: 'group', 'aria-label': 'View' }, viewBtns),
          h('button.btn.primary.sm.cal-new', { type: 'button', 'aria-label': 'New event', onclick: () => openForm(null, defaultStart()) }, icon('plus', 'sm'), h('span', 'New event')))),
      body)]);
  root.appendChild(page);

  // ---- state ------------------------------------------------------------------------

  function range() {
    const c = state.cursor;
    if (state.view === 'week') {
      const s = startOfWeek(c);
      return { from: s, to: addDays(s, 7) };
    }
    const first = new Date(c.getFullYear(), c.getMonth(), 1);
    const next = new Date(c.getFullYear(), c.getMonth() + 1, 1);
    if (state.view === 'list') return { from: first, to: next };
    const s = startOfWeek(first);
    const weeks = Math.ceil(daysBetween(s, next) / 7);
    return { from: s, to: addDays(s, weeks * 7) };
  }

  function syncUrl() {
    const params = { view: state.view === 'month' ? '' : state.view, date: sameDay(state.cursor, new Date()) ? '' : dayKey(state.cursor) };
    history.replaceState({}, '', `/calendar${q(params)}`);
  }

  function setView(v) {
    state.view = v;
    syncUrl();
    loadEvents();
  }

  function go(dir) {
    const c = state.cursor;
    if (dir === 0) state.cursor = startOfDay(new Date());
    else if (state.view === 'week') state.cursor = addDays(c, 7 * dir);
    else state.cursor = new Date(c.getFullYear(), c.getMonth() + dir, Math.min(c.getDate(), 28));
    syncUrl();
    loadEvents();
  }

  function defaultStart(day) {
    const now = new Date();
    const base = day || (state.view === 'week' && !sameDay(startOfWeek(state.cursor), startOfWeek(now)) ? state.cursor : now);
    if (sameDay(base, now)) {
      const s = new Date(now);
      s.setMinutes(0, 0, 0);
      s.setHours(s.getHours() + 1);
      if (sameDay(s, now)) return s;
    }
    const s = startOfDay(base);
    s.setHours(9);
    return s;
  }

  async function loadCalendars() {
    const { calendars } = await api.get('/api/calendar/calendars');
    state.calendars = calendars;
    drawCalendars();
  }

  async function loadEvents() {
    const seq = ++loadSeq;
    const { from, to } = range();
    drawTitle();
    try {
      // A day of slack on both sides: all-day events are stored at UTC midnight.
      const { events } = await api.get(`/api/calendar/events${q({ from: addDays(from, -1).toISOString(), to: addDays(to, 1).toISOString() })}`);
      if (seq !== loadSeq) return;
      state.events = events.filter((e) => e.starts_at || e.ends_at).map(prepare);
    } catch (e) {
      if (seq !== loadSeq) return;
      errorToast(e);
    }
    draw();
  }

  async function reloadAll() {
    try {
      await loadCalendars();
    } catch (e) {
      errorToast(e);
    }
    await loadEvents();
  }

  const visible = () => state.events.filter((e) => !state.hidden.has(e.calendar));

  // ---- drawing ------------------------------------------------------------------------

  function drawTitle() {
    const c = state.cursor;
    if (state.view === 'week') {
      const s = startOfWeek(c);
      const e = addDays(s, 6);
      const sameMonth = s.getMonth() === e.getMonth();
      title.textContent = sameMonth
        ? `${fmt(s, { month: 'long' })} ${s.getDate()} – ${e.getDate()}, ${e.getFullYear()}`
        : `${fmt(s, { month: 'short', day: 'numeric' })} – ${fmt(e, { month: 'short', day: 'numeric', year: 'numeric' })}`;
    } else {
      title.textContent = fmt(c, { month: 'long', year: 'numeric' });
    }
    viewBtns.forEach((b) => {
      const on = b.dataset.view === state.view;
      b.classList.toggle('active', on);
      b.setAttribute('aria-pressed', String(on));
    });
  }

  function drawCalendars() {
    clear(calList);
    for (const c of state.calendars) {
      const color = normColor(c.color);
      const box = h('input', {
        type: 'checkbox', checked: !state.hidden.has(c.uri), 'aria-label': `Show ${c.name}`,
        onchange: () => {
          if (box.checked) state.hidden.delete(c.uri); else state.hidden.add(c.uri);
          saveHidden(state.hidden);
          row.classList.toggle('off', !box.checked);
          draw();
        },
      });
      const more = h('button.btn.ghost.icon.sm.cal-cal-more', {
        type: 'button', 'aria-label': `Options for ${c.name}`, 'aria-haspopup': 'menu',
        onclick: () => menu(more, [
          { label: 'Rename', icon: 'edit', run: () => renameCalendar(c) },
          { label: 'Change color', icon: 'tag', run: () => recolorCalendar(c) },
          'sep',
          { label: 'Delete', icon: 'trash', danger: true, run: () => deleteCalendar(c) },
        ]),
      }, icon('more'));
      const row = h('div.cal-cal', { role: 'listitem', dataset: { uri: c.uri }, class: state.hidden.has(c.uri) ? 'off' : '' },
        h('label.cal-cal-label', box, h('span.ellipsis', c.name)), more);
      row.style.setProperty('--cal-color', color);
      calList.appendChild(row);
    }
  }

  function draw() {
    drawTitle();
    // Keep the scroll position across live refreshes of the same page (week, month, list).
    const pageKey = state.view === 'week' ? `week:${dayKey(startOfWeek(state.cursor))}` : `${state.view}:${state.cursor.getFullYear()}-${state.cursor.getMonth()}`;
    const keepScroll = body.scrollTop;
    clearInterval(nowTimer);
    clear(body);
    body.className = `cal-body view-${state.view}`;
    if (state.view === 'month') body.appendChild(monthView());
    else if (state.view === 'week') body.appendChild(weekView());
    else body.appendChild(listView());
    if (drawnPage === pageKey) body.scrollTop = keepScroll;
    else if (state.view === 'week') {
      // A new week opens at 7:00, or earlier when an event starts before that.
      const s = startOfWeek(state.cursor);
      const timed = visible().filter((e) => !e.long && e.first >= s && e.first < addDays(s, 7));
      const hour = Math.min(7, ...timed.map((e) => e.s.getHours()));
      body.scrollTop = Math.max(0, hour * HOUR_PX - 16);
    } else body.scrollTop = 0;
    drawnPage = pageKey;
  }

  /** A clickable event chip; `seg` adds continuation edges for bars. */
  function chip(ev, seg, { showTime = true } = {}) {
    const bar = ev.span;
    const el = h(`button.cal-chip${bar ? '.bar' : '.dot'}`, {
      type: 'button', dataset: { eventId: ev.id }, title: `${ev.summary}\n${whenText(ev)}`,
      'aria-label': `${ev.summary || 'Untitled'}, ${whenText(ev)}${ev.recurring ? ', repeats' : ''}`,
      onclick: (e) => { e.stopPropagation(); showEvent(ev); },
    },
    !bar && h('span.cal-chip-dot'),
    !bar && showTime && h('span.cal-chip-time', fmtShort(ev.s)),
    bar && !ev.all_day && (!seg || !seg.contL) && h('span.cal-chip-time', fmtShort(ev.s)),
    ev.component === 'VTODO' && icon('check', 'sm'),
    h('span.cal-chip-title', ev.summary || 'Untitled'),
    ev.recurring && icon('repeat', 'sm'));
    if (seg?.contL) el.classList.add('cont-l');
    if (seg?.contR) el.classList.add('cont-r');
    return colorVars(el, ev.color);
  }

  /** A row of days (a month week or the week view's all-day row) with lane-placed events. */
  function laneRow(days, events, maxLanes, { dayHeads, cls }) {
    const today = new Date();
    const { placed, more } = layoutLanes(days, events, maxLanes);
    const month = state.cursor.getMonth();
    const bg = h('div.cal-row-bg', days.map((d) => h('div.cal-cell', {
      class: [sameDay(d, today) && 'today', dayHeads && d.getMonth() !== month && 'outside'].filter(Boolean).join(' '),
      dataset: { day: dayKey(d) },
      onclick: () => openForm(null, defaultStart(d)),
    })));
    const fg = h('div.cal-row-fg');
    if (dayHeads) {
      days.forEach((d, i) => fg.appendChild(h('button.cal-daynum', {
        type: 'button', style: { gridColumn: `${i + 1}`, gridRow: '1' }, class: [sameDay(d, today) && 'today', d.getMonth() !== month && 'outside'].filter(Boolean).join(' '),
        'aria-label': `Week of ${fmt(d, { weekday: 'long', month: 'long', day: 'numeric' })}`,
        onclick: () => { state.cursor = d; setView('week'); },
      }, d.getDate() === 1 ? fmt(d, { month: 'short', day: 'numeric' }) : String(d.getDate()))));
    }
    const top = dayHeads ? 2 : 1;
    for (const sg of placed) {
      const el = chip(sg.ev, sg);
      el.style.gridColumn = `${sg.c0 + 1} / ${sg.c1 + 2}`;
      el.style.gridRow = `${sg.lane + top}`;
      fg.appendChild(el);
    }
    more.forEach((list, i) => {
      if (!list.length) return;
      fg.appendChild(h('button.cal-more', {
        type: 'button', style: { gridColumn: `${i + 1}`, gridRow: `${maxLanes - 1 + top}` },
        onclick: (e) => { e.stopPropagation(); dayDialog(days[i]); },
      }, `+${list.length} more`));
    });
    return h(`div.cal-row${cls ? '.' + cls : ''}`, bg, fg);
  }

  function weekdayHeads(days, gutter) {
    const today = new Date();
    return h('div.cal-heads', { class: gutter ? 'with-gutter' : '' },
      gutter && h('div.cal-gutter-head'),
      days.map((d) => h('div.cal-head', { class: sameDay(d, today) ? 'today' : '' },
        gutter
          ? [h('span.cal-head-wd', fmt(d, { weekday: 'short' })), h('span.cal-head-day', String(d.getDate()))]
          : h('span', { title: fmt(d, { weekday: 'long' }) }, fmt(d, { weekday: 'short' })))));
  }

  function monthView() {
    const { from, to } = range();
    const evs = visible();
    const weeks = [];
    for (let d = from; d < to; d = addDays(d, 7)) {
      const days = Array.from({ length: 7 }, (_, i) => addDays(d, i));
      const inWeek = evs.filter((e) => e.first <= days[6] && e.last >= days[0]);
      weeks.push(laneRow(days, inWeek, MONTH_LANES, { dayHeads: true, cls: 'cal-week' }));
    }
    const grid = h('div.cal-month', weekdayHeads(weeks.length ? Array.from({ length: 7 }, (_, i) => addDays(from, i)) : [], false), h('div.cal-weeks', weeks));
    grid.style.setProperty('--weeks', weeks.length);
    return grid;
  }

  function weekView() {
    const s = startOfWeek(state.cursor);
    const days = Array.from({ length: 7 }, (_, i) => addDays(s, i));
    const evs = visible();
    const allDay = evs.filter((e) => e.long && e.first <= days[6] && e.last >= days[0]);
    const today = new Date();
    const cols = days.map((d) => {
      const dayStart = d;
      const dayEnd = addDays(d, 1);
      // Timed events (under a day) cut at midnight, so a late event continues next morning.
      const segs = evs.filter((e) => !e.long && e.s < dayEnd && (e.e > dayStart || (+e.e === +e.s && e.s >= dayStart)))
        .map((e) => ({ ev: e, a: Math.max(0, (e.s - dayStart) / 60000), b: Math.min(1440, (e.e - dayStart) / 60000) }))
        .sort((x, y) => (x.a - y.a) || (y.b - x.b));
      layoutColumns(segs);
      const col = h('div.cal-col', {
        class: sameDay(d, today) ? 'today' : '', dataset: { day: dayKey(d) },
        'aria-label': fmt(d, { weekday: 'long', month: 'long', day: 'numeric' }),
        onclick: (e) => {
          if (e.target !== col) return;
          const mins = Math.floor(e.offsetY / HOUR_PX * 2) * 30;
          const start = new Date(d);
          start.setMinutes(Math.min(mins, 1410));
          openForm(null, start);
        },
      });
      for (const sg of segs) {
        const ev = sg.ev;
        const dur = Math.max(sg.b - sg.a, 20);
        const el = h('button.cal-block', {
          type: 'button', dataset: { eventId: ev.id }, title: `${ev.summary}\n${whenText(ev)}`,
          'aria-label': `${ev.summary || 'Untitled'}, ${whenText(ev)}${ev.recurring ? ', repeats' : ''}`,
          class: dur < 45 ? 'short' : '',
          style: {
            top: `${sg.a / 60 * HOUR_PX}px`, height: `${dur / 60 * HOUR_PX - 2}px`,
            left: `calc(${sg.col / sg.cols * 100}% + 2px)`, width: `calc(${100 / sg.cols}% - 4px)`,
          },
          onclick: (e) => { e.stopPropagation(); showEvent(ev); },
        },
        h('span.cal-block-title', ev.component === 'VTODO' && icon('check', 'sm'), ev.summary || 'Untitled', ev.recurring && icon('repeat', 'sm')),
        h('span.cal-block-time', fmtRange(ev.s, ev.e)),
        ev.location && dur >= 60 && h('span.cal-block-loc', ev.location));
        col.appendChild(colorVars(el, ev.color));
      }
      if (sameDay(d, today)) {
        const now = h('div.cal-now', { 'aria-hidden': 'true' });
        const place = () => { const n = new Date(); now.style.top = `${(n.getHours() * 60 + n.getMinutes()) / 60 * HOUR_PX}px`; };
        place();
        clearInterval(nowTimer);
        nowTimer = setInterval(place, 60000);
        col.appendChild(now);
      }
      return col;
    });
    const hours = Array.from({ length: 24 }, (_, i) => {
      const t = new Date(s);
      t.setHours(i);
      return h('div.cal-hour', i ? h('span', fmt(t, { hour: 'numeric' })) : null);
    });
    const wrap = h('div.cal-week-view',
      h('div.cal-week-top',
        weekdayHeads(days, true),
        h('div.cal-allday', h('div.cal-gutter-head.small.muted', 'All day'), laneRow(days, allDay, WEEK_LANES, { dayHeads: false, cls: 'cal-allday-row' }))),
      h('div.cal-grid', h('div.cal-gutter', hours), h('div.cal-cols', cols)));
    wrap.style.setProperty('--hour', `${HOUR_PX}px`);
    return wrap;
  }

  /** Overlapping timed segments share the column width: sets sg.col and sg.cols. */
  function layoutColumns(segs) {
    let cluster = [];
    let clusterEnd = -1;
    const flush = () => {
      const n = Math.max(1, ...cluster.map((x) => x.col + 1));
      cluster.forEach((x) => { x.cols = n; });
      cluster = [];
    };
    for (const sg of segs) {
      const end = Math.max(sg.b, sg.a + 20);
      if (cluster.length && sg.a >= clusterEnd) flush();
      const used = new Set(cluster.filter((x) => Math.max(x.b, x.a + 20) > sg.a).map((x) => x.col));
      let c = 0;
      while (used.has(c)) c++;
      sg.col = c;
      cluster.push(sg);
      clusterEnd = Math.max(clusterEnd, end);
    }
    if (cluster.length) flush();
  }

  /** Events of one day for the list view and the "+N more" dialog, all-day first. */
  function eventsOn(d, list = visible()) {
    return list.filter((e) => e.first <= d && e.last >= d)
      .sort((a, b) => (b.span - a.span) || (a.s - b.s) || a.summary.localeCompare(b.summary));
  }

  function listRow(ev, d) {
    let time;
    if (ev.all_day) time = 'All day';
    else if (!ev.span) time = fmtRange(ev.s, ev.e);
    else if (sameDay(ev.first, d)) time = `From ${fmtTime(ev.s)}`;
    else if (sameDay(ev.last, d)) time = `Until ${fmtTime(ev.e)}`;
    else time = 'All day';
    return colorVars(h('button.cal-item', {
      type: 'button', dataset: { eventId: ev.id }, onclick: () => showEvent(ev),
      'aria-label': `${ev.summary || 'Untitled'}, ${whenText(ev)}${ev.recurring ? ', repeats' : ''}`,
    },
    h('span.cal-item-time', time),
    h('span.cal-item-dot'),
    h('span.cal-item-main',
      h('span.cal-item-title', ev.component === 'VTODO' && icon('check', 'sm'), ev.summary || 'Untitled', ev.recurring && icon('repeat', 'sm')),
      (ev.location || ev.first < ev.last) && h('span.cal-item-sub.small.muted', [ev.first < ev.last && `Day ${daysBetween(ev.first, d) + 1} of ${daysBetween(ev.first, ev.last) + 1}`, ev.location].filter(Boolean).join(' · '))),
    h('span.cal-item-cal.small.muted', ev.calendar_name)), ev.color);
  }

  function listView() {
    const { from, to } = range();
    const evs = visible();
    const today = new Date();
    const out = h('div.cal-list');
    let any = false;
    for (let d = from; d < to; d = addDays(d, 1)) {
      const list = eventsOn(d, evs);
      if (!list.length) continue;
      any = true;
      out.appendChild(h('section.cal-day', { class: sameDay(d, today) ? 'today' : '', dataset: { day: dayKey(d) } },
        h('h3.cal-day-head',
          h('span.cal-day-num', String(d.getDate())),
          h('span', fmt(d, { weekday: 'long' })),
          sameDay(d, today) && h('span.pill.info', 'Today')),
        h('div.cal-day-items', list.map((ev) => listRow(ev, d)))));
    }
    if (!any) {
      out.appendChild(h('div.empty', h('div', icon('calendar', 'icon-lg')),
        h('h3', `Nothing planned in ${fmt(state.cursor, { month: 'long' })}`),
        h('p', 'Create an event, or sync a calendar from another app.'),
        h('button.btn.primary', { type: 'button', onclick: () => openForm(null, defaultStart()) }, icon('plus'), 'New event')));
    }
    // Recurring series are not expanded by the server; ones that began before this month
    // would otherwise be invisible here.
    const series = evs.filter((e) => e.recurring && e.last < from);
    if (series.length) {
      out.appendChild(h('section.cal-day.cal-series',
        h('h3.cal-day-head', icon('repeat'), h('span', 'Repeating series that started earlier')),
        h('p.small.muted', 'Shown at their first occurrence; repetitions are not expanded here.'),
        h('div.cal-day-items', series.map((ev) => listRow(ev, ev.first)))));
    }
    return out;
  }

  // ---- dialogs ------------------------------------------------------------------------

  function dayDialog(d) {
    modal({
      title: fmt(d, { weekday: 'long', month: 'long', day: 'numeric' }),
      body: (close) => h('div.cal-day-items.cal-day-dialog', eventsOn(d).map((ev) => {
        const row = listRow(ev, d);
        row.addEventListener('click', () => close(), { capture: true });
        return row;
      })),
      actions: [
        { label: 'New event', run: (close) => { close(); openForm(null, defaultStart(d)); } },
        { label: 'Close', kind: 'primary', value: true },
      ],
    });
  }

  function showEvent(ev) {
    const isTask = ev.component === 'VTODO';
    const info = (ic, ...content) => h('div.cal-info-row', icon(ic, 'sm'), h('div.grow', content));
    const dot = h('span.cal-info-dot');
    dot.style.setProperty('--ev-color', normColor(ev.color));
    modal({
      title: ev.summary || 'Untitled',
      body: h('div.cal-info', { dataset: { eventId: ev.id } },
        info('clock', whenText(ev)),
        ev.recurring && info('repeat', 'Repeats', h('div.small.muted', 'Only the first occurrence is shown in this calendar. Edits keep the repeat rule set by your calendar app.')),
        ev.location && info('pin', ev.location),
        h('div.cal-info-row', dot, h('div.grow', ev.calendar_name)),
        isTask && info('check', 'Task', h('div.small.muted', 'Tasks are shown read-only here; edit them in your tasks app.')),
        ev.description && h('div.cal-info-desc', ev.description)),
      actions: [
        { label: 'Delete', kind: 'danger', run: (close) => { close(); deleteEvent(ev); } },
        !isTask && { label: 'Edit', run: (close) => { close(); openForm(ev); } },
        { label: 'Close', kind: 'primary', value: true },
      ].filter(Boolean),
    });
  }

  async function deleteEvent(ev) {
    if (!await confirmDialog('Delete event', `Delete “${ev.summary || 'Untitled'}”${ev.recurring ? ' and all its repetitions' : ''}?`, { confirm: 'Delete', danger: true })) return;
    try {
      await api.del(`/api/calendar/events/${encodeURIComponent(ev.id)}`);
      state.events = state.events.filter((e) => e.id !== ev.id);
      draw();
      toast('Event deleted', 'ok', { timeout: 2500 });
    } catch (e) {
      errorToast(e);
    }
  }

  /** Create (ev = null, starting at `start`) or edit form. */
  async function openForm(ev, start) {
    if (!state.calendars.length) await reloadAll();
    const editing = !!ev;
    const s0 = editing ? ev.s : start;
    const allDay0 = editing ? ev.all_day : false;
    const e0 = editing ? (ev.all_day ? ev.last : ev.e) : new Date(+start + 3600000);
    const uid = Math.random().toString(36).slice(2, 8);
    const field = (label, input, extra) => h('div.field', h('label', { for: input.id }, label), input, extra);
    const titleIn = h('input', { type: 'text', id: `ev-title-${uid}`, value: ev?.summary || '', placeholder: 'Add a title', required: true, autocomplete: 'off' });
    const allDay = h('input', { type: 'checkbox', checked: allDay0 });
    const sDate = h('input', { type: 'date', id: `ev-sd-${uid}`, value: dayKey(s0), 'aria-label': 'Start date', required: true });
    const sTime = h('input', { type: 'time', id: `ev-st-${uid}`, value: timeKey(s0), 'aria-label': 'Start time', step: 300 });
    const eDate = h('input', { type: 'date', id: `ev-ed-${uid}`, value: dayKey(e0), 'aria-label': 'End date', required: true });
    const eTime = h('input', { type: 'time', id: `ev-et-${uid}`, value: timeKey(e0), 'aria-label': 'End time', step: 300 });
    const visibleCal = state.calendars.find((c) => !state.hidden.has(c.uri)) || state.calendars[0];
    const calSel = h('select', { id: `ev-cal-${uid}`, disabled: editing },
      state.calendars.map((c) => h('option', { value: c.uri, selected: editing ? c.uri === ev.calendar : c === visibleCal }, c.name)));
    const loc = h('input', { type: 'text', id: `ev-loc-${uid}`, value: ev?.location || '', placeholder: 'Add a place or link', autocomplete: 'off' });
    const desc = h('textarea', { id: `ev-desc-${uid}`, rows: 3, placeholder: 'Notes' }, ev?.description || '');
    const err = h('div.error-text', { role: 'alert' });

    const read = () => {
      const sd = parseKey(sDate.value);
      const ed = parseKey(eDate.value);
      if (!sd || !ed) return null;
      if (allDay.checked) return { s: sd, e: ed };
      const [sh, sm] = (sTime.value || '00:00').split(':').map(Number);
      const [eh, em] = (eTime.value || '00:00').split(':').map(Number);
      return { s: new Date(sd.getFullYear(), sd.getMonth(), sd.getDate(), sh, sm), e: new Date(ed.getFullYear(), ed.getMonth(), ed.getDate(), eh, em) };
    };
    // Moving the start keeps the duration, as calendar apps do.
    let dur = (() => { const r = read(); return r ? r.e - r.s : 3600000; })();
    const onStart = () => {
      const r = read();
      if (!r) return;
      const e = allDay.checked ? addDays(r.s, Math.round(dur / DAY)) : new Date(+r.s + dur);
      eDate.value = dayKey(e);
      if (!allDay.checked) eTime.value = timeKey(e);
    };
    const onEnd = () => { const r = read(); if (r && r.e >= r.s) dur = r.e - r.s; };
    sDate.addEventListener('change', onStart);
    sTime.addEventListener('change', onStart);
    eDate.addEventListener('change', onEnd);
    eTime.addEventListener('change', onEnd);
    const times = [sTime, eTime];
    const syncAllDay = () => {
      times.forEach((t) => { t.hidden = allDay.checked; });
      const r = read();
      if (r) dur = Math.max(0, r.e - r.s);
    };
    allDay.addEventListener('change', syncAllDay);
    syncAllDay();

    const save = async () => {
      err.textContent = '';
      const summary = titleIn.value.trim();
      if (!summary) { err.textContent = 'Give the event a title.'; titleIn.focus(); return undefined; }
      const r = read();
      if (!r) { err.textContent = 'Pick a start and end date.'; return undefined; }
      if (r.e < r.s) { err.textContent = 'The event ends before it starts.'; return undefined; }
      const utcMidnight = (d) => new Date(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate())).toISOString();
      const payload = {
        summary,
        all_day: allDay.checked,
        start: allDay.checked ? utcMidnight(r.s) : r.s.toISOString(),
        // All-day ends are exclusive in iCalendar: the form shows the last day.
        end: allDay.checked ? utcMidnight(addDays(r.e, 1)) : r.e.toISOString(),
        location: loc.value.trim(),
        description: desc.value.trim(),
      };
      if (editing) {
        await api.patch(`/api/calendar/events/${encodeURIComponent(ev.id)}`, payload);
      } else {
        payload.calendar = calSel.value;
        await api.post('/api/calendar/events', payload);
        if (state.hidden.delete(calSel.value)) { saveHidden(state.hidden); drawCalendars(); }
      }
      return true;
    };

    const form = h('form.cal-form', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
      field('Title', titleIn),
      h('label.check', allDay, 'All day'),
      h('div.cal-form-when',
        h('div.field', h('label', { for: sDate.id }, 'Starts'), h('div.cal-form-dt', sDate, sTime)),
        h('div.field', h('label', { for: eDate.id }, 'Ends'), h('div.cal-form-dt', eDate, eTime))),
      field('Calendar', calSel, editing && state.calendars.length > 1 ? h('div.small.muted', 'Events can’t be moved to another calendar here.') : null),
      field('Location', loc),
      field('Description', desc),
      h('button', { type: 'submit', hidden: true, tabindex: '-1', 'aria-hidden': 'true' }),
      err);
    setTimeout(() => { titleIn.focus(); }, 0);
    const saved = await modal({
      title: editing ? 'Edit event' : 'New event',
      body: form,
      actions: [
        { label: 'Cancel', value: null },
        { label: editing ? 'Save' : 'Create', kind: 'primary', run: save },
      ],
    });
    if (saved) {
      await loadEvents();
      toast(editing ? 'Event updated' : 'Event created', 'ok', { timeout: 2500 });
    }
  }

  async function newCalendar() {
    let color = CAL_COLORS[(state.calendars.length + 1) % CAL_COLORS.length];
    const name = h('input', { type: 'text', id: 'cal-new-name', placeholder: 'e.g. Family', required: true, autocomplete: 'off' });
    const form = h('form', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
      h('label', { for: name.id }, 'Name'), name,
      h('label', 'Color'), colorPicker(color, (c) => { color = c; }));
    const created = await modal({
      title: 'New calendar',
      body: form,
      actions: [
        { label: 'Cancel', value: null },
        {
          label: 'Create calendar', kind: 'primary', run: async () => {
            if (!name.value.trim()) { name.focus(); return undefined; }
            const { calendar } = await api.post('/api/calendar/calendars', { name: name.value.trim(), color });
            return calendar;
          },
        },
      ],
    });
    if (created) {
      page.classList.remove('side-open');
      await loadCalendars();
      toast(`Calendar “${created.name}” created`, 'ok', { timeout: 2500 });
    }
  }

  async function renameCalendar(c) {
    const name = await promptDialog('Rename calendar', { label: 'Name', value: c.name, confirm: 'Rename' });
    if (!name || name === c.name) return;
    try {
      await api.patch(`/api/calendar/calendars/${encodeURIComponent(c.uri)}`, { name });
      await reloadAll();
    } catch (e) {
      errorToast(e);
    }
  }

  async function recolorCalendar(c) {
    let color = normColor(c.color);
    const custom = h('input.cal-color-custom', { type: 'color', value: color, 'aria-label': 'Custom color', oninput: (e) => { color = e.target.value; } });
    const v = await modal({
      title: `Color of “${c.name}”`,
      body: h('div', colorPicker(color, (x) => { color = x; custom.value = x; }), h('label.row.cal-color-row', custom, h('span', 'Custom color'))),
      actions: [{ label: 'Cancel', value: null }, { label: 'Save', kind: 'primary', run: () => color }],
    });
    if (!v || v === normColor(c.color)) return;
    try {
      await api.patch(`/api/calendar/calendars/${encodeURIComponent(c.uri)}`, { color: v });
      await reloadAll();
    } catch (e) {
      errorToast(e);
    }
  }

  async function deleteCalendar(c) {
    const msg = state.calendars.length === 1
      ? `Delete “${c.name}” and all its events? An empty “Personal” calendar takes its place.`
      : `Delete “${c.name}” and all its events? Sync clients will remove it as well.`;
    if (!await confirmDialog('Delete calendar', msg, { confirm: 'Delete calendar', danger: true })) return;
    try {
      await api.del(`/api/calendar/calendars/${encodeURIComponent(c.uri)}`);
      state.hidden.delete(c.uri);
      saveHidden(state.hidden);
      await reloadAll();
      toast(`Calendar “${c.name}” deleted`, 'ok', { timeout: 2500 });
    } catch (e) {
      errorToast(e);
    }
  }

  // ---- keyboard, live updates -----------------------------------------------------------

  const onKey = (e) => {
    if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey) return;
    const t = e.target;
    if (t.closest?.('input, textarea, select, [contenteditable="true"]') || document.querySelector('.modal-backdrop, .menu')) return;
    const keys = { t: () => go(0), ArrowLeft: () => go(-1), ArrowRight: () => go(1), m: () => setView('month'), w: () => setView('week'), l: () => setView('list') };
    const fn = keys[e.key];
    if (fn) { e.preventDefault(); fn(); }
  };
  document.addEventListener('keydown', onKey);
  cleanups.push(() => document.removeEventListener('keydown', onKey));

  const refresh = debounce(reloadAll, 200);
  cleanups.push(live.on('groupware.changed', (ev) => { if (ev.kind !== 'addressbook') refresh(); }));
  cleanups.push(live.on('hello', refresh));
  const onNarrow = () => page.classList.remove('side-open');
  narrow.addEventListener('change', onNarrow);
  cleanups.push(() => narrow.removeEventListener('change', onNarrow));
  cleanups.push(() => syncBtn.closePopover());
  cleanups.push(() => clearInterval(nowTimer));

  syncUrl();
  drawTitle();
  body.appendChild(h('div.loading', h('span.spinner')));
  await reloadAll();
  return () => cleanups.forEach((fn) => fn());
}
