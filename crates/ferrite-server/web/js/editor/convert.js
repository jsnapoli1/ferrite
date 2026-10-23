// Converting between the rich editor and plain text, with the same rules the server uses for
// the text/plain alternative of HTML mail (crates/ferrite-core/src/mail/richtext.rs), so
// switching modes shows what a plain-text reader gets: paragraphs and line breaks, "> "
// quoting for blockquotes, "-"/"1." lists, *bold*, /italic/, _underline_, links as
// "text <url>", image alt text, and a "-- " line before the signature.

import { h } from '../ui.js';

const SIGNATURE = 'ferrite-signature';

class TextWriter {
  constructor() {
    this.lines = []; // [depth, text]
    this.cur = '';
    this.curDepth = 0;
    this.depth = 0;
    this.space = false;
    this.pre = 0;
    this.lists = [];
  }

  startLine() { if (!this.cur) this.curDepth = this.depth; }

  word(s) {
    if (!s) return;
    this.startLine();
    if (this.space && this.cur) this.cur += ' ';
    this.space = false;
    this.cur += s;
  }

  text(t) {
    if (this.pre) {
      t.split('\n').forEach((part, i) => {
        if (i) this.forceLine();
        this.startLine();
        this.cur += part;
      });
      return;
    }
    let word = '';
    for (const c of t.replace(/\u200b/g, '')) {
      if (/\s/.test(c) && c !== ' ') {
        if (word) { this.word(word); word = ''; }
        this.space = true;
      } else word += c === ' ' ? ' ' : c;
    }
    if (word) this.word(word);
  }

  forceLine() {
    this.startLine();
    const line = this.cur === '-- ' ? this.cur : this.cur.replace(/\s+$/, '');
    this.lines.push([this.curDepth, line]);
    this.cur = '';
    this.space = false;
  }

  newline() {
    if (this.cur) this.forceLine();
    this.space = false;
  }

  blank() {
    this.newline();
    const last = this.lines[this.lines.length - 1];
    if (last && last[1] !== '') this.lines.push([this.depth, '']);
  }

  finish() {
    this.newline();
    let out = this.lines.map(([d, t]) => (d === 0 ? t : (t ? `${'>'.repeat(d)} ${t}` : '>'.repeat(d))));
    while (out.length && out[out.length - 1] === '') out.pop();
    while (out.length && out[0] === '') out.shift();
    const lines = [];
    let prevBlank = false;
    for (const l of out) {
      const blank = l.replace(/^>+/, '') === '';
      if (blank && prevBlank && l === '') continue;
      prevBlank = blank;
      lines.push(l);
    }
    return lines.join('\n');
  }
}

const hasText = (el) => [...el.childNodes].some((n) => (n.nodeType === Node.TEXT_NODE ? n.nodeValue.trim() !== '' : (n.nodeType === Node.ELEMENT_NODE && (n.localName === 'img' || hasText(n)))));

function renderAll(el, w) {
  for (const n of el.childNodes) {
    if (n.nodeType === Node.TEXT_NODE) w.text(n.nodeValue);
    else if (n.nodeType === Node.ELEMENT_NODE) render(n, w);
  }
}

