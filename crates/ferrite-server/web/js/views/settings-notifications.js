// Settings → Notifications: Web Push on this device, what to push, devices, and a test push.

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, confirmDialog, relTime, spinner } from '../ui.js';
import { card, toggle, pill } from './settings-common.js';
import { unsupportedReason, permission, currentSubscription, enablePush, disablePush, isIOS, isStandalone } from '../push.js';

const CATEGORIES = [
  ['chat', 'Chat', 'Mentions, direct messages and conversations you follow'],
  ['calls', 'Incoming calls', 'Someone starts a call in one of your conversations'],
  ['calendar', 'Calendar', 'Invitations and event updates'],
  ['files', 'Files & shares', 'Files and folders shared with you'],
  ['boards', 'Boards', 'Boards shared with you and cards assigned to you'],
];

export async function notificationsTab(host) {
  const deviceHost = h('div', spinner());
  const devicesHost = h('div', spinner());
  const prefsHost = h('div', spinner());
  const testResult = h('div.set-push-results', { 'aria-live': 'polite' });
  let state = { public_key: '', settings: null, subscriptions: [] };
  let mySub = null;
  let perm = 'default';

  async function load() {
    try {
      [state, mySub, perm] = await Promise.all([api.get('/api/push'), currentSubscription().catch(() => null), permission()]);
    } catch (e) {
      clear(deviceHost).appendChild(h('p.error-text', e.message));
      return;
    }
    renderDevice();
    renderPrefs();
    renderDevices();
  }

  const registered = () => !!mySub && state.subscriptions.some((d) => d.endpoint === mySub.endpoint);

  function note(kind, text, extra) {
    return h(`div.set-push-note.${kind}`, icon(kind === 'ok' ? 'check' : 'bell', 'sm'), h('div', h('div', text), extra || null));
  }

  function renderDevice() {
    const why = unsupportedReason();
    let body;
    if (why === 'insecure') {
      body = note('warn', 'Push notifications need a secure (https) connection to this server.');
    } else if (why === 'ios-browser' || (why && isIOS())) {
      body = note('warn', 'On iPhone and iPad, push notifications work in the installed app.',
        h('div.small.muted', 'In Safari, tap Share → Add to Home Screen, open Ferrite from the Home Screen, then come back here.'));
    } else if (why) {
      body = note('warn', 'This browser doesn’t support push notifications.');
    } else if (perm === 'denied') {
      body = note('warn', 'Notifications are blocked for this site.',
        h('div.small.muted', 'Allow notifications in your browser’s site settings (the icon next to the address), then reload this page.'));
    } else if (perm === 'granted' && registered()) {
      const off = h('button.btn', { type: 'button', onclick: () => run(off, async () => { await disablePush(state.subscriptions); toast('Push notifications turned off on this device', 'ok', { timeout: 2500 }); }) }, 'Turn off on this device');
      body = h('div.set-push-row', note('ok', 'This device receives push notifications.'), off);
    } else {
      const on = h('button.btn.primary', { type: 'button', onclick: () => run(on, async () => { await enablePush(); toast('Push notifications are on for this device', 'ok', { timeout: 2500 }); }) }, icon('bell', 'sm'), 'Turn on push notifications');
      body = h('div.set-push-row', h('p.muted.small', { style: { margin: 0 } },
        isIOS() && isStandalone() ? 'Get notified on this iPhone or iPad even when Ferrite is closed.' : 'Get notified on this device even when Ferrite isn’t open.'), on);
    }
    clear(deviceHost).appendChild(body);
  }

  async function run(btn, fn) {
    btn.disabled = true;
    try {
      await fn();
    } catch (e) {
      errorToast(e);
    } finally {
      btn.disabled = false;
      await load();
    }
  }

  function renderPrefs() {
    const s = { ...state.settings };
    const switches = {};
    const sync = () => { for (const [k] of CATEGORIES) switches[k].input.disabled = !s.enabled; };
    const save = async (input, key) => {
      s[key] = input.checked;
      sync();
      try {
        ({ settings: state.settings } = await api.put('/api/push/settings', s));
      } catch (e) {
        input.checked = !input.checked;
        s[key] = input.checked;
        sync();
        errorToast(e);
      }
    };
    const master = toggle(s.enabled, 'Send push notifications to my devices', (_, input) => save(input, 'enabled'));
    master.input.dataset.pref = 'enabled';
    const rows = CATEGORIES.map(([key, label, help]) => {
      switches[key] = toggle(s[key], label, (_, input) => save(input, key));
      switches[key].input.dataset.pref = key;
      switches[key].input.disabled = !s.enabled;
      return h('div.set-push-pref', switches[key].el, h('div.small.muted', help));
    });
    clear(prefsHost).append(h('div.set-push-master', master.el), h('div.set-push-prefs', rows));
  }

  function renderDevices() {
    const list = state.subscriptions;
    if (!list.length) {
      clear(devicesHost).appendChild(h('p.muted.small.set-empty', 'No devices receive push notifications yet.'));
      return;
    }
    clear(devicesHost).appendChild(h('div.list.panel', list.map((d) => {
      const mine = mySub && d.endpoint === mySub.endpoint;
      const remove = h('button.btn.sm', {
        type: 'button', 'aria-label': `Remove ${d.label}`,
        onclick: async () => {
          if (!(await confirmDialog('Remove this device?', `“${d.label}” stops receiving push notifications.`, { confirm: 'Remove', danger: true }))) return;
          await run(remove, async () => {
            await api.del(`/api/push/subscriptions/${encodeURIComponent(d.id)}`);
            if (mine) await mySub.unsubscribe();
          });
        },
      }, 'Remove');
      return h('div.list-item.set-push-device', icon(/Android|iOS|iPadOS/.test(d.label) ? 'phone' : 'screen', 'sm'),
        h('div.grow',
          h('div.row.wrap', { style: { gap: '6px' } }, h('strong', d.label), mine ? pill('This device', 'ok') : null),
          h('div.small.muted', `${d.service} · added ${relTime(d.created_at)}${d.last_used_at ? ` · last push ${relTime(d.last_used_at)}` : ''}`)),
        remove);
    })));
  }

  const testBtn = h('button.btn', {
    type: 'button',
    onclick: async () => {
      testBtn.disabled = true;
      clear(testResult);
      try {
        const r = await api.post('/api/push/test', {});
        const failed = r.results.filter((x) => x.status !== 'delivered');
        toast(r.delivered ? `Test notification sent to ${r.delivered} device${r.delivered === 1 ? '' : 's'}` : 'The test notification could not be delivered', r.delivered ? 'ok' : 'error', { timeout: 3000 });
        if (failed.length) testResult.append(...failed.map((x) => h('div.small.set-push-err', `${x.label}: ${x.error || x.status}`)));
      } catch (e) {
        errorToast(e);
      } finally {
        testBtn.disabled = false;
        load();
      }
    },
  }, icon('send', 'sm'), 'Send test notification');

  host.append(
    card('This device', 'Push notifications reach this browser or installed app even when Ferrite is closed.', deviceHost),
    card('What to send', 'Choose what reaches your devices. Notifications always appear in Ferrite itself.', prefsHost),
    card('Devices', 'Browsers and installed apps that receive your push notifications.', [devicesHost, testResult], testBtn));
  await load();
}
