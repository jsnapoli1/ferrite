// Settings → AI (consent; providers, roles and jobs for admins) and Settings → MCP servers
// (external MCP servers whose tools the assistant may use).

import { api } from '../api.js';
import { h, clear, icon, toast, errorToast, confirmDialog, relTime, dateTime, spinner } from '../ui.js';
import { refreshMe } from '../app.js';
import { card, field, toggle, pill, uid } from './settings-common.js';

const PRESETS = [
  ['Ollama', 'openai', 'http://127.0.0.1:11434/v1'],
  ['llama.cpp', 'openai', 'http://127.0.0.1:8080/v1'],
  ['LM Studio', 'openai', 'http://127.0.0.1:1234/v1'],
  ['vLLM', 'openai', 'http://127.0.0.1:8000/v1'],
  ['Claude', 'anthropic', 'https://api.anthropic.com'],
];
const ANTHROPIC_DEFAULT = 'claude-opus-5-5';

export async function aiTab(host, ctx) {
  const isAdmin = ctx.me.user.is_admin;
  let status = await api.get('/api/ai/status');

  // ---- consent (everyone) ----
  const sw = toggle(status.consent, 'Use the assistant', async (on, input) => {
    input.disabled = true;
    try {
      const { user } = await api.patch('/api/me', { ai_consent: on });
      Object.assign(ctx.me.user, user);
      refreshMe().catch(() => {});
      toast(on ? 'The assistant is on' : 'The assistant is off', 'ok', { timeout: 2000 });
    } catch (e) { input.checked = !on; errorToast(e); } finally { input.disabled = false; }
  });
  const statusLine = h('div.row.wrap.small', { style: { gap: '8px', marginTop: '12px' } },
    status.configured ? pill('A model is connected', 'ok') : pill('No model connected yet', 'warn'),
    !status.configured && !isAdmin ? h('span.muted', 'An administrator needs to connect one before the assistant can answer.') : null);
  host.appendChild(card('Your assistant',
    'The assistant answers questions about your files, conversations and boards, and can create documents, mind maps and cards for you.',
    [
      sw.el,
      h('ul.set-points.small',
        h('li', 'It reads your data only when you ask it something, through the same permissions you have — never anything you couldn’t open yourself.'),
        h('li', 'What it reads is sent to the AI model configured on this server', isAdmin ? '.' : ' by your administrator.', ' A local model keeps everything on your own hardware.'),
        h('li', 'It is read-only unless you tick “Allow changes” for a conversation.'),
        h('li', '@assistant in a Talk conversation works the same way, in conversations that allow it.')),
      statusLine,
    ]));

  if (isAdmin) await adminAi(host, ctx, () => status, async () => { status = await api.get('/api/ai/status'); return status; });

  // ---- jobs (everyone sees their own) ----
  const jobs = h('div', spinner());
  host.appendChild(card('Recent AI requests', 'Your last requests to the assistant and quick actions.', jobs));
  try {
    const { jobs: list } = await api.get('/api/ai/jobs');
    const STATUS = { done: 'ok', failed: 'danger', running: 'info' };
    clear(jobs).appendChild(list.length
      ? h('div.set-table-wrap', h('table.table.set-table',
        h('thead', h('tr', h('th', 'Kind'), h('th', 'Status'), h('th', 'Started'), h('th.fl-hide-sm', 'Took'), h('th.fl-hide-sm', 'Error'))),
        h('tbody', list.slice(0, 20).map((j) => h('tr',
          h('td', h('code', j.kind)),
          h('td', pill(j.status, STATUS[j.status] || '')),
          h('td.small.muted', { title: dateTime(j.created_at) }, relTime(j.created_at)),
          h('td.fl-hide-sm.small.muted', j.finished_at ? `${Math.max(0, (new Date(j.finished_at) - new Date(j.created_at)) / 1000).toFixed(1)} s` : '—'),
          h('td.fl-hide-sm.small.set-err', { title: j.error || '' }, j.error || ''))))))
      : h('p.muted.small.set-empty', 'No requests yet.'));
  } catch (e) {
    clear(jobs).appendChild(h('p.error-text', e.message));
  }
}

