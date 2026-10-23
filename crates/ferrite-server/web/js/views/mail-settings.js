// Mail settings: the user's layout and list preferences (stored on the server), tags, the
// mailto: handler, and, for administrators, the defaults everyone starts with.

import { api } from '../api.js';
import { h, icon, modal, toast, errorToast } from '../ui.js';
import { manageTagsDialog } from './mail-dialogs.js';

const LAYOUTS = [['vertical', 'Side by side', 'List and message next to each other'], ['horizontal', 'Stacked', 'List above the message'], ['list', 'List only', 'A message opens in place of the list']];

/** Asks the browser to open mailto: links here. */
export function registerMailto() {
  try {
    navigator.registerProtocolHandler('mailto', `${location.origin}/mail?mailto=%s`);
    toast('Your browser will ask to confirm; mailto: links then open in Ferrite.', 'ok');
  } catch (e) {
    toast(`This browser didn't allow it: ${e.message}`, 'error');
  }
}

function prefsForm(p, { admin = false } = {}) {
  const id = (k) => `ml-${admin ? 'def' : 'pref'}-${k}`;
  const layout = h('div.ml-layouts', { role: 'radiogroup', 'aria-label': 'Layout' }, LAYOUTS.map(([v, label, hint]) => h('label.ml-layout-opt',
    h('input', { type: 'radio', name: id('layout'), value: v, checked: p.layout === v }),
    h('span.ml-layout-pic', { dataset: { layout: v } }, h('i'), h('i'), h('i')),
    h('span', h('strong', label), h('span.small.muted', hint)))));
  const box = (k, label) => h('label.check', h('input', { type: 'checkbox', id: id(k), checked: !!p[k], dataset: { key: k } }), label);
  const sort = h('select', { id: id('sort') }, h('option', { value: 'newest', selected: p.sort === 'newest' }, 'Newest first'), h('option', { value: 'oldest', selected: p.sort === 'oldest' }, 'Oldest first'));
  const el = h('div.ml-prefs',
    h('h3', 'Layout'), layout,
    box('compact', 'Compact list (no avatars or previews, checkboxes always shown)'),
    h('h3', 'Messages'),
    box('threaded', 'One row per conversation (else one per message)'),
    h('div.field', h('label', { for: sort.id }, 'Sort'), sort),
    box('favorites_top', 'Show favourites in their own section at the top'),
    h('h3', 'Folders'),
    box('all_inboxes', 'Show “All inboxes” when there are several accounts'),
    box('priority_inbox', 'Show the priority inbox (Important and Others)'),
    box('show_unsubscribed', 'Show folders I am not subscribed to'),
    h('h3', 'Search'),
    box('body_search', 'Search message bodies (also in All inboxes)'));
  const value = () => {
    const out = { layout: el.querySelector(`input[name="${id('layout')}"]:checked`)?.value || 'vertical', sort: sort.value };
    el.querySelectorAll('input[type=checkbox][data-key]').forEach((b) => { out[b.dataset.key] = b.checked; });
    return out;
  };
  return { el, value };
}

/** The settings dialog. Resolves with the saved preferences, or null when cancelled. */
export async function mailSettingsDialog(ctx, { prefs, tags }) {
  const own = prefsForm(prefs);
  let admin = null;
  let defaults = null;
  if (ctx.me.user.is_admin) {
    try {
      ({ defaults } = await api.get('/api/mail/admin/defaults'));
      admin = prefsForm(defaults, { admin: true });
    } catch (e) {
      errorToast(e);
    }
  }
  const classify = defaults && h('input', { type: 'checkbox', id: 'ml-def-classify', checked: defaults.classify });
  const body = h('div.ml-settings',
    own.el,
    h('h3', 'Tags'),
    h('p.small.muted', 'Tags are stored on the mail server, so other mail apps see them too.'),
    h('button.btn.sm', { type: 'button', onclick: () => manageTagsDialog(tags) }, icon('tag', 'sm'), 'Manage tags…'),
    h('h3', 'Mail links'),
    h('p.small.muted', 'Open mailto: links from other sites and apps in Ferrite’s composer.'),
    h('button.btn.sm', { type: 'button', onclick: registerMailto }, icon('mail', 'sm'), 'Open mailto: links in Ferrite'),
    admin && h('details.ml-admin-defaults',
      h('summary', 'Defaults for everyone (administrator)'),
      h('p.small.muted', 'Users who haven’t changed a setting get these.'),
      admin.el,
      h('h3', 'Accounts'),
      h('label.check', classify, 'Importance classification starts on for accounts')));
  return modal({
    title: 'Mail settings',
    wide: true,
    body,
    actions: [
      { label: 'Cancel', value: null },
      {
        label: 'Save', kind: 'primary',
        run: async () => {
          const changed = {};
          const v = own.value();
          for (const [k, x] of Object.entries(v)) if (prefs[k] !== x) changed[k] = x;
          let saved = prefs;
          if (Object.keys(changed).length) ({ prefs: saved } = await api.put('/api/mail/preferences', changed));
          if (admin) {
            const dv = { ...admin.value(), classify: classify.checked };
            const dchanged = {};
            for (const [k, x] of Object.entries(dv)) if (defaults[k] !== x) dchanged[k] = x;
            if (Object.keys(dchanged).length) {
              await api.put('/api/mail/admin/defaults', dchanged);
              // The user's own unchanged settings follow the new defaults.
              ({ prefs: saved } = await api.get('/api/mail/preferences'));
            }
          }
          toast('Mail settings saved', 'ok', { timeout: 2000 });
          return saved;
        },
      },
    ],
  });
}
