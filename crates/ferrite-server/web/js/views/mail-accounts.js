// Mail accounts beyond setup: sharing an account with other users (delegation), the storage
// quota bar, connecting Google/Microsoft accounts through their consent screen, and the
// Mail configuration the server publishes (/api/mail/config).

import { api, q } from '../api.js';
import { h, clear, icon, avatar, toast, errorToast, modal, bytes, relTime, spinner, debounce } from '../ui.js';

let configPromise = null;

/** The server's Mail configuration (OAuth providers, whether users may add accounts, …). */
export function mailConfig(refresh = false) {
  if (refresh || !configPromise) {
    configPromise = api.get('/api/mail/config').catch((e) => { configPromise = null; throw e; });
  }
  return configPromise;
}

/** Providers whose OAuth client the administrator configured. */
export async function oauthProviders() {
  try {
    return (await mailConfig()).oauth.filter((p) => p.configured);
  } catch (_) {
    return [];
  }
}

/** Sends the browser to the provider's consent screen; it comes back to /mail. */
export async function startOAuth(provider, { email, name, accountId } = {}) {
  const { url } = await api.post('/api/mail/oauth/start', { provider, email, name, account_id: accountId || undefined });
  location.assign(url);
}

export const PROVIDER_LABEL = { google: 'Google', microsoft: 'Microsoft' };

/** "Used 1.2 GB of 10 GB" with a bar, or null when the server reports no quota. */
export function quotaBar(a) {
  if (!a.quota_limit) return null;
  const used = a.quota_used || 0;
  const pct = Math.min(100, Math.round((used / a.quota_limit) * 100));
  return h(`div.ml-quota${pct >= 90 ? '.full' : ''}`, { title: `${pct}% of the mailbox storage is used` },
    h('div.progress', { role: 'progressbar', 'aria-valuemin': '0', 'aria-valuemax': '100', 'aria-valuenow': String(pct), 'aria-label': `Storage of ${a.email}` },
      h('div', { style: { width: `${pct}%` } })),
    h('div.small.muted', `${bytes(used)} of ${bytes(a.quota_limit)} used`));
}

/** Who may do what with an account, explained for its settings dialog. */
export function accountNotice(a) {
  if (a.delegated) {
    return h('div.ml-notice.small', icon('users', 'sm'),
      h('span', `${a.owner_name || 'Its owner'} shared this account with you. You can read, send, move and delete its mail and change the sender name and signature; only the owner can change how it signs in or remove it.`));
  }
  if (a.provisioning_id) {
    return h('div.ml-notice.small', icon('lock', 'sm'),
      h('span', 'Your administrator set up this account. You can change the sender name, signature and Sent copy; the servers and sign-in are managed for you.'));
  }
  return null;
}

/** Owner's dialog for sharing an account: list, add and revoke delegates. */
export async function shareDialog(account) {
  const list = h('div.list.panel', spinner());
  const listId = `ml-share-users-${account.id}`;
  const dl = h('datalist', { id: listId });
  const who = h('input', { type: 'text', list: listId, id: 'ml-share-user', placeholder: 'Username', 'aria-label': 'Username to share with', autocomplete: 'off', spellcheck: 'false' });
  const err = h('div.error-text', { role: 'alert' });
  who.addEventListener('input', debounce(async () => {
    try {
      const { users } = await api.get(`/api/users${q({ q: who.value.trim() })}`);
      clear(dl).append(...users.map((u) => h('option', { value: u.username }, u.display_name)));
    } catch (_) { /* suggestions only */ }
  }, 200));

  async function refresh() {
    const { delegations } = await api.get(`/api/mail/accounts/${encodeURIComponent(account.id)}/delegations`);
    clear(list).append(...(delegations.length
      ? delegations.map((d) => h('div.list-item.ml-share-row', { dataset: { user: d.username } },
        avatar(d.display_name, { size: 'sm', seed: d.user_id }),
        h('div.grow', h('div', d.display_name), h('div.small.muted', `@${d.username} · since ${relTime(d.created_at)}`)),
        h('button.btn.sm.ghost.danger', {
          type: 'button', 'aria-label': `Stop sharing with ${d.username}`, title: 'Stop sharing',
          onclick: async () => {
            try {
              await api.del(`/api/mail/delegations/${encodeURIComponent(d.id)}`);
              toast(`${account.email} is no longer shared with @${d.username}`, 'ok', { timeout: 2500 });
              refresh().catch(errorToast);
            } catch (e) { errorToast(e); }
          },
        }, icon('x', 'sm'), h('span', 'Stop sharing'))))
      : [h('div.list-item.muted.small', 'Not shared with anyone.')]));
  }
  refresh().catch((e) => { clear(list).appendChild(h('p.error-text', e.message)); });

  const form = h('form.row.ml-share-form', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      const user = who.value.trim();
      if (!user) { who.focus(); return; }
      try {
        await api.post(`/api/mail/accounts/${encodeURIComponent(account.id)}/delegations`, { user });
        who.value = '';
        toast(`Shared ${account.email} with @${user}`, 'ok', { timeout: 2500 });
        await refresh();
      } catch (ex) { err.textContent = ex.message; }
    },
  }, who, dl, h('button.btn.primary', { type: 'submit' }, icon('share', 'sm'), 'Share'));

  await modal({
    title: `Share ${account.email}`,
    body: h('div.ml-share',
      h('p.small.muted', 'People you share this account with see it in their Mail. They can read, send, move and delete its mail as you, and change its signature; they cannot remove it, change how it signs in, or share it further.'),
      list, form, err),
    actions: [{ label: 'Done', kind: 'primary', value: true }],
  });
}
