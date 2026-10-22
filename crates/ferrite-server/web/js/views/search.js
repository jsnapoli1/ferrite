// Unified search across files, messages and cards.

import { api, q } from '../api.js';
import { h, icon, emptyState, clear } from '../ui.js';

const KINDS = [['', 'Everything'], ['file', 'Files'], ['message', 'Messages'], ['card', 'Cards']];

export default async function render(root, ctx) {
  ctx.setTitle('Search');
  const input = h('input', { type: 'search', value: ctx.query.q || '', placeholder: 'Search…', 'aria-label': 'Search' });
  const kind = ctx.query.kinds || '';
  const results = h('div');
  const run = async () => {
    const term = input.value.trim();
    clear(results);
    if (!term) return;
    const { results: hits } = await api.get(`/api/search${q({ q: term, kinds: kind })}`);
    if (!hits.length) return results.appendChild(emptyState('search', 'No results', `Nothing matches “${term}”.`));
    results.appendChild(h('div.panel.list', hits.map((r) => {
      const [href, ic, where] = r.kind === 'file'
        ? [`/files/${r.context}?select=${r.id}`, 'file', 'File']
        : r.kind === 'message'
          ? [`/talk/${r.context}?message=${r.id}`, 'talk', `Message from ${r.title}`]
          : [`/boards/${r.context}?card=${r.id}`, 'boards', 'Card'];
      const snippet = h('div.small.muted');
      // Highlight the [matched] terms from the FTS snippet without using HTML.
      r.snippet.split(/(\[[^\]]*\])/).forEach((part) => {
        if (part.startsWith('[') && part.endsWith(']')) snippet.appendChild(h('mark', part.slice(1, -1)));
        else snippet.appendChild(document.createTextNode(part));
      });
      return h('a.list-item.clickable', { href, onclick: (e) => { e.preventDefault(); ctx.navigate(href); } },
        icon(ic), h('div.grow', h('div', r.kind === 'message' ? where : r.title), snippet), h('span.pill', where.split(' ')[0]));
    })));
  };
  root.appendChild(h('div.page',
    h('div.page-head', h('h1', 'Search')),
    h('form.row', { style: { marginBottom: '12px' }, onsubmit: (e) => { e.preventDefault(); ctx.navigate(`/search${q({ q: input.value.trim(), kinds: kind })}`); } },
      input, h('button.btn.primary', { type: 'submit' }, icon('search'), 'Search')),
    h('div.tabs', KINDS.map(([k, label]) => h(`a${k === kind ? '.active' : ''}`, { href: `/search${q({ q: input.value, kinds: k })}`, onclick: (e) => { e.preventDefault(); ctx.navigate(`/search${q({ q: input.value.trim(), kinds: k })}`); } }, label))),
    results));
  await run();
}
