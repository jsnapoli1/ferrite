// Home: what needs attention — unread conversations, my cards, recent files.

import { api } from '../api.js';
import { h, icon, relTime, fileIcon, avatar, dateTime } from '../ui.js';

export default async function render(root, ctx) {
  ctx.setTitle('Home');
  const [rooms, cards, recent, ai] = await Promise.all([
    api.get('/api/talk/rooms'),
    api.get('/api/boards/my-cards'),
    api.get('/api/files/recent'),
    api.get('/api/ai/status'),
  ]);
  const unread = rooms.rooms.filter((r) => r.unread > 0);
  const name = ctx.me.user.display_name.split(' ')[0];
  const go = (href) => (e) => { e.preventDefault(); ctx.navigate(href); };
  const section = (title, href, body) => h('div.card.pad',
    h('div.row', { style: { marginBottom: '10px' } }, h('h3.grow', { style: { margin: 0 } }, title), href && h('a.small', { href, onclick: go(href) }, 'Open')),
    body);
  root.appendChild(h('div.page',
    h('div.page-head', h('h1', `Hello, ${name}`),
      h('div.actions',
        h('a.btn', { href: '/files', onclick: go('/files') }, icon('upload'), 'Upload'),
        h('a.btn', { href: '/talk', onclick: go('/talk') }, icon('talk'), 'New conversation'),
        h('a.btn.primary', { href: '/assistant', onclick: go('/assistant') }, icon('assistant'), 'Ask the assistant'))),
    !ai.configured && ctx.me.user.is_admin && h('div.card.pad', { style: { marginBottom: '14px', borderColor: 'var(--accent)' } },
      h('strong', 'Connect an AI model. '), 'Point Ferrite at a local model server (Ollama, llama.cpp, vLLM, LM Studio) or Claude in ',
      h('a', { href: '/settings/ai', onclick: go('/settings/ai') }, 'Settings → AI'), '. Everything else works without it.'),
    h('div', { style: { display: 'grid', gap: '14px', gridTemplateColumns: 'repeat(auto-fit, minmax(300px, 1fr))' } },
      section('Unread conversations', '/talk', unread.length
        ? h('div.list', unread.slice(0, 8).map((r) => h('a.list-item.clickable', { href: `/talk/${r.token}`, onclick: go(`/talk/${r.token}`) },
          avatar(r.display_name, { size: 'sm', seed: r.id }),
          h('div.grow', h('div.ellipsis', r.display_name), r.last_message && h('div.small.muted.ellipsis', `${r.last_message.actor_name}: ${r.last_message.body}`)),
          h('span.badge', String(r.unread)))))
        : h('p.muted', 'You are all caught up.')),
      section('My cards', '/boards', cards.cards.length
        ? h('div.list', cards.cards.slice(0, 8).map((c) => h('a.list-item.clickable', { href: `/boards/${c.board_id}?card=${c.id}`, onclick: go(`/boards/${c.board_id}?card=${c.id}`) },
          icon('boards'), h('div.grow.ellipsis', c.title),
          c.due_at && h(`span.pill${new Date(c.due_at) < new Date() ? '.danger' : ''}`, icon('calendar', 'sm'), dateTime(c.due_at)))))
        : h('p.muted', 'Nothing assigned to you.')),
      section('Recent files', '/files', recent.files.length
        ? h('div.list', recent.files.slice(0, 8).map(({ node, path }) => {
          const href = node.live_editor || ['md', 'txt', 'csv', 'fmind', 'fcanvas'].includes(node.name.split('.').pop().toLowerCase()) ? `/edit/${node.id}` : `/files/${node.parent_id}`;
          return h('a.list-item.clickable', { href, onclick: go(href) }, icon(fileIcon(node)), h('div.grow', h('div.ellipsis', node.name), h('div.small.muted.ellipsis', path)), h('span.small.muted', relTime(node.modified_at)));
        }))
        : h('p.muted', 'No files yet.')))));
}
