// Ferrite web app: boot, sign-in, the shell (sidebar, header) and the router.
//
// View contract: each module in ./views exports `default async function render(root, ctx)`
// which fills `root` and may return a cleanup function, called when navigating away.
// ctx = { params, query, me, config, navigate(path), setTitle(text) }.

import { api, ApiError } from './api.js';
import { live } from './ws.js';
import { h, clear, icon, avatar, toast, errorToast, menu, relTime, spinner } from './ui.js';
import { showSecondStep } from './twofactor.js';

const ROUTES = [
  ['/', 'home'],
  ['/files', 'files'],
  ['/files/:id', 'files'],
  ['/talk', 'talk'],
  ['/talk/:token', 'talk'],
  ['/boards', 'boards'],
  ['/boards/:id', 'boards'],
  ['/calendar', 'calendar'],
  ['/contacts', 'contacts'],
  ['/edit/:id', 'editor'],
  ['/assistant', 'assistant'],
  ['/search', 'search'],
  ['/settings', 'settings'],
  ['/settings/:tab', 'settings'],
];
const PUBLIC_ROUTES = [
  ['/s/:token', 'public'],
  ['/join/:token', 'guest'],
];

const NAV = [
  ['/', 'Home', 'activity', 'home'],
  ['/files', 'Files', 'files', 'files'],
  ['/talk', 'Talk', 'talk', 'talk'],
  ['/boards', 'Boards', 'boards', 'boards'],
  ['/calendar', 'Calendar', 'calendar', 'calendar'],
  ['/contacts', 'Contacts', 'contacts', 'contacts'],
  ['/assistant', 'Assistant', 'assistant', 'assistant'],
];

const state = { me: null, config: null, cleanup: null, unread: 0, shell: null };

function match(path, routes) {
  for (const [pattern, view] of routes) {
    const pp = pattern.split('/').filter(Boolean);
    const sp = path.split('/').filter(Boolean);
    if (pp.length !== sp.length) continue;
    const params = {};
    let ok = true;
    pp.forEach((seg, i) => {
      if (seg.startsWith(':')) params[seg.slice(1)] = decodeURIComponent(sp[i]);
      else if (seg !== sp[i]) ok = false;
    });
    if (ok) return { view, params };
  }
  return null;
}

export function navigate(path, { replace = false } = {}) {
  if (replace) history.replaceState({}, '', path);
  else history.pushState({}, '', path);
  route();
}

function setTitle(t) {
  document.title = t ? `${t} · Ferrite` : 'Ferrite';
}

// ---- theme -------------------------------------------------------------------------
function applyTheme() {
  const t = localStorage.getItem('ferrite-theme');
  if (t) document.documentElement.dataset.theme = t;
  else delete document.documentElement.dataset.theme;
}

function toggleTheme() {
  const dark = document.documentElement.dataset.theme === 'dark'
    || (!document.documentElement.dataset.theme && matchMedia('(prefers-color-scheme: dark)').matches);
  localStorage.setItem('ferrite-theme', dark ? 'light' : 'dark');
  applyTheme();
}

// ---- auth screens --------------------------------------------------------------------
function authScreen(title, subtitle, fields, submitLabel, onSubmit, footer) {
  const err = h('div.error-text');
  const inputs = {};
  const form = h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      btn.disabled = true;
      try {
        await onSubmit(Object.fromEntries(Object.entries(inputs).map(([k, el]) => [k, el.value])));
      } catch (ex) {
        err.textContent = ex.message;
      } finally {
        btn.disabled = false;
      }
    },
  },
  fields.map(([name, label, type, auto]) => {
    inputs[name] = h('input', { type, name, autocomplete: auto, required: name !== 'display_name' });
    return h('div.field', h('label', { for: `f-${name}` }, label), Object.assign(inputs[name], { id: `f-${name}` }));
  }),
  err);
  const btn = h('button.btn.primary', { type: 'submit', style: { width: '100%', marginTop: '8px' } }, submitLabel);
  form.appendChild(btn);
  const app = document.getElementById('app');
  clear(app).appendChild(h('div.auth', h('div.card',
    h('h1', h('span.brand-mark', 'Fe'), title),
    subtitle && h('p.muted', subtitle),
    form,
    footer)));
  setTimeout(() => form.querySelector('input')?.focus(), 0);
}