async function adminAi(host, ctx, getStatus, reloadStatus) {
  const providersHost = h('div', spinner());
  const rolesHost = h('div', spinner());
  let providers = [];

  async function load() {
    try {
      providers = (await api.get('/api/ai/providers')).providers;
      renderProviders();
      renderRoles();
    } catch (e) {
      clear(providersHost).appendChild(h('p.error-text', e.message));
    }
  }

  function renderProviders() {
    clear(providersHost).appendChild(providers.length
      ? h('div.list.panel', providers.map((p) => {
        const refreshBtn = h('button.btn.sm', {
          type: 'button',
          onclick: async () => {
            refreshBtn.disabled = true;
            try {
              const { models } = await api.post(`/api/ai/providers/${p.id}/models`);
              p.models = models;
              toast(`${p.name}: ${models.length} model${models.length === 1 ? '' : 's'} available`, 'ok');
              renderProviders();
              renderRoles();
            } catch (e) { errorToast(e); refreshBtn.disabled = false; }
          },
        }, icon('refresh', 'sm'), h('span.fl-hide-sm', 'Refresh models'));
        return h('div.list-item.set-provider',
          h('span.set-provider-icon', icon(p.kind === 'anthropic' ? 'assistant' : 'cpu', 'sm')),
          h('div.grow',
            h('div.row.wrap', { style: { gap: '6px' } }, h('strong', p.name), pill(p.kind === 'anthropic' ? 'Anthropic' : 'OpenAI-compatible', 'info'), p.has_key ? pill('API key set') : null),
            h('div.small.muted.mono.ellipsis', p.base_url),
            h('div.small.muted', p.models.length ? `${p.models.length} model${p.models.length === 1 ? '' : 's'}: ${p.models.slice(0, 4).join(', ')}${p.models.length > 4 ? '…' : ''}` : 'Models not listed yet — refresh, or type a model name in the roles below.')),
          refreshBtn,
          h('button.btn.sm.ghost.danger', {
            type: 'button', 'aria-label': `Delete provider ${p.name}`, title: 'Delete',
            onclick: async () => {
              if (!(await confirmDialog('Delete provider?', `Roles using “${p.name}” stop working until you assign another model.`, { confirm: 'Delete', danger: true }))) return;
              try { await api.del(`/api/ai/providers/${p.id}`); load(); } catch (e) { errorToast(e); }
            },
          }, icon('trash', 'sm')));
      }))
      : h('p.muted.small.set-empty', 'No providers yet. Add one below.'));
  }

  function renderRoles() {
    const roles = getStatus().roles || [];
    if (!providers.length) {
      clear(rolesHost).appendChild(h('p.muted.small.set-empty', 'Add a provider first, then choose which model serves each role.'));
      return;
    }
    clear(rolesHost).appendChild(h('div.set-roles', getStatus().roles_available.map(({ role, description }) => {
      const cur = roles.find((r) => r.role === role);
      const listId = uid('models');
      const dl = h('datalist', { id: listId });
      const prov = h('select', { 'aria-label': `Provider for ${role}` }, providers.map((p) => h('option', { value: p.id, selected: cur?.provider_id === p.id }, p.name)));
      const model = h('input', { type: 'text', list: listId, value: cur?.model || '', placeholder: 'Model name', 'aria-label': `Model for ${role}`, autocomplete: 'off' });
      const fill = () => {
        const p = providers.find((x) => x.id === prov.value);
        const models = p?.models?.length ? p.models : (p?.kind === 'anthropic' ? [ANTHROPIC_DEFAULT] : []);
        clear(dl).append(...models.map((m) => h('option', { value: m })));
        model.placeholder = models[0] || 'Model name, e.g. llama3.1';
        if (!model.value && p?.kind === 'anthropic') model.value = ANTHROPIC_DEFAULT;
      };
      prov.addEventListener('change', fill);
      fill();
      const save = h('button.btn.primary.sm', { type: 'submit', 'aria-label': `Save ${role} role` }, 'Save');
      return h('form.set-role', {
        onsubmit: async (e) => {
          e.preventDefault();
          save.disabled = true;
          try {
            await api.put('/api/ai/roles', { role, provider_id: prov.value, model: model.value.trim() });
            await reloadStatus();
            toast(`${role} → ${model.value.trim()}`, 'ok', { timeout: 2000 });
            renderRoles();
          } catch (ex) { errorToast(ex); } finally { save.disabled = false; }
        },
      },
      h('div.set-role-name', h('div.row', { style: { gap: '6px' } }, h('strong', role), cur ? pill('set', 'ok') : pill('not set')), h('div.small.muted', description)),
      prov, h('div.set-role-model', model, dl), save);
    })));
  }

  // Add provider
  const kind = h('select', h('option', { value: 'openai' }, 'OpenAI-compatible (Ollama, llama.cpp, LM Studio, vLLM, …)'), h('option', { value: 'anthropic' }, 'Anthropic (Claude)'));
  const name = h('input', { type: 'text', placeholder: 'e.g. Local Ollama', required: true });
  const url = h('input', { type: 'url', placeholder: 'http://127.0.0.1:11434/v1', required: true });
  const key = h('input', { type: 'password', placeholder: 'Optional for local servers', autocomplete: 'off' });
  const err = h('div.error-text', { role: 'alert' });
  const addBtn = h('button.btn.primary', { type: 'submit' }, icon('plus', 'sm'), 'Add provider');
  const presets = h('div.set-presets', h('span.small.muted', 'Presets:'), PRESETS.map(([n, k, u]) => h('button.btn.sm', {
    type: 'button',
    onclick: () => {
      kind.value = k; name.value = n; url.value = u;
      key.placeholder = k === 'anthropic' ? 'sk-ant-… (required)' : 'Optional for local servers';
      (k === 'anthropic' ? key : name).focus();
    },
  }, n)));
  kind.addEventListener('change', () => { key.placeholder = kind.value === 'anthropic' ? 'sk-ant-… (required)' : 'Optional for local servers'; });
  const addForm = h('form.set-add', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      addBtn.disabled = true;
      try {
        await api.post('/api/ai/providers', { name: name.value.trim(), kind: kind.value, base_url: url.value.trim(), api_key: key.value || null });
        toast(`Added ${name.value.trim()}. Refresh its models, then assign it to the chat role.`, 'ok');
        name.value = url.value = key.value = '';
        load();
      } catch (ex) { err.textContent = ex.message; } finally { addBtn.disabled = false; }
    },
  },
  h('h3', 'Add a provider'),
  presets,
  h('div.set-grid', field('Kind', kind), field('Name', name)),
  h('div.set-grid', field('Base URL', url, 'For OpenAI-compatible servers this usually ends in /v1.'), field('API key', key)),
  err, h('div.set-actions', addBtn));

  host.append(
    card('Model providers', 'Where the models run. Any OpenAI-compatible server works (local or remote), as does Anthropic’s API.', [providersHost, addForm]),
    card('Roles', 'Which model does what. The chat role powers the assistant and the Talk bot; summaries fall back to it.', rolesHost));
  await load();
}

