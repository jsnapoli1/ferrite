// Assistant: a chat with the AI that acts through Ferrite's MCP tools with the user's own
// permissions. The conversation lives in sessionStorage (per tab). While a request runs,
// progress arrives over the live socket as `ai.event` with our client-generated run_id.

import { api } from '../api.js';
import { live } from '../ws.js';
import { h, clear, icon, avatar, errorToast, spinner } from '../ui.js';
import { markdown } from '../markdown.js';
import { refreshMe } from '../app.js';

const STORE = 'ferrite-assistant';

const SUGGESTIONS = [
  ['talk', 'What did I miss in my conversations?'],
  ['calendar', 'What’s due this week on my boards?'],
  ['mindmap', 'Turn /Notes.md into a mind map'],
  ['history', 'Summarize the files I changed recently'],
];

function loadStore() {
  try {
    const s = JSON.parse(sessionStorage.getItem(STORE));
    if (s && Array.isArray(s.messages)) return { messages: s.messages, allowWrites: !!s.allowWrites };
  } catch (_) { /* fresh */ }
  return { messages: [], allowWrites: false };
}

function saveStore(s) {
  try { sessionStorage.setItem(STORE, JSON.stringify(s)); } catch (_) { /* storage full or blocked */ }
}

function newRunId() {
  if (crypto.randomUUID) return crypto.randomUUID();
  return Array.from(crypto.getRandomValues(new Uint8Array(16)), (b) => b.toString(16).padStart(2, '0')).join('');
}

/** Short, single-line rendering of tool arguments. */
function argSummary(args) {
  if (!args || typeof args !== 'object') return '';
  const parts = Object.entries(args).slice(0, 3).map(([k, v]) => `${k}: ${typeof v === 'string' ? v : JSON.stringify(v)}`);
  const s = parts.join(', ');
  return s.length > 90 ? `${s.slice(0, 90)}…` : s;
}

/**
 * Renders agent trace events as compact steps: each tool call is an expandable <details>
 * with its arguments and (once it arrives) the result preview. Returns {el, add(event)}.
 */
function stepList() {
  const el = h('div.ai-steps');
  let open = null; // the step waiting for its result
  function add(ev) {
    if (ev.type === 'commentary' && ev.text?.trim()) {
      el.appendChild(h('div.ai-commentary', ev.text.trim()));
    } else if (ev.type === 'tool_call') {
      const status = h('span.ai-step-status', h('span.spinner'));
      const body = h('div.ai-step-body', h('div.small.muted', 'Arguments'), h('pre', JSON.stringify(ev.arguments ?? {}, null, 2)));
      const d = h('details.ai-step',
        h('summary', icon('cpu', 'sm'), h('code', ev.name), h('span.ai-step-args.ellipsis', argSummary(ev.arguments)), status),
        body);
      el.appendChild(d);
      open = { d, status, body, name: ev.name };
    } else if (ev.type === 'tool_result') {
      const step = open && open.name === ev.name ? open : null;
      const target = step || { d: el.appendChild(h('details.ai-step', h('summary', icon('cpu', 'sm'), h('code', ev.name)))), status: null, body: null };
      if (target.status) clear(target.status).appendChild(ev.is_error ? h('span.pill.danger', 'error') : icon('check', 'sm'));
      if (ev.is_error) target.d.classList.add('error');
      const body = target.body || target.d.appendChild(h('div.ai-step-body'));
      body.append(h('div.small.muted', ev.is_error ? 'Error' : 'Result'), h('pre', ev.preview || '(empty)'));
      open = null;
    }
  }
  return { el, add };
}

