// A rich text (HTML) editor without a framework: a contenteditable area with a toolbar
// (undo/redo, paragraph and headings, font size, bold, italic, underline, strikethrough,
// text colour, lists, indent, quote, links, inline images, horizontal rule, clear
// formatting) and the usual shortcuts (Ctrl/Cmd+B, I, U, K for a link, Shift+7/8 for lists,
// Shift+9 for a quote, \ to clear formatting, Tab/Shift+Tab in lists, Z/Y for undo/redo).
// Formatting goes through the browser's editing commands so undo and redo keep working.
//
// Everything that enters from outside (paste, drop, inserted snippets) passes the whitelist
// in sanitize.js first; pasted and dropped images are uploaded and shown from the
// composer's uploads, which the server sends as inline (cid:) parts.

import { h, clear, icon, promptDialog, toast } from '../ui.js';
import { sanitizeFragment, sanitizeNodes, serialize } from './sanitize.js';
import { htmlToText } from './convert.js';

const SVG_NS = 'http://www.w3.org/2000/svg';
const PATHS = {
  bold: 'M7 5h6a3.5 3.5 0 0 1 0 7H7zM7 12h7a3.5 3.5 0 0 1 0 7H7z',
  italic: 'M10 5h8M6 19h8M14 5l-4 14',
  underline: 'M7 4v7a5 5 0 0 0 10 0V4M5 20h14',
  strike: 'M4 12h16M16 7a4 3 0 0 0-4-2c-2.2 0-4 1.2-4 3 0 1 .5 1.7 1.3 2.2M8 17a4 3 0 0 0 4 2c2.2 0 4-1.2 4-3 0-.7-.2-1.2-.6-1.7',
  ul: 'M9 6h11M9 12h11M9 18h11M4.5 6h.01M4.5 12h.01M4.5 18h.01',
  ol: 'M10 6h10M10 12h10M10 18h10M4 4.5h1.5V9M4 9h3M4 14.5c.4-.6 1-1 1.7-1 .9 0 1.5.6 1.5 1.3 0 1.4-3.2 2.1-3.2 4.2h3.3',
  quote: 'M5 17h4v-5H6c0-2.5 1-4 3-5M14 17h4v-5h-3c0-2.5 1-4 3-5',
  indent: 'M4 5h16M11 10h9M11 14h9M4 19h16M4 9l3 3-3 3',
  outdent: 'M4 5h16M11 10h9M11 14h9M4 19h16M7 9l-3 3 3 3',
  hr: 'M3 12h18M8 7h8M8 17h8',
  clear: 'M6 5h11M11 5L8 19M14 14l6 6M20 14l-6 6',
  undo: 'M9 14L4 9l5-5M4 9h10a6 6 0 0 1 0 12h-3',
  redo: 'M15 14l5-5-5-5M20 9H10a6 6 0 0 0 0 12h3',
  color: 'M7 16l5-12 5 12M8.7 12h6.6',
};

