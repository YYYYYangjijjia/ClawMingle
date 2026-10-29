// Trusted, fixed worker for an anonymous browser in a networkless bubblewrap root.
// The browser receives no host paths, cookies, bearer credentials or arbitrary JS.
import { createInterface } from 'node:readline';
import { chromium } from '/runtime/node_modules/playwright/index.mjs';

const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
const pending = new Map();
let started = false;
let nextId = 0;
let browser;
const emit = (value) => process.stdout.write(`${JSON.stringify(value)}\n`);
function fail() { emit({ type: 'result', ok: false, code: 'browser_worker_failed' }); }
function publicUrl(value) {
  try {
    const url = new URL(value);
    return url.protocol === 'https:' && !url.username && !url.password && (!url.port || url.port === '443');
  } catch { return false; }
}
function fetchFromHost(url, method) {
  const id = ++nextId;
  if (id > 48 || method !== 'GET' || !publicUrl(url)) return Promise.resolve(null);
  return new Promise((resolve) => {
    const timer = setTimeout(() => { pending.delete(id); resolve(null); }, 12000);
    pending.set(id, (response) => { clearTimeout(timer); resolve(response); });
    emit({ type: 'fetch', id, url, method });
  });
}
async function run(config) {
  if (!publicUrl(config.url) || !Number.isInteger(config.wait_ms) || config.wait_ms < 0 || config.wait_ms > 5000 || typeof config.screenshot !== 'boolean') throw new Error('invalid');
  browser = await chromium.launch({
    executablePath: '/runtime/chromium/chrome', headless: true, chromiumSandbox: false,
    args: ['--disable-dev-shm-usage', '--disable-background-networking', '--disable-features=WebRtcHideLocalIpsWithMdns', '--no-pings', '--disable-breakpad', '--disable-extensions', '--renderer-process-limit=2', '--js-flags=--max-old-space-size=128'],
  });
  const context = await browser.newContext({ viewport: { width: 1280, height: 720 }, serviceWorkers: 'block', acceptDownloads: false, permissions: [], locale: 'zh-CN', timezoneId: 'Asia/Shanghai' });
  await context.routeWebSocket('**/*', (socket) => socket.close());
  await context.route('**/*', async (route) => {
    try {
      const request = route.request();
      const response = await fetchFromHost(request.url(), request.method());
      if (!response?.ok) { await route.abort('blockedbyclient'); return; }
      await route.fulfill({ status: response.status, contentType: response.content_type, body: Buffer.from(response.body, 'base64') });
    } catch { await route.abort('blockedbyclient').catch(() => {}); }
  });
  const page = await context.newPage();
  page.on('dialog', (dialog) => dialog.dismiss().catch(() => {}));
  page.on('download', (download) => download.cancel().catch(() => {}));
  context.on('page', (other) => { if (other !== page) void other.close().catch(() => {}); });
  await page.goto(config.url, { waitUntil: 'domcontentloaded', timeout: 18000 });
  await page.waitForTimeout(config.wait_ms);
  const snapshot = await page.evaluate(() => ({
    title: document.title.slice(0, 300),
    text: (document.body?.innerText || '').slice(0, 24000),
    links: Array.from(document.querySelectorAll('a[href]')).slice(0, 40).map((a) => ({ text: (a.innerText || '').slice(0, 120), url: a.href.slice(0, 2048) })),
  }));
  const result = { type: 'result', ok: true, url: page.url(), ...snapshot };
  if (config.screenshot) {
    const png = await page.screenshot({ type: 'png', fullPage: false, animations: 'disabled', timeout: 5000 });
    if (png.length > 2 * 1024 * 1024) throw new Error('screenshot_limit');
    result.image = { mime_type: 'image/png', data: png.toString('base64') };
  }
  await browser.close(); browser = null;
  emit(result); input.close();
}
input.on('line', (line) => {
  try {
    if (line.length > 3 * 1024 * 1024) throw new Error('input_limit');
    const value = JSON.parse(line);
    if (!started) { started = true; void run(value).catch(async () => { await browser?.close().catch(() => {}); fail(); input.close(); }); }
    else if (value.type === 'fetch_result' && Number.isInteger(value.id)) { const resolve = pending.get(value.id); pending.delete(value.id); resolve?.(value); }
  } catch { fail(); input.close(); }
});
input.on('close', () => { for (const resolve of pending.values()) resolve(null); pending.clear(); void browser?.close().catch(() => {}); });