export default async function render(root, ctx) {
  ctx.setTitle('Assistant');
  const store = loadStore();
  const isAdmin = ctx.me.user.is_admin;
  const cleanups = [];
  let busy = false;
  let status = await api.get('/api/ai/status');

  const page = h('div.page-full.ai-page');
  root.appendChild(page);

  const goSettings = (e) => { e.preventDefault(); ctx.navigate('/settings/ai'); };

  // ---- blocking states ----
  function notConfigured() {
    return h('div.ai-center', h('div.card.pad.ai-gate',
      h('div.ai-gate-icon', icon('assistant', 'icon-lg')),
      h('h2', 'The assistant isn’t set up yet'),
      isAdmin
        ? h('p.muted', 'Connect a model to turn it on: a local server (Ollama, llama.cpp, LM Studio, vLLM — any OpenAI-compatible endpoint) or Claude. Then assign it to the chat role.')
        : h('p.muted', 'An administrator needs to connect an AI model to this server first. Everything else in Ferrite works without it.'),
      isAdmin ? h('a.btn.primary', { href: '/settings/ai', onclick: goSettings }, icon('settings', 'sm'), 'Open Settings → AI')
        : h('a.btn', { href: '/settings/ai', onclick: goSettings }, 'Learn more')));
  }

  function noConsent() {
    const btn = h('button.btn.primary', {
      type: 'button',
      onclick: async () => {
        btn.disabled = true;
        try {
          await api.patch('/api/me', { ai_consent: true });
          await refreshMe().then((m) => { ctx.me.user = m.user; }).catch(() => {});
          status = await api.get('/api/ai/status');
          draw();
        } catch (e) { errorToast(e); btn.disabled = false; }
      },
    }, icon('check', 'sm'), 'Turn on the assistant');
    return h('div.ai-center', h('div.card.pad.ai-gate',
      h('div.ai-gate-icon', icon('assistant', 'icon-lg')),
      h('h2', 'Turn on the assistant'),
      h('p.muted', 'The assistant reads your files, conversations and boards only when you ask it something, using your own permissions. What it reads is sent to the model your administrator connected to answer you.'),
      h('ul.ai-gate-points.small',
        h('li', icon('eye', 'sm'), 'Read-only by default — it can only change things when you allow it for a request.'),
        h('li', icon('lock', 'sm'), 'It never sees anything you can’t see yourself.'),
        h('li', icon('settings', 'sm'), 'You can turn it off any time in Settings → AI.')),
      btn));
  }

  // ---- chat ----
  const log = h('div.ai-log', { role: 'log', 'aria-label': 'Conversation', 'aria-live': 'polite' });
  const scroller = h('div.ai-scroll', log);
  const input = h('textarea.ai-input', {
    rows: 1, placeholder: 'Ask about your files, conversations and boards…', 'aria-label': 'Message the assistant',
    oninput: () => autosize(),
    onkeydown: (e) => {
      if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); send(); }
    },
  });
  const sendBtn = h('button.btn.primary.icon.ai-send', { type: 'submit', 'aria-label': 'Send' }, icon('send'));
  const writes = h('input', {
    type: 'checkbox', checked: store.allowWrites, id: 'ai-writes',
    onchange: () => { store.allowWrites = writes.checked; saveStore(store); modeHint(); },
  });
  const hint = h('div.ai-mode.small');
  function modeHint() {
    hint.className = `ai-mode small${writes.checked ? ' writes' : ''}`;
    clear(hint).append(icon(writes.checked ? 'edit' : 'eye', 'sm'), writes.checked
      ? 'Changes allowed: the assistant may create, edit, move, share and post for you, and will say what it changed.'
      : 'Read-only: the assistant can look things up but not change anything.');
  }
  modeHint();

  function autosize() {
    input.style.height = 'auto';
    input.style.height = `${Math.min(input.scrollHeight, 200)}px`;
  }

  function scrollDown() {
    scroller.scrollTop = scroller.scrollHeight;
  }

  function bubble(m) {
    if (m.role === 'user') {
      return h('div.ai-msg.user', h('div.ai-bubble', m.content));
    }
    const steps = stepList();
    (m.trace || []).forEach((ev) => steps.add(ev));
    const body = h('div.ai-body');
    if (m.trace && m.trace.some((ev) => ev.type === 'tool_call')) {
      const n = m.trace.filter((ev) => ev.type === 'tool_call').length;
      body.appendChild(h('details.ai-trace', h('summary.small.muted', icon('cpu', 'sm'), `Used ${n} tool${n === 1 ? '' : 's'}`), steps.el));
    }
    if (m.error) {
      body.appendChild(h('div.ai-error', icon('x', 'sm'), h('span.grow', m.content),
        h('button.btn.sm', { type: 'button', onclick: retry }, icon('refresh', 'sm'), 'Retry')));
    } else {
      body.appendChild(markdown(m.content || '_(no answer)_', { me: ctx.me.user.username }));
    }
    return h('div.ai-msg.assistant', h('span.ai-avatar', icon('assistant', 'sm')), body);
  }

  function welcome() {
    const name = ctx.me.user.display_name.split(' ')[0];
    return h('div.ai-welcome',
      h('div.ai-gate-icon', icon('assistant', 'icon-lg')),
      h('h1', `How can I help, ${name}?`),
      h('p.muted', 'I can search and read your files, conversations and boards, and — when you allow changes — create documents, mind maps, cards and messages.'),
      h('div.ai-suggest', SUGGESTIONS.map(([ic, text]) => h('button.ai-chip', { type: 'button', onclick: () => { input.value = text; send(); } }, icon(ic, 'sm'), h('span', text)))));
  }

  function drawLog() {
    clear(log);
    if (!store.messages.length) log.appendChild(welcome());
    store.messages.forEach((m) => log.appendChild(bubble(m)));
    scrollDown();
  }

  async function send() {
    const text = input.value.trim();
    if (!text || busy) return;
    input.value = '';
    autosize();
    store.messages.push({ role: 'user', content: text });
    saveStore(store);
    await run();
  }

  function retry() {
    // Drop the failed answer and ask again.
    if (store.messages.length && store.messages[store.messages.length - 1].error) store.messages.pop();
    saveStore(store);
    run();
  }

  async function run() {
    drawLog();
    busy = true;
    sendBtn.disabled = true;
    const runId = newRunId();
    const steps = stepList();
    const thinking = h('div.ai-thinking.small.muted', h('span.spinner'), 'Thinking…');
    const pending = h('div.ai-msg.assistant.pending', h('span.ai-avatar', icon('assistant', 'sm')), h('div.ai-body', steps.el, thinking));
    log.appendChild(pending);
    scrollDown();
    const off = live.on('ai.event', (ev) => {
      if (ev.run_id !== runId || !ev.event) return;
      if (ev.event.type === 'tool_call') thinking.lastChild.textContent = `Using ${ev.event.name}…`;
      else if (ev.event.type === 'final') thinking.lastChild.textContent = 'Writing the answer…';
      steps.add(ev.event);
      scrollDown();
    });
    cleanups.push(off);
    try {
      const history = store.messages.filter((m) => !m.error).map(({ role, content }) => ({ role, content }));
      const res = await api.post('/api/ai/chat', { messages: history, allow_writes: writes.checked, run_id: runId });
      store.messages.push({ role: 'assistant', content: res.answer, trace: res.trace || [] });
    } catch (e) {
      store.messages.push({ role: 'assistant', content: e.message || 'The assistant could not answer.', error: true });
    } finally {
      off();
      busy = false;
      sendBtn.disabled = false;
      saveStore(store);
      if (document.body.contains(log)) {
        drawLog();
        input.focus();
      }
    }
  }

  function chat() {
    const newChat = h('button.btn', {
      type: 'button', disabled: false,
      onclick: () => {
        if (busy) return;
        store.messages = [];
        saveStore(store);
        drawLog();
        input.focus();
      },
    }, icon('plus', 'sm'), 'New chat');
    const form = h('form.ai-composer', { onsubmit: (e) => { e.preventDefault(); send(); } },
      h('div.ai-compose-box', input, sendBtn),
      h('div.ai-compose-foot',
        h('label.check.ai-toggle', { for: 'ai-writes' }, writes, h('span', 'Allow changes')),
        hint));
    drawLog();
    setTimeout(() => input.focus(), 0);
    return [
      h('div.ai-head',
        h('h1', icon('assistant'), 'Assistant'),
        h('div.actions', newChat)),
      scroller,
      form,
    ];
  }

  function draw() {
    clear(page);
    if (!status.configured) page.appendChild(notConfigured());
    else if (!status.consent) page.appendChild(noConsent());
    else page.append(...chat());
  }

  page.appendChild(spinner());
  draw();
  return () => cleanups.forEach((fn) => fn());
}
