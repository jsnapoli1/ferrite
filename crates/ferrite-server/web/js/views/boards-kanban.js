// A single kanban board: columns (stacks) of cards with drag and drop, inline card creation,
// column management and a board menu. Re-fetches on `boards.changed` for this board, keeping
// scroll positions, focus, open composers and the open card modal.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, menu, toast, errorToast, confirmDialog, promptDialog, debounce } from '../ui.js';
import { colorDialog, membersDialog, labelsDialog, labelChip } from './boards-dialogs.js';
import { openCardModal } from './boards-card.js';
import { helpButton, inField } from './editor-common.js';

const SHORTCUTS = [
  ['Enter', 'Open the focused card'],
  ['↑ / ↓', 'Focus the previous / next card'],
  ['← / →', 'Focus a card in the neighbouring column'],
  ['Alt+↑ / Alt+↓', 'Move the card up / down'],
  ['Alt+← / Alt+→', 'Move the card to the previous / next column'],
  ['M', 'Card menu (“Move to…”, done, archive)'],
  ['/', 'Filter cards'],
  ['Esc', 'Close the card / cancel'],
];

/** Formats a due date compactly: "Oct 21, 14:30" (omits the time at midnight). */
export function dueText(ts) {
  const d = new Date(ts);
  const sameYear = d.getFullYear() === new Date().getFullYear();
  const date = d.toLocaleDateString([], { month: 'short', day: 'numeric', year: sameYear ? undefined : 'numeric' });
  return d.getHours() || d.getMinutes() ? `${date}, ${d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })}` : date;
}

/** pill class for a due date: done → ok, overdue → danger, within a day → warn. */
export function dueClass(card) {
  if (!card.due_at) return '';
  if (card.done) return 'ok';
  const t = new Date(card.due_at).getTime();
  if (t < Date.now()) return 'danger';
  if (t - Date.now() < 86400000) return 'warn';
  return '';
}

