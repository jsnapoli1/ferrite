// The whitelist for HTML entering the rich editor: pasted and dropped content, text blocks,
// signatures and the server's reply templates. It mirrors the server's composer sanitizer
// (crates/ferrite-core/src/mail/richtext.rs), which checks everything again before it is
// stored or sent, so this is about a clean editor, not the last line of defence.
//
// Markup is parsed in an inert document (DOMParser: nothing runs, nothing loads) and rebuilt
// node by node with createElement and createTextNode, never by assigning innerHTML. Unknown
// elements are unwrapped (their text stays); scripts, styles, forms and frames go with their
// content; images keep only sources the composer can send (its uploads, outbox copies,
// inline parts of mail, embedded data); links only http(s), mailto and tel.

const TAGS = new Set(['a', 'b', 'big', 'blockquote', 'br', 'caption', 'center', 'code', 'del', 'div', 'em', 'font', 'h1', 'h2', 'h3', 'h4',
  'h5', 'h6', 'hr', 'i', 'img', 'ins', 'li', 'mark', 'ol', 'p', 'pre', 's', 'small', 'span', 'strike', 'strong', 'sub', 'sup', 'table',
  'tbody', 'td', 'tfoot', 'th', 'thead', 'tr', 'u', 'ul']);
const DROP = new Set(['script', 'style', 'head', 'title', 'template', 'textarea', 'select', 'noscript', 'iframe', 'object', 'embed',
  'frame', 'frameset', 'form', 'input', 'button', 'svg', 'math', 'video', 'audio', 'canvas', 'link', 'meta', 'base']);
const ATTRS = {
  a: ['href'],
  img: ['src', 'alt', 'width', 'height'],
  font: ['color', 'size', 'face'],
  blockquote: ['type'],
  ol: ['start', 'type'],
  table: ['cellpadding', 'cellspacing', 'border', 'width'],
  td: ['colspan', 'rowspan', 'width', 'valign'],
  th: ['colspan', 'rowspan', 'width', 'valign'],
};
const GENERIC = ['align', 'dir', 'title'];
const CLASSES = new Set(['ferrite-signature', 'ferrite-quote']);
const STYLES = new Set(['background-color', 'border', 'border-collapse', 'border-left', 'border-left-color', 'border-left-style',
  'border-left-width', 'color', 'font-family', 'font-size', 'font-style', 'font-weight', 'height', 'line-height', 'list-style-type',
  'margin', 'margin-bottom', 'margin-left', 'margin-right', 'margin-top', 'max-width', 'padding', 'padding-left', 'text-align',
  'text-decoration', 'text-decoration-line', 'vertical-align', 'width']);

/** Image sources the composer can send. */
export function localImage(src) {
  const s = (src || '').trim();
  const l = s.toLowerCase();
  return (s.startsWith('/api/mail/') && !s.includes('..') && !s.includes('//'))
    || l.startsWith('cid:')
    || /^data:image\/(png|jpeg|gif|webp);/.test(l);
}

const linkOk = (href) => /^(https?:|mailto:|tel:)/i.test((href || '').trim());

function cleanStyle(from, to) {
  const st = from.style;
  for (let i = 0; i < st.length; i++) {
    const name = st[i];
    const value = st.getPropertyValue(name);
    if (!STYLES.has(name) || /url\(|expression\(|javascript:/i.test(value)) continue;
    to.style.setProperty(name, value);
  }
  if (!to.getAttribute('style')) to.removeAttribute('style');
}

function cleanNode(node, out) {
  if (node.nodeType === Node.TEXT_NODE) {
    // Zero-width spaces only ever hold the caret in the editor.
    const text = node.nodeValue.replace(/\u200b/g, '');
    if (text) out.appendChild(document.createTextNode(text));
    return;
  }
  if (node.nodeType !== Node.ELEMENT_NODE) return;
  const tag = node.localName;
  if (DROP.has(tag)) return;
  if (!TAGS.has(tag)) {
    for (const c of node.childNodes) cleanNode(c, out);
    return;
  }
  if (tag === 'img' && !localImage(node.getAttribute('src'))) return;
  const el = document.createElement(tag);
  for (const a of [...GENERIC, ...(ATTRS[tag] || [])]) {
    const v = node.getAttribute(a);
    if (v === null) continue;
    if (a === 'href' && !linkOk(v)) continue;
    el.setAttribute(a, a === 'href' || a === 'src' ? v.trim() : v);
  }
  if (tag === 'div' && node.classList) {
    const keep = [...node.classList].filter((c) => CLASSES.has(c));
    if (keep.length) el.className = keep.join(' ');
  }
  if (node.getAttribute('style')) cleanStyle(node, el);
  for (const c of node.childNodes) cleanNode(c, el);
  out.appendChild(el);
}

/** Sanitized nodes of `html`, as a fragment of this document. */
export function sanitizeFragment(html) {
  const doc = new DOMParser().parseFromString(`<!doctype html><body>${html || ''}`, 'text/html');
  const frag = document.createDocumentFragment();
  for (const c of doc.body.childNodes) cleanNode(c, frag);
  return frag;
}

/** Sanitized nodes of an element of this document (the editor's content). */
export function sanitizeNodes(root) {
  const frag = document.createDocumentFragment();
  for (const c of root.childNodes) cleanNode(c, frag);
  return frag;
}

/** Serializes nodes to an HTML string (reading markup; nothing is parsed into the page). */
export function serialize(nodes) {
  const box = document.createElement('div');
  box.appendChild(nodes);
  return box.innerHTML;
}

/** The sanitized HTML string of `html`. */
export function sanitizeHtml(html) {
  return serialize(sanitizeFragment(html));
}
