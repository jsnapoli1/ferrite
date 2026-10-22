// Browser smoke test of the app shell: setup, sign-in state, navigation, no console errors.
import { chromium } from 'playwright';

const base = process.env.FERRITE_URL || 'http://127.0.0.1:8811';
const shots = process.env.SHOTS || '/tmp';
const browser = await chromium.launch();
const page = await browser.newPage();
const errors = [];
page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
page.on('pageerror', (e) => errors.push(e.message));
await page.goto(base);
await page.getByLabel('Username').fill('admin');
await page.getByLabel('Your name').fill('Ada Admin');
await page.getByLabel('Password (8+ characters)').fill('admin-password');
await page.getByRole('button', { name: 'Create administrator' }).click();
await page.getByRole('heading', { name: 'Hello, Ada' }).waitFor();
await page.screenshot({ path: `${shots}/home.png` });
await page.getByLabel('Search').fill('anything');
await page.keyboard.press('Enter');
await page.getByRole('heading', { name: 'Search' }).waitFor();
await page.reload();
await page.getByRole('heading', { name: 'Search' }).waitFor();
console.log(JSON.stringify({ ok: errors.length === 0, errors, url: page.url() }));
await browser.close();
process.exit(errors.length ? 1 : 0);