/** After a password step: a session, or the two-factor step first. */
async function afterPassword(res, back) {
  if (res.challenge) return showSecondStep(res, { onDone: start, onCancel: back });
  return start();
}

async function showLogin(status) {
  setTitle('Sign in');
  const footer = [];
  // Single sign-on: one button per provider; errors come back as ?sso_error=….
  const params = new URLSearchParams(location.search);
  const ssoError = params.get('sso_error');
  if (ssoError) {
    params.delete('sso_error');
    history.replaceState({}, '', location.pathname + (params.toString() ? `?${params}` : ''));
  }
  const returnTo = location.pathname + location.search;
  if (status.sso?.length) {
    footer.push(h('div.auth-or', 'or'), h('div.auth-sso', status.sso.map((p) => h('a.btn.auth-sso-btn', {
      href: `/api/auth/oidc/${encodeURIComponent(p.id)}/start?return_to=${encodeURIComponent(returnTo)}`,
    }, icon('key', 'sm'), `Continue with ${p.name}`))));
  }
  if (status.open_registration) {
    footer.push(h('p.small.muted', { style: { marginTop: '14px' } }, 'No account? ', h('a', { href: '#', onclick: (e) => { e.preventDefault(); showRegister(); } }, 'Create one')));
  }
  authScreen('Ferrite', 'Sign in to your workspace.', [
    ['username', 'Username', 'text', 'username'],
    ['password', 'Password', 'password', 'current-password'],
  ], 'Sign in', async (v) => {
    await afterPassword(await api.post('/api/auth/login', v), () => showLogin(status));
  }, footer);
  if (ssoError) document.querySelector('.auth form')?.before(h('div.auth-alert', { role: 'alert' }, icon('lock', 'sm'), h('span', ssoError)));
}

async function showRegister() {
  setTitle('Create account');
  authScreen('Create account', null, [
    ['username', 'Username', 'text', 'username'],
    ['display_name', 'Your name', 'text', 'name'],
    ['password', 'Password (8+ characters)', 'password', 'new-password'],
  ], 'Create account', async (v) => {
    await afterPassword(await api.post('/api/auth/register', v), async () => showLogin(await api.get('/api/status')));
  });
}

async function showSetup() {
  setTitle('Welcome');
  authScreen('Welcome to Ferrite', 'Create the administrator account to finish setting up this server.', [
    ['username', 'Username', 'text', 'username'],
    ['display_name', 'Your name', 'text', 'name'],
    ['password', 'Password (8+ characters)', 'password', 'new-password'],
  ], 'Create administrator', async (v) => {
    await afterPassword(await api.post('/api/setup', v), async () => showLogin(await api.get('/api/status')));
  });
}

// ---- shell ---------------------------------------------------------------------------
function buildShell() {
  const me = state.me.user;
  const navLinks = NAV.map(([href, label, ic, key]) => h('a', { href, dataset: { nav: key }, onclick: link },
    icon(ic), label, key === 'talk' && h('span.badge', { id: 'talk-badge', hidden: true })));
  const search = h('input', {
    type: 'search', placeholder: 'Search files, chats, cards, events…', 'aria-label': 'Search',
    onkeydown: (e) => {
      if (e.key === 'Enter' && search.value.trim()) navigate(`/search?q=${encodeURIComponent(search.value.trim())}`);
    },
  });
  const bell = h('button.btn.ghost.icon', { 'aria-label': 'Notifications', onclick: () => showNotifications(bell) }, icon('bell'), h('span.badge', { id: 'notif-badge', hidden: true, style: { position: 'absolute', marginLeft: '18px', marginTop: '-16px' } }));
  const userBtn = h('button.btn.ghost', { onclick: () => menu(userBtn, [
    { label: 'Settings', icon: 'settings', run: () => navigate('/settings') },
    { label: 'Toggle dark mode', icon: 'moon', run: toggleTheme },
    me.is_admin && { label: 'Administration', icon: 'users', run: () => navigate('/settings/admin') },
    'sep',
    { label: 'Sign out', icon: 'logout', run: async () => { await api.post('/api/auth/logout'); location.href = '/'; } },
  ]) }, avatar(me.display_name, { size: 'sm', seed: me.id }), h('span.ellipsis', { style: { maxWidth: '140px' } }, me.display_name));
  const main = h('main.main', { id: 'main' });
  const shell = h('div.shell',
    h('aside.sidebar',
      h('div.brand', h('span.brand-mark', 'Fe'), 'Ferrite'),
      h('nav.nav', navLinks,
        h('div.nav-section', 'Workspace'),
        h('a', { href: '/search', dataset: { nav: 'search' }, onclick: link }, icon('search'), 'Search'),
        h('a', { href: '/settings', dataset: { nav: 'settings' }, onclick: link }, icon('settings'), 'Settings')),
      h('div.sidebar-foot', h('span', { id: 'conn-state' }, 'Connected'), ' · v', state.config.version)),
    h('header.header',
      h('button.btn.ghost.icon.menu-toggle', { 'aria-label': 'Menu', onclick: () => shell.classList.toggle('nav-open') }, icon('menu')),
      h('div.search', search),
      h('div.spacer'),
      bell, userBtn),
    main);
  clear(document.getElementById('app')).appendChild(shell);
  state.shell = shell;
  refreshBadges();
}

