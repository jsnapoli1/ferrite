// Ferrite service worker: shows Web Push notifications and opens the app when one is clicked.
// It has no fetch handler on purpose: the app always loads from the network, so a new
// version of this worker can take over at once without stale pages or reload loops.

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

function windows() {
  return self.clients.matchAll({ type: 'window', includeUncontrolled: true });
}

function parse(event) {
  if (!event.data) return { title: 'Ferrite' };
  try {
    return event.data.json();
  } catch (_) {
    return { title: 'Ferrite', body: event.data.text() };
  }
}

async function show(data) {
  // A focused Ferrite window already shows the live in-app notification. Test pushes always
  // show, since that is what the person is checking.
  if (data.kind !== 'test') {
    const focused = (await windows()).some((c) => c.focused && c.visibilityState === 'visible');
    if (focused) return;
  }
  const actions = Array.isArray(data.actions) ? data.actions : [];
  const call = data.kind === 'call';
  const options = {
    body: data.body || '',
    icon: '/icons/icon-192.png',
    badge: '/icons/badge-96.png',
    timestamp: data.ts || Date.now(),
    requireInteraction: call,
    data: {
      url: typeof data.url === 'string' ? data.url : '/',
      actions: Object.fromEntries(actions.map((a) => [a.action, a.url || null])),
      notificationId: data.notification_id ?? null,
    },
  };
  if (data.tag) {
    options.tag = data.tag;
    options.renotify = true;
  }
  if (actions.length && (self.Notification?.maxActions ?? 0) > 0) {
    options.actions = actions.map((a) => ({ action: a.action, title: a.title }));
  }
  if (call) options.vibrate = [300, 150, 300, 150, 300];
  await self.registration.showNotification(data.title || 'Ferrite', options);
}

self.addEventListener('push', (event) => {
  event.waitUntil(show(parse(event)));
});

/** Same-origin app path for a notification URL, or null for anything else. */
function appPath(url) {
  try {
    const u = new URL(url, self.location.origin);
    return u.origin === self.location.origin ? u.pathname + u.search + u.hash : null;
  } catch (_) {
    return null;
  }
}

async function open(path) {
  const all = await windows();
  const win = all.find((c) => c.focused) || all.find((c) => c.visibilityState === 'visible') || all[0];
  if (win) {
    // The app routes the message itself (no reload, the live socket stays up).
    win.postMessage({ type: 'ferrite.navigate', url: path });
    try {
      await win.focus();
    } catch (_) { /* some browsers only allow focus right after the click */ }
    return;
  }
  await self.clients.openWindow(path);
}

async function markRead(id) {
  if (id == null) return;
  try {
    await fetch('/api/notifications/read', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', 'X-Ferrite-Request': '1' },
      body: JSON.stringify({ id }),
    });
  } catch (_) { /* offline: it stays unread in the app */ }
}

self.addEventListener('notificationclick', (event) => {
  const n = event.notification;
  const data = n.data || {};
  n.close();
  if (event.action === 'dismiss') return;
  const target = (event.action && data.actions && data.actions[event.action]) || data.url || '/';
  const path = appPath(target) || '/';
  event.waitUntil(Promise.all([open(path), markRead(data.notificationId)]));
});

// The browser renewed (or dropped) the subscription: register the new one in its place.
self.addEventListener('pushsubscriptionchange', (event) => {
  event.waitUntil((async () => {
    const old = event.oldSubscription;
    let sub = event.newSubscription;
    if (!sub) {
      const key = old && old.options && old.options.applicationServerKey;
      if (!key) return;
      sub = await self.registration.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
    }
    await fetch('/api/push/subscriptions', {
      method: 'POST',
      credentials: 'same-origin',
      headers: { 'Content-Type': 'application/json', 'X-Ferrite-Request': '1' },
      body: JSON.stringify({ ...sub.toJSON(), replaces: old ? old.endpoint : null }),
    });
  })().catch(() => { /* signed out or offline: the app re-registers on its next start */ }));
});