export async function renderBoard(root, ctx) {
  const boardId = ctx.params.id;
  const me = ctx.me.user;
  let board = (await api.get(`/api/boards/${encodeURIComponent(boardId)}`)).board;
  let disposed = false;
  let dragging = null; // {kind:'card'|'stack', id, el}
  let refreshPending = false;
  let filter = '';
  const cardEls = new Map(); // card id -> {sig, el}
  const footers = new Map(); // stack id -> composer footer element (kept across renders)
  let cardModal = null;
  let dialog = null; // open members/labels dialog controller

  const placeholder = h('div.kcard-placeholder', { 'aria-hidden': 'true' });
  const stackPlaceholder = h('div.stack-placeholder', { 'aria-hidden': 'true' });

  // ---- header ----
  const titleEl = h('h1.kanban-title');
  const readOnlyPill = h('span.pill', { title: 'You can view this board and comment on cards, but not change it.' }, icon('lock', 'sm'), 'Read only');
  const membersEl = h('button.btn.ghost.kanban-members', { 'aria-label': 'Members', title: 'Members', onclick: () => openMembers() });
  const filterInput = h('input.kanban-filter', {
    type: 'search', placeholder: 'Filter cards', 'aria-label': 'Filter cards',
    oninput: () => { filter = filterInput.value.trim().toLowerCase(); applyFilter(); },
  });
  const addColBtn = h('button.btn', { onclick: () => addStack() }, icon('plus'), 'Column');
  const moreBtn = h('button.btn.ghost.icon', { 'aria-label': 'Board menu', title: 'Board menu', onclick: () => boardMenu() }, icon('more'));
  const help = helpButton(SHORTCUTS, 'Board shortcuts');
  const head = h('div.kanban-head',
    h('a.btn.ghost.icon', { href: '/boards', 'aria-label': 'All boards', title: 'All boards', onclick: (e) => { e.preventDefault(); ctx.navigate('/boards'); } }, icon('arrow-left')),
    h('span.board-dot'), titleEl, readOnlyPill,
    h('div.spacer'),
    filterInput, membersEl, addColBtn, help, moreBtn);
  const kanban = h('div.kanban', { role: 'list', 'aria-label': 'Columns' });
  const page = h('div.page-full.kanban-page', head, kanban);
  root.appendChild(page);

  // ---- rendering ----
  const labelById = () => new Map(board.labels.map((l) => [l.id, l]));
  const memberById = () => new Map(board.members.map((m) => [m.user_id, m]));

  function renderHead() {
    ctx.setTitle(board.title);
    page.style.setProperty('--board-color', board.color);
    titleEl.textContent = board.title;
    titleEl.title = board.can_edit ? 'Rename board' : board.title;
    titleEl.classList.toggle('editable', board.can_edit);
    titleEl.onclick = board.can_edit ? () => renameBoard() : null;
    readOnlyPill.hidden = board.can_edit;
    addColBtn.hidden = !board.can_edit;
    clear(membersEl);
    board.members.slice(0, 5).forEach((m) => membersEl.appendChild(avatar(m.display_name, { size: 'sm', seed: m.user_id })));
    if (board.members.length > 5) membersEl.appendChild(h('span.small.muted', `+${board.members.length - 5}`));
    if (board.archived) titleEl.appendChild(h('span.pill.warn', { style: { marginLeft: '8px' } }, 'Archived'));
  }

  function cardSig(c, labels, members) {
    return JSON.stringify([c, c.labels.map((id) => labels.get(id)), c.assignees.map((id) => members.get(id)?.display_name), board.can_edit, new Date().getDate()]);
  }

  function buildCard(c, labels, members) {
    const due = c.due_at && h(`span.pill.${dueClass(c) || 'plain'}`, { title: `Due ${new Date(c.due_at).toLocaleString()}` }, icon('calendar', 'sm'), dueText(c.due_at));
    const el = h('article.kcard', {
      tabindex: 0, role: 'listitem', draggable: board.can_edit ? 'true' : null, dataset: { id: c.id }, class: c.done ? 'done' : '',
      'aria-label': `${c.title}${c.done ? ' (done)' : ''}`,
      onclick: (e) => { if (!e.target.closest('button')) openCard(c.id); },
      onkeydown: (e) => cardKey(e, c.id),
      ondragstart: (e) => onCardDragStart(e, c.id, el),
      ondragend: () => onDragEnd(),
      oncontextmenu: (e) => { e.preventDefault(); cardMenu(c.id, e); },
    },
    c.labels.length > 0 && h('div.kcard-labels', c.labels.map((id) => labels.get(id)).filter(Boolean).map((l) => labelChip(l, { small: true }))),
    h('div.kcard-title', c.done && h('span.kcard-check', { title: 'Done' }, icon('check', 'sm')), h('span', c.title)),
    !!(due || c.description || c.links.length || c.comment_count || c.assignees.length) && h('div.kcard-badges',
      due,
      !!c.description && h('span.kcard-badge', { title: 'Has a description' }, icon('doc', 'sm')),
      c.links.length > 0 && h('span.kcard-badge', { title: `${c.links.length} link(s)` }, icon('link', 'sm'), String(c.links.length)),
      c.comment_count > 0 && h('span.kcard-badge', { title: `${c.comment_count} comment(s)` }, icon('comment', 'sm'), String(c.comment_count)),
      h('span.grow'),
      h('span.kcard-assignees', c.assignees.map((id) => members.get(id)).filter(Boolean).map((m) => avatar(m.display_name, { size: 'sm', seed: m.user_id })))),
    h('button.btn.ghost.icon.sm.kcard-more', { 'aria-label': `Card actions for ${c.title}`, tabindex: -1, onclick: (e) => { e.stopPropagation(); cardMenu(c.id, e.currentTarget); } }, icon('more', 'sm')));
    return el;
  }

  function footerFor(stack) {
    let f = footers.get(stack.id);
    if (f) return f;
    const input = h('textarea.composer-input', { rows: 2, placeholder: 'Card title', 'aria-label': `New card in ${stack.title}` });
    const form = h('form.composer', {
      hidden: true,
      onsubmit: async (e) => {
        e.preventDefault();
        const title = input.value.trim();
        if (!title) return;
        input.value = '';
        try {
          await api.post(`/api/stacks/${stack.id}/cards`, { title });
          await refresh();
          input.focus();
        } catch (err) { errorToast(err); input.value = title; }
      },
    }, input, h('div.row', h('button.btn.primary.sm', { type: 'submit' }, 'Add card'), h('button.btn.ghost.sm', { type: 'button', onclick: () => closeComposer() }, 'Cancel')));
    const openBtn = h('button.btn.ghost.sm.add-card', { type: 'button', onclick: () => { form.hidden = false; openBtn.hidden = true; input.focus(); } }, icon('plus', 'sm'), 'Add card');
    const closeComposer = () => { form.hidden = true; openBtn.hidden = false; input.value = ''; };
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); form.requestSubmit(); }
      if (e.key === 'Escape') { e.stopPropagation(); closeComposer(); openBtn.focus(); }
    });
    f = h('div.stack-foot', form, openBtn);
    footers.set(stack.id, f);
    return f;
  }

  function render() {
    renderHead();
    // Remember scroll and focus, rebuild the columns, reuse unchanged card elements.
    const scrolls = new Map([...kanban.querySelectorAll('.stack')].map((s) => [s.dataset.id, s.querySelector('.stack-cards').scrollTop]));
    const focusedId = document.activeElement?.closest?.('.kcard')?.dataset.id;
    const left = kanban.scrollLeft;
    const labels = labelById();
    const members = memberById();
    const seen = new Set();
    clear(kanban);
    board.stacks.forEach((s, si) => {
      const cards = h('div.stack-cards', { dataset: { stack: s.id } });
      for (const c of s.cards) {
        seen.add(c.id);
        const sig = cardSig(c, labels, members);
        let rec = cardEls.get(c.id);
        if (!rec || rec.sig !== sig) {
          rec = { sig, el: buildCard(c, labels, members) };
          cardEls.set(c.id, rec);
        }
        cards.appendChild(rec.el);
      }
      if (!s.cards.length) cards.appendChild(h('div.stack-empty', board.can_edit ? 'Drop cards here' : 'No cards'));
      const titleBtn = h('h2.stack-title', { title: s.title, ondblclick: () => board.can_edit && renameStack(s) }, s.title);
      const col = h('section.stack', {
        dataset: { id: s.id }, role: 'listitem', 'aria-label': s.title,
        ondragover: (e) => onCardDragOver(e, cards),
        ondrop: (e) => onCardDrop(e, s.id, cards),
      },
      h('header.stack-head', {
        draggable: board.can_edit ? 'true' : null,
        ondragstart: (e) => onStackDragStart(e, s.id, col),
        ondragend: () => onDragEnd(),
      },
      titleBtn, h('span.badge.muted', String(s.cards.length)),
      board.can_edit && h('button.btn.ghost.icon.sm', { 'aria-label': `Column actions for ${s.title}`, onclick: (e) => stackMenu(s, si, e.currentTarget) }, icon('more', 'sm'))),
      cards,
      board.can_edit && footerFor(s));
      kanban.appendChild(col);
      const st = scrolls.get(s.id);
      if (st) requestAnimationFrame(() => { cards.scrollTop = st; });
    });
    if (board.can_edit) kanban.appendChild(h('button.stack-add', { type: 'button', onclick: () => addStack() }, icon('plus'), 'Add column'));
    for (const id of [...cardEls.keys()]) if (!seen.has(id)) cardEls.delete(id);
    for (const id of [...footers.keys()]) if (!board.stacks.some((s) => s.id === id)) footers.delete(id);
    kanban.scrollLeft = left;
    if (focusedId) cardEls.get(focusedId)?.el.focus({ preventScroll: true });
    applyFilter();
  }

  function applyFilter() {
    const labels = labelById();
    const members = memberById();
    for (const s of board.stacks) {
      for (const c of s.cards) {
        const el = cardEls.get(c.id)?.el;
        if (!el) continue;
        const hay = [c.title, c.description, ...c.labels.map((l) => labels.get(l)?.title), ...c.assignees.map((a) => members.get(a)?.display_name)].join(' ').toLowerCase();
        el.hidden = !!filter && !hay.includes(filter);
      }
    }
  }

  async function refresh() {
    if (disposed) return;
    if (dragging) { refreshPending = true; return; }
    try {
      board = (await api.get(`/api/boards/${encodeURIComponent(boardId)}`)).board;
    } catch (e) {
      if (e.status === 404 || e.status === 403) {
        toast('This board is no longer available.', 'warn');
        ctx.navigate('/boards');
        return;
      }
      errorToast(e);
      return;
    }
    if (disposed) return;
    render();
    if (cardModal) cardModal.update(board);
    if (dialog) dialog.refresh();
  }

  // ---- drag and drop ----
  function onCardDragStart(e, id, el) {
    if (!board.can_edit || e.target !== el) return;
    dragging = { kind: 'card', id, el };
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', id);
    placeholder.style.height = `${el.offsetHeight}px`;
    requestAnimationFrame(() => el.classList.add('dragging'));
  }

  function onStackDragStart(e, id, col) {
    if (!board.can_edit || dragging) return;
    dragging = { kind: 'stack', id, el: col };
    e.dataTransfer.effectAllowed = 'move';
    e.dataTransfer.setData('text/plain', id);
    stackPlaceholder.style.height = `${Math.min(col.offsetHeight, 240)}px`;
    requestAnimationFrame(() => col.classList.add('dragging'));
  }

  function onDragEnd() {
    if (dragging) dragging.el.classList.remove('dragging');
    placeholder.remove();
    stackPlaceholder.remove();
    dragging = null;
    if (refreshPending) { refreshPending = false; refresh(); }
  }

  function onCardDragOver(e, cards) {
    if (!dragging || dragging.kind !== 'card') return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'move';
    const after = [...cards.querySelectorAll('.kcard:not(.dragging)')].find((c) => {
      const r = c.getBoundingClientRect();
      return e.clientY < r.top + r.height / 2;
    });
    cards.querySelector('.stack-empty')?.setAttribute('hidden', '');
    if (after) {
      if (placeholder.nextSibling !== after) cards.insertBefore(placeholder, after);
    } else if (placeholder.parentNode !== cards || placeholder.nextSibling) {
      cards.appendChild(placeholder);
    }
  }

  async function onCardDrop(e, stackId, cards) {
    if (!dragging || dragging.kind !== 'card') return;
    e.preventDefault();
    const { id, el } = dragging;
    if (placeholder.parentNode !== cards) return;
    const index = [...cards.children].filter((c) => c === placeholder || (c.classList.contains('kcard') && c !== el)).indexOf(placeholder);
    cards.insertBefore(el, placeholder); // optimistic
    onDragEnd();
    await moveCard(id, stackId, index);
  }

  kanban.addEventListener('dragover', (e) => {
    if (!dragging || dragging.kind !== 'stack') return;
    e.preventDefault();
    const cols = [...kanban.querySelectorAll('.stack:not(.dragging)')];
    const after = cols.find((c) => { const r = c.getBoundingClientRect(); return e.clientX < r.left + r.width / 2; });
    if (after) { if (stackPlaceholder.nextSibling !== after) kanban.insertBefore(stackPlaceholder, after); } else {
      const add = kanban.querySelector('.stack-add');
      if (stackPlaceholder.nextSibling !== add) kanban.insertBefore(stackPlaceholder, add);
    }
  });
  kanban.addEventListener('drop', async (e) => {
    if (!dragging || dragging.kind !== 'stack') return;
    e.preventDefault();
    const { id, el } = dragging;
    const index = [...kanban.children].filter((c) => c === stackPlaceholder || (c.classList.contains('stack') && c !== el)).indexOf(stackPlaceholder);
    if (index >= 0) kanban.insertBefore(el, stackPlaceholder);
    onDragEnd();
    if (index >= 0) await moveStack(id, index);
  });

  async function moveCard(id, stackId, index) {
    try {
      await api.post(`/api/cards/${id}/move`, { stack_id: stackId, index });
    } catch (err) { errorToast(err); }
    await refresh();
  }

  async function moveStack(id, index) {
    try { await api.patch(`/api/stacks/${id}`, { index }); } catch (err) { errorToast(err); }
    await refresh();
  }

  // ---- keyboard ----
  function locate(id) {
    for (let si = 0; si < board.stacks.length; si++) {
      const ci = board.stacks[si].cards.findIndex((c) => c.id === id);
      if (ci >= 0) return { si, ci, stack: board.stacks[si], card: board.stacks[si].cards[ci] };
    }
    return null;
  }

  function focusCard(id) {
    const el = cardEls.get(id)?.el;
    if (el) { el.focus(); el.scrollIntoView({ block: 'nearest', inline: 'nearest' }); }
  }

  async function cardKey(e, id) {
    if (e.target !== e.currentTarget) return;
    const loc = locate(id);
    if (!loc) return;
    const { si, ci } = loc;
    const visible = (s) => board.stacks[s].cards.filter((c) => !cardEls.get(c.id)?.el.hidden);
    if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); openCard(id); return; }
    if (e.key === 'm' || e.key === 'M' || e.key === 'ContextMenu' || (e.shiftKey && e.key === 'F10')) { e.preventDefault(); cardMenu(id, e.currentTarget); return; }
    if (e.altKey && board.can_edit) {
      const moves = {
        ArrowUp: () => ci > 0 && moveCard(id, loc.stack.id, ci - 1),
        ArrowDown: () => ci < loc.stack.cards.length - 1 && moveCard(id, loc.stack.id, ci + 1),
        ArrowLeft: () => si > 0 && moveCard(id, board.stacks[si - 1].id, board.stacks[si - 1].cards.length),
        ArrowRight: () => si < board.stacks.length - 1 && moveCard(id, board.stacks[si + 1].id, board.stacks[si + 1].cards.length),
      };
      if (moves[e.key]) { e.preventDefault(); await moves[e.key](); focusCard(id); }
      return;
    }
    const list = visible(si);
    const vi = list.findIndex((c) => c.id === id);
    if (e.key === 'ArrowDown' && list[vi + 1]) { e.preventDefault(); focusCard(list[vi + 1].id); }
    if (e.key === 'ArrowUp' && vi > 0) { e.preventDefault(); focusCard(list[vi - 1].id); }
    if (e.key === 'ArrowLeft' || e.key === 'ArrowRight') {
      const d = e.key === 'ArrowLeft' ? -1 : 1;
      for (let s = si + d; s >= 0 && s < board.stacks.length; s += d) {
        const l = visible(s);
        if (l.length) { e.preventDefault(); focusCard(l[Math.min(vi, l.length - 1)].id); break; }
      }
    }
  }

  function onGlobalKey(e) {
    if (e.key === '/' && !inField(e) && !document.querySelector('.modal-backdrop')) { e.preventDefault(); filterInput.focus(); }
  }
  document.addEventListener('keydown', onGlobalKey);

  // ---- menus and actions ----
  function cardMenu(id, anchor) {
    const loc = locate(id);
    if (!loc) return;
    const { si, ci, stack, card } = loc;
    const items = [{ label: 'Open', icon: 'edit', run: () => openCard(id) }];
    if (board.can_edit) {
      items.push('sep');
      board.stacks.forEach((s, i) => {
        if (i !== si) items.push({ label: `Move to “${s.title}”`, icon: 'move', run: async () => { await moveCard(id, s.id, s.cards.length); focusCard(id); } });
      });
      if (ci > 0) items.push({ label: 'Move to top', icon: 'chevron-right', run: async () => { await moveCard(id, stack.id, 0); focusCard(id); } });
      if (ci > 0) items.push({ label: 'Move up', icon: 'chevron-right', run: async () => { await moveCard(id, stack.id, ci - 1); focusCard(id); } });
      if (ci < stack.cards.length - 1) items.push({ label: 'Move down', icon: 'chevron-down', run: async () => { await moveCard(id, stack.id, ci + 1); focusCard(id); } });
      items.push('sep',
        { label: card.done ? 'Mark as not done' : 'Mark as done', icon: 'check', run: () => patchCard(id, { done: !card.done }) },
        { label: 'Archive', icon: 'download', run: () => patchCard(id, { archived: true }, 'Card archived') },
        { label: 'Delete', icon: 'trash', danger: true, run: () => deleteCard(card) });
    }
    menu(anchor, items);
  }

  async function patchCard(id, body, msg) {
    try {
      await api.patch(`/api/cards/${id}`, body);
      if (msg) toast(msg, 'ok', { timeout: 2500 });
    } catch (e) { errorToast(e); }
    await refresh();
  }

  async function deleteCard(card) {
    if (!await confirmDialog('Delete card?', `“${card.title}” and its comments will be deleted.`, { confirm: 'Delete', danger: true })) return false;
    try { await api.del(`/api/cards/${card.id}`); } catch (e) { errorToast(e); return false; }
    await refresh();
    return true;
  }

  function stackMenu(s, si, anchor) {
    menu(anchor, [
      { label: 'Rename', icon: 'edit', run: () => renameStack(s) },
      si > 0 && { label: 'Move left', icon: 'arrow-left', run: () => moveStack(s.id, si - 1) },
      si < board.stacks.length - 1 && { label: 'Move right', icon: 'chevron-right', run: () => moveStack(s.id, si + 1) },
      'sep',
      { label: 'Delete column', icon: 'trash', danger: true, run: async () => {
        const n = s.cards.length;
        if (!await confirmDialog('Delete column?', n ? `“${s.title}” and its ${n} card(s) will be deleted.` : `“${s.title}” will be deleted.`, { confirm: 'Delete', danger: true })) return;
        try { await api.del(`/api/stacks/${s.id}`); } catch (e) { errorToast(e); }
        await refresh();
      } },
    ]);
  }

  async function renameStack(s) {
    const t = await promptDialog('Rename column', { value: s.title, confirm: 'Rename' });
    if (!t || t === s.title) return;
    try { await api.patch(`/api/stacks/${s.id}`, { title: t }); } catch (e) { errorToast(e); }
    await refresh();
  }

  async function addStack() {
    const t = await promptDialog('New column', { placeholder: 'Column title', confirm: 'Add column' });
    if (!t) return;
    try { await api.post(`/api/boards/${board.id}/stacks`, { title: t }); } catch (e) { errorToast(e); }
    await refresh();
    kanban.scrollLeft = kanban.scrollWidth;
  }

  async function renameBoard() {
    const t = await promptDialog('Rename board', { value: board.title, confirm: 'Rename' });
    if (!t || t === board.title) return;
    try { await api.patch(`/api/boards/${board.id}`, { title: t }); } catch (e) { errorToast(e); }
    await refresh();
  }

  function openMembers() {
    dialog = membersDialog(board, me, () => board, () => ctx.navigate('/boards'));
    dialog.closed.then(() => { dialog = null; });
  }

  function boardMenu() {
    const owner = board.owner_id === me.id;
    menu(moreBtn, [
      board.can_edit && { label: 'Rename', icon: 'edit', run: renameBoard },
      board.can_edit && { label: 'Color', icon: 'star', run: async () => { try { await colorDialog(board); } catch (e) { errorToast(e); } await refresh(); } },
      { label: 'Members', icon: 'users', run: openMembers },
      { label: 'Labels', icon: 'tag', run: () => { dialog = labelsDialog(() => board); dialog.closed.then(() => { dialog = null; }); } },
      board.can_edit && 'sep',
      board.can_edit && { label: board.archived ? 'Unarchive board' : 'Archive board', icon: 'download', run: async () => {
        try { await api.patch(`/api/boards/${board.id}`, { archived: !board.archived }); } catch (e) { errorToast(e); }
        await refresh();
      } },
      owner && { label: 'Delete board', icon: 'trash', danger: true, run: async () => {
        if (!await confirmDialog('Delete board?', `“${board.title}”, its columns, cards and comments will be deleted for everyone.`, { confirm: 'Delete board', danger: true })) return;
        try {
          await api.del(`/api/boards/${board.id}`);
          ctx.navigate('/boards');
        } catch (e) { errorToast(e); }
      } },
      !owner && { label: 'Leave board', icon: 'logout', danger: true, run: async () => {
        if (!await confirmDialog('Leave board?', `You will lose access to “${board.title}”.`, { confirm: 'Leave', danger: true })) return;
        try { await api.del(`/api/boards/${board.id}/members/${me.id}`); ctx.navigate('/boards'); } catch (e) { errorToast(e); }
      } },
    ]);
  }

  // ---- card modal (with ?card= deep link) ----
  function openCard(id) {
    if (cardModal) cardModal.close();
    const url = new URL(location.href);
    url.searchParams.set('card', id);
    history.replaceState(history.state, '', url);
    cardModal = openCardModal({
      cardId: id, board, me, ctx, refresh, deleteCard,
      onClose: () => {
        cardModal = null;
        const u = new URL(location.href);
        if (u.searchParams.get('card') === id) {
          u.searchParams.delete('card');
          history.replaceState(history.state, '', u);
        }
        if (!disposed) focusCard(id);
      },
    });
  }

  render();
  if (ctx.query.card) {
    if (locate(ctx.query.card)) openCard(ctx.query.card);
    else {
      // Archived cards are not part of the board view but can still be opened by link.
      try { await api.get(`/api/cards/${encodeURIComponent(ctx.query.card)}`); openCard(ctx.query.card); } catch (_) { toast('That card no longer exists.', 'warn'); }
    }
  }

  const debouncedRefresh = debounce(refresh, 200);
  const off = live.on('boards.changed', (ev) => {
    if (ev.board_id !== boardId) return;
    if (ev.what === 'deleted') {
      toast('This board was deleted.', 'warn');
      ctx.navigate('/boards');
      return;
    }
    debouncedRefresh();
  });
  // Resync after a reconnect (events may have been missed).
  const offHello = live.on('hello', debouncedRefresh);

  return () => {
    disposed = true;
    off();
    offHello();
    help.cleanup();
    document.removeEventListener('keydown', onGlobalKey);
    if (cardModal) cardModal.close();
  };
}
