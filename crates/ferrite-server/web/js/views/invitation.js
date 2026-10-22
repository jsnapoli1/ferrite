// Invitation response page (/invitation/:token): works signed out. People invited by email
// open it from the Accept / Maybe / Decline links; the token names them and the event. The
// link's ?response= preselects an answer, but nothing is recorded until they press a button
// (mail scanners open links on their own).

import { api } from '../api.js';
import { h, clear, icon, errorToast, spinner } from '../ui.js';

const ANSWERS = [
  ['accepted', 'ACCEPTED', 'Yes, I’ll come', 'check'],
  ['tentative', 'TENTATIVE', 'Maybe', 'clock'],
  ['declined', 'DECLINED', 'No, I can’t', 'x'],
];
const CONFIRM = {
  ACCEPTED: 'You accepted. The organizer has been told.',
  TENTATIVE: 'You answered maybe. The organizer has been told.',
  DECLINED: 'You declined. The organizer has been told.',
};

export default async function render(root, ctx) {
  const token = ctx.params.token;
  const wanted = ANSWERS.find(([r]) => r === String(ctx.query.response || '').toLowerCase());
  const main = h('main.pub-main', spinner());
  root.appendChild(h('div.pub',
    h('header.pub-bar',
      h('a.pub-brand', { href: ctx.me ? '/calendar' : `/invitation/${encodeURIComponent(token)}` }, h('span.brand-mark', 'Fe'), 'Ferrite'),
      h('div.spacer'),
      ctx.me ? h('a.btn.sm', { href: '/calendar' }, icon('calendar', 'sm'), 'Your calendar') : null),
    main,
    h('footer.pub-foot.small.muted', 'Sent with Ferrite, a self-hosted collaboration server.')));

  function notFound() {
    ctx.setTitle('Invitation not found');
    clear(main).appendChild(h('div.pub-card.card.pub-404',
      h('div.pub-404-code', '404'),
      h('h1', 'This invitation is no longer open'),
      h('p.muted', 'The event may have been cancelled, or you were removed from it. Ask the organizer if you think this is a mistake.')));
  }

  function draw(inv, justAnswered) {
    ctx.setTitle(inv.summary || 'Invitation');
    const current = inv.attendee.partstat;
    const organizer = inv.organizer.name || inv.organizer.email;
    const row = (ic, ...content) => h('div.inv-row', icon(ic, 'sm'), h('div.grow', content));
    const status = h('p.inv-status', { role: 'status' });
    if (justAnswered) status.append(icon('check', 'sm'), CONFIRM[current]);
    else if (CONFIRM[current]) status.append(`Your answer: ${ANSWERS.find(([, p]) => p === current)[2]}. You can change it.`);
    else if (wanted) status.append('Press an answer to send it.');
    const buttons = ANSWERS.map(([resp, p, label, ic]) => {
      const chosen = current === p;
      const b = h(`button.btn.inv-answer${(chosen || (!CONFIRM[current] && wanted && wanted[0] === resp)) ? '.primary' : ''}`, {
        type: 'button', dataset: { response: resp }, 'aria-pressed': String(chosen),
        onclick: async () => {
          buttons.forEach((x) => { x.disabled = true; });
          try {
            const { invitation } = await api.post(`/api/invitation/${encodeURIComponent(token)}`, { response: resp });
            draw(invitation, true);
          } catch (e) {
            if (e.status === 404) notFound();
            else errorToast(e);
            buttons.forEach((x) => { x.disabled = false; });
          }
        },
      }, icon(ic, 'sm'), label);
      return b;
    });
    clear(main).appendChild(h('div.pub-card.card.inv-card',
      h('div.inv-kicker.small', icon('calendar', 'sm'), `${organizer} invited ${inv.attendee.name || inv.attendee.email}`),
      h('h1.inv-title', inv.summary || 'Untitled event'),
      h('div.inv-details',
        row('clock', inv.when),
        inv.location && row('pin', inv.location),
        row('user', `Organizer: ${organizer}`, h('div.small.muted', inv.organizer.email))),
      inv.description && h('div.inv-desc', inv.description),
      h('h2.inv-q', inv.recurring ? 'Will you attend this series?' : 'Will you attend?'),
      h('div.inv-answers', { role: 'group', 'aria-label': 'Your answer' }, buttons),
      status));
    if (wanted && !justAnswered && !CONFIRM[current]) buttons[ANSWERS.indexOf(wanted)].focus();
  }

  try {
    const { invitation } = await api.get(`/api/invitation/${encodeURIComponent(token)}`);
    draw(invitation, false);
  } catch (e) {
    if (e.status === 404) notFound();
    else clear(main).appendChild(h('div.pub-card.card', h('h1', 'Something went wrong'), h('p.muted', e.message)));
  }
}
