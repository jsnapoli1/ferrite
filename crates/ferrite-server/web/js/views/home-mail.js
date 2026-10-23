// Home widgets for mail: unread mail and important mail across the user's inboxes (shown only
// when a mail account is connected). Loaded after the rest of Home, so they never hold it up.

import { api, q } from '../api.js';
import { h, icon, avatar, relTime } from '../ui.js';

const link = (m) => `/mail${q({ m: m.id, mid: m.message_id })}`;

function messageItem(m, go) {
  const who = m.from_name || m.from_addr || '(unknown sender)';
  return h(`a.list-item.clickable.home-mail${m.seen ? '' : '.unread'}`, { href: link(m), onclick: go(link(m)) },
    avatar(who, { size: 'sm', seed: m.from_addr }),
    h('div.grow',
      h('div.ellipsis', m.seen ? who : h('strong', who)),
      h('div.small.muted.ellipsis', m.subject || '(no subject)')),
    h('span.small.muted', relTime(m.date)));
}

/** Adds the mail cards to Home's grid. */
export async function mailWidgets(grid, { go, section }) {
  if (!grid) return;
  let w;
  try {
    w = await api.get('/api/mail/widgets');
  } catch (_) {
    return; // No mail scope or no mail: Home works without these cards.
  }
  if (!w.accounts || !grid.isConnected) return;
  const title = (text, n) => h('span', text, n > 0 && h('span.badge', { style: { marginLeft: '8px', verticalAlign: '2px' } }, String(n)));
  grid.appendChild(section(title('Unread mail', w.unread_count), '/mail', w.unread.length
    ? h('div.list', w.unread.map((m) => messageItem(m, go)))
    : h('p.muted', h('span', icon('check', 'sm')), ' No unread mail.')));
  grid.appendChild(section(title('Important mail', w.important_unread), '/mail?view=priority', w.important.length
    ? h('div.list', w.important.map((m) => messageItem(m, go)))
    : h('p.muted', 'Nothing marked important. Mail from people you read and answer shows up here.')));
}