// ---- MCP servers ------------------------------------------------------------------------

export async function mcpServersTab(host, ctx) {
  const isAdmin = ctx.me.user.is_admin;
  const listHost = h('div', spinner());

  async function load() {
    try {
      const { servers } = await api.get('/api/mcp/servers');
      clear(listHost).appendChild(servers.length
        ? h('div.list.panel', servers.map((s) => {
          const tools = h('div.set-probe', { hidden: true });
          const canDelete = s.owner_id ? true : isAdmin;
          const args = (() => { try { return JSON.parse(s.args || '[]'); } catch (_) { return []; } })();
          const probe = h('button.btn.sm', {
            type: 'button',
            onclick: async () => {
              probe.disabled = true;
              tools.hidden = false;
              clear(tools).appendChild(spinner());
              try {
                const { tools: list } = await api.post(`/api/mcp/servers/${s.id}/probe`);
                clear(tools).append(h('div.small.muted', `${list.length} tool${list.length === 1 ? '' : 's'} — the assistant sees them as `, h('code', `ext_${s.name.toLowerCase().replace(/[^a-z0-9]+/g, '_').replace(/^_+|_+$/g, '')}__<tool>`)),
                  h('div.list', list.map((t) => h('div.list-item', h('div.grow', h('code', t.name), t.description ? h('div.small.muted', t.description) : null),
                    t.annotations?.readOnlyHint ? pill('read-only', 'ok') : null))));
              } catch (e) {
                clear(tools).appendChild(h('p.error-text', e.message));
              } finally { probe.disabled = false; }
            },
          }, icon('plug', 'sm'), 'Probe');
          return h('div.set-server',
            h('div.list-item',
              h('span.set-provider-icon', icon(s.transport === 'stdio' ? 'cpu' : 'globe', 'sm')),
              h('div.grow',
                h('div.row.wrap', { style: { gap: '6px' } }, h('strong', s.name), pill(s.transport), s.owner_id ? pill('Only you') : pill('Everyone', 'info'), s.enabled ? null : pill('disabled', 'warn')),
                h('div.small.muted.mono.ellipsis', s.transport === 'http' ? s.url : [s.command, ...args].join(' '))),
              probe,
              canDelete ? h('button.btn.sm.ghost.danger', {
                type: 'button', 'aria-label': `Remove ${s.name}`, title: 'Remove',
                onclick: async () => {
                  if (!(await confirmDialog('Remove MCP server?', `The assistant will no longer use “${s.name}”.`, { confirm: 'Remove', danger: true }))) return;
                  try { await api.del(`/api/mcp/servers/${s.id}`); load(); } catch (e) { errorToast(e); }
                },
              }, icon('trash', 'sm')) : null),
            tools);
        }))
        : h('p.muted.small.set-empty', 'No MCP servers connected.'));
    } catch (e) {
      clear(listHost).appendChild(h('p.error-text', e.message));
    }
  }

  const name = h('input', { type: 'text', placeholder: 'e.g. GitHub', required: true, maxlength: 64 });
  const transport = h('select', h('option', { value: 'http' }, 'HTTP (Streamable HTTP)'), isAdmin ? h('option', { value: 'stdio' }, 'stdio (runs a command on this server)') : null);
  const url = h('input', { type: 'url', placeholder: 'https://example.com/mcp' });
  const command = h('input', { type: 'text', placeholder: 'npx' });
  const args = h('textarea', { rows: 2, placeholder: 'One argument per line\n-y\n@modelcontextprotocol/server-everything' });
  const headerRows = h('div.set-headers');
  const addHeader = (k = '', v = '') => {
    const row = h('div.row.set-header-row',
      h('input', { type: 'text', placeholder: 'Header', value: k, 'aria-label': 'Header name' }),
      h('input', { type: 'text', placeholder: 'Value', value: v, 'aria-label': 'Header value' }),
      h('button.btn.ghost.icon.sm', { type: 'button', 'aria-label': 'Remove header', onclick: () => row.remove() }, icon('x', 'sm')));
    headerRows.appendChild(row);
  };
  const global = h('input', { type: 'checkbox' });
  const httpFields = h('div', field('URL', url),
    h('label', 'Headers'), h('div.small.muted.set-help', 'Optional, e.g. Authorization: Bearer …'), headerRows,
    h('button.btn.sm.ghost', { type: 'button', onclick: () => addHeader() }, icon('plus', 'sm'), 'Add header'));
  const stdioFields = h('div', { hidden: true },
    h('p.small.set-warn', icon('lock', 'sm'), 'stdio servers run a program on this machine and are available to everyone.'),
    h('div.set-grid', field('Command', command), field('Arguments', args)));
  transport.addEventListener('change', () => {
    httpFields.hidden = transport.value !== 'http';
    stdioFields.hidden = transport.value !== 'stdio';
  });
  const err = h('div.error-text', { role: 'alert' });
  const addBtn = h('button.btn.primary', { type: 'submit' }, icon('plus', 'sm'), 'Add server');
  const form = h('form.set-add', {
    onsubmit: async (e) => {
      e.preventDefault();
      err.textContent = '';
      const body = { name: name.value.trim(), transport: transport.value, global: global.checked };
      if (transport.value === 'http') {
        body.url = url.value.trim();
        body.headers = {};
        headerRows.querySelectorAll('.set-header-row').forEach((r) => {
          const [k, v] = r.querySelectorAll('input');
          if (k.value.trim()) body.headers[k.value.trim()] = v.value;
        });
      } else {
        body.command = command.value.trim();
        body.args = args.value.split('\n').map((s) => s.trim()).filter(Boolean);
      }
      addBtn.disabled = true;
      try {
        await api.post('/api/mcp/servers', body);
        toast(`Added ${body.name}. Probe it to check the connection.`, 'ok');
        name.value = url.value = command.value = args.value = '';
        clear(headerRows);
        load();
      } catch (ex) { err.textContent = ex.message; } finally { addBtn.disabled = false; }
    },
  },
  h('h3', 'Connect a server'),
  h('div.set-grid', field('Name', name), field('Transport', transport)),
  httpFields, stdioFields,
  isAdmin ? h('label.check', global, h('span', 'Available to everyone on this server')) : null,
  err, h('div.set-actions', addBtn));

  host.appendChild(card('Connected MCP servers',
    'Give the assistant more tools by connecting other MCP servers. Their tools run with the credentials you configure here; servers you add are only used for you.',
    [listHost, h('hr'), form]));
  await load();
}