function render(e, w) {
  const tag = e.localName;
  const kids = () => renderAll(e, w);
  switch (tag) {
    case 'script': case 'style': case 'head': case 'title': case 'template': return;
    case 'br': w.forceLine(); return;
    case 'p': case 'h1': case 'h2': case 'h3': case 'h4': case 'h5': case 'h6': case 'table':
      w.blank(); kids(); w.blank(); return;
    case 'hr': w.blank(); w.word('----------'); w.blank(); return;
    case 'pre': w.blank(); w.pre++; kids(); w.pre--; w.blank(); return;
    case 'blockquote': {
      w.blank();
      w.depth++;
      kids();
      w.newline();
      const last = w.lines[w.lines.length - 1];
      if (last && last[0] === w.depth && last[1] === '') w.lines.pop();
      w.depth--;
      w.blank();
      return;
    }
    case 'ul': case 'ol': {
      if (w.lists.length) w.newline(); else w.blank();
      w.lists.push(tag === 'ol' ? (Number(e.getAttribute('start')) || 1) : null);
      kids();
      w.lists.pop();
      if (w.lists.length) w.newline(); else w.blank();
      return;
    }
    case 'li': {
      w.newline();
      const indent = '  '.repeat(Math.max(0, w.lists.length - 1));
      const top = w.lists.length - 1;
      let marker = '-';
      if (top >= 0 && w.lists[top] !== null) { marker = `${w.lists[top]}.`; w.lists[top]++; }
      w.word(`${indent}${marker}`);
      w.space = true;
      kids();
      w.newline();
      return;
    }
    case 'div':
      if (e.classList.contains(SIGNATURE)) {
        w.newline();
        if (hasText(e)) { w.word('-- '); w.forceLine(); kids(); }
        w.newline();
        return;
      }
      w.newline(); kids(); w.newline(); return;
    case 'tr': case 'center': case 'caption': case 'thead': case 'tbody': case 'tfoot':
      w.newline(); kids(); w.newline(); return;
    case 'td': case 'th': kids(); w.space = true; return;
    case 'b': case 'strong': case 'u': case 'i': case 'em': {
      if (!hasText(e)) { kids(); return; }
      const mark = { b: '*', strong: '*', u: '_' }[tag] || '/';
      w.word(mark);
      w.space = false;
      kids();
      const sp = w.space;
      w.startLine();
      w.cur += mark;
      w.space = sp;
      return;
    }
    case 'a': {
      kids();
      const href = e.getAttribute('href');
      if (href) {
        const label = e.textContent.split(/\s+/).filter(Boolean).join(' ');
        const bare = href.replace(/^mailto:/, '');
        if (label && label !== href && label !== bare) { w.space = true; w.word(`<${href}>`); } else if (!label) w.word(href);
      }
      return;
    }
    case 'img': {
      const alt = (e.getAttribute('alt') || '').trim();
      if (alt) w.word(`[${alt}]`);
      return;
    }
    default: kids();
  }
}

/** Plain text of an element's content (the editor), as a plain-text reader gets it. */
export function htmlToText(root) {
  const w = new TextWriter();
  renderAll(root, w);
  return w.finish();
}

function quoteDepth(line) {
  let depth = 0;
  let rest = line;
  while (rest.startsWith('>')) {
    depth++;
    rest = rest.slice(1);
    if (rest.startsWith(' ')) rest = rest.slice(1);
    if (!rest.startsWith('>')) break;
  }
  return [depth, rest];
}

function linkified(line) {
  const out = [];
  const re = /https?:\/\/[^\s<>"')\]]+/g;
  let last = 0;
  for (const m of line.matchAll(re)) {
    const url = m[0].replace(/[.,;:!?]+$/, '');
    out.push(line.slice(last, m.index));
    out.push(h('a', { href: url }, url));
    last = m.index + url.length;
  }
  out.push(line.slice(last));
  return out.filter((x) => x !== '');
}

/** Plain text as editor content: a div per line, ">" quotes as blockquotes, links. */
export function textToFragment(text) {
  const frag = document.createDocumentFragment();
  const stack = [frag];
  for (const line of (text || '').replace(/\r\n/g, '\n').split('\n')) {
    const [d, content] = quoteDepth(line);
    while (stack.length - 1 < d) {
      const q = h('blockquote', { type: 'cite' });
      stack[stack.length - 1].appendChild(q);
      stack.push(q);
    }
    while (stack.length - 1 > d) stack.pop();
    stack[stack.length - 1].appendChild(content ? h('div', linkified(content)) : h('div', h('br')));
  }
  return frag;
}
