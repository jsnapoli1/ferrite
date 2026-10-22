// Unified search across files, messages, cards, events and contacts (full text, plus
// meaning-based matches when the server has an embedding model).

import { api, q } from '../api.js';
import { h, icon, emptyState, clear } from '../ui.js';

const KINDS = [['', 'Everything'], ['file', 'Files'], ['message', 'Messages'], ['card', 'Cards'], ['event', 'Events'], ['contact', 'Contacts']];

/** [href, icon, label] for a hit. */
function target(r) {
  switch (r.kind) {
    case 'file': return [`/files/${r.context}?select=${r.id}`, 'file', 'File'];
    case 'message': return [`/talk/${r.context}?message=${r.id}`, 'talk', `Message from ${r.title}`];
    case 'card': return [`/boards/${r.context}?card=${r.id}`, 'boards', 'Card'];
    case 'event': return [`/calendar${q({ view: 'week', date: r.context || '' })}`, 'calendar', 'Event'];
    default: return [`/contacts${q({ c: r.id })}`, 'contacts', 'Contact'];
  }
}

export default async function render(root, ctx) {
  ctx.setTitle('Search');
  const input = h('input', { type: 'search', value: ctx.query.q || '', placeholder: 'Search…', 'aria-label': 'Search' });
  const kind = ctx.query.kinds || '';
  const results = h('div');
  const run = async () => {
    const term = input.value.trim();
    clear(results);
    if (!term) return;
    const { results: hits, semantic } = await api.get(`/api/search${q({ q: term, kinds: kind })}`);
    if (!hits.length) return results.appendChild(emptyState('search', 'No results', `Nothing matches “${term}”.`));
    if (semantic) results.appendChild(h('p.small.muted', icon('assistant', 'sm'), ' Includes matches by meaning, not only by words.'));
    results.appendChild(h('div.panel.list', hits.map((r) => {
      const [href, ic, where] = target(r);
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
