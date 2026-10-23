// Settings: /settings/:tab with Profile, Security, Notifications, AI, Tokens & MCP, MCP servers, Craft apps,
// Administration (admins) and About. The bigger tabs live in settings-*.js.

import { api } from '../api.js';
import { h, clear, icon, avatar, toast, errorToast, modal, confirmDialog, relTime, dateTime, spinner } from '../ui.js';
import { refreshMe } from '../app.js';
import { card, field, codeBlock, pill } from './settings-common.js';
import { aiTab, mcpServersTab } from './settings-ai.js';
import { adminTab } from './settings-admin.js';
import { securityTab } from './settings-security.js';
import { notificationsTab } from './settings-notifications.js';
import { availabilityCard } from './settings-availability.js';
import { copyField } from './files-dialogs.js';

const TABS = [
  ['profile', 'Profile', 'user'],
  ['security', 'Security', 'lock'],
  ['notifications', 'Notifications', 'bell'],
  ['ai', 'AI', 'assistant'],
  ['tokens', 'Tokens & MCP', 'key'],
  ['mcp', 'MCP servers', 'plug'],
  ['craft', 'Craft apps', 'craft'],
  ['admin', 'Administration', 'users', true],
  ['about', 'About', 'activity'],
];

export default async function render(root, ctx) {
  const isAdmin = ctx.me.user.is_admin;
  const tabs = TABS.filter((t) => !t[3] || isAdmin);
  let tab = ctx.params.tab || 'profile';
  if (!tabs.some(([k]) => k === tab)) tab = 'profile';
  const [, label] = tabs.find(([k]) => k === tab);
  ctx.setTitle(`${label} · Settings`);

  const body = h('div.set-body', spinner());
  root.appendChild(h('div.page.set-page',
    h('div.page-head', h('h1', 'Settings')),
    h('nav.tabs.set-tabs', { 'aria-label': 'Settings sections' }, tabs.map(([k, l, ic]) => h(`a${k === tab ? '.active' : ''}`, {
      href: `/settings/${k}`, 'aria-current': k === tab ? 'page' : null,
      onclick: (e) => { e.preventDefault(); ctx.navigate(`/settings/${k}`); },
    }, icon(ic, 'sm'), l))),
    body));

  const fn = { profile: profileTab, security: securityTab, notifications: notificationsTab, ai: aiTab, tokens: tokensTab, mcp: mcpServersTab, craft: craftTab, admin: adminTab, about: aboutTab }[tab];
  const host = h('div.set-stack');
  const cleanup = await fn(host, ctx);
  clear(body).appendChild(host);
  return typeof cleanup === 'function' ? cleanup : null;
}

// ---- Profile ---------------------------------------------------------------------------

async function profileTab(host, ctx) {
  const u = ctx.me.user;
  const name = h('input', { type: 'text', value: u.display_name, autocomplete: 'name', required: true });
  const email = h('input', { type: 'email', value: u.email || '', autocomplete: 'email', placeholder: 'you@example.com' });
  const saveBtn = h('button.btn.primary', { type: 'submit' }, 'Save profile');
  const head = h('div.set-profile-head', avatar(u.display_name, { size: 'lg', seed: u.id }),
    h('div', h('strong', u.display_name), h('div.small.muted', `@${u.username}`, u.is_admin ? ' · Administrator' : '')));
  const profile = h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      saveBtn.disabled = true;
      try {
        const { user } = await api.patch('/api/me', { display_name: name.value.trim(), email: email.value.trim() });
        Object.assign(ctx.me.user, user);
        await refreshMe().catch(() => {});
        clear(head).append(avatar(user.display_name, { size: 'lg', seed: user.id }), h('div', h('strong', user.display_name), h('div.small.muted', `@${user.username}`)));
        toast('Profile saved', 'ok', { timeout: 2000 });
      } catch (ex) { errorToast(ex); } finally { saveBtn.disabled = false; }
    },
  }, head, h('div.set-grid', field('Display name', name), field('Email', email, 'Used for notifications by email, if the server sends them.')), h('div.set-actions', saveBtn));

  const cur = h('input', { type: 'password', autocomplete: 'current-password', required: true });
  const next = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8, required: true });
  const again = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8, required: true });
  const pwErr = h('div.error-text', { role: 'alert' });
  const pwBtn = h('button.btn.primary', { type: 'submit' }, 'Change password');
  const password = h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      pwErr.textContent = '';
      if (next.value !== again.value) { pwErr.textContent = 'The new passwords don’t match.'; return; }
      if (next.value.length < 8) { pwErr.textContent = 'Use at least 8 characters.'; return; }
      pwBtn.disabled = true;
      try {
        await api.patch('/api/me', { password: next.value, current_password: cur.value });
        cur.value = next.value = again.value = '';
        toast('Password changed', 'ok');
      } catch (ex) { pwErr.textContent = ex.message; } finally { pwBtn.disabled = false; }
    },
  }, h('input', { type: 'text', autocomplete: 'username', value: ctx.me.user.username, hidden: true, 'aria-hidden': 'true', tabindex: '-1' }),
  field('Current password', cur), h('div.set-grid', field('New password', next, 'At least 8 characters.'), field('Repeat new password', again)), pwErr, h('div.set-actions', pwBtn));

  // Theme: unset follows the system; light/dark override it (same key the shell uses).
  const current = (() => { try { return localStorage.getItem('ferrite-theme') || ''; } catch (_) { return ''; } })();
  const seg = h('div.fl-seg', { role: 'radiogroup', 'aria-label': 'Theme' }, [['', 'System', 'settings'], ['light', 'Light', 'sun'], ['dark', 'Dark', 'moon']].map(([v, l, ic]) =>
    h('button', {
      type: 'button', role: 'radio', 'aria-checked': String(v === current), 'aria-selected': String(v === current),
      onclick: (e) => {
        try { if (v) localStorage.setItem('ferrite-theme', v); else localStorage.removeItem('ferrite-theme'); } catch (_) { /* private mode */ }
        if (v) document.documentElement.dataset.theme = v; else delete document.documentElement.dataset.theme;
        seg.querySelectorAll('button').forEach((b) => { b.setAttribute('aria-checked', String(b === e.currentTarget)); b.setAttribute('aria-selected', String(b === e.currentTarget)); });
      },
    }, icon(ic, 'sm'), l)));

  host.append(
    card('Profile', 'How other people see you in Ferrite.', profile),
    card('Password', 'You need your current password to set a new one.', password),
    card('Appearance', 'Choose a theme for this browser. “System” follows your device setting.', seg),
    await availabilityCard());
}

