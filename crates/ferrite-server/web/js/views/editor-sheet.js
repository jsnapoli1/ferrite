// Sheet editor (.csv): a grid over the shared Y.Map "cells" ("row,col" → string; formulas
// start with "="). Formulas are evaluated client-side (editor-formula.js). Peers' selected
// cells (awareness field `cell`) show as colored outlines.

import { Y } from '../collab.js';
import { h, clear, icon } from '../ui.js';
import { evaluator, display, colName, SheetError, MAX_ROWS, MAX_COLS } from './editor-formula.js';
import { rafBatch } from './editor-common.js';

const MIN_ROWS = 40;
const MIN_COLS = 12;

export default function mount({ body, toolbar, provider, doc, entry, readOnly, setHelp }) {
  const cells = doc.getMap('cells');
  const undo = readOnly ? null : new Y.UndoManager(cells, { captureTimeout: 400, trackedOrigins: new Set(['local']) });
  let rows = MIN_ROWS;
  let cols = MIN_COLS;
  let sel = { r: 0, c: 0 };
  let editing = null; // {r, c, original}
  const tds = []; // tds[r][c]
  const shown = new Map(); // "r,c" -> displayed text

  const raw = (r, c) => {
    const v = cells.get(`${r},${c}`);
    return v === undefined || v === null ? '' : String(v);
  };

  // ---- toolbar: name box, formula bar, actions ----
  const nameBox = h('span.sheet-namebox', { 'aria-label': 'Selected cell' }, 'A1');
  const fx = h('input.sheet-fx', { type: 'text', 'aria-label': 'Cell contents', placeholder: readOnly ? '' : 'Value or =formula, e.g. =SUM(A1:A9)', readonly: readOnly, spellcheck: 'false' });
  toolbar.append(
    nameBox, h('span.sheet-fx-label', { 'aria-hidden': 'true' }, 'fx'), fx,
    !readOnly && h('button.btn.sm', { type: 'button', title: 'Add 20 rows', onclick: () => { grow(rows + 20, cols); } }, icon('plus', 'sm'), 'Rows'),
    !readOnly && h('button.btn.sm', { type: 'button', title: 'Add 5 columns', onclick: () => { grow(rows, cols + 5); } }, icon('plus', 'sm'), 'Columns'),
    h('a.btn.sm', { href: `/api/files/${entry.id}/download`, download: entry.name, title: 'Download as CSV' }, icon('download', 'sm'), 'CSV'));

  // ---- grid ----
  const headRow = h('tr', h('th.sheet-corner', { scope: 'col' }));
  const tbody = h('tbody');
  const table = h('table.sheet-grid', { role: 'grid', 'aria-label': entry.name }, h('thead', headRow), tbody);
  const input = h('input.sheet-input', { type: 'text', 'aria-label': 'Edit cell', hidden: true, spellcheck: 'false' });
  const overlays = h('div.sheet-overlays');
  const scroller = h('div.sheet-scroll', { tabindex: 0, 'aria-label': 'Sheet. Arrow keys move, type or Enter to edit.' }, table, overlays, input);
  body.appendChild(scroller);

  function addColumns(from, to) {
    for (let c = from; c < to; c++) headRow.appendChild(h('th.col-head', { scope: 'col', dataset: { c } }, colName(c)));
    for (let r = 0; r < tds.length; r++) {
      const tr = tbody.children[r];
      for (let c = from; c < to; c++) {
        const td = h('td', { dataset: { r, c } });
        tr.appendChild(td);
        tds[r][c] = td;
      }
    }
  }

  function addRows(from, to) {
    const frag = document.createDocumentFragment();
    for (let r = from; r < to; r++) {
      const tr = h('tr', h('th.row-head', { scope: 'row' }, String(r + 1)));
      tds[r] = [];
      for (let c = 0; c < cols; c++) {
        const td = h('td', { dataset: { r, c } });
        tr.appendChild(td);
        tds[r][c] = td;
      }
      frag.appendChild(tr);
    }
    tbody.appendChild(frag);
  }

  function grow(nr, nc) {
    nr = Math.min(MAX_ROWS, Math.max(rows, nr));
    nc = Math.min(MAX_COLS, Math.max(cols, nc));
    if (nc > cols) { const old = cols; cols = nc; addColumns(old, nc); }
    if (nr > rows) { const old = rows; rows = nr; addRows(old, nr); }
    queuePeers();
  }

  for (let c = 0; c < cols; c++) headRow.appendChild(h('th.col-head', { scope: 'col', dataset: { c } }, colName(c)));
  addRows(0, rows);

  // ---- values ----
  /** Re-evaluates every formula and updates only cells whose displayed text changed. */
  function recompute() {
    let maxR = -1;
    let maxC = -1;
    const keys = [];
    cells.forEach((_, k) => {
      const [r, c] = k.split(',').map(Number);
      if (!Number.isInteger(r) || !Number.isInteger(c) || r < 0 || c < 0 || r >= MAX_ROWS || c >= MAX_COLS) return;
      keys.push([k, r, c]);
      if (raw(r, c) !== '') { maxR = Math.max(maxR, r); maxC = Math.max(maxC, c); }
    });
    // Grow when content gets near the edge.
    if (maxR >= rows - 2 || maxC >= cols - 1) grow(Math.max(rows, maxR + 10), Math.max(cols, maxC + 3));
    const value = evaluator(raw);
    const next = new Map();
    for (const [k, r, c] of keys) {
      const v = value(r, c);
      const text = display(v);
      if (text !== '') next.set(k, { text, num: typeof v === 'number', err: v instanceof SheetError, formula: raw(r, c).startsWith('=') });
    }
    for (const k of shown.keys()) if (!next.has(k)) paint(k, null);
    for (const [k, v] of next) paint(k, v);
  }

  function paint(k, v) {
    const prev = shown.get(k);
    const sig = v ? `${v.text}|${v.num}|${v.err}|${v.formula}` : null;
    if (prev === sig) return;
    const [r, c] = k.split(',').map(Number);
    const td = tds[r] && tds[r][c];
    if (!td) return;
    if (v) {
      shown.set(k, sig);
      td.textContent = v.text;
      td.className = [v.num && 'num', v.err && 'err', v.formula && 'formula'].filter(Boolean).join(' ');
      td.title = v.formula ? raw(r, c) : '';
    } else {
      shown.delete(k);
      td.textContent = '';
      td.className = '';
      td.title = '';
    }
    if (r === sel.r && c === sel.c) td.classList.add('sel');
  }

  const queueRecompute = rafBatch(() => { recompute(); syncFx(); });
  const onCells = () => queueRecompute();
  cells.observe(onCells);

  // ---- selection ----
  function syncFx() {
    nameBox.textContent = `${colName(sel.c)}${sel.r + 1}`;
    if (document.activeElement !== fx) fx.value = raw(sel.r, sel.c);
  }

  function select(r, c, { scroll = true } = {}) {
    r = Math.max(0, Math.min(r, MAX_ROWS - 1));
    c = Math.max(0, Math.min(c, MAX_COLS - 1));
    if (r >= rows - 1 || c >= cols) grow(r >= rows - 1 ? r + 10 : rows, c >= cols ? c + 3 : cols);
    tds[sel.r]?.[sel.c]?.classList.remove('sel');
    headRow.children[sel.c + 1]?.classList.remove('active');
    tbody.children[sel.r]?.firstChild.classList.remove('active');
    sel = { r, c };
    const td = tds[r][c];
    td.classList.add('sel');
    headRow.children[c + 1].classList.add('active');
    tbody.children[r].firstChild.classList.add('active');
    syncFx();
    if (scroll) ensureVisible(td);
    provider.setLocal('cell', `${r},${c}`);
  }

  function ensureVisible(td) {
    const head = headRow.offsetHeight;
    const side = tbody.children[0]?.firstChild.offsetWidth || 46;
    const top = td.offsetTop;
    const left = td.offsetLeft;
    if (top - head < scroller.scrollTop) scroller.scrollTop = top - head;
    else if (top + td.offsetHeight > scroller.scrollTop + scroller.clientHeight) scroller.scrollTop = top + td.offsetHeight - scroller.clientHeight;
    if (left - side < scroller.scrollLeft) scroller.scrollLeft = left - side;
    else if (left + td.offsetWidth > scroller.scrollLeft + scroller.clientWidth) scroller.scrollLeft = left + td.offsetWidth - scroller.clientWidth;
  }

  // ---- editing ----
  function setCell(r, c, value) {
    const k = `${r},${c}`;
    doc.transact(() => {
      if (value === '' || value === null || value === undefined) { if (cells.has(k)) cells.delete(k); } else if (cells.get(k) !== value) cells.set(k, value);
    }, 'local');
  }

  function startEdit(initial) {
    if (readOnly) return;
    const td = tds[sel.r][sel.c];
    editing = { r: sel.r, c: sel.c };
    input.value = initial ?? raw(sel.r, sel.c);
    Object.assign(input.style, { left: `${td.offsetLeft}px`, top: `${td.offsetTop}px`, width: `${Math.max(td.offsetWidth, 120)}px`, height: `${td.offsetHeight}px` });
    input.hidden = false;
    input.focus();
    const end = input.value.length;
    input.setSelectionRange(end, end);
  }

  function commitEdit(dr = 0, dc = 0) {
    if (!editing) return;
    const { r, c } = editing;
    editing = null;
    input.hidden = true;
    setCell(r, c, input.value);
    scroller.focus({ preventScroll: true });
    select(r + dr, c + dc);
  }

  function cancelEdit() {
    editing = null;
    input.hidden = true;
    scroller.focus({ preventScroll: true });
    syncFx();
  }

  input.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); commitEdit(e.shiftKey ? -1 : 1, 0); } else if (e.key === 'Tab') { e.preventDefault(); commitEdit(0, e.shiftKey ? -1 : 1); } else if (e.key === 'Escape') { e.preventDefault(); cancelEdit(); } else if ((e.key === 'ArrowUp' || e.key === 'ArrowDown') && !input.value.startsWith('=')) {
      e.preventDefault();
      commitEdit(e.key === 'ArrowUp' ? -1 : 1, 0);
    }
  });
  input.addEventListener('input', () => { fx.value = input.value; });
  input.addEventListener('blur', () => { if (editing) commitEdit(); });

  fx.addEventListener('keydown', (e) => {
    if (e.key === 'Enter') { e.preventDefault(); setCell(sel.r, sel.c, fx.value); scroller.focus(); select(sel.r + 1, sel.c); }
    if (e.key === 'Escape') { e.preventDefault(); fx.value = raw(sel.r, sel.c); scroller.focus(); }
  });
  fx.addEventListener('blur', () => { if (!readOnly && fx.value !== raw(sel.r, sel.c)) setCell(sel.r, sel.c, fx.value); });

  scroller.addEventListener('keydown', (e) => {
    if (e.target !== scroller) return;
    const k = e.key;
    const mod = e.ctrlKey || e.metaKey;
    const moves = { ArrowUp: [-1, 0], ArrowDown: [1, 0], ArrowLeft: [0, -1], ArrowRight: [0, 1] };
    if (moves[k] && !e.altKey) {
      e.preventDefault();
      const [dr, dc] = moves[k];
      if (mod) select(dr < 0 ? 0 : dr > 0 ? lastRow(sel.c) : sel.r, dc < 0 ? 0 : dc > 0 ? lastCol(sel.r) : sel.c);
      else select(sel.r + dr, sel.c + dc);
    } else if (k === 'Tab') {
      e.preventDefault();
      select(sel.r, sel.c + (e.shiftKey ? -1 : 1));
    } else if (k === 'Enter' || k === 'F2') {
      e.preventDefault();
      if (k === 'Enter' && e.shiftKey) select(sel.r - 1, sel.c); else startEdit();
    } else if ((k === 'Delete' || k === 'Backspace') && !readOnly) {
      e.preventDefault();
      setCell(sel.r, sel.c, '');
    } else if (mod && (k === 'z' || k === 'Z' || k === 'y') && undo) {
      e.preventDefault();
      if (k === 'y' || e.shiftKey) undo.redo(); else undo.undo();
    } else if (k === 'Home') {
      e.preventDefault();
      select(mod ? 0 : sel.r, 0);
    } else if (k === 'PageDown' || k === 'PageUp') {
      e.preventDefault();
      const page = Math.max(1, Math.floor(scroller.clientHeight / 28) - 2);
      select(sel.r + (k === 'PageDown' ? page : -page), sel.c);
    } else if (k.length === 1 && !mod && !e.altKey && !readOnly) {
      e.preventDefault();
      startEdit(k);
    }
  });

  function lastRow(c) { let m = 0; cells.forEach((v, k) => { const [r, cc] = k.split(',').map(Number); if (cc === c && v !== '') m = Math.max(m, r); }); return m; }
  function lastCol(r) { let m = 0; cells.forEach((v, k) => { const [rr, c] = k.split(',').map(Number); if (rr === r && v !== '') m = Math.max(m, c); }); return m; }

  // Copy / paste: tab- and newline-separated blocks, like other spreadsheets.
  scroller.addEventListener('copy', (e) => {
    if (e.target !== scroller) return;
    e.preventDefault();
    e.clipboardData.setData('text/plain', raw(sel.r, sel.c));
  });
  scroller.addEventListener('cut', (e) => {
    if (e.target !== scroller || readOnly) return;
    e.preventDefault();
    e.clipboardData.setData('text/plain', raw(sel.r, sel.c));
    setCell(sel.r, sel.c, '');
  });
  scroller.addEventListener('paste', (e) => {
    if (e.target !== scroller || readOnly) return;
    e.preventDefault();
    const text = e.clipboardData.getData('text/plain').replace(/\r\n?/g, '\n').replace(/\n$/, '');
    const grid = text.split('\n').map((line) => line.split('\t'));
    doc.transact(() => {
      grid.forEach((row, i) => row.forEach((v, j) => {
        const k = `${sel.r + i},${sel.c + j}`;
        if (v === '') cells.delete(k); else cells.set(k, v);
      }));
    }, 'local');
  });

  table.addEventListener('mousedown', (e) => {
    const td = e.target.closest('td');
    if (!td) return;
    if (editing) commitEdit();
    e.preventDefault();
    scroller.focus({ preventScroll: true });
    select(Number(td.dataset.r), Number(td.dataset.c), { scroll: false });
  });
  table.addEventListener('dblclick', (e) => {
    const td = e.target.closest('td');
    if (td) startEdit();
  });

  // ---- peers' selections ----
  const peerEls = new Map(); // clientId -> element
  function renderPeers() {
    const live = new Set();
    provider.awareness.getStates().forEach((state, clientId) => {
      if (clientId === doc.clientID || !state.user || typeof state.cell !== 'string') return;
      const [r, c] = state.cell.split(',').map(Number);
      const td = tds[r] && tds[r][c];
      if (!td) return;
      live.add(clientId);
      let el = peerEls.get(clientId);
      if (!el) {
        el = h('div.peer-cell', h('span.peer-cell-name'));
        overlays.appendChild(el);
        peerEls.set(clientId, el);
      }
      el.style.setProperty('--peer', state.user.color);
      el.firstChild.textContent = state.user.name;
      Object.assign(el.style, { left: `${td.offsetLeft}px`, top: `${td.offsetTop}px`, width: `${td.offsetWidth}px`, height: `${td.offsetHeight}px` });
    });
    for (const [id, el] of peerEls) if (!live.has(id)) { el.remove(); peerEls.delete(id); }
  }
  const queuePeers = rafBatch(renderPeers);
  provider.awareness.on('change', queuePeers);

  setHelp([
    ['↑ ↓ ← →', 'Move the selection'],
    ['Ctrl+arrow', 'Jump to the edge of the data'],
    ['Enter / F2 / double-click', 'Edit the cell'],
    ['Type', 'Replace the cell contents'],
    ['Enter / Tab', 'Save and move down / right (Shift: back)'],
    ['Esc', 'Cancel editing'],
    ['Delete', 'Clear the cell'],
    ['Ctrl+C / Ctrl+V', 'Copy / paste (tab-separated blocks)'],
    ['Ctrl+Z / Ctrl+Shift+Z', 'Undo / redo your changes'],
    ['=SUM(A1:A9)', 'Formulas: + - * /, SUM, AVERAGE, MIN, MAX, COUNT'],
  ]);

  recompute();
  select(0, 0, { scroll: false });
  provider.synced.then(() => { recompute(); syncFx(); });
  setTimeout(() => scroller.focus({ preventScroll: true }), 0);

  return () => {
    cells.unobserve(onCells);
    provider.awareness.off('change', queuePeers);
    if (undo) undo.destroy();
    clear(body);
  };
}
