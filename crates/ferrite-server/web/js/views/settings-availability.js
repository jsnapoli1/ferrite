// Settings → Profile → Out of office: the user's absence (whole days in their time zone, a
// short status and a message). Others see it while it runs; mail accounts whose
// autoresponder follows it reply with the message during those days.

import { api } from '../api.js';
import { h, clear, toast, spinner } from '../ui.js';
import { card, field, pill } from './settings-common.js';

const today = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
};

export async function availabilityCard() {
  const body = h('div', spinner());
  const el = card('Out of office', 'When you are away: others see it, and mail autoresponders that follow it answer for you.', body);
  let state;
  try {
    state = await api.get('/api/me/absence');
  } catch (e) {
    clear(body).appendChild(h('p.error-text', e.message));
    return el;
  }
  const a = state.absence;
  const first = h('input', { type: 'date', value: a?.first_day || today(), required: true });
  const last = h('input', { type: 'date', value: a?.last_day || today(), required: true });
  const status = h('input', { type: 'text', value: a?.status || '', maxlength: 100, placeholder: 'On vacation' });
  const message = h('textarea', { rows: 4, placeholder: 'I am out of the office until Monday. For urgent matters, contact …' }, a?.message || '');
  const replacement = h('input', { type: 'text', value: a?.replacement || '', placeholder: 'Name or address' });
  const err = h('div.error-text', { role: 'alert' });
  const badge = h('span.set-absence-state');
  const setBadge = (s) => clear(badge).appendChild(s.absence ? pill(s.active ? 'Away now' : 'Planned', s.active ? 'warn' : 'info') : pill('Not set'));
  setBadge(state);
  const report = (errors) => {
    if (errors?.length) err.textContent = `The autoresponder of ${errors.map((x) => `${x.account} (${x.error})`).join(', ')} could not be updated.`;
  };
  const save = h('button.btn.primary', { type: 'submit' }, 'Save absence');
  const clearBtn = h('button.btn', {
    type: 'button', hidden: !a,
    onclick: async () => {
      err.textContent = '';
      try {
        const r = await api.del('/api/me/absence');
        setBadge({ absence: null });
        clearBtn.hidden = true;
        report(r.autoresponder_errors);
        toast('Absence removed', 'ok', { timeout: 2000 });
      } catch (e) { err.textContent = e.message; }
    },
  }, 'Remove absence');
  const form = h('form', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      if (last.value < first.value) { err.textContent = 'The last day must not be before the first day.'; return; }
      save.disabled = true;
      try {
        const r = await api.put('/api/me/absence', {
          first_day: first.value, last_day: last.value, timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
          status: status.value.trim(), message: message.value, replacement: replacement.value.trim(),
        });
        setBadge(r);
        clearBtn.hidden = false;
        report(r.autoresponder_errors);
        toast('Absence saved', 'ok', { timeout: 2000 });
      } catch (ex) { err.textContent = ex.message; } finally { save.disabled = false; }
    },
  },
  h('div.set-grid', field('First day', first), field('Last day', last)),
  h('div.set-grid', field('Status', status, 'Shown to others, e.g. “On vacation”.'), field('Contact instead', replacement)),
  field('Message', message, 'Also the autoresponder’s reply for mail accounts set to follow your absence (Mail → account menu → Filters and autoresponder → Settings).'),
  err,
  h('div.set-actions', badge, h('div.grow'), clearBtn, save));
  clear(body).appendChild(form);
  return el;
}
