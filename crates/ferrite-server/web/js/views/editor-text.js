// Text editor (.md / .txt): a textarea bound to the shared Y.Text "content" with a live
// Markdown preview (edit / split / preview), word count and the list of people editing.

import { bindTextarea } from '../collab.js';
import { markdown } from '../markdown.js';
import { h, clear } from '../ui.js';

const MODES = [['edit', 'Edit'], ['split', 'Split'], ['preview', 'Preview']];

export default function mount({ body, toolbar, provider, doc, entry, me, readOnly, setHelp }) {
  const ytext = doc.getText('content');
  const isMarkdown = /\.(md|markdown)$/i.test(entry.name);
  const storeKey = `ferrite-editor-mode:${isMarkdown ? 'md' : 'txt'}`;
  let mode = readOnly ? 'preview' : (localStorage.getItem(storeKey) || (isMarkdown ? 'split' : 'edit'));
  if (!isMarkdown && mode !== 'edit' && !readOnly) mode = 'edit';

  const textarea = h('textarea.text-input', {
    spellcheck: 'true', 'aria-label': `Contents of ${entry.name}`, readonly: readOnly,
    placeholder: readOnly ? '' : (isMarkdown ? 'Start writing… Markdown works: # headings, **bold**, - lists, [links](https://…)' : 'Start writing…'),
  });
  const preview = h('div.text-preview', { 'aria-label': 'Preview', tabindex: 0 });
  const paneEdit = h('div.text-pane.text-pane-edit', textarea);
  const panePreview = h('div.text-pane.text-pane-preview', preview);
  const wrap = h('div.text-wrap', paneEdit, panePreview);
  body.appendChild(wrap);

  // ---- toolbar ----
  const modeBtns = new Map();
  const seg = h('div.segmented', { role: 'group', 'aria-label': 'View' });
  for (const [m, label] of MODES) {
    if (!isMarkdown && m !== 'edit' && !readOnly) continue;
    const b = h('button', { type: 'button', 'aria-pressed': 'false', onclick: () => setMode(m) }, label);
    modeBtns.set(m, b);
    seg.appendChild(b);
  }
  const counts = h('span.small.muted.text-counts', { 'aria-live': 'off' });
  const editing = h('span.small.muted.text-editing');
  toolbar.append(isMarkdown || readOnly ? seg : null, h('div.grow'), editing, counts);

  function setMode(m) {
    mode = m;
    if (!readOnly) { try { localStorage.setItem(storeKey, m); } catch (_) { /* storage off */ } }
    wrap.dataset.mode = m;
    modeBtns.forEach((b, k) => b.setAttribute('aria-pressed', String(k === m)));
    if (m !== 'edit') renderPreview();
    if (m !== 'preview' && !readOnly) textarea.focus();
  }

  // ---- preview (rendered at most once per frame, only when visible) ----
  let previewQueued = false;
  function renderPreview() {
    if (previewQueued) return;
    previewQueued = true;
    requestAnimationFrame(() => {
      previewQueued = false;
      if (mode === 'edit') return;
      const scroll = panePreview.scrollTop;
      const text = ytext.toString();
      clear(preview).appendChild(isMarkdown ? markdown(text, { me: me.user.username }) : h('pre.text-plain', text));
      if (!text.trim()) preview.appendChild(h('p.muted', 'Nothing here yet.'));
      panePreview.scrollTop = scroll;
    });
  }

  function updateCounts() {
    const text = textarea.value;
    const words = (text.match(/[^\s#*_>`~\-|[\]()]+/g) || []).length;
    counts.textContent = `${words.toLocaleString()} word${words === 1 ? '' : 's'} · ${text.length.toLocaleString()} characters`;
  }

  const unbind = bindTextarea(ytext, textarea, () => { renderPreview(); updateCounts(); });
  updateCounts();
  renderPreview();

  // Tab inserts two spaces (Shift+Tab removes up to two at the line start); Escape leaves the
  // textarea so keyboard users can tab onwards.
  textarea.addEventListener('keydown', (e) => {
    if (readOnly) return;
    if (e.key === 'Tab' && !e.ctrlKey && !e.altKey && !e.metaKey) {
      e.preventDefault();
      const { selectionStart: s, value } = textarea;
      if (e.shiftKey) {
        const lineStart = value.lastIndexOf('\n', s - 1) + 1;
        const n = value.slice(lineStart, lineStart + 2).match(/^ {0,2}/)[0].length;
        if (n) {
          textarea.setRangeText('', lineStart, lineStart + n, 'preserve');
          textarea.setSelectionRange(Math.max(lineStart, s - n), Math.max(lineStart, textarea.selectionEnd));
        }
      } else {
        textarea.setRangeText('  ', s, textarea.selectionEnd, 'end');
      }
      textarea.dispatchEvent(new Event('input'));
    } else if (e.key === 'Escape') {
      textarea.blur();
    }
  });

  // Keep the preview roughly aligned with the editor in split mode.
  textarea.addEventListener('scroll', () => {
    if (mode !== 'split') return;
    const max = textarea.scrollHeight - textarea.clientHeight;
    if (max > 0) panePreview.scrollTop = (textarea.scrollTop / max) * (panePreview.scrollHeight - panePreview.clientHeight);
  });

  // Who else is here.
  const onAwareness = () => {
    const names = [...new Map(provider.peers().map((p) => [p.id, p])).values()];
    clear(editing);
    if (!names.length) return;
    names.slice(0, 3).forEach((p, i) => editing.append(i ? ', ' : '', h('span.peer-name', { style: { color: p.color } }, p.name)));
    editing.append(names.length > 3 ? ` and ${names.length - 3} more` : '', names.length === 1 ? ' is also here' : ' are also here');
  };
  provider.awareness.on('change', onAwareness);
  onAwareness();

  setMode(mode);
  setHelp([
    ['Tab', 'Indent (two spaces)'],
    ['Shift+Tab', 'Outdent'],
    ['Esc', 'Leave the text area'],
    ['Ctrl+B / Ctrl+I', 'Bold / italic (Markdown)'],
  ]);

  // Ctrl+B / Ctrl+I wrap the selection in Markdown emphasis.
  textarea.addEventListener('keydown', (e) => {
    if (readOnly || !isMarkdown || !(e.ctrlKey || e.metaKey) || e.altKey) return;
    const mark = e.key === 'b' ? '**' : e.key === 'i' ? '*' : null;
    if (!mark) return;
    e.preventDefault();
    const { selectionStart: s, selectionEnd: t } = textarea;
    const sel = textarea.value.slice(s, t);
    textarea.setRangeText(`${mark}${sel}${mark}`, s, t, 'select');
    if (!sel) textarea.setSelectionRange(s + mark.length, s + mark.length);
    else textarea.setSelectionRange(s + mark.length, t + mark.length);
    textarea.dispatchEvent(new Event('input'));
  });

  return () => {
    unbind();
    provider.awareness.off('change', onAwareness);
  };
}