function edIcon(name) {
  const svg = document.createElementNS(SVG_NS, 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('class', 'icon sm');
  svg.setAttribute('aria-hidden', 'true');
  const p = document.createElementNS(SVG_NS, 'path');
  p.setAttribute('d', PATHS[name]);
  svg.appendChild(p);
  return svg;
}

const COLORS = ['#16191f', '#6b7280', '#b91c1c', '#c2410c', '#b45309', '#15803d', '#0f766e', '#1d4ed8', '#7c3aed', '#be185d'];
const BLOCKS = [['div', 'Paragraph'], ['h1', 'Heading 1'], ['h2', 'Heading 2'], ['h3', 'Heading 3'], ['pre', 'Preformatted']];
const SIZES = [['2', 'Small'], ['3', 'Normal'], ['5', 'Large'], ['6', 'Huge']];
const BLOCK_TAGS = new Set(['div', 'p', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'pre', 'li', 'blockquote']);

/**
 * Creates an editor. opts: { html (sanitized string or fragment), label, onChange(),
 * uploadImage(file) -> Promise<src>, onFiles(files) for dropped non-image files,
 * onKeydown(e) -> true when handled (the composer's pickers) }.
 */
export function createRichEditor(opts = {}) {
  const area = h('div.ed-area', {
    contenteditable: 'true', role: 'textbox', 'aria-multiline': 'true', 'aria-label': opts.label || 'Message', spellcheck: 'true',
  });
  let saved = null;
  const changed = () => opts.onChange?.();

  const inArea = (node) => node && (node === area || area.contains(node));
  function saveSelection() {
    const sel = document.getSelection();
    if (sel.rangeCount && inArea(sel.getRangeAt(0).commonAncestorContainer)) saved = sel.getRangeAt(0).cloneRange();
  }
  /** Brings the caret back into the editor (after a toolbar control or dialog took focus).
   *  A live selection inside the editor always wins over the remembered one. */
  function restoreSelection() {
    const sel = document.getSelection();
    const live = sel.rangeCount && inArea(sel.getRangeAt(0).commonAncestorContainer);
    area.focus({ preventScroll: true });
    if (live) return;
    if (saved && inArea(saved.commonAncestorContainer)) {
      sel.removeAllRanges();
      sel.addRange(saved);
    } else if (!sel.rangeCount || !inArea(sel.getRangeAt(0).commonAncestorContainer)) {
      const r = document.createRange();
      r.selectNodeContents(area);
      r.collapse(true);
      sel.removeAllRanges();
      sel.addRange(r);
    }
  }
  /** Selects `range` in the editor (after a dialog took the focus). */
  function select(range) {
    area.focus({ preventScroll: true });
    const sel = document.getSelection();
    sel.removeAllRanges();
    sel.addRange(range);
    saved = range;
  }

  /** Moves the caret out of inline formatting it is at the end of (after inserting a
   *  snippet, typing should not continue in its italics or link). */
  function leaveInline() {
    const sel = document.getSelection();
    if (!sel.rangeCount || !inArea(sel.getRangeAt(0).startContainer)) return;
    let outer = null;
    for (let el = sel.getRangeAt(0).startContainer.parentElement; el && el !== area; el = el.parentElement) {
      if (/^(b|strong|i|em|u|s|strike|span|font|a|code|mark|sub|sup|small|big|del|ins)$/.test(el.localName)) outer = el;
    }
    if (!outer) return;
    // A zero-width space holds the caret outside; it is removed whenever content is read.
    const holder = document.createTextNode('\u200b');
    outer.after(holder);
    const r = document.createRange();
    r.setStart(holder, 1);
    r.collapse(true);
    select(r);
  }

  let attached = false;
  const onSelection = () => {
    // The listener goes away by itself once the editor has left the page.
    if (area.isConnected) attached = true;
    else if (attached) { document.removeEventListener('selectionchange', onSelection); return; }
    saveSelection();
    paintState();
  };
  document.addEventListener('selectionchange', onSelection);

  function exec(cmd, value = null, css = false) {
    restoreSelection();
    document.execCommand('styleWithCSS', false, css);
    document.execCommand(cmd, false, value);
    document.execCommand('styleWithCSS', false, false);
    saveSelection();
    changed();
    paintState();
  }

  /** Inserts sanitized content at the caret (one undo step). */
  function insertFragment(frag) {
    const html = serialize(frag);
    if (html) exec('insertHTML', html);
  }

  function insertText(text) {
    restoreSelection();
    document.execCommand('insertText', false, text);
    saveSelection();
    changed();
  }

  const closestIn = (node, sel) => {
    const el = node && (node.nodeType === Node.ELEMENT_NODE ? node : node.parentElement);
    const found = el?.closest(sel);
    return found && inArea(found) && found !== area ? found : null;
  };
  const caretNode = () => {
    const sel = document.getSelection();
    return sel.rangeCount && inArea(sel.getRangeAt(0).startContainer) ? sel.getRangeAt(0).startContainer : null;
  };

  /** The block elements the selection touches (direct children of a common parent). */
  function selectedBlocks() {
    restoreSelection();
    const r = document.getSelection().getRangeAt(0);
    const block = (n) => {
      let el = n.nodeType === Node.ELEMENT_NODE ? n : n.parentElement;
      while (el && el !== area && !BLOCK_TAGS.has(el.localName)) el = el.parentElement;
      return el && el !== area ? el : null;
    };
    const a = block(r.startContainer);
    const b = block(r.endContainer);
    if (!a) return [];
    if (!b || a === b || a.parentElement !== b.parentElement) return [a];
    const out = [];
    for (let n = a; n; n = n.nextElementSibling) { out.push(n); if (n === b) break; }
    return out;
  }

  function indent(dir) {
    if (closestIn(caretNode(), 'li')) { exec(dir > 0 ? 'indent' : 'outdent'); return; }
    for (const b of selectedBlocks()) {
      const cur = parseInt(b.style.marginLeft, 10) || 0;
      const next = Math.max(0, cur + dir * 40);
      b.style.marginLeft = next ? `${next}px` : '';
      if (!b.getAttribute('style')) b.removeAttribute('style');
    }
    changed();
  }

  function toggleQuote() {
    const q = closestIn(caretNode(), 'blockquote');
    if (!q) { exec('formatBlock', 'blockquote'); return; }
    while (q.firstChild) q.parentNode.insertBefore(q.firstChild, q);
    q.remove();
    changed();
  }

  async function editLink() {
    saveSelection();
    const a = closestIn(caretNode(), 'a');
    const range = saved;
    let url = await promptDialog(a ? 'Edit link' : 'Insert link', {
      label: 'Link address', value: a?.getAttribute('href') || 'https://', confirm: a ? 'Save' : 'Insert', type: 'text',
    });
    if (range) select(range); else restoreSelection();
    if (!url) return;
    if (/^[^\s@]+@[^\s@]+\.[^\s@]+$/.test(url)) url = `mailto:${url}`;
    else if (!/^[a-z][a-z0-9+.-]*:/i.test(url)) url = `https://${url}`;
    if (!/^(https?:|mailto:|tel:)/i.test(url)) { toast('Links can point to web pages, email addresses or phone numbers', 'warn'); return; }
    if (a) { a.setAttribute('href', url); changed(); return; }
    if (!range || range.collapsed) {
      const frag = document.createDocumentFragment();
      frag.appendChild(h('a', { href: url }, url));
      insertFragment(frag);
    } else exec('createLink', url);
  }

  async function insertImageFile(file) {
    if (!opts.uploadImage) return;
    try {
      const src = await opts.uploadImage(file);
      const img = document.createElement('img');
      img.setAttribute('src', src);
      img.setAttribute('alt', file.name.replace(/\.[^.]+$/, ''));
      const frag = document.createDocumentFragment();
      frag.appendChild(img);
      insertFragment(frag);
    } catch (e) {
      toast(e.message || String(e), 'error');
    }
  }

  // ---- toolbar ------------------------------------------------------------------------------
  const stateButtons = [];
  const btn = (name, label, run, state) => {
    const b = h('button.btn.ghost.icon.sm.ed-btn', {
      type: 'button', title: label, 'aria-label': label,
      onmousedown: (e) => e.preventDefault(),
      onclick: run,
    }, name === 'link' || name === 'image' ? icon(name, 'sm') : edIcon(name));
    if (state) { b.setAttribute('aria-pressed', 'false'); stateButtons.push([b, state]); }
    return b;
  };
  const blockSelect = h('select.ed-select', {
    'aria-label': 'Paragraph style', title: 'Paragraph style',
    onchange: () => { exec('formatBlock', blockSelect.value); },
  }, BLOCKS.map(([v, l]) => h('option', { value: v }, l)));
  const sizeSelect = h('select.ed-select', {
    'aria-label': 'Font size', title: 'Font size',
    onchange: () => { exec('fontSize', sizeSelect.value, true); sizeSelect.value = '3'; },
  }, SIZES.map(([v, l]) => h('option', { value: v, selected: v === '3' }, l)));
  const colorBar = h('span.ed-color-bar');
  const colorBtn = h('button.btn.ghost.icon.sm.ed-btn.ed-color', {
    type: 'button', title: 'Text colour', 'aria-label': 'Text colour', 'aria-haspopup': 'true',
    onmousedown: (e) => e.preventDefault(),
    onclick: () => colorPopup(),
  }, edIcon('color'), colorBar);
  const fileInput = h('input', {
    type: 'file', accept: 'image/png,image/jpeg,image/gif,image/webp', multiple: true, hidden: true,
    onchange: () => { [...fileInput.files].forEach(insertImageFile); fileInput.value = ''; },
  });

  function colorPopup() {
    document.querySelectorAll('.ed-pop').forEach((p) => p.remove());
    saveSelection();
    const pop = h('div.ed-pop', { role: 'menu', 'aria-label': 'Text colour' },
      COLORS.map((c) => h('button.ed-swatch', {
        type: 'button', role: 'menuitem', title: c, 'aria-label': `Colour ${c}`, style: { background: c },
        onmousedown: (e) => e.preventDefault(),
        onclick: () => { pop.remove(); exec('foreColor', c, true); colorBar.style.background = c; },
      })),
      h('button.btn.ghost.sm.ed-swatch-auto', {
        type: 'button', role: 'menuitem', onmousedown: (e) => e.preventDefault(),
        onclick: () => { pop.remove(); removeColor(); },
      }, 'Automatic'));
    document.body.appendChild(pop);
    const r = colorBtn.getBoundingClientRect();
    const pr = pop.getBoundingClientRect();
    pop.style.left = `${Math.max(8, Math.min(r.left, innerWidth - pr.width - 8))}px`;
    pop.style.top = `${Math.min(r.bottom + 4, innerHeight - pr.height - 8)}px`;
    const away = (e) => { if (!pop.contains(e.target)) { pop.remove(); document.removeEventListener('mousedown', away, true); } };
    setTimeout(() => document.addEventListener('mousedown', away, true), 0);
  }

  function removeColor() {
    restoreSelection();
    const r = document.getSelection().getRangeAt(0);
    const root = r.commonAncestorContainer.nodeType === Node.ELEMENT_NODE ? r.commonAncestorContainer : r.commonAncestorContainer.parentElement;
    const scope = inArea(root) ? root : area;
    scope.querySelectorAll('[style*="color"], font[color]').forEach((el) => {
      if (!r.intersectsNode(el)) return;
      el.style.removeProperty('color');
      el.removeAttribute('color');
      if (!el.getAttribute('style')) el.removeAttribute('style');
    });
    const own = closestIn(r.startContainer, '[style*="color"], font[color]');
    if (own) { own.style.removeProperty('color'); own.removeAttribute('color'); }
    colorBar.style.background = '';
    changed();
  }

  const toolbar = h('div.ed-toolbar', { role: 'toolbar', 'aria-label': 'Formatting' },
    btn('undo', 'Undo (Ctrl+Z)', () => exec('undo')),
    btn('redo', 'Redo (Ctrl+Y)', () => exec('redo')),
    h('span.ed-sep'),
    blockSelect, sizeSelect,
    h('span.ed-sep'),
    btn('bold', 'Bold (Ctrl+B)', () => exec('bold'), 'bold'),
    btn('italic', 'Italic (Ctrl+I)', () => exec('italic'), 'italic'),
    btn('underline', 'Underline (Ctrl+U)', () => exec('underline'), 'underline'),
    btn('strike', 'Strikethrough', () => exec('strikeThrough'), 'strikeThrough'),
    colorBtn,
    h('span.ed-sep'),
    btn('ul', 'Bulleted list (Ctrl+Shift+8)', () => exec('insertUnorderedList'), 'insertUnorderedList'),
    btn('ol', 'Numbered list (Ctrl+Shift+7)', () => exec('insertOrderedList'), 'insertOrderedList'),
    btn('outdent', 'Decrease indent (Shift+Tab)', () => indent(-1)),
    btn('indent', 'Increase indent (Tab)', () => indent(1)),
    btn('quote', 'Quote (Ctrl+Shift+9)', toggleQuote),
    h('span.ed-sep'),
    btn('link', 'Link (Ctrl+K)', editLink),
    btn('image', 'Insert image', () => { saveSelection(); fileInput.click(); }),
    btn('hr', 'Horizontal line', () => exec('insertHorizontalRule')),
    btn('clear', 'Clear formatting (Ctrl+\\)', () => { exec('removeFormat'); exec('unlink'); }),
    fileInput);

  function paintState() {
    if (!inArea(caretNode())) return;
    for (const [b, cmd] of stateButtons) {
      let on = false;
      try { on = document.queryCommandState(cmd); } catch (_) { /* not supported */ }
      b.classList.toggle('active', on);
      b.setAttribute('aria-pressed', String(on));
    }
    const block = closestIn(caretNode(), 'h1, h2, h3, pre');
    blockSelect.value = block ? block.localName : 'div';
  }

  // ---- input ----------------------------------------------------------------------------------
  area.addEventListener('input', changed);
  area.addEventListener('keydown', (e) => {
    if (opts.onKeydown?.(e)) return;
    const mod = e.ctrlKey || e.metaKey;
    const k = e.key.toLowerCase();
    if (mod && !e.shiftKey && !e.altKey) {
      if (k === 'b') { e.preventDefault(); exec('bold'); return; }
      if (k === 'i') { e.preventDefault(); exec('italic'); return; }
      if (k === 'u') { e.preventDefault(); exec('underline'); return; }
      if (k === 'k') { e.preventDefault(); editLink(); return; }
      if (e.key === '\\') { e.preventDefault(); exec('removeFormat'); return; }
    }
    if (mod && e.shiftKey) {
      if (e.code === 'Digit7') { e.preventDefault(); exec('insertOrderedList'); return; }
      if (e.code === 'Digit8') { e.preventDefault(); exec('insertUnorderedList'); return; }
      if (e.code === 'Digit9') { e.preventDefault(); toggleQuote(); return; }
    }
    if (e.key === 'Tab' && !mod && closestIn(caretNode(), 'li')) {
      e.preventDefault();
      indent(e.shiftKey ? -1 : 1);
    }
  });
  area.addEventListener('paste', (e) => {
    const dt = e.clipboardData;
    if (!dt) return;
    const images = [...dt.files].filter((f) => f.type.startsWith('image/'));
    if (images.length) { e.preventDefault(); images.forEach(insertImageFile); return; }
    const html = dt.getData('text/html');
    if (html) { e.preventDefault(); insertFragment(sanitizeFragment(html)); return; }
    const text = dt.getData('text/plain');
    if (text) { e.preventDefault(); insertText(text); }
  });
  area.addEventListener('dragover', (e) => { if (e.dataTransfer.types.includes('Files')) e.preventDefault(); });
  area.addEventListener('drop', (e) => {
    const files = [...e.dataTransfer.files];
    const html = e.dataTransfer.getData('text/html');
    if (!files.length && !html) return;
    e.preventDefault();
    e.stopPropagation();
    const pos = document.caretRangeFromPoint?.(e.clientX, e.clientY);
    if (pos && inArea(pos.startContainer)) {
      const sel = document.getSelection();
      sel.removeAllRanges();
      sel.addRange(pos);
      saveSelection();
    }
    if (files.length) {
      files.filter((f) => f.type.startsWith('image/')).forEach(insertImageFile);
      const others = files.filter((f) => !f.type.startsWith('image/'));
      if (others.length) opts.onFiles?.(others);
      return;
    }
    insertFragment(sanitizeFragment(html));
  });

  const el = h('div.ed', toolbar, area);
  const api = {
    el, area, toolbar,
    /** Replaces the content with sanitized nodes (not an undo step: used when opening). */
    setContent(content) {
      clear(area);
      area.appendChild(typeof content === 'string' ? sanitizeFragment(content) : content);
      if (!area.firstChild) area.appendChild(h('div', h('br')));
    },
    getHtml: () => serialize(sanitizeNodes(area)),
    getText: () => htmlToText(area),
    isEmpty: () => !htmlToText(area).trim() && !area.querySelector('img'),
    insertFragment,
    /** Inserts a snippet (a text block) and leaves the caret after its formatting. */
    insertSnippet(frag) { insertFragment(frag); leaveInline(); },
    leaveInline,
    insertText,
    saveSelection,
    restoreSelection,
    /** Whether the caret was in the editor since it was opened. */
    hasCaret: () => !!saved,
    /** Puts the caret at the start (writing above a quote). */
    caretToStart() {
      const r = document.createRange();
      const first = area.firstElementChild;
      r.setStart(first || area, 0);
      r.collapse(true);
      saved = r;
      restoreSelection();
    },
    focus: () => restoreSelection(),
    destroy: () => document.removeEventListener('selectionchange', onSelection),
  };
  api.setContent(opts.html || '');
  document.execCommand('defaultParagraphSeparator', false, 'div');
  return api;
}
