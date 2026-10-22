// A small, safe Markdown renderer that builds DOM nodes directly (no HTML strings), used
// for chat messages, card descriptions, documents and assistant answers.
//
// Supports: headings, paragraphs, line breaks, **bold**, *italic*, ~~strike~~, `code`,
// fenced code, [links](url), autolinks, lists (incl. task lists), > quotes, tables, ---,
// and @mentions.

import { h } from './ui.js';

function safeUrl(url) {
  const u = url.trim();
  if (/^(https?:|mailto:)/i.test(u)) return u;
  if (u.startsWith('/') && !u.startsWith('//')) return u;
  return null;
}

const INLINE = /(`+)([\s\S]*?[^`])\1(?!`)|\*\*([\s\S]+?)\*\*|__([\s\S]+?)__|~~([\s\S]+?)~~|\*([^*\s][\s\S]*?)\*|(?<![\w])_([^_\s][\s\S]*?)_(?![\w])|\[([^\]]+)\]\(([^)\s]+)\)|(https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"])|(?<![\w@])@([A-Za-z0-9._-]*[A-Za-z0-9])/g;

export function inline(text, opts = {}) {
  const out = [];
  let last = 0;
  INLINE.lastIndex = 0;
  let m;
  while ((m = INLINE.exec(text))) {
    if (m.index > last) out.push(document.createTextNode(text.slice(last, m.index)));
    if (m[2] !== undefined) out.push(h('code', m[2]));
    else if (m[3] !== undefined || m[4] !== undefined) out.push(h('strong', inline(m[3] ?? m[4], opts)));
    else if (m[5] !== undefined) out.push(h('del', inline(m[5], opts)));
    else if (m[6] !== undefined || m[7] !== undefined) out.push(h('em', inline(m[6] ?? m[7], opts)));
    else if (m[8] !== undefined) {
      const href = safeUrl(m[9]);
      out.push(href ? h('a', { href, target: href.startsWith('/') ? null : '_blank', rel: 'noopener noreferrer' }, inline(m[8], opts)) : document.createTextNode(m[0]));
    } else if (m[10] !== undefined) {
      out.push(h('a', { href: m[10], target: '_blank', rel: 'noopener noreferrer' }, m[10]));
    } else if (m[11] !== undefined) {
      const name = m[11];
      const isMe = opts.me && name.toLowerCase() === opts.me.toLowerCase();
      out.push(h(`span.mention${isMe ? '.me' : ''}`, `@${name}`));
    }
    last = INLINE.lastIndex;
  }
  if (last < text.length) out.push(document.createTextNode(text.slice(last)));
  return out;
}

function inlineWithBreaks(text, opts) {
  const parts = [];
  text.split('\n').forEach((line, i) => {
    if (i) parts.push(h('br'));
    parts.push(...inline(line, opts));
  });
  return parts;
}

function splitRow(line) {
  return line.trim().replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim());
}

/** Renders Markdown into a `div.md`. opts.me highlights mentions of that username. */
export function markdown(src, opts = {}) {
  const root = h('div.md');
  const lines = String(src ?? '').replace(/\r\n?/g, '\n').split('\n');
  let i = 0;
  let para = [];
  const flush = () => {
    if (para.length) root.appendChild(h('p', inlineWithBreaks(para.join('\n'), opts)));
    para = [];
  };
  while (i < lines.length) {
    const line = lines[i];
    let m;
    if ((m = line.match(/^\s*(```|~~~)\s*([\w+-]*)\s*$/))) {
      flush();
      const fence = m[1];
      const code = [];
      i++;
      while (i < lines.length && !lines[i].trim().startsWith(fence)) code.push(lines[i++]);
      i++;
      root.appendChild(h('pre', h('code', { 'data-lang': m[2] || null }, code.join('\n'))));
      continue;
    }
    if ((m = line.match(/^(#{1,6})\s+(.*)$/))) {
      flush();
      root.appendChild(h(`h${Math.min(m[1].length, 4)}`, inline(m[2], opts)));
      i++;
      continue;
    }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      flush();
      root.appendChild(h('hr'));
      i++;
      continue;
    }
    if (/^\s*>/.test(line)) {
      flush();
      const quoted = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) quoted.push(lines[i++].replace(/^\s*>\s?/, ''));
      root.appendChild(h('blockquote', [...markdown(quoted.join('\n'), opts).childNodes]));
      continue;
    }
    if (/^\s*\|.*\|\s*$/.test(line) && i + 1 < lines.length && /^\s*\|?\s*:?-{2,}/.test(lines[i + 1])) {
      flush();
      const head = splitRow(line);
      i += 2;
      const rows = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) rows.push(splitRow(lines[i++]));
      root.appendChild(h('table', h('thead', h('tr', head.map((c) => h('th', inline(c, opts))))), h('tbody', rows.map((r) => h('tr', r.map((c) => h('td', inline(c, opts))))))));
      continue;
    }
    if ((m = line.match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/))) {
      flush();
      const ordered = /\d/.test(m[2]);
      const list = h(ordered ? 'ol' : 'ul');
      while (i < lines.length && (m = lines[i].match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/))) {
        let text = m[3];
        const li = h('li');
        const task = text.match(/^\[([ xX])\]\s+(.*)$/);
        if (task) {
          li.appendChild(h('input', { type: 'checkbox', checked: task[1] !== ' ', disabled: true }));
          text = task[2];
        }
        li.append(...inline(text, opts));
        list.appendChild(li);
        i++;
      }
      root.appendChild(list);
      continue;
    }
    if (!line.trim()) {
      flush();
      i++;
      continue;
    }
    para.push(line);
    i++;
  }
  flush();
  return root;
}
