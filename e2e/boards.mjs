// Browser test of boards and the live editors with two people (alice and bob), run against a
// server with a fresh data dir: FERRITE_URL=http://127.0.0.1:8823 node boards.mjs
// Screenshots go to $SHOTS (default /tmp).
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8823';
const shots = process.env.SHOTS || '/tmp';
const H = { 'X-Ferrite-Request': '1' };
const t0 = Date.now();
const log = (...a) => console.log(`[${((Date.now() - t0) / 1000).toFixed(1)}s]`, ...a);

function assert(cond, msg) {
  if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

async function waitFor(fn, what, timeout = 10000) {
  const end = Date.now() + timeout;
  let last;
  while (Date.now() < end) {
    try {
      last = await fn();
      if (last) return last;
    } catch (e) { last = e; }
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`Timed out waiting for ${what} (last: ${last instanceof Error ? last.message : JSON.stringify(last)})`);
}

const browser = await chromium.launch();
const errors = [];
async function newPage(name) {
  const ctx = await browser.newContext({ viewport: { width: 1360, height: 860 } });
  const page = await ctx.newPage();
  page.on('console', (m) => { if (m.type() === 'error') errors.push(`${name}: ${m.text()}`); });
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  page.api = async (method, path, data) => {
    const r = await page.request.fetch(`${base}${path}`, { method, data, headers: H });
    const type = r.headers()['content-type'] || '';
    const body = type.includes('json') ? await r.json() : await r.text();
    if (!r.ok()) throw new Error(`${method} ${path} -> ${r.status()} ${JSON.stringify(body)}`);
    return body;
  };
  return page;
}
const shot = async (page, name, dark = false) => {
  await page.emulateMedia({ colorScheme: dark ? 'dark' : 'light' });
  await page.waitForTimeout(150);
  await page.screenshot({ path: `${shots}/${name}${dark ? '-dark' : ''}.png` });
  if (dark) await page.emulateMedia({ colorScheme: 'light' });
};

let failed = null;
try {
  // ---- setup: alice (admin) through the UI, bob through the admin API ----
  const alice = await newPage('alice');
  await alice.goto(base);
  await alice.getByLabel('Username').fill('alice');
  await alice.getByLabel('Your name').fill('Alice Archer');
  await alice.getByLabel('Password (8+ characters)').fill('alice-password');
  await alice.getByRole('button', { name: 'Create administrator' }).click();
  await alice.getByRole('heading', { name: 'Hello, Alice' }).waitFor();
  await alice.api('POST', '/api/admin/users', { username: 'bob', password: 'bob-password', display_name: 'Bob Builder' });
  const bob = await newPage('bob');
  await bob.api('POST', '/api/auth/login', { username: 'bob', password: 'bob-password' });
  const me = await alice.api('GET', '/api/me');
  const bobMe = await bob.api('GET', '/api/me');

  // Files: a shared folder with a document of each kind and a plain file.
  const { node: folder } = await alice.api('POST', `/api/files/${me.root_id}/mkdir`, { name: 'Shared' });
  await alice.api('POST', `/api/files/${folder.id}/shares`, { kind: 'user', target: 'bob', permissions: 31 });
  const doc = async (title, kind) => (await alice.api('POST', `/api/files/${folder.id}/documents`, { title, kind })).node;
  const textDoc = await doc('Notes', 'text');
  const sheetDoc = await doc('Budget', 'sheet');
  const mapDoc = await doc('Plan', 'mindmap');
  const canvasDoc = await doc('Wall', 'canvas');
  const upload = await alice.request.put(`${base}/api/files/${folder.id}/upload?name=spec.txt`, { data: 'The specification.\n', headers: H });
  const specFile = (await upload.json()).node;
  log('seeded');

  // ---- boards ----
  await alice.goto(`${base}/boards`);
  await alice.getByRole('button', { name: 'New board' }).first().click();
  await alice.getByLabel('Board title').fill('Launch');
  await alice.getByRole('button', { name: 'Create board' }).click();
  await alice.waitForSelector('.stack');
  const boardId = alice.url().split('/boards/')[1];
  // A new column.
  await alice.getByRole('button', { name: 'Column', exact: true }).click();
  await alice.getByRole('dialog', { name: 'New column' }).locator('input').fill('Review');
  await alice.getByRole('button', { name: 'Add column' }).last().click();
  await alice.locator('.stack', { hasText: 'Review' }).waitFor();
  // Cards, typed inline at the bottom of "To do".
  const todo = alice.locator('.stack', { hasText: 'To do' });
  await todo.locator('.add-card').click();
  for (const t of ['Write spec', 'Design mockups', 'Pick a date']) {
    await alice.keyboard.type(t);
    await alice.keyboard.press('Enter');
    await todo.locator('.kcard', { hasText: t }).waitFor();
  }
  await alice.keyboard.press('Escape');
  // Invite bob through the members dialog.
  await alice.getByRole('button', { name: 'Members' }).click();
  await alice.getByLabel('Username to add').fill('bob');
  await alice.locator('.members-add').getByRole('button', { name: 'Add' }).click();
  await alice.locator('.members-list', { hasText: 'Bob Builder' }).waitFor();
  await alice.getByRole('button', { name: 'Done', exact: true }).click();
  log('board created');

  await bob.goto(`${base}/boards/${boardId}`);
  await bob.locator('.kcard', { hasText: 'Write spec' }).waitFor();

  // Drag "Write spec" into "Doing" (HTML5 drag and drop); check the server and bob's view.
  await alice.locator('.kcard', { hasText: 'Write spec' }).dragTo(alice.locator('.stack', { hasText: 'Doing' }).locator('.stack-cards'));
  const findCard = (b, title) => {
    for (const s of b.stacks) for (const c of s.cards) if (c.title === title) return { card: c, stack: s };
    return null;
  };
  await waitFor(async () => findCard((await alice.api('GET', `/api/boards/${boardId}`)).board, 'Write spec')?.stack.title === 'Doing', 'card moved on the server');
  await bob.locator('.stack', { hasText: 'Doing' }).locator('.kcard', { hasText: 'Write spec' }).waitFor();
  // Keyboard alternative: Alt+→ moves the focused card to the next column.
  await alice.locator('.kcard', { hasText: 'Pick a date' }).focus();
  await alice.keyboard.press('Alt+ArrowRight');
  await waitFor(async () => findCard((await alice.api('GET', `/api/boards/${boardId}`)).board, 'Pick a date')?.stack.title === 'Doing', 'keyboard move');
  log('drag and drop ok');

  // Card modal: description, due date, label, assignee, a file link and a comment.
  await alice.locator('.kcard', { hasText: 'Write spec' }).click();
  const modalEl = alice.locator('.card-modal');
  await modalEl.waitFor();
  assert(alice.url().includes('card='), 'URL carries ?card=');
  await modalEl.locator('.card-desc-view').click();
  await modalEl.locator('.card-desc-input').fill('Cover the **API** and the *web UI*.\n\n- [x] outline\n- [ ] review');
  await modalEl.getByRole('button', { name: 'Save' }).click();
  await modalEl.locator('.card-desc-view strong', { hasText: 'API' }).waitFor();
  await modalEl.getByLabel('Due date').fill('2026-11-05T14:30');
  await modalEl.getByLabel('Due date').dispatchEvent('change');
  await modalEl.locator('.label-toggle', { hasText: 'Urgent' }).click();
  await modalEl.locator('.label-toggle[aria-pressed="true"]', { hasText: 'Urgent' }).waitFor();
  await modalEl.locator('.person-toggle', { hasText: 'Bob Builder' }).click();
  await modalEl.locator('.person-toggle[aria-pressed="true"]', { hasText: 'Bob Builder' }).waitFor();
  await modalEl.getByRole('button', { name: 'Attach' }).click();
  await alice.getByRole('menuitem', { name: 'File…' }).click();
  await alice.locator('.picker-list .list-item', { hasText: 'Shared' }).click();
  await alice.locator('.picker-list .list-item', { hasText: 'spec.txt' }).click();
  await alice.getByRole('button', { name: 'Link file' }).click();
  await modalEl.locator('.card-link', { hasText: 'spec.txt' }).waitFor();
  await modalEl.getByLabel('New comment').fill('Draft is **ready** for review @bob');
  await modalEl.getByRole('button', { name: 'Comment' }).click();
  await modalEl.locator('.comment', { hasText: 'ready' }).waitFor();
  await shot(alice, 'board-card-modal');
  await shot(alice, 'board-card-modal', true);
  const { board } = await alice.api('GET', `/api/boards/${boardId}`);
  const { card } = findCard(board, 'Write spec');
  const urgent = board.labels.find((l) => l.title === 'Urgent');
  assert(card.description.includes('**API**'), 'description saved');
  assert(new Date(card.due_at).getTime() === new Date('2026-11-05T14:30').getTime(), `due date saved (${card.due_at})`);
  assert(card.labels.includes(urgent.id), 'label saved');
  assert(card.assignees.includes(bobMe.user.id), 'assignee saved');
  assert(card.links.some((l) => l.kind === 'file' && l.target === specFile.id), 'file link saved');
  assert(card.comment_count === 1, 'comment saved');
  // Bob's board updated live: label chip, avatar, link and comment counts on the card face.
  const bobCard = bob.locator('.kcard', { hasText: 'Write spec' });
  await bobCard.locator('.label-chip', { hasText: 'Urgent' }).waitFor();
  await bobCard.locator('.kcard-badge', { hasText: '1' }).first().waitFor();
  await bobCard.locator('.avatar').waitFor();
  // Close with Escape: the URL loses ?card=.
  await alice.keyboard.press('Escape');
  await modalEl.waitFor({ state: 'detached' });
  assert(!alice.url().includes('card='), 'URL cleared after close');
  await shot(alice, 'board');
  await shot(alice, 'board', true);
  // Deep link for bob opens the card; opening the file link goes to the files view.
  await bob.goto(`${base}/boards/${boardId}?card=${card.id}`);
  await bob.locator('.card-modal .comment', { hasText: 'ready' }).waitFor();
  await bob.locator('.card-modal .card-desc-view strong', { hasText: 'API' }).waitFor();
  await bob.keyboard.press('Escape');
  log('boards ok');

  // ---- text: concurrent typing converges ----
  await alice.goto(`${base}/edit/${textDoc.id}`);
  await bob.goto(`${base}/edit/${textDoc.id}`);
  for (const p of [alice, bob]) await p.locator('.editor-status[data-status="synced"]').waitFor();
  await alice.locator('.text-input').click();
  await alice.keyboard.press('Control+End');
  await bob.locator('.text-input').click();
  await bob.keyboard.press('Control+Home');
  await Promise.all([
    alice.keyboard.type('\nAlice wrote **this** part.\n', { delay: 15 }),
    bob.keyboard.type('Bob was here first. ', { delay: 15 }),
  ]);
  const converged = await waitFor(async () => {
    const [a, b] = await Promise.all([alice.locator('.text-input').inputValue(), bob.locator('.text-input').inputValue()]);
    return a === b && a.includes('Alice wrote **this** part.') && a.includes('Bob was here first. ') && a;
  }, 'text to converge');
  assert(converged.startsWith('Bob was here first. # Notes'), `bob's text at the start: ${JSON.stringify(converged.slice(0, 40))}`);
  await alice.locator('.text-preview strong', { hasText: 'this' }).waitFor();
  await alice.locator('.editor-peers .avatar').waitFor();
  await shot(alice, 'editor-text');
  await shot(alice, 'editor-text', true);
  log('text ok');

  // ---- sheet: numbers and a SUM, computed for both, saved as CSV ----
  await alice.goto(`${base}/edit/${sheetDoc.id}`);
  await bob.goto(`${base}/edit/${sheetDoc.id}`);
  for (const p of [alice, bob]) await p.locator('.editor-status[data-status="synced"]').waitFor();
  await alice.locator('.sheet-grid td[data-r="0"][data-c="0"]').click();
  for (const v of ['10', '20', '12.5', '=SUM(A1:A3)']) {
    await alice.keyboard.type(v);
    await alice.keyboard.press('Enter');
  }
  await alice.locator('.sheet-grid td[data-r="0"][data-c="1"]').click();
  await alice.keyboard.type('=A4*2');
  await alice.keyboard.press('Tab');
  for (const p of [alice, bob]) {
    await waitFor(async () => (await p.locator('.sheet-grid td[data-r="3"][data-c="0"]').textContent()) === '42.5', 'SUM computed');
    await waitFor(async () => (await p.locator('.sheet-grid td[data-r="0"][data-c="1"]').textContent()) === '85', 'reference computed');
  }
  await bob.locator('.sheet-grid td[data-r="3"][data-c="0"]').click();
  assert((await bob.locator('.sheet-fx').inputValue()) === '=SUM(A1:A3)', 'formula bar shows the raw formula');
  await alice.locator('.peer-cell', { hasText: 'Bob Builder' }).waitFor();
  const csv = await waitFor(async () => {
    const t = await alice.api('GET', `/api/files/${sheetDoc.id}/download`);
    return typeof t === 'string' && t.includes('=SUM(A1:A3)') && t.includes('=A4*2') && t;
  }, 'CSV saved', 15000);
  assert(csv.replace(/\r/g, '').startsWith('10,=A4*2\n20,\n12.5,\n=SUM(A1:A3),'), `CSV content: ${JSON.stringify(csv)}`);
  await shot(alice, 'editor-sheet');
  await shot(alice, 'editor-sheet', true);
  log('sheet ok');

  // ---- mind map: keyboard editing, bob sees it, JSON saved, MCP edits appear ----
  await alice.goto(`${base}/edit/${mapDoc.id}`);
  await bob.goto(`${base}/edit/${mapDoc.id}`);
  for (const p of [alice, bob]) await p.locator('.editor-status[data-status="synced"]').waitFor();
  await alice.locator('.mm-node.depth-0').waitFor();
  await alice.locator('.mm-wrap').focus();
  const typeTopic = async (key, text) => {
    await alice.keyboard.press(key);
    await alice.locator('.mm-edit:not([hidden])').waitFor();
    await alice.keyboard.type(text);
    await alice.keyboard.press('Enter');
    await alice.locator('.mm-edit').waitFor({ state: 'hidden' });
  };
  await typeTopic('Tab', 'Goals');
  await typeTopic('Tab', 'Grow');
  await typeTopic('Enter', 'Retain');
  await alice.keyboard.press('ArrowLeft'); // back to "Goals"
  await typeTopic('Enter', 'Risks');
  // "Risks" is the only branch on the left: → goes to the centre, → to "Goals", → into its
  // children (the one nearest), ↑ to "Grow" (no-op when already there).
  await alice.keyboard.press('ArrowRight');
  await alice.keyboard.press('ArrowRight');
  await alice.keyboard.press('ArrowRight');
  await alice.keyboard.press('ArrowUp');
  await alice.keyboard.press('F2');
  await alice.locator('.mm-edit:not([hidden])').waitFor();
  await alice.keyboard.press('Control+A');
  await alice.keyboard.type('Grow fast');
  await alice.keyboard.press('Enter');
  for (const t of ['Goals', 'Grow fast', 'Retain', 'Risks']) await bob.locator('.mm-node text', { hasText: new RegExp(`^${t}$`) }).waitFor();
  const mapJson = await waitFor(async () => {
    const v = JSON.parse(await alice.api('GET', `/api/files/${mapDoc.id}/content`));
    const texts = v.nodes.map((n) => n.text);
    return ['Plan', 'Goals', 'Grow fast', 'Retain', 'Risks'].every((t) => texts.includes(t)) && v;
  }, 'mind map JSON');
  const goals = mapJson.nodes.find((n) => n.text === 'Goals');
  const grow = mapJson.nodes.find((n) => n.text === 'Grow fast');
  const retain = mapJson.nodes.find((n) => n.text === 'Retain');
  assert(grow.parent === goals.id && retain.parent === goals.id && retain.order > grow.order, 'tree structure and order');
  // An AI/MCP client adds nodes to the live map.
  const { secret } = await alice.api('POST', '/api/me/tokens', { name: 'e2e', scopes: ['mcp', 'files', 'docs', 'boards'] });
  const mcp = await alice.request.post(`${base}/mcp`, {
    headers: { Authorization: `Bearer ${secret}`, 'MCP-Protocol-Version': '2026-07-28', 'Mcp-Method': 'tools/call', 'Mcp-Name': 'mindmap_add', Accept: 'application/json, text/event-stream' },
    data: { jsonrpc: '2.0', id: 1, method: 'tools/call', params: { name: 'mindmap_add', arguments: { path: '/Shared/Plan.fmind', parent: 'Risks', nodes: [{ text: 'Competition', children: [{ text: 'Pricing' }] }] }, _meta: { 'io.modelcontextprotocol/protocolVersion': '2026-07-28', 'io.modelcontextprotocol/clientCapabilities': {} } } },
  });
  const mcpText = await mcp.text();
  assert(mcp.ok() && !mcpText.includes('"isError":true'), `MCP mindmap_add: ${mcp.status()} ${mcpText}`);
  for (const p of [alice, bob]) {
    await p.locator('.mm-node text', { hasText: /^Competition$/ }).waitFor();
    await p.locator('.mm-node text', { hasText: /^Pricing$/ }).waitFor();
  }
  await shot(alice, 'editor-mindmap');
  await shot(alice, 'editor-mindmap', true);
  log('mind map ok');

  // ---- canvas: note, file, mind map, board, an arrow; moves show up for bob ----
  await alice.goto(`${base}/edit/${canvasDoc.id}`);
  await bob.goto(`${base}/edit/${canvasDoc.id}`);
  for (const p of [alice, bob]) await p.locator('.editor-status[data-status="synced"]').waitFor();
  await alice.getByRole('button', { name: 'Note', exact: true }).click();
  await alice.locator('.cv-item.editing textarea').waitFor();
  await alice.keyboard.type('Launch **checklist**');
  await alice.locator('.cv-wrap').click({ position: { x: 30, y: 30 } });
  await alice.locator('.type-note strong', { hasText: 'checklist' }).waitFor();
  const pick = async (button, name, confirm) => {
    await alice.getByRole('button', { name: button, exact: true }).click();
    await alice.locator('.picker-list .list-item', { hasText: name }).click();
    await alice.locator('.modal-foot').getByRole('button', { name: confirm, exact: true }).click();
  };
  await pick('File', 'spec.txt', 'Add');
  await alice.locator('.type-file', { hasText: 'spec.txt' }).waitFor();
  await alice.getByRole('button', { name: 'Mind map', exact: true }).click();
  await alice.getByRole('menuitem', { name: 'Existing mind map…' }).click();
  await alice.locator('.picker-list .list-item', { hasText: 'Plan.fmind' }).click();
  await alice.locator('.modal-foot').getByRole('button', { name: 'Add', exact: true }).click();
  await alice.locator('.type-mindmap .cv-branches li', { hasText: 'Goals' }).waitFor();
  await alice.getByRole('button', { name: 'Board', exact: true }).click();
  await alice.locator('.picker-list .list-item', { hasText: 'Launch' }).click();
  await alice.locator('.modal-foot').getByRole('button', { name: 'Add', exact: true }).click();
  await alice.locator('.type-board .cv-board-col', { hasText: 'Doing' }).waitFor();
  // Spread the items out by dragging (new items land near the middle).
  const dragBy = async (loc, dx, dy) => {
    const b = await loc.boundingBox();
    await alice.mouse.move(b.x + 30, b.y + 20);
    await alice.mouse.down();
    await alice.mouse.move(b.x + 30 + dx / 2, b.y + 20 + dy / 2, { steps: 4 });
    await alice.mouse.move(b.x + 30 + dx, b.y + 20 + dy, { steps: 4 });
    await alice.mouse.up();
  };
  // Topmost (latest) first, so each drag grabs the item it means to.
  await dragBy(alice.locator('.type-board'), 320, 150);
  await dragBy(alice.locator('.type-mindmap'), -360, 140);
  await dragBy(alice.locator('.type-file'), 300, -230);
  await dragBy(alice.locator('.type-note'), -380, -200);
  // Connect the note to the file.
  await alice.getByRole('button', { name: 'Connect' }).click();
  await alice.locator('.type-note').click();
  await alice.locator('.type-file').click();
  await alice.keyboard.press('Escape');
  await alice.locator('.cv-edges [data-edge]').waitFor();
  await bob.locator('.cv-edges [data-edge]').waitFor();
  for (const t of ['note', 'file', 'mindmap', 'board']) await bob.locator(`.type-${t}`).waitFor();
  // Move the note and watch it move for bob.
  const before = await bob.locator('.type-note').evaluate((el) => el.style.left);
  await dragBy(alice.locator('.type-note'), 0, 90);
  await waitFor(async () => (await bob.locator('.type-note').evaluate((el) => el.style.top)) !== '' && (await bob.locator('.type-note').evaluate((el) => el.style.left)) === before
    && (await bob.locator('.type-note').evaluate((el) => el.style.top)) === (await alice.locator('.type-note').evaluate((el) => el.style.top)), 'note moved for bob');
  // Bob's pointer shows on alice's canvas.
  const bw = await bob.locator('.cv-wrap').boundingBox();
  await bob.mouse.move(bw.x + 200, bw.y + 200);
  await bob.mouse.move(bw.x + 240, bw.y + 220, { steps: 3 });
  await alice.locator('.cv-cursor', { hasText: 'Bob Builder' }).waitFor();
  await shot(alice, 'editor-canvas');
  await shot(alice, 'editor-canvas', true);
  // Persistence: the file holds items and the edge; a reload shows them again.
  const canvasJson = await waitFor(async () => {
    const v = JSON.parse(await alice.api('GET', `/api/files/${canvasDoc.id}/content`));
    return v.items.length === 4 && v.edges.length === 1 && v;
  }, 'canvas JSON');
  const types = canvasJson.items.map((i) => i.type).sort().join(',');
  assert(types === 'board,file,mindmap,note', `item types ${types}`);
  const fileItem = canvasJson.items.find((i) => i.type === 'file');
  assert(fileItem.ref === specFile.id && canvasJson.items.find((i) => i.type === 'board').ref === boardId, 'item refs');
  assert(canvasJson.edges[0].from === canvasJson.items.find((i) => i.type === 'note').id && canvasJson.edges[0].to === fileItem.id, 'edge endpoints');
  await waitFor(async () => !(await alice.api('GET', `/api/files/${canvasDoc.id}/download`)).includes('"items": []'), 'canvas flushed to disk', 15000);
  await alice.reload();
  await alice.locator('.editor-status[data-status="synced"]').waitFor();
  for (const t of ['note', 'file', 'mindmap', 'board']) await alice.locator(`.type-${t}`).waitFor();
  await alice.locator('.cv-edges [data-edge]').waitFor();
  log('canvas ok');

  // ---- views without data ----
  await alice.goto(`${base}/boards`);
  await alice.locator('.board-tile', { hasText: 'Launch' }).waitFor();
  await shot(alice, 'boards-list');
} catch (e) {
  failed = e;
}

await browser.close();
const ok = !failed && errors.length === 0;
console.log(JSON.stringify({ ok, error: failed ? failed.message : null, errors, seconds: Math.round((Date.now() - t0) / 1000) }));
process.exit(ok ? 0 : 1);
