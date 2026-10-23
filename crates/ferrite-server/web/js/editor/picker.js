// Keyboard pickers in the message body: a trigger character followed by a filter opens a
// list at the caret ("@ann" for people, "!greet" for text blocks). Arrow keys move, Enter or
// Tab picks, Escape closes. Works in the rich editor (contenteditable) and in the plain
// textarea; the picked item replaces the trigger and the filter.

import { h, clear } from '../ui.js';

let seq = 0;

/** Caret rectangle in a textarea (a mirror element measures it). */
function textareaCaret(ta) {
  const cs = getComputedStyle(ta);
  const mirror = h('div', {
    style: {
      position: 'absolute', visibility: 'hidden', whiteSpace: 'pre-wrap', overflowWrap: 'break-word',
      width: `${ta.clientWidth}px`, font: cs.font, padding: cs.padding, border: cs.border, letterSpacing: cs.letterSpacing, lineHeight: cs.lineHeight,
    },
  }, ta.value.slice(0, ta.selectionStart));
  const mark = h('span', '​');
  mirror.appendChild(mark);
  document.body.appendChild(mirror);
  const r = ta.getBoundingClientRect();
  const top = r.top + mark.offsetTop - ta.scrollTop;
  const left = r.left + mark.offsetLeft - ta.scrollLeft;
  const lh = parseFloat(cs.lineHeight) || 20;
  mirror.remove();
  return { left, top, bottom: top + lh };
}

/**
 * Attaches pickers to a body field. opts: { field: element, mode: () => 'rich'|'plain',
 * triggers: { '@': async (query) => items, '!': ... }, onPick(trigger, item, replaceWith) }.
 * Items are { label, sub, data }. `replaceWith(nodesOrText)` swaps trigger and filter for the
 * picked content. Returns { onKey(e) -> handled, close(), destroy() }.
 */
export function attachPicker({ field, mode, triggers, onPick }) {
  const id = `ed-picker-${++seq}`;
  const box = h('div.ed-picker', { id, role: 'listbox', hidden: true });
  document.body.appendChild(box);
  let items = [];
  let active = 0;
  let ctx = null; // {trigger, query, node/offset or start}
  let req = 0;

  function context() {
    if (mode() === 'plain') {
      const ta = field();
      if (document.activeElement !== ta || ta.selectionStart !== ta.selectionEnd) return null;
      const before = ta.value.slice(0, ta.selectionStart);
      const m = before.match(/(?:^|[\s(])([@!])([^\s@!]{0,40})$/u);
      if (!m || !triggers[m[1]]) return null;
      return { trigger: m[1], query: m[2], start: ta.selectionStart - m[2].length - 1, end: ta.selectionStart };
    }
    const sel = document.getSelection();
    if (!sel.rangeCount || !sel.isCollapsed) return null;
    const r = sel.getRangeAt(0);
    const node = r.startContainer;
    if (node.nodeType !== Node.TEXT_NODE || !field().contains(node)) return null;
    const before = node.nodeValue.slice(0, r.startOffset);
    const m = before.match(/(?:^|[\s( ])([@!])([^\s@! ]{0,40})$/u);
    if (!m || !triggers[m[1]]) return null;
    return { trigger: m[1], query: m[2], node, start: r.startOffset - m[2].length - 1, end: r.startOffset };
  }

  function caretRect() {
    if (mode() === 'plain') return textareaCaret(field());
    const r = document.getSelection().getRangeAt(0).cloneRange();
    const rects = r.getClientRects();
    if (rects.length) return rects[0];
    return field().getBoundingClientRect();
  }

  function paint() {
    [...box.children].forEach((el, i) => {
      el.classList.toggle('active', i === active);
      el.setAttribute('aria-selected', String(i === active));
      if (i === active) el.scrollIntoView({ block: 'nearest' });
    });
  }

  function close() {
    box.hidden = true;
    items = [];
    ctx = null;
    field()?.removeAttribute('aria-activedescendant');
  }

  function show(list) {
    items = list;
    active = 0;
    clear(box);
    if (!list.length) { box.hidden = true; return; }
    list.forEach((it, i) => box.appendChild(h('div.ed-picker-item', {
      id: `${id}-${i}`, role: 'option',
      onmousedown: (e) => { e.preventDefault(); active = i; pick(); },
    }, h('span.ellipsis.grow', it.label), it.sub && h('span.small.muted.ellipsis', it.sub))));
    box.hidden = false;
    const r = caretRect();
    const bw = Math.min(320, innerWidth - 16);
    box.style.width = `${bw}px`;
    box.style.left = `${Math.max(8, Math.min(r.left, innerWidth - bw - 8))}px`;
    const below = r.bottom + 4;
    const height = box.getBoundingClientRect().height;
    box.style.top = `${below + height > innerHeight - 8 ? Math.max(8, r.top - height - 4) : below}px`;
    paint();
  }

  async function update() {
    const c = context();
    if (!c) { close(); return; }
    ctx = c;
    const mine = ++req;
    const list = await triggers[c.trigger](c.query);
    if (mine !== req || !ctx || ctx.trigger !== c.trigger) return;
    show(list.slice(0, 8));
  }

  function replaceWith(content) {
    const c = ctx;
    if (!c) return;
    if (mode() === 'plain') {
      const ta = field();
      ta.focus();
      ta.setRangeText(content, c.start, c.end, 'end');
      ta.dispatchEvent(new Event('input', { bubbles: true }));
      return;
    }
    const r = document.createRange();
    r.setStart(c.node, c.start);
    r.setEnd(c.node, Math.min(c.end, c.node.nodeValue.length));
    const sel = document.getSelection();
    sel.removeAllRanges();
    sel.addRange(r);
    if (typeof content === 'string') document.execCommand('insertText', false, content);
    else {
      const box2 = document.createElement('div');
      box2.appendChild(content);
      document.execCommand('insertHTML', false, box2.innerHTML);
    }
  }

  function pick() {
    const it = items[active];
    const c = ctx;
    if (!it || !c) { close(); return; }
    onPick(c.trigger, it, replaceWith);
    close();
  }

  const onInput = () => { update(); };
  const listen = (el) => el.addEventListener('input', onInput);
  listen(document);

  return {
    onKey(e) {
      if (box.hidden) return false;
      if (e.key === 'ArrowDown') { active = (active + 1) % items.length; paint(); } else if (e.key === 'ArrowUp') { active = (active - 1 + items.length) % items.length; paint(); } else if (e.key === 'Enter' || e.key === 'Tab') pick();
      else if (e.key === 'Escape') close();
      else return false;
      e.preventDefault();
      e.stopPropagation();
      return true;
    },
    close,
    get open() { return !box.hidden; },
    destroy() {
      document.removeEventListener('input', onInput);
      box.remove();
    },
  };
}