// ---- Tokens & MCP ----------------------------------------------------------------------

const SCOPES = [
  ['files', 'Files'], ['talk', 'Talk'], ['boards', 'Boards'], ['docs', 'Live documents'],
  ['craft', 'Craft apps'], ['ai', 'Assistant'], ['mcp', 'MCP endpoint'], ['admin', 'Administration'],
];

function mcpJson(url, secret) {
  return JSON.stringify({ mcpServers: { ferrite: { type: 'http', url, headers: { Authorization: `Bearer ${secret || '<token>'}` } } } }, null, 2);
}

async function tokensTab(host, ctx) {
  const isAdmin = ctx.me.user.is_admin;
  const tokenList = h('div', spinner());
  const sessionList = h('div', spinner());
  const snippetHost = h('div');
  let lastSecret = null;

  function renderSnippet() {
    clear(snippetHost).append(
      h('h3', 'Over HTTP (recommended)'),
      h('p.small.muted', 'Point any MCP client that supports Streamable HTTP at this URL and send an app token as a bearer token.'),
      copyField(ctx.config.mcp_url, 'MCP URL'),
      codeBlock(mcpJson(ctx.config.mcp_url, lastSecret), 'MCP configuration'),
      lastSecret ? h('p.small.set-warn', icon('key', 'sm'), 'This snippet contains the token you just created. Keep it private.') : h('p.small.muted', 'Replace <token> with an app token that has the mcp scope (or full access).'),
      h('h3', 'Over stdio (on the server)'),
      h('p.small.muted', 'Clients running on the server itself can start Ferrite’s MCP server directly; no token needed.'),
      codeBlock(`ferrite mcp --user ${ctx.me.user.username}`, 'Command'));
  }
  renderSnippet();

  async function loadTokens() {
    try {
      const { tokens } = await api.get('/api/me/tokens');
      const apps = tokens.filter((t) => t.kind !== 'session');
      const sessions = tokens.filter((t) => t.kind === 'session');
      const revoke = async (t, what) => {
        if (!(await confirmDialog(`Revoke ${what}?`, `“${t.name}” stops working immediately.`, { confirm: 'Revoke', danger: true }))) return;
        try { await api.del(`/api/me/tokens/${t.id}`); loadTokens(); } catch (ex) { errorToast(ex); }
      };
      clear(tokenList).appendChild(apps.length
        ? h('div.set-table-wrap', h('table.table.set-table',
          h('thead', h('tr', h('th', 'Name'), h('th', 'Scopes'), h('th.fl-hide-sm', 'Last used'), h('th.fl-hide-sm', 'Expires'), h('th', h('span.sr-only', 'Actions')))),
          h('tbody', apps.map((t) => h('tr',
            h('td', h('strong', t.name), h('div.small.muted', `Created ${relTime(t.created_at)}`)),
            h('td', h('div.row.wrap', { style: { gap: '4px' } }, t.scopes.split(',').map((s) => pill(s === '*' ? 'full access' : s, s === '*' ? 'warn' : '')))),
            h('td.fl-hide-sm.muted.small', t.last_used_at ? relTime(t.last_used_at) : 'Never'),
            h('td.fl-hide-sm.muted.small', t.expires_at ? dateTime(t.expires_at) : 'Never'),
            h('td', { style: { textAlign: 'right' } }, h('button.btn.sm.danger', { type: 'button', onclick: () => revoke(t, 'token'), 'aria-label': `Revoke ${t.name}` }, 'Revoke')))))))
        : h('p.muted.small.set-empty', 'No app tokens yet.'));
      clear(sessionList).appendChild(sessions.length
        ? h('div.list.panel', sessions.map((t) => h('div.list-item', icon('screen', 'sm'),
          h('div.grow', h('div.ellipsis', { title: t.name }, t.name), h('div.small.muted', `Signed in ${relTime(t.created_at)}${t.last_used_at ? ` · active ${relTime(t.last_used_at)}` : ''}`)),
          h('button.btn.sm', { type: 'button', onclick: () => revoke(t, 'session') }, 'Sign out'))))
        : h('p.muted.small', 'No sessions.'));
    } catch (e) {
      clear(tokenList).appendChild(h('p.error-text', e.message));
    }
  }

  // Create form
  const name = h('input', { type: 'text', placeholder: 'e.g. Laptop sync, Claude Desktop', required: true, maxlength: 80 });
  const full = h('input', { type: 'checkbox', checked: true, value: '*' });
  const boxes = SCOPES.filter(([s]) => s !== 'admin' || isAdmin).map(([s, l]) => {
    const b = h('input', { type: 'checkbox', value: s, disabled: true });
    return { s, b, el: h('label.check.set-scope', b, h('span', l), h('code.small', s)) };
  });
  full.addEventListener('change', () => boxes.forEach(({ b }) => { b.disabled = full.checked; }));
  const expiry = h('select', h('option', { value: '' }, 'Never'), h('option', { value: '30' }, '30 days'), h('option', { value: '90' }, '90 days'), h('option', { value: '365' }, '1 year'));
  const err = h('div.error-text', { role: 'alert' });
  const createBtn = h('button.btn.primary', { type: 'submit' }, icon('plus', 'sm'), 'Create token');
  const form = h('form.set-token-form', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      const scopes = full.checked ? ['*'] : boxes.filter(({ b }) => b.checked).map(({ s }) => s);
      if (!scopes.length) { err.textContent = 'Pick at least one scope.'; return; }
      createBtn.disabled = true;
      try {
        const res = await api.post('/api/me/tokens', { name: name.value.trim(), scopes, expires_in_days: expiry.value ? Number(expiry.value) : null });
        lastSecret = res.secret;
        name.value = '';
        renderSnippet();
        loadTokens();
        await modal({
          title: 'Your new app token',
          body: h('div',
            h('p', 'Copy it now: for your security it won’t be shown again.'),
            copyField(res.secret, 'Token'),
            h('p.small.muted', 'Use it as the password in WebDAV/sync clients (with your username), or as a bearer token for the API and MCP.')),
          actions: [{ label: 'Done', kind: 'primary', value: true }],
        });
      } catch (ex) { err.textContent = ex.message; } finally { createBtn.disabled = false; }
    },
  },
  h('div.set-grid', field('Name', name), field('Expires', expiry)),
  h('label', 'Access'),
  h('label.check', full, h('span', h('strong', 'Full access'), h('span.muted', ' — everything you can do'))),
  h('div.set-scopes', boxes.map((b) => b.el)),
  err, h('div.set-actions', createBtn));

  // Ferrite's own tools, grouped by name prefix.
  const toolsHost = h('div', spinner());
  api.get('/api/mcp/tools').then(({ tools }) => {
    const groups = new Map();
    const LABEL = { files: 'Files', trash: 'Trash', talk: 'Talk', boards: 'Boards', docs: 'Live documents', sheet: 'Live documents', mindmap: 'Live documents', canvas: 'Live documents', craft: 'Craft apps', summarize: 'Quick actions', board: 'Quick actions' };
    for (const t of tools) {
      const g = LABEL[t.name.split('_')[0]] || 'General';
      if (!groups.has(g)) groups.set(g, []);
      groups.get(g).push(t);
    }
    clear(toolsHost).appendChild(h('div.set-tools', [...groups].map(([g, list]) => h('details.set-toolgroup', { open: g === 'Files' },
      h('summary', h('strong', g), h('span.badge.muted', String(list.length))),
      h('div.list', list.map((t) => h('div.list-item.set-tool',
        h('div.grow', h('div.row.wrap', { style: { gap: '6px' } }, h('code', t.name), t.title ? h('span.small.muted', t.title) : null),
          h('div.small.muted', t.description)),
        t.annotations?.readOnlyHint ? pill('read-only', 'ok') : t.annotations?.destructiveHint ? pill('destructive', 'danger') : pill('writes', 'warn'))))))));
  }).catch((e) => { clear(toolsHost).appendChild(h('p.error-text', e.message)); });

  loadTokens();
  host.append(
    card('App tokens', 'Tokens let sync clients, scripts and MCP clients act as you, limited to the scopes you choose. Revoke one to cut it off immediately.', [form, h('hr'), tokenList]),
    card('Connect an MCP client', 'Use Ferrite from Claude Desktop, Claude Code, IDEs and other MCP clients: they get the same tools as the built-in assistant, with your permissions.', snippetHost),
    card('Ferrite’s tools', 'What MCP clients (and the assistant) can do. Read-only tools never change anything; others need the matching token scope.', toolsHost),
    card('Signed-in sessions', 'Browsers where you are signed in.', sessionList));
}