function link(e) {
  if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
  const a = e.currentTarget;
  e.preventDefault();
  state.shell?.classList.remove('nav-open');
  navigate(a.getAttribute('href'));
}

async function refreshBadges() {
  try {
    const [rooms, notes] = await Promise.all([api.get('/api/talk/rooms'), api.get('/api/notifications?unread=true')]);
    const unread = rooms.rooms.reduce((n, r) => n + (r.notify === 'none' ? 0 : r.unread), 0);
    setBadge('talk-badge', unread);
    setBadge('notif-badge', notes.notifications.length);
  } catch (_) { /* offline; next event refreshes */ }
}

function setBadge(id, n) {
  const el = document.getElementById(id);
  if (!el) return;
  el.hidden = !n;
  el.textContent = n > 99 ? '99+' : String(n);
}

async function showNotifications(anchor) {
  const { notifications } = await api.get('/api/notifications');
  const items = notifications.slice(0, 20);
  const box = h('div.popover', { style: { width: '360px', maxHeight: '420px', overflow: 'auto', right: '16px', top: 'calc(var(--header-h) - 6px)', position: 'fixed' } },
    h('div.row', { style: { padding: '10px 14px', borderBottom: '1px solid var(--border)' } }, h('strong.grow', 'Notifications'),
      h('button.btn.sm.ghost', { onclick: async () => { await api.post('/api/notifications/read', {}); box.remove(); refreshBadges(); } }, 'Mark all read')),
    items.length ? items.map((n) => h('div.list-item.clickable', {
      style: { alignItems: 'flex-start', opacity: n.read ? 0.65 : 1 },
      onclick: async () => {
        box.remove();
        await api.post('/api/notifications/read', { id: n.id });
        refreshBadges();
        if (n.link) navigate(n.link);
      },
    }, h('div.grow', h('div', n.title), n.body && h('div.small.muted.ellipsis', n.body), h('div.small.muted', relTime(n.created_at))))) : h('div.empty', 'Nothing new'));
  document.querySelectorAll('.popover.notif').forEach((p) => p.remove());
  box.classList.add('notif');
  document.body.appendChild(box);
  const away = (e) => { if (!box.contains(e.target) && !anchor.contains(e.target)) { box.remove(); document.removeEventListener('mousedown', away); } };
  setTimeout(() => document.addEventListener('mousedown', away), 0);
}

function wireLiveEvents() {
  live.on('notification', (ev) => {
    refreshBadges();
    const n = ev.notification;
    toast(n.body ? `${n.title}\n${n.body}` : n.title, 'info', n.link ? { action: { label: 'Open', run: () => navigate(n.link) } } : {});
  });
  live.on('talk.message', (ev) => {
    if (ev.message.actor_id !== state.me.user.id) refreshBadges();
  });
  live.on('talk.read', () => refreshBadges());
  live.on('talk.rooms_changed', () => refreshBadges());
  live.on('call.joined', async (ev) => {
    // Someone started a call in one of my rooms while I'm elsewhere: offer to join.
    if (ev.member.actor.id === state.me.user.id) return;
    if (location.pathname.startsWith('/talk/')) return;
    try {
      const { rooms } = await api.get('/api/talk/rooms');
      const r = rooms.find((x) => x.id === ev.room_id);
      if (!r) return;
      toast(`${ev.member.actor.name} is in a call in “${r.display_name}”`, 'info', { timeout: 15000, action: { label: 'Join', run: () => navigate(`/talk/${r.token}?call=1`) } });
    } catch (_) { /* ignore */ }
  });
  live.on('disconnected', () => { const el = document.getElementById('conn-state'); if (el) el.textContent = 'Reconnecting…'; });
  live.on('hello', () => { const el = document.getElementById('conn-state'); if (el) el.textContent = 'Connected'; refreshBadges(); });
}

