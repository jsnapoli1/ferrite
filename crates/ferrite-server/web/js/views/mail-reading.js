// Reading parts that belong to composing and sending: the read-receipt question (when a
// sender asked for one, the user decides; a proper receipt goes back and the decision is
// remembered on the server), attached messages (.eml / message/rfc822) shown inline as a
// nested message with its own body and attachments, and printing a message.

import { api } from '../api.js';
import { h, clear, append, icon, toast, errorToast, bytes, dateTime, spinner } from '../ui.js';

const addrList = (list) => (list || []).map((a) => (a.name ? `${a.name} <${a.address}>` : a.address)).join(', ');

/** A banner asking whether to send the read receipt a message asks for (filled in later). */
export function mdnBanner(message) {
  const box = h('div.ml-mdn', { hidden: true, role: 'region', 'aria-label': 'Read receipt' });
  if (message.draft) return box;
  api.get(`/api/mail/messages/${encodeURIComponent(message.id)}/mdn`).then(({ mdn }) => {
    if (!mdn.requested || mdn.handled) return;
    const who = mdn.to.name || mdn.to.address;
    const answer = async (send) => {
      box.querySelectorAll('button').forEach((b) => { b.disabled = true; });
      try {
        await api.post(`/api/mail/messages/${encodeURIComponent(message.id)}/mdn`, { send });
        clear(box).append(icon(send ? 'check' : 'x', 'sm'), h('span.grow', send ? `Read receipt sent to ${who}.` : 'No read receipt was sent.'));
        setTimeout(() => box.remove(), 4000);
      } catch (e) {
        box.querySelectorAll('button').forEach((b) => { b.disabled = false; });
        errorToast(e);
      }
    };
    clear(box).append(icon('mail', 'sm'),
      h('span.grow', `${who} asked for a read receipt${mdn.to.address !== who ? ` (${mdn.to.address})` : ''}.`),
      h('button.btn.sm', { type: 'button', onclick: () => answer(true) }, 'Send receipt'),
      h('button.btn.ghost.sm', { type: 'button', onclick: () => answer(false) }, 'Don’t send'));
    box.hidden = false;
  }).catch(() => {});
  return box;
}

const isMessage = (a) => a.mime === 'message/rfc822' || /\.eml$/i.test(a.name);

/** Plain text with links, quoted lines dimmed. */
function textView(text) {
  const box = h('div.ml-text');
  const urlRe = /(https?:\/\/[^\s<>"')\]]+)/g;
  for (const line of (text || '').replace(/\r\n/g, '\n').replace(/\s+$/, '').split('\n')) {
    const el = h(line.startsWith('>') ? 'div.ml-quoted' : 'div');
    if (!line) el.appendChild(document.createElement('br'));
    line.split(urlRe).forEach((part, i) => {
      if (i % 2) el.appendChild(h('a', { href: part, target: '_blank', rel: 'noopener noreferrer' }, part));
      else if (part) el.appendChild(document.createTextNode(part));
    });
    box.appendChild(el);
  }
  return box;
}

function frameFor(src, title) {
  const frame = h('iframe.ml-frame', { title, sandbox: 'allow-same-origin allow-popups allow-popups-to-escape-sandbox', referrerpolicy: 'no-referrer', src });
  let observer = null;
  frame.addEventListener('load', () => {
    const doc = frame.contentDocument;
    if (!doc) return;
    const fit = () => { frame.style.height = `${Math.max(60, doc.documentElement.scrollHeight)}px`; };
    fit();
    observer?.disconnect();
    observer = new ResizeObserver(fit);
    observer.observe(doc.body);
    doc.querySelectorAll('img').forEach((img) => img.addEventListener('load', fit));
  });
  return frame;
}

/** The nested view of an attached message. */
function nestedView(messageId, attachment) {
  const base = `/api/mail/messages/${encodeURIComponent(messageId)}/attachments/${attachment.index}`;
  const box = h('section.ml-nested', { 'aria-label': `Attached message ${attachment.name}` }, spinner());
  api.get(`${base}/message`).then(({ message: m }) => {
    const files = m.attachments.filter((a) => !a.inline);
    append(clear(box), [
      h('header.ml-nested-head',
        icon('mail', 'sm'),
        h('div.grow',
          h('div.ml-nested-subject', m.subject || '(no subject)'),
          h('div.small.muted', `From ${m.from ? addrList([m.from]) : '(unknown sender)'}${m.date ? ` · ${dateTime(m.date)}` : ''}`),
          m.to.length > 0 && h('div.small.muted.ellipsis', `To ${addrList(m.to)}${m.cc.length ? `, cc ${addrList(m.cc)}` : ''}`))),
      m.has_html ? frameFor(`${base}/message/html`, `Attached message ${m.subject}`) : textView(m.text),
      files.length > 0 && h('div.ml-attachments', files.map((a) => h('div.ml-attachment',
        icon('paperclip', 'sm'),
        h('a.ellipsis', { href: `${base}/parts/${a.index}`, download: a.name, title: `Download ${a.name}` }, a.name),
        h('span.small.muted', bytes(a.size))))),
    ]);
  }).catch((e) => { clear(box).appendChild(h('p.error-text', e.message)); });
  return box;
}

/** A "View" button for an attached message (null for other attachments). */
export function attachedMessageButton(body, message, attachment) {
  if (!isMessage(attachment)) return null;
  let view = null;
  const btn = h('button.btn.ghost.sm', {
    type: 'button', 'aria-expanded': 'false', title: 'Show the attached message here',
    onclick: () => {
      if (view) { view.remove(); view = null; btn.setAttribute('aria-expanded', 'false'); btn.lastChild.textContent = 'View'; return; }
      view = nestedView(message.id, attachment);
      body.appendChild(view);
      btn.setAttribute('aria-expanded', 'true');
      btn.lastChild.textContent = 'Hide';
      view.scrollIntoView({ block: 'nearest' });
    },
  }, icon('eye', 'sm'), h('span', 'View'));
  return btn;
}

/** Prints a message: its print layout (headers, body, attachments) in a hidden frame. */
export function printMessage(id) {
  document.querySelectorAll('iframe.ml-print-frame').forEach((f) => f.remove());
  const frame = h('iframe.ml-print-frame', {
    title: 'Print', 'aria-hidden': 'true', tabindex: '-1', sandbox: 'allow-same-origin allow-modals',
    src: `/api/mail/messages/${encodeURIComponent(id)}/print`,
  });
  frame.addEventListener('load', () => {
    const win = frame.contentWindow;
    if (!win) return;
    const done = () => setTimeout(() => frame.remove(), 1000);
    win.addEventListener('afterprint', done);
    try {
      win.focus();
      win.print();
    } catch (e) {
      toast('Printing is not available here', 'error');
      done();
    }
    // Some browsers print without an afterprint event; never keep the frame for long.
    setTimeout(() => frame.remove(), 120000);
  }, { once: true });
  document.body.appendChild(frame);
  return frame;
}
