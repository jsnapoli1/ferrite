// Web Push on this device: the service worker, subscribing and unsubscribing, and routing
// notification clicks into the app.

import { api } from './api.js';

let registration = null;

/** Why push can't work here, or null when it can. */
export function unsupportedReason() {
  if (!window.isSecureContext) return 'insecure';
  if (!('serviceWorker' in navigator) || !('PushManager' in window) || !('Notification' in window)) {
    return isIOS() && !isStandalone() ? 'ios-browser' : 'unsupported';
  }
  return null;
}

export function isIOS() {
  return /iPhone|iPad|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
}

export function isStandalone() {
  return matchMedia('(display-mode: standalone)').matches || navigator.standalone === true;
}

/**
 * 'granted', 'denied' or 'default' ('unsupported' without the Notification API). The
 * Permissions API is asked first: it reflects changes made while the page is open.
 */
export async function permission() {
  if (!('Notification' in window)) return 'unsupported';
  try {
    const { state } = await navigator.permissions.query({ name: 'notifications' });
    return state === 'prompt' ? 'default' : state;
  } catch (_) {
    return Notification.permission;
  }
}

/** Registers /sw.js and lets notification clicks navigate this window. Called once at boot. */
export function registerServiceWorker(navigate) {
  if (!window.isSecureContext || !('serviceWorker' in navigator)) return null;
  navigator.serviceWorker.addEventListener('message', (e) => {
    const m = e.data;
    if (m && m.type === 'ferrite.navigate' && typeof m.url === 'string' && m.url.startsWith('/') && !m.url.startsWith('//')) navigate(m.url);
  });
  registration = navigator.serviceWorker.register('/sw.js', { scope: '/', updateViaCache: 'none' })
    .catch((e) => { console.warn('Service worker registration failed:', e.message); return null; });
  return registration;
}

async function activeRegistration() {
  const reg = await (registration || navigator.serviceWorker.getRegistration('/'));
  if (!reg) throw new Error('The service worker is not available in this browser window.');
  if (reg.active) return reg;
  // Installing: wait until it activates.
  return Promise.race([
    navigator.serviceWorker.ready,
    new Promise((_, reject) => setTimeout(() => reject(new Error('The service worker did not start.')), 10000)),
  ]);
}

function keyBytes(b64) {
  const s = atob(b64.replace(/-/g, '+').replace(/_/g, '/') + '==='.slice((b64.length + 3) % 4));
  return Uint8Array.from(s, (c) => c.charCodeAt(0));
}

function sameKey(buffer, bytes) {
  if (!buffer) return false;
  const a = new Uint8Array(buffer);
  return a.length === bytes.length && a.every((v, i) => v === bytes[i]);
}

/** This browser's current push subscription, if any. */
export async function currentSubscription() {
  if (unsupportedReason()) return null;
  const reg = await (registration || navigator.serviceWorker.getRegistration('/'));
  return reg ? reg.pushManager.getSubscription() : null;
}

async function subscribe(publicKey) {
  const reg = await activeRegistration();
  const key = keyBytes(publicKey);
  let sub = await reg.pushManager.getSubscription();
  // Subscribed for another server key (e.g. a restored backup): start over.
  if (sub && !sameKey(sub.options.applicationServerKey, key)) {
    await sub.unsubscribe();
    sub = null;
  }
  if (!sub) {
    try {
      sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
    } catch (e) {
      if (await permission() === 'denied') throw new Error('Notifications are blocked for this site.');
      throw new Error(`This browser could not reach its push service (${e.message || e.name}).`);
    }
  }
  const { subscription } = await api.post('/api/push/subscriptions', sub.toJSON());
  return subscription;
}

/** Asks for permission (a user gesture must lead here) and registers this device. */
export async function enablePush() {
  const why = unsupportedReason();
  if (why) throw new Error('Push notifications are not available in this browser.');
  let result = await permission();
  if (result !== 'granted') result = await Notification.requestPermission();
  if (result !== 'granted') {
    throw new Error(result === 'denied' ? 'Notifications are blocked for this site.' : 'Notification permission was not granted.');
  }
  const { public_key } = await api.get('/api/push');
  return subscribe(public_key);
}

/** Stops push on this device (server side and in the browser). */
export async function disablePush(devices) {
  const sub = await currentSubscription();
  if (!sub) return;
  const mine = devices.find((d) => d.endpoint === sub.endpoint);
  if (mine) await api.del(`/api/push/subscriptions/${encodeURIComponent(mine.id)}`);
  await sub.unsubscribe();
}

/**
 * At sign-in: if this browser holds a subscription the signed-in user doesn't own (removed
 * from another device, or left by someone else on a shared browser) drop it; if the server's
 * key changed, renew it. Never prompts.
 */
export async function resyncPush() {
  try {
    if (unsupportedReason()) return;
    const sub = await currentSubscription();
    if (!sub) return;
    const { public_key, subscriptions } = await api.get('/api/push');
    if (!subscriptions.some((d) => d.endpoint === sub.endpoint)) {
      await sub.unsubscribe();
    } else if (!sameKey(sub.options.applicationServerKey, keyBytes(public_key)) && await permission() === 'granted') {
      await subscribe(public_key);
    }
  } catch (_) { /* best effort; Settings → Notifications shows the details */ }
}

/** At sign-out: this browser stops receiving the account's notifications. */
export async function forgetDevice() {
  try {
    const sub = await currentSubscription();
    if (!sub) return;
    const { subscriptions } = await api.get('/api/push');
    await disablePush(subscriptions);
  } catch (_) { /* signing out proceeds regardless */ }
}
