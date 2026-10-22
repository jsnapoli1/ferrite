// JSON API client. Every request carries the CSRF header the server requires for
// cookie-authenticated writes.

export class ApiError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function request(method, path, body, extraHeaders = {}) {
  const headers = { 'X-Ferrite-Request': '1', ...extraHeaders };
  let payload;
  if (body !== undefined && !(body instanceof Blob) && !(body instanceof FormData)) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  } else {
    payload = body;
  }
  let resp;
  try {
    resp = await fetch(path, { method, headers, body: payload, credentials: 'same-origin' });
  } catch (e) {
    throw new ApiError(0, 'Network error — is the server reachable?');
  }
  const type = resp.headers.get('content-type') || '';
  const data = type.includes('application/json') ? await resp.json().catch(() => null) : await resp.text();
  if (!resp.ok) {
    const msg = (data && data.error) || (typeof data === 'string' && data) || resp.statusText;
    if (resp.status === 401 && api.onUnauthorized) api.onUnauthorized();
    throw new ApiError(resp.status, msg);
  }
  return data;
}

export const api = {
  onUnauthorized: null,
  get: (p, h) => request('GET', p, undefined, h),
  post: (p, b = {}, h) => request('POST', p, b, h),
  put: (p, b = {}, h) => request('PUT', p, b, h),
  patch: (p, b = {}, h) => request('PATCH', p, b, h),
  del: (p, h) => request('DELETE', p, undefined, h),
  text: async (p) => {
    const r = await fetch(p, { credentials: 'same-origin' });
    if (!r.ok) throw new ApiError(r.status, await r.text());
    return r.text();
  },
  /** Streams a file to a folder; reports progress 0..1. Resolves to the created node. */
  upload(parentId, file, onProgress, name = file.name) {
    return new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      xhr.open('PUT', `/api/files/${encodeURIComponent(parentId)}/upload?name=${encodeURIComponent(name)}`);
      xhr.setRequestHeader('X-Ferrite-Request', '1');
      xhr.upload.onprogress = (e) => e.lengthComputable && onProgress && onProgress(e.loaded / e.total);
      xhr.onload = () => {
        let data = null;
        try { data = JSON.parse(xhr.responseText); } catch (_) { /* not JSON */ }
        if (xhr.status >= 200 && xhr.status < 300) resolve(data.node);
        else reject(new ApiError(xhr.status, (data && data.error) || xhr.statusText));
      };
      xhr.onerror = () => reject(new ApiError(0, 'Network error during upload'));
      xhr.send(file);
    });
  },
};

export const q = (params) => {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== null && v !== '') s.set(k, v);
  const str = s.toString();
  return str ? `?${str}` : '';
};
