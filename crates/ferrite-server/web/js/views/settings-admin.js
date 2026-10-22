// Settings → Administration: server stats, users and groups (admins only).

import { api, q } from '../api.js';
import { h, clear, icon, avatar, modal, toast, errorToast, confirmDialog, bytes, relTime, spinner, debounce } from '../ui.js';
import { card, field, pill, uid } from './settings-common.js';

const GB = 1024 ** 3;

function quotaInput(bytesValue) {
  return h('input', { type: 'number', min: '0', step: '0.5', placeholder: 'Unlimited', value: bytesValue ? String(+(bytesValue / GB).toFixed(2)) : '' });
}
const quotaValue = (input) => (input.value.trim() === '' ? null : Math.round(Number(input.value) * GB));

export async function adminTab(host, ctx) {
  const statsHost = h('div', spinner());
  const usersHost = h('div', spinner());
  const groupsHost = h('div', spinner());

  async function loadStats() {
    try {
      const s = await api.get('/api/admin/stats');
      const saved = Math.max(0, s.logical_bytes - s.stored_bytes);
      const tile = (label, value, sub) => h('div.set-stat', h('div.small.muted', label), h('div.set-stat-value', value), sub ? h('div.small.muted', sub) : null);
      clear(statsHost).appendChild(h('div.set-stats',
        tile('Users', String(s.users), `${s.online_users} online now`),
        tile('Files', String(s.files)),
        tile('Storage used', bytes(s.stored_bytes), `${bytes(s.logical_bytes)} before deduplication`),
        tile('Saved by deduplication', bytes(saved), s.logical_bytes ? `${Math.round((saved / s.logical_bytes) * 100)}% of file data` : 'No files yet'),
        tile('Memory (RSS)', s.rss_bytes ? bytes(s.rss_bytes) : '—', 'Server process'),
        tile('Live documents', String(s.live_documents), 'Open for editing'),
        tile('Conversations', String(s.rooms), `${s.messages} messages`),
        tile('Boards', String(s.boards))));
    } catch (e) {
      clear(statsHost).appendChild(h('p.error-text', e.message));
    }
  }

  // ---- users ----
  async function loadUsers() {
    try {
      const { users } = await api.get('/api/admin/users');
      clear(usersHost).appendChild(h('div.set-table-wrap', h('table.table.set-table',
        h('thead', h('tr', h('th', 'User'), h('th.fl-hide-sm', 'Storage'), h('th.fl-hide-sm', 'Joined'), h('th', h('span.sr-only', 'Actions')))),
        h('tbody', users.map(({ user: u, quota }) => h(`tr${u.disabled ? '.set-disabled' : ''}`,
          h('td', h('div.row', { style: { gap: '10px' } }, avatar(u.display_name, { size: 'sm', seed: u.id }),
            h('div.grow', h('div.row.wrap', { style: { gap: '6px' } }, h('strong', u.display_name),
              u.is_admin ? pill('admin', 'info') : null, u.disabled ? pill('disabled', 'danger') : null, u.id === ctx.me.user.id ? pill('you') : null),
            h('div.small.muted.ellipsis', `@${u.username}${u.email ? ` · ${u.email}` : ''}`)))),
          h('td.fl-hide-sm.small', bytes(quota.used), quota.limit ? h('span.muted', ` of ${bytes(quota.limit)}`) : h('span.muted', ' · no limit'),
            quota.limit ? h('div.progress.set-mini', h('div', { style: { width: `${Math.min(100, (quota.used / quota.limit) * 100)}%` } })) : null),
          h('td.fl-hide-sm.small.muted', relTime(u.created_at)),
          h('td', { style: { textAlign: 'right', whiteSpace: 'nowrap' } },
            h('button.btn.sm', { type: 'button', onclick: () => editUser(u), 'aria-label': `Edit ${u.username}` }, icon('edit', 'sm'), h('span.fl-hide-sm', 'Edit')),
            u.id !== ctx.me.user.id ? h('button.btn.sm.ghost.danger', { type: 'button', onclick: () => deleteUser(u), 'aria-label': `Delete ${u.username}`, title: 'Delete' }, icon('trash', 'sm')) : null)))))));
    } catch (e) {
      clear(usersHost).appendChild(h('p.error-text', e.message));
    }
  }

  async function createUser() {
    const username = h('input', { type: 'text', required: true, autocomplete: 'off' });
    const name = h('input', { type: 'text' });
    const email = h('input', { type: 'email' });
    const password = h('input', { type: 'password', autocomplete: 'new-password', minlength: 8, required: true });
    const admin = h('input', { type: 'checkbox' });
    const quota = quotaInput(null);
    const form = h('form', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
      h('div.set-grid', field('Username', username), field('Display name', name)),
      field('Email', email),
      h('div.set-grid', field('Password', password, 'At least 8 characters.'), field('Storage quota (GB)', quota, 'Leave empty for no limit.')),
      h('label.check', admin, h('span', 'Administrator')),
      h('button', { type: 'submit', hidden: true }));
    await modal({
      title: 'New user',
      body: form,
      actions: [{ label: 'Cancel', value: null }, {
        label: 'Create user', kind: 'primary',
        run: async () => {
          await api.post('/api/admin/users', {
            username: username.value.trim(), display_name: name.value.trim() || null, email: email.value.trim() || null,
            password: password.value, is_admin: admin.checked, quota_bytes: quotaValue(quota),
          });
          toast(`Created @${username.value.trim()}`, 'ok');
          loadUsers();
          return true;
        },
      }],
    });
  }

  async function editUser(u) {
    const name = h('input', { type: 'text', value: u.display_name });
    const email = h('input', { type: 'email', value: u.email || '' });
    const password = h('input', { type: 'password', autocomplete: 'new-password', placeholder: 'Leave empty to keep' });
    const admin = h('input', { type: 'checkbox', checked: u.is_admin, disabled: u.id === ctx.me.user.id });
    const disabled = h('input', { type: 'checkbox', checked: u.disabled, disabled: u.id === ctx.me.user.id });
    const quota = quotaInput(u.quota_bytes);
    const form = h('form', { onsubmit: (e) => { e.preventDefault(); form.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
      h('div.set-grid', field('Display name', name), field('Email', email)),
      h('div.set-grid', field('Reset password', password), field('Storage quota (GB)', quota, 'Leave empty for no limit.')),
      h('label.check', admin, h('span', 'Administrator')),
      h('label.check', disabled, h('span', 'Disabled (can’t sign in; tokens stop working)')),
      h('button', { type: 'submit', hidden: true }));
    await modal({
      title: `Edit @${u.username}`,
      body: form,
      actions: [{ label: 'Cancel', value: null }, {
        label: 'Save', kind: 'primary',
        run: async () => {
          const body = { display_name: name.value.trim(), email: email.value.trim(), is_admin: admin.checked, disabled: disabled.checked, quota_bytes: quotaValue(quota) };
          if (password.value) body.password = password.value;
          await api.patch(`/api/admin/users/${u.id}`, body);
          toast('Saved', 'ok', { timeout: 2000 });
          loadUsers();
          return true;
        },
      }],
    });
  }

  async function deleteUser(u) {
    if (!(await confirmDialog(`Delete @${u.username}?`, `${u.display_name}’s account, files and tokens are removed permanently. Consider disabling the account instead.`, { confirm: 'Delete user', danger: true }))) return;
    try { await api.del(`/api/admin/users/${u.id}`); toast('User deleted', 'ok'); loadUsers(); loadStats(); } catch (e) { errorToast(e); }
  }

  // ---- groups ----
  async function loadGroups() {
    try {
      const { groups } = await api.get('/api/admin/groups');
      const members = await Promise.all(groups.map((g) => api.get(`/api/admin/groups/${g.id}/members`).then((r) => r.members).catch(() => [])));
      clear(groupsHost).appendChild(groups.length
        ? h('div.list.panel', groups.map((g, i) => h('div.list-item',
          h('span.set-provider-icon', icon('users', 'sm')),
          h('div.grow', h('strong', g.name), h('div.small.muted', `${members[i].length} member${members[i].length === 1 ? '' : 's'}`)),
          h('div.set-avatars.fl-hide-sm', members[i].slice(0, 5).map((m) => avatar(m.display_name, { size: 'sm', seed: m.id }))),
          h('button.btn.sm', { type: 'button', onclick: () => manageGroup(g) }, 'Members'),
          h('button.btn.sm.ghost.danger', {
            type: 'button', 'aria-label': `Delete group ${g.name}`, title: 'Delete group',
            onclick: async () => {
              if (!(await confirmDialog(`Delete group “${g.name}”?`, 'Shares with this group stop working for its members.', { confirm: 'Delete group', danger: true }))) return;
              try { await api.del(`/api/admin/groups/${g.id}`); loadGroups(); } catch (e) { errorToast(e); }
            },
          }, icon('trash', 'sm')))))
        : h('p.muted.small.set-empty', 'No groups yet. Groups make it easy to share with a team.'));
    } catch (e) {
      clear(groupsHost).appendChild(h('p.error-text', e.message));
    }
  }

  async function manageGroup(g) {
    const list = h('div.list.panel', spinner());
    const listId = uid('group-users');
    const dl = h('datalist', { id: listId });
    const who = h('input', { type: 'text', list: listId, placeholder: 'Username', 'aria-label': 'Username to add', autocomplete: 'off' });
    who.addEventListener('input', debounce(async () => {
      try {
        const { users } = await api.get(`/api/users${q({ q: who.value.trim() })}`);
        clear(dl).append(...users.map((u) => h('option', { value: u.username }, u.display_name)));
      } catch (_) { /* suggestions only */ }
    }, 200));
    async function refresh() {
      const { members } = await api.get(`/api/admin/groups/${g.id}/members`);
      clear(list).append(...(members.length ? members.map((m) => h('div.list-item', avatar(m.display_name, { size: 'sm', seed: m.id }),
        h('div.grow', h('div', m.display_name), h('div.small.muted', `@${m.username}`)),
        h('button.btn.sm.ghost', {
          type: 'button', 'aria-label': `Remove ${m.username}`,
          onclick: async () => { try { await api.del(`/api/admin/groups/${g.id}/members/${m.id}`); refresh(); } catch (e) { errorToast(e); } },
        }, icon('x', 'sm')))) : [h('div.list-item.muted.small', 'No members yet.')]));
    }
    refresh().catch(errorToast);
    const form = h('form.row', {
      style: { marginTop: '12px' },
      onsubmit: async (e) => {
        e.preventDefault();
        if (!who.value.trim()) return;
        try { await api.post(`/api/admin/groups/${g.id}/members`, { username: who.value.trim() }); who.value = ''; refresh(); } catch (ex) { errorToast(ex); }
      },
    }, who, dl, h('button.btn.primary', { type: 'submit' }, 'Add'));
    await modal({ title: `Members of ${g.name}`, body: h('div', list, form), actions: [{ label: 'Done', kind: 'primary', value: true }] });
    loadGroups();
  }

  const groupName = h('input', { type: 'text', placeholder: 'New group name', 'aria-label': 'New group name', required: true });
  const groupForm = h('form.row', {
    style: { marginTop: '12px' },
    onsubmit: async (e) => {
      e.preventDefault();
      try { await api.post('/api/admin/groups', { name: groupName.value.trim() }); groupName.value = ''; loadGroups(); } catch (ex) { errorToast(ex); }
    },
  }, groupName, h('button.btn', { type: 'submit' }, icon('plus', 'sm'), 'Create group'));

  host.append(
    card('Server', 'At a glance.', statsHost, h('button.btn.sm.ghost', { type: 'button', onclick: loadStats, 'aria-label': 'Refresh stats' }, icon('refresh', 'sm'))),
    card('Users', 'People with an account on this server.', usersHost, h('button.btn.primary.sm', { type: 'button', onclick: createUser }, icon('plus', 'sm'), 'New user')),
    card('Groups', 'Share files with a whole team at once.', [groupsHost, groupForm]));
  await Promise.all([loadStats(), loadUsers(), loadGroups()]);
}
