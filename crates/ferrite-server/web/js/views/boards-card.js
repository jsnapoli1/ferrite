// The card modal: title, Markdown description, due date, done, labels, assignees, links
// (files, folders, conversations, web addresses), comments, archive and delete. It stays open
// across live board refreshes: `update(board)` refreshes whatever the user isn't editing.

import { api } from '../api.js';
import { markdown } from '../markdown.js';
import { h, clear, icon, avatar, menu, toast, errorToast, promptDialog, relTime, dateTime, spinner } from '../ui.js';
import { labelChip, newLabelForm, pickRoom } from './boards-dialogs.js';
import { dueText, dueClass } from './boards-kanban.js';
import { pickFile, openFileId } from './editor-common.js';

/** ISO timestamp → value for <input type=datetime-local> in local time. */
function toLocalInput(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  const p = (n) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}

function findCard(board, id) {
  for (const s of board.stacks) {
    const c = s.cards.find((x) => x.id === id);
    if (c) return { card: c, stack: s };
  }
  return null;
}

export function openCardModal({ cardId, board, me, ctx, refresh, deleteCard, onClose }) {
  let card = null;
  let stack = null;
  let comments = null;
  let lastCommentCount = -1;
  let editingDesc = false;
  let closed = false;
  const canEdit = () => board.can_edit;
  const prevFocus = document.activeElement;

  // ---- skeleton ----
  const titleInput = h('textarea.card-title-input', {
    rows: 1, 'aria-label': 'Card title', spellcheck: 'true',
    onkeydown: (e) => { if (e.key === 'Enter') { e.preventDefault(); titleInput.blur(); } if (e.key === 'Escape') { e.stopPropagation(); titleInput.value = card.title; titleInput.blur(); } },
    onblur: () => saveTitle(),
    oninput: () => autosize(titleInput),
  });
  const titleText = h('h2.card-title-text');
  const where = h('div.small.muted.card-where');
  const doneBadge = h('span.pill.ok', icon('check', 'sm'), 'Done');
  const archivedBadge = h('span.pill.warn', 'Archived');
  const descBox = h('div.card-desc');
  const linksBox = h('div.card-links');
  const commentsBox = h('div.card-comments');
  const side = h('aside.card-side');

  const commentInput = h('textarea', { rows: 2, placeholder: 'Write a comment… (Markdown, Ctrl+Enter to send)', 'aria-label': 'New comment' });
  const commentBtn = h('button.btn.primary.sm', { type: 'submit' }, 'Comment');
  const commentForm = h('form.comment-form', {
    onsubmit: async (e) => {
      e.preventDefault();
      const body = commentInput.value.trim();
      if (!body) return;
      commentBtn.disabled = true;
      try {
        await api.post(`/api/cards/${cardId}/comments`, { body });
        commentInput.value = '';
        await loadComments();
        refresh();
      } catch (err) { errorToast(err); } finally { commentBtn.disabled = false; }
    },
  }, avatar(me.display_name, { size: 'sm', seed: me.id }), h('div.grow', commentInput, h('div.row', { style: { justifyContent: 'flex-end', marginTop: '6px' } }, commentBtn)));
  commentInput.addEventListener('keydown', (e) => { if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); commentForm.requestSubmit(); } });

  const closeBtn = h('button.btn.ghost.icon.sm', { 'aria-label': 'Close', onclick: () => close() }, icon('x'));
  const dlg = h('div.modal.wide.card-modal', { role: 'dialog', 'aria-modal': 'true', 'aria-label': 'Card' },
    h('div.modal-head.card-head', h('div.grow', titleInput, titleText, h('div.row.wrap', where, doneBadge, archivedBadge)), closeBtn),
    h('div.modal-body.card-body',
      h('div.card-main',
        h('section', h('h3.card-section', icon('doc', 'sm'), 'Description'), descBox),
        h('section', h('div.card-section-row', h('h3.card-section', icon('link', 'sm'), 'Links'), h('span.grow'), h('span.card-links-add')), linksBox),
        h('section', h('h3.card-section', icon('comment', 'sm'), 'Comments'), commentsBox, commentForm)),
      side));
  const backdrop = h('div.modal-backdrop', { onmousedown: (e) => { if (e.target === backdrop) close(); } }, dlg);
  const onKey = (e) => {
    if (e.key !== 'Escape' || document.querySelectorAll('.modal-backdrop').length > 1 || document.querySelector('.menu')) return;
    if (editingDesc && dlg.contains(document.activeElement) && document.activeElement.tagName === 'TEXTAREA') return;
    close();
  };
  document.body.appendChild(backdrop);
  document.addEventListener('keydown', onKey);

  function autosize(t) {
    t.style.height = 'auto';
    t.style.height = `${t.scrollHeight}px`;
  }

  async function patch(body) {
    try {
      await api.patch(`/api/cards/${cardId}`, body);
      await refresh();
    } catch (e) { errorToast(e); }
  }

  async function saveTitle() {
    const t = titleInput.value.trim();
    if (!card || !t || t === card.title) { if (card) titleInput.value = card.title; return; }
    await patch({ title: t });
  }

  // ---- description ----
  function renderDesc() {
    clear(descBox);
    if (!editingDesc) {
      const md = card.description ? markdown(card.description, { me: me.username }) : h('p.muted', canEdit() ? 'Add a more detailed description…' : 'No description.');
      const view = h('div.card-desc-view', {
        class: canEdit() ? 'editable' : '', tabindex: canEdit() ? 0 : null, role: canEdit() ? 'button' : null, 'aria-label': canEdit() ? 'Edit description' : null,
        onclick: (e) => { if (canEdit() && !e.target.closest('a')) startDesc(); },
        onkeydown: (e) => { if (canEdit() && e.key === 'Enter' && e.target === e.currentTarget) { e.preventDefault(); startDesc(); } },
      }, md);
      descBox.appendChild(view);
      return;
    }
    const ta = h('textarea.card-desc-input', { rows: 8, 'aria-label': 'Description (Markdown)', placeholder: 'Markdown: **bold**, - lists, [links](https://…), - [ ] tasks' });
    ta.value = card.description;
    const preview = h('div.card-desc-preview', { hidden: true });
    const writeTab = h('button.active', { type: 'button', onclick: () => { preview.hidden = true; ta.hidden = false; writeTab.classList.add('active'); previewTab.classList.remove('active'); ta.focus(); } }, 'Write');
    const previewTab = h('button', { type: 'button', onclick: () => { clear(preview).appendChild(ta.value.trim() ? markdown(ta.value, { me: me.username }) : h('p.muted', 'Nothing to preview.')); preview.hidden = false; ta.hidden = true; previewTab.classList.add('active'); writeTab.classList.remove('active'); } }, 'Preview');
    const save = async () => { editingDesc = false; const v = ta.value; if (v !== card.description) { card = { ...card, description: v }; await patch({ description: v }); } renderDesc(); };
    const cancel = () => { editingDesc = false; renderDesc(); };
    ta.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); save(); }
      if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); cancel(); }
    });
    descBox.append(h('div.tabs.card-desc-tabs', writeTab, previewTab), ta, preview,
      h('div.row', { style: { marginTop: '8px' } }, h('button.btn.primary.sm', { type: 'button', onclick: save }, 'Save'), h('button.btn.ghost.sm', { type: 'button', onclick: cancel }, 'Cancel'), h('span.small.muted', 'Ctrl+Enter to save')));
    ta.focus();
  }
  function startDesc() { editingDesc = true; renderDesc(); }

  // ---- links ----
  function linkIcon(l) { return l.kind === 'room' ? 'talk' : l.kind === 'url' ? 'globe' : 'file'; }
  async function openLinkItem(l) {
    try {
      if (l.kind === 'url') { window.open(l.target, '_blank', 'noopener'); return; }
      if (l.kind === 'room') { close(); ctx.navigate(`/talk/${encodeURIComponent(l.target)}`); return; }
      await openFileId(l.target, (href) => { close(); ctx.navigate(href); });
    } catch (e) { errorToast(e); }
  }
  async function addLink(kind, target, title) {
    try {
      await api.post(`/api/cards/${cardId}/links`, { kind, target, title: title || undefined });
      await refresh();
    } catch (e) { errorToast(e); }
  }
  function renderLinks() {
    const addHost = dlg.querySelector('.card-links-add');
    clear(addHost);
    if (canEdit()) {
      const addBtn = h('button.btn.sm', {
        type: 'button',
        onclick: () => menu(addBtn, [
          { label: 'File…', icon: 'file', run: async () => { const e = await pickFile({ title: 'Link a file', rootId: ctx.me.root_id, confirm: 'Link file' }); if (e) addLink('file', e.id); } },
          { label: 'Folder…', icon: 'folder', run: async () => { const e = await pickFile({ title: 'Link a folder', rootId: ctx.me.root_id, folders: true }); if (e) addLink('file', e.id); } },
          { label: 'Conversation…', icon: 'talk', run: async () => { const r = await pickRoom(); if (r) addLink('room', r.token); } },
          { label: 'Web address…', icon: 'globe', run: async () => {
            const u = await promptDialog('Link a web address', { placeholder: 'https://…', type: 'url', confirm: 'Link' });
            if (u) addLink('url', /^https?:\/\//i.test(u) ? u : `https://${u}`);
          } },
        ]),
      }, icon('plus', 'sm'), 'Attach');
      addHost.appendChild(addBtn);
    }
    clear(linksBox);
    if (!card.links.length) { linksBox.appendChild(h('p.muted.small', 'No links. Attach files, folders, conversations or web pages.')); return; }
    for (const l of card.links) {
      linksBox.appendChild(h('div.card-link',
        h('button.card-link-open', { type: 'button', onclick: () => openLinkItem(l), title: l.kind === 'url' ? l.target : `Open ${l.title}` },
          icon(linkIcon(l), 'sm'), h('span.ellipsis', l.title), l.kind === 'url' && icon('external', 'sm')),
        canEdit() && h('button.btn.ghost.icon.sm', {
          type: 'button', 'aria-label': `Remove link ${l.title}`, title: 'Remove link',
          onclick: async () => { try { await api.del(`/api/card-links/${l.id}`); await refresh(); } catch (e) { errorToast(e); } },
        }, icon('x', 'sm'))));
    }
  }

  // ---- comments ----
  async function loadComments() {
    try {
      const r = await api.get(`/api/cards/${cardId}`);
      comments = r.comments;
      if (!findCard(board, cardId)) { card = { labels: [], assignees: [], links: [], comment_count: 0, ...r.card }; renderAll(); }
    } catch (e) {
      if (e.status === 404) { toast('This card was deleted.', 'warn'); close(); return; }
      errorToast(e);
      return;
    }
    renderComments();
  }
  function renderComments() {
    clear(commentsBox);
    if (!comments) { commentsBox.appendChild(spinner()); return; }
    if (!comments.length) commentsBox.appendChild(h('p.muted.small', 'No comments yet.'));
    for (const c of comments) {
      commentsBox.appendChild(h('div.comment',
        avatar(c.author_name, { size: 'sm', seed: c.author_id }),
        h('div.grow',
          h('div.comment-meta', h('strong', c.author_name), h('span.small.muted', { title: dateTime(c.created_at) }, relTime(c.created_at))),
          markdown(c.body, { me: me.username }))));
    }
  }

  // ---- sidebar ----
  function sideSection(title, ...body) { return h('div.side-section', h('h4', title), ...body); }

  function renderSide() {
    clear(side);
    const edit = canEdit();
    // Column (moving between columns from the modal).
    if (stack) {
      side.appendChild(sideSection('Column', edit
        ? h('select', {
          'aria-label': 'Column',
          onchange: async (e) => {
            const target = board.stacks.find((s) => s.id === e.target.value);
            try { await api.post(`/api/cards/${cardId}/move`, { stack_id: target.id, index: target.cards.length }); } catch (err) { errorToast(err); }
            await refresh();
          },
        }, board.stacks.map((s) => h('option', { value: s.id, selected: s.id === stack.id }, s.title)))
        : h('div', stack.title)));
    }
    // Due date and done.
    const dueInput = h('input', {
      type: 'datetime-local', value: toLocalInput(card.due_at), 'aria-label': 'Due date', disabled: !edit,
      onchange: (e) => { if (e.target.value) patch({ due_at: new Date(e.target.value).toISOString() }); },
    });
    side.appendChild(sideSection('Due',
      edit ? h('div.row', dueInput, card.due_at && h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Clear due date', title: 'Clear due date', onclick: () => patch({ due_at: null }) }, icon('x', 'sm')))
        : h('div', card.due_at ? dueText(card.due_at) : h('span.muted', 'None')),
      card.due_at && dueClass(card) === 'danger' && h('div.small', { style: { color: 'var(--danger)', marginTop: '4px' } }, 'Overdue'),
      h('label.check', h('input', { type: 'checkbox', checked: card.done, disabled: !edit, onchange: (e) => patch({ done: e.target.checked }) }), 'Done')));
    // Labels.
    const labelBox = h('div.side-labels');
    for (const l of board.labels) {
      const on = card.labels.includes(l.id);
      if (!edit && !on) continue;
      labelBox.appendChild(edit
        ? h('button.label-toggle', {
          type: 'button', 'aria-pressed': String(on), title: on ? `Remove “${l.title}”` : `Add “${l.title}”`,
          onclick: async () => {
            try { if (on) await api.del(`/api/cards/${cardId}/labels/${l.id}`); else await api.post(`/api/cards/${cardId}/labels/${l.id}`); await refresh(); } catch (e) { errorToast(e); }
          },
        }, labelChip(l), on && icon('check', 'sm'))
        : labelChip(l));
    }
    if (!labelBox.childNodes.length) labelBox.appendChild(h('span.small.muted', edit ? 'No labels on this board.' : 'No labels.'));
    let labelForm = null;
    if (edit) {
      labelForm = newLabelForm(board.id, async (label) => {
        try { await api.post(`/api/cards/${cardId}/labels/${label.id}`); } catch (e) { errorToast(e); }
        await refresh();
      });
      labelForm.hidden = true;
    }
    side.appendChild(sideSection('Labels', labelBox,
      edit && h('button.btn.ghost.sm', { type: 'button', onclick: (e) => { labelForm.hidden = !labelForm.hidden; e.currentTarget.hidden = true; labelForm.querySelector('input').focus(); } }, icon('plus', 'sm'), 'New label'),
      labelForm));
    // Assignees.
    const people = h('div.side-people');
    for (const m of board.members) {
      const on = card.assignees.includes(m.user_id);
      if (!edit && !on) continue;
      people.appendChild(h(edit ? 'button.person-toggle' : 'div.person-toggle', {
        type: edit ? 'button' : null, 'aria-pressed': edit ? String(on) : null, title: edit ? (on ? `Unassign ${m.display_name}` : `Assign ${m.display_name}`) : null,
        onclick: edit ? async () => {
          try { await api.post(`/api/cards/${cardId}/assignees`, { user: m.user_id, assigned: !on }); await refresh(); } catch (e) { errorToast(e); }
        } : null,
      }, avatar(m.display_name, { size: 'sm', seed: m.user_id }), h('span.grow.ellipsis', m.display_name), on && icon('check', 'sm')));
    }
    if (!people.childNodes.length) people.appendChild(h('span.small.muted', 'Nobody assigned.'));
    side.appendChild(sideSection('Assignees', people));
    // Actions.
    if (edit) {
      side.appendChild(sideSection('Actions',
        h('button.btn.sm.side-action', { type: 'button', onclick: () => patch({ archived: !card.archived }).then(() => { if (!card.archived) { toast('Card archived', 'ok', { timeout: 2500 }); close(); } }) }, icon('download', 'sm'), card.archived ? 'Unarchive' : 'Archive'),
        h('button.btn.sm.danger.side-action', { type: 'button', onclick: async () => { if (await deleteCard(card)) close(); } }, icon('trash', 'sm'), 'Delete')));
    }
    side.appendChild(h('div.small.muted.side-meta', `Created ${relTime(card.created_at)}`, h('br'), `Updated ${relTime(card.updated_at)}`));
  }

  function renderAll() {
    if (!card) return;
    const edit = canEdit();
    titleInput.hidden = !edit;
    titleText.hidden = edit;
    titleText.textContent = card.title;
    if (document.activeElement !== titleInput) { titleInput.value = card.title; requestAnimationFrame(() => autosize(titleInput)); }
    dlg.setAttribute('aria-label', card.title);
    clear(where).append(...(stack ? ['in ', h('strong', stack.title)] : ['Not on the board']));
    doneBadge.hidden = !card.done;
    archivedBadge.hidden = !card.archived;
    if (!editingDesc) renderDesc();
    renderLinks();
    if (!side.contains(document.activeElement) || document.activeElement.tagName !== 'INPUT') renderSide();
    if (card.comment_count !== lastCommentCount) {
      lastCommentCount = card.comment_count;
      loadComments();
    }
  }

  function update(b) {
    if (closed) return;
    board = b;
    const found = findCard(board, cardId);
    if (found) {
      ({ card, stack } = found);
      renderAll();
    } else {
      // Archived (not in the board view) or deleted: fetch it directly.
      stack = null;
      loadComments();
    }
  }

  function close() {
    if (closed) return;
    closed = true;
    if (card && canEdit() && document.activeElement === titleInput) saveTitle();
    backdrop.remove();
    document.removeEventListener('keydown', onKey);
    if (prevFocus && prevFocus.focus && document.body.contains(prevFocus)) prevFocus.focus();
    onClose();
  }

  renderComments();
  update(board);
  if (!card) clear(descBox).appendChild(spinner());
  setTimeout(() => (canEdit() ? null : closeBtn.focus()), 0);
  return { update, close, id: cardId };
}
