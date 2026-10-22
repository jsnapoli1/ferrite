// Boards: the list of kanban boards (/boards) and a single board (/boards/:id, in
// boards-kanban.js). Cards open in a modal (boards-card.js), also via ?card=<id>.

import { api, q } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, modal, errorToast, debounce, emptyState } from '../ui.js';
import { BOARD_COLORS, colorSwatches } from './boards-dialogs.js';
import { withVars } from './editor-common.js';

export default async function render(root, ctx) {
  if (ctx.params.id) {
    const mod = await import('./boards-kanban.js');
    return mod.renderBoard(root, ctx);
  }
  return renderList(root, ctx);
}

async function renderList(root, ctx) {
  ctx.setTitle('Boards');
  let showArchived = ctx.query.archived === '1';
  const grid = h('div.boards-grid');
  const archivedToggle = h('input', { type: 'checkbox', checked: showArchived, onchange: () => { showArchived = archivedToggle.checked; history.replaceState({}, '', showArchived ? '/boards?archived=1' : '/boards'); load(); } });

  async function load() {
    let boards;
    try {
      ({ boards } = await api.get(`/api/boards${q({ archived: showArchived ? 'true' : '' })}`));
    } catch (e) {
      errorToast(e);
      return;
    }
    clear(grid);
    if (!boards.length) {
      grid.appendChild(emptyState('boards', 'No boards yet', 'Boards organize work in columns of cards that link to files, mind maps and conversations.',
        h('button.btn.primary', { onclick: create }, icon('plus'), 'New board')));
      grid.classList.add('is-empty');
      return;
    }
    grid.classList.remove('is-empty');
    boards.sort((a, b) => (a.archived - b.archived) || a.title.localeCompare(b.title));
    for (const b of boards) {
      const href = `/boards/${b.id}`;
      grid.appendChild(withVars(h('a.board-tile', {
        href, dataset: { id: b.id }, class: b.archived ? 'archived' : '',
        onclick: (e) => { if (e.metaKey || e.ctrlKey) return; e.preventDefault(); ctx.navigate(href); },
      },
      h('span.board-tile-stripe'),
      h('span.board-tile-body',
        h('span.board-tile-title', b.title),
        h('span.board-tile-meta',
          b.owner_id === ctx.me.user.id ? h('span.small.muted', 'Owned by you') : h('span.pill', icon('users', 'sm'), 'Shared with you'),
          b.archived && h('span.pill.warn', 'Archived')))), { '--board-color': b.color }));
    }
    grid.appendChild(h('button.board-tile.board-tile-new', { type: 'button', onclick: create }, icon('plus'), 'New board'));
  }

  async function create() {
    let color = BOARD_COLORS[Math.floor(Math.random() * BOARD_COLORS.length)];
    const title = h('input', { type: 'text', placeholder: 'e.g. Product launch', 'aria-label': 'Board title', required: true });
    const cols = h('textarea', { rows: 4, 'aria-label': 'Columns, one per line' }, 'To do\nDoing\nDone');
    const sw = colorSwatches(color, (c) => { color = c; });
    const result = await modal({
      title: 'New board',
      body: h('form.board-new', { onsubmit: (e) => { e.preventDefault(); e.target.closest('.modal').querySelector('.modal-foot .btn.primary').click(); } },
        h('label', 'Title'), title,
        h('label', 'Color'), sw,
        h('label', 'Columns ', h('span.muted', '(one per line)')), cols),
      actions: [
        { label: 'Cancel', value: null },
        {
          label: 'Create board', kind: 'primary', run: async () => {
            if (!title.value.trim()) { title.focus(); return undefined; }
            const stacks = cols.value.split('\n').map((s) => s.trim()).filter(Boolean);
            const { board } = await api.post('/api/boards', { title: title.value.trim(), color, stacks: stacks.length ? stacks : undefined });
            return board;
          },
        },
      ],
    });
    if (result) ctx.navigate(`/boards/${result.id}`);
  }

  root.appendChild(h('div.page.boards-page',
    h('div.page-head',
      h('h1', 'Boards'),
      h('div.actions',
        h('label.check', archivedToggle, 'Show archived'),
        h('button.btn.primary', { onclick: create }, icon('plus'), 'New board'))),
    grid));
  await load();
  const reload = debounce(load, 250);
  const off = live.on('boards.changed', reload);
  return () => off();
}
