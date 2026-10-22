// Board dialogs: color swatches, members, labels and the conversation picker.

import { api } from '../api.js';
import { h, clear, icon, avatar, modal, errorToast, confirmDialog, spinner, emptyState } from '../ui.js';

export const BOARD_COLORS = ['#3b82f6', '#0f766e', '#7c3aed', '#be185d', '#c2410c', '#15803d', '#b45309', '#475569', '#0e7490', '#dc2626'];
export const LABEL_COLORS = ['#dc2626', '#ea580c', '#d97706', '#65a30d', '#16a34a', '#0891b2', '#2563eb', '#7c3aed', '#c026d3', '#64748b'];

/** A row of round color buttons; calls onPick(color). */
export function colorSwatches(current, onPick, colors = BOARD_COLORS) {
  const row = h('div.swatches', { role: 'radiogroup', 'aria-label': 'Color' });
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

/** Label chip in the board label's color. */
export function labelChip(label, { small = false } = {}) {
  return h(`span.label-chip${small ? '.sm' : ''}`, { style: { '--label-color': label.color }, title: label.title }, label.title);
}

export async function colorDialog(board) {
  let color = board.color;
  const v = await modal({
    title: 'Board color',
    body: colorSwatches(color, (c) => { color = c; }),
    actions: [{ label: 'Cancel', value: null }, { label: 'Save', kind: 'primary', run: () => color }],
  });
  if (v && v !== board.color) await api.patch(`/api/boards/${board.id}`, { color: v });
}

/** Members dialog. The owner adds/removes members and sets edit rights; others can leave. */
export function membersDialog(board, me, getBoard, onLeft) {
  const isOwner = board.owner_id === me.id;
  const list = h('div.list.members-list');
  const draw = () => {
    const b = getBoard();
    clear(list);
    for (const m of b.members) {
      const owner = m.user_id === b.owner_id;
      const roleSel = isOwner && !owner && h('select', {
        'aria-label': `Access for ${m.display_name}`,
        onchange: async (e) => {
          try {
            await api.post(`/api/boards/${b.id}/members`, { username: m.username, can_edit: e.target.value === 'edit' });
          } catch (err) { errorToast(err); }
        },
      }, h('option', { value: 'edit', selected: m.can_edit }, 'Can edit'), h('option', { value: 'read', selected: !m.can_edit }, 'Read only'));
      list.appendChild(h('div.list-item', avatar(m.display_name, { size: 'sm', seed: m.user_id }),
        h('div.grow', h('div.ellipsis', m.display_name), h('div.small.muted', `@${m.username}`)),
        owner ? h('span.pill', 'Owner') : (roleSel || h('span.pill', m.can_edit ? 'Can edit' : 'Read only')),
        !owner && (isOwner || m.user_id === me.id) && h('button.btn.ghost.icon.sm', {
          'aria-label': m.user_id === me.id ? 'Leave board' : `Remove ${m.display_name}`, title: m.user_id === me.id ? 'Leave board' : 'Remove',
          onclick: async () => {
            const self = m.user_id === me.id;
            if (!await confirmDialog(self ? 'Leave board?' : 'Remove member?', self ? `You will lose access to “${b.title}”.` : `${m.display_name} will lose access to “${b.title}”.`, { confirm: self ? 'Leave' : 'Remove', danger: true })) return;
            try {
              await api.del(`/api/boards/${b.id}/members/${m.user_id}`);
              if (self) { closeFn(); onLeft(); }
            } catch (err) { errorToast(err); }
          },
        }, icon(m.user_id === me.id ? 'logout' : 'x', 'sm'))));
    }
  };
  const username = h('input', { type: 'text', placeholder: 'Username', 'aria-label': 'Username to add', autocomplete: 'off' });
  const canEdit = h('select', { 'aria-label': 'Access' }, h('option', { value: 'edit' }, 'Can edit'), h('option', { value: 'read' }, 'Read only'));
  const add = async (e) => {
    e.preventDefault();
    const u = username.value.trim().replace(/^@/, '');
    if (!u) return;
    try {
      await api.post(`/api/boards/${board.id}/members`, { username: u, can_edit: canEdit.value === 'edit' });
      username.value = '';
    } catch (err) { errorToast(err); }
  };
  let closeFn = () => {};
  draw();
  const p = modal({
    title: 'Members',
    body: (close) => {
      closeFn = close;
      return h('div', list, isOwner && h('form.row.members-add', { onsubmit: add }, username, canEdit, h('button.btn.primary', { type: 'submit' }, 'Add')));
    },
    actions: [{ label: 'Done', kind: 'primary', value: null }],
  });
  return { refresh: draw, closed: p };
}

/** Labels dialog: create, recolor-by-recreate is not supported by the API, so create/delete. */
export function labelsDialog(getBoard) {
  const list = h('div.list');
  const draw = () => {
    const b = getBoard();
    clear(list);
    if (!b.labels.length) list.appendChild(h('p.muted.small', 'No labels yet.'));
    for (const l of b.labels) {
      list.appendChild(h('div.list-item', labelChip(l), h('span.grow'),
        b.can_edit && h('button.btn.ghost.icon.sm', {
          'aria-label': `Delete label ${l.title}`,
          onclick: async () => {
            if (!await confirmDialog('Delete label?', `“${l.title}” will be removed from all cards.`, { confirm: 'Delete', danger: true })) return;
            try { await api.del(`/api/labels/${l.id}`); } catch (e) { errorToast(e); }
          },
        }, icon('trash', 'sm'))));
    }
  };
  draw();
  const b = getBoard();
  const form = b.can_edit && newLabelForm(b.id);
  const p = modal({ title: 'Labels', body: h('div', list, form && h('hr'), form), actions: [{ label: 'Done', kind: 'primary', value: null }] });
  return { refresh: draw, closed: p };
}

/** Inline form creating a label; calls onCreated(label). */
export function newLabelForm(boardId, onCreated) {
  let color = LABEL_COLORS[Math.floor(Math.random() * LABEL_COLORS.length)];
  const title = h('input', { type: 'text', placeholder: 'New label', 'aria-label': 'New label name' });
  return h('form.new-label', {
    onsubmit: async (e) => {
      e.preventDefault();
      const t = title.value.trim();
      if (!t) return;
      try {
        const { label } = await api.post(`/api/boards/${boardId}/labels`, { title: t, color });
        title.value = '';
        if (onCreated) onCreated(label);
      } catch (err) { errorToast(err); }
    },
  }, h('div.row', title, h('button.btn', { type: 'submit' }, icon('plus', 'sm'), 'Add')), colorSwatches(color, (c) => { color = c; }, LABEL_COLORS));
}

/** Picks a conversation; resolves with the room or null. */
export function pickRoom() {
  const list = h('div.list.picker-list', spinner());
  (async () => {
    try {
      const { rooms } = await api.get('/api/talk/rooms');
      clear(list);
      if (!rooms.length) list.appendChild(emptyState('talk', 'No conversations', 'Start one in Talk first.'));
      for (const r of rooms) {
        list.appendChild(h('div.list-item.clickable', {
          tabindex: 0, role: 'option',
          onclick: () => { chosen = r; list.closest('.modal').querySelector('.modal-foot .btn.primary').click(); },
          onkeydown: (e) => { if (e.key === 'Enter') e.currentTarget.click(); },
        }, avatar(r.display_name, { size: 'sm', seed: r.id }), h('span.grow.ellipsis', r.display_name)));
      }
    } catch (e) {
      clear(list).appendChild(h('p.error-text', e.message));
    }
  })();
  let chosen = null;
  return modal({ title: 'Link a conversation', body: list, actions: [{ label: 'Cancel', value: null }, { label: 'Link', kind: 'primary', run: () => chosen }] });
}