// ---- routing --------------------------------------------------------------------------
async function route() {
  const path = location.pathname;
  const query = Object.fromEntries(new URLSearchParams(location.search));
  const pub = match(path, PUBLIC_ROUTES);
  if (pub) {
    if (state.cleanup) { try { state.cleanup(); } catch (_) { /* ignore */ } state.cleanup = null; }
    const root = clear(document.getElementById('app'));
    const mod = await import(`./views/${pub.view}.js`);
    state.cleanup = (await mod.default(root, { params: pub.params, query, navigate, setTitle, me: state.me, config: state.config })) || null;
    return;
  }
  if (!state.me) return;
  if (!state.shell || !document.body.contains(state.shell)) buildShell();
  const found = match(path, ROUTES) || { view: 'notfound', params: {} };
  document.querySelectorAll('.nav a').forEach((a) => {
    const key = a.dataset.nav;
    const active = key === found.view || (key === 'files' && found.view === 'editor');
    a.classList.toggle('active', active);
  });
  if (state.cleanup) {
    try { state.cleanup(); } catch (e) { console.error(e); }
    state.cleanup = null;
  }
  const main = document.getElementById('main');
  clear(main).appendChild(spinner());
  const token = (state.navToken = Symbol('nav'));
  try {
    if (found.view === 'notfound') {
      clear(main).appendChild(h('div.page', h('h1', 'Not found'), h('p', h('a', { href: '/', onclick: link }, 'Go home'))));
      return;
    }
    const mod = await import(`./views/${found.view}.js`);
    if (token !== state.navToken) return;
    const root = clear(main);
    const cleanup = await mod.default(root, { params: found.params, query, me: state.me, config: state.config, navigate, setTitle });
    if (token !== state.navToken) {
      if (typeof cleanup === 'function') cleanup();
      return;
    }
    state.cleanup = typeof cleanup === 'function' ? cleanup : null;
  } catch (e) {
    console.error(e);
    if (token !== state.navToken) return;
    clear(main).appendChild(h('div.page', h('h1', e instanceof ApiError && e.status === 404 ? 'Not found' : 'Something went wrong'), h('p.muted', e.message)));
  }
}

// ---- boot ------------------------------------------------------------------------------
async function start() {
  try {
    const [me, config] = await Promise.all([api.get('/api/me'), api.get('/api/config')]);
    state.me = me;
    state.config = config;
  } catch (e) {
    if (e.status === 401) {
      state.me = null;
      return showLogin(await api.get('/api/status'));
    }
    throw e;
  }
  live.start();
  wireLiveEvents();
  buildShell();
  route();
}

export async function refreshMe() {
  state.me = await api.get('/api/me');
  return state.me;
}

async function boot() {
  applyTheme();
  window.addEventListener('popstate', route);
  // Internal links anywhere in the app use the router.
  document.addEventListener('click', (e) => {
    const a = e.target.closest && e.target.closest('a[data-route]');
    if (a && !e.metaKey && !e.ctrlKey && e.button === 0) {
      e.preventDefault();
      navigate(a.getAttribute('href'));
    }
  });
  if (match(location.pathname, PUBLIC_ROUTES)) {
    // Public pages work without an account, but show the signed-in experience if there is one.
    // Ask /api/status first so a signed-out visitor never triggers a 401 in the console.
    try {
      const st = await api.get('/api/status');
      state.me = st.signed_in ? await api.get('/api/me') : null;
    } catch (_) { state.me = null; }
    return route();
  }
  let status;
  try {
    status = await api.get('/api/status');
  } catch (e) {
    document.getElementById('app').textContent = `Cannot reach the server: ${e.message}`;
    return;
  }
  if (status.setup_needed) return showSetup();
  api.onUnauthorized = () => { if (state.me) location.reload(); };
  // Signed out: straight to the sign-in form, without a failing /api/me first.
  if (!status.signed_in) return showLogin(status);
  try {
    await start();
  } catch (e) {
    errorToast(e);
  }
}

boot();