// ---- Craft apps ------------------------------------------------------------------------

async function craftTab(host, ctx) {
  const { apps } = await api.get('/api/craft/apps');
  const installed = apps.filter((a) => a.installed).length;
  host.append(
    h('div.card.pad.set-intro',
      h('div.row.wrap', { style: { gap: '14px', alignItems: 'flex-start' } },
        h('span.fl-craft-mark', icon('craft')),
        h('div.grow',
          h('h2', 'ArtCraft Crafting Apps'),
          h('p.muted.small', 'Native, open-source desktop apps that replace the usual office and creative suites. Ferrite recognises their files, locks a file while an app edits it, and keeps a version of every save.'),
          h('p.small', h('strong', 'Two ways to use them: '), '(1) install an app on your computer and open files from your WebDAV-synced Ferrite folder; (2) install it on the server so the assistant and MCP clients can open documents with ', h('code', 'craft_open'), ' and work inside them.')),
        pill(`${installed} of ${apps.length} on this server`, installed ? 'ok' : ''))),
    h('div.set-craft', apps.map((a) => h('div.card.pad.set-craft-app',
      h('div.row', { style: { gap: '10px' } },
        h('span.fl-craft-mark', icon('craft')),
        h('div.grow', h('strong', a.name), h('div.small.muted', `Replaces ${a.replaces}`))),
      h('p.small', a.summary),
      h('div.small.muted', 'Native formats'),
      h('div.set-exts', a.native.map(([ext]) => h('span.set-ext.native', `.${ext}`))),
      a.opens.length ? [h('div.small.muted', 'Also opens'), h('div.set-exts', a.opens.map((ext) => h('span.set-ext', `.${ext}`)))] : null,
      h('div.set-craft-foot',
        a.installed
          ? h('div', pill('Installed on server', 'ok'), h('code.small.ellipsis', { title: a.command, style: { display: 'block', marginTop: '4px' } }, a.command))
          : h('div.small.muted', pill('Not on server'), h('div', { style: { marginTop: '4px' } }, 'Needs ', h('code', a.cli), ' on the server’s PATH')),
        /^https:\/\//.test(a.repo) ? h('a.btn.sm.ghost', { href: a.repo, target: '_blank', rel: 'noopener noreferrer' }, icon('external', 'sm'), 'Repository') : null)))));
}

// ---- About -----------------------------------------------------------------------------

async function aboutTab(host, ctx) {
  const dav = `${ctx.config.public_url}/remote.php/dav/files/${encodeURIComponent(ctx.me.user.username)}/`;
  host.append(
    card('Ferrite', 'A self-hosted collaboration server: files, chat and calls, boards, live documents and an assistant — in one Rust binary.',
      h('dl.set-dl',
        h('dt', 'Version'), h('dd', ctx.config.version),
        h('dt', 'Server address'), h('dd', h('code', ctx.config.public_url)),
        h('dt', 'Signed in as'), h('dd', `${ctx.me.user.display_name} (@${ctx.me.user.username})`))),
    card('WebDAV', 'Connect desktop sync clients, file managers and mobile apps. Sign in with your username and an app token.',
      [copyField(dav, 'WebDAV URL'), h('a.btn.sm', { href: '/settings/tokens', onclick: (e) => { e.preventDefault(); ctx.navigate('/settings/tokens'); } }, icon('key', 'sm'), 'Create an app token')]),
    card('MCP', 'Connect AI clients to Ferrite’s tools.', [copyField(ctx.config.mcp_url, 'MCP URL')]));
}
