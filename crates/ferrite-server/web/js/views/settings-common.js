// Small building blocks shared by the settings tabs.

import { h, icon, copyText } from '../ui.js';

let n = 0;
export const uid = (p = 'set') => `${p}-${++n}`;

/** A settings card: title, optional description and an optional action in the head. */
export function card(title, desc, body, action) {
  return h('section.card.set-card',
    h('div.set-card-head', h('div.grow', h('h2', title), desc ? h('p.muted.small', desc) : null), action || null),
    h('div.set-card-body', body));
}

/** Labelled input: field('Name', input, 'help text'). Gives the input an id when missing. */
export function field(label, input, help) {
  if (!input.id) input.id = uid('f');
  return h('div.field', h('label', { for: input.id }, label), input, help ? h('div.small.muted.set-help', help) : null);
}

/** Code block with a copy button. */
export function codeBlock(text, what = 'Copied') {
  return h('div.set-code', h('pre', h('code', text)),
    h('button.btn.sm.set-code-copy', { type: 'button', onclick: () => copyText(text, what), 'aria-label': `Copy ${what.toLowerCase()}` }, icon('copy', 'sm'), 'Copy'));
}

/** A small on/off switch built on a checkbox (keyboard and screen reader friendly). */
export function toggle(checked, label, onchange) {
  const input = h('input', { type: 'checkbox', role: 'switch', checked, onchange: () => onchange(input.checked, input) });
  return { input, el: h('label.set-switch', input, h('span.set-switch-track', h('span.set-switch-thumb')), h('span.set-switch-label', label)) };
}

export function pill(text, kind = '') {
  return h(`span.pill${kind ? `.${kind}` : ''}`, text);
}
