// Isolated, short-lived official QQ login. Never registers or edits documents.
import { constants } from 'node:fs';
import { lstat, open, realpath, rename, unlink } from 'node:fs/promises';
import { createHash, randomBytes } from 'node:crypto';
import { isAbsolute, join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const MAX_SECONDS = 15 * 60;
const MAX_JSON_BYTES = 1024 * 1024;
const REASONS = new Set(['group_access_denied', 'login_expired', 'cancelled', 'browser_failed', 'login_failed']);
const STATES = new Set(['starting', 'waiting_scan', 'authorized', 'expired', 'failed']);
const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
class LoginError extends Error {
  constructor(reason) { super(reason); this.reason = reason; }
}
const fail = (reason = 'login_failed') => { throw new LoginError(reason); };

export function validateInput(value, now = Date.now() / 1000) {
  const groupCode = (code) => typeof code === 'string' && code.length >= 5 && code.length <= 12 && !/[^0-9]/.test(code);
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || typeof value.nonce !== 'string' || value.nonce.length !== 64 || /[^a-f0-9]/.test(value.nonce)
      || !groupCode(value.group_code)
      || !Array.isArray(value.group_codes) || value.group_codes.length < 1 || value.group_codes.length > 64
      || value.group_codes.some((code) => !groupCode(code))
      || !value.group_codes.includes(value.group_code)
      || !Number.isSafeInteger(value.expires_at)
      || value.expires_at > Math.floor(now) + MAX_SECONDS) fail();
  if (value.expires_at <= now) fail('login_expired');
  return { nonce: value.nonce, group_code: value.group_code,
    group_codes: [...new Set(value.group_codes)], expires_at: value.expires_at };
}

export async function checkFlowDirectory(path) {
  if (!isAbsolute(path) || resolve(path) !== await realpath(path)) fail();
  const info = await lstat(path);
  if (!info.isDirectory() || info.uid !== process.getuid() || (info.mode & 0o777) !== 0o700) fail();
}

export async function readPrivateFile(path, limit = MAX_JSON_BYTES) {
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const info = await file.stat();
    if (!info.isFile() || info.uid !== process.getuid() || (info.mode & 0o777) !== 0o600
        || info.nlink !== 1 || info.size > limit) fail();
    const value = await file.readFile();
    if (value.length > limit) fail();
    return value;
  } finally { await file.close(); }
}

async function checkExistingPrivate(path) {
  try {
    const info = await lstat(path);
    if (!info.isFile() || info.uid !== process.getuid() || (info.mode & 0o777) !== 0o600 || info.nlink !== 1) fail();
  } catch (error) { if (error.code !== 'ENOENT') throw error; }
}

export async function writePrivateFile(directory, name, bytes) {
  if (!['status.json', 'qr.png', 'browser-state.json'].includes(name)) fail();
  await checkFlowDirectory(directory);
  const target = join(directory, name);
  await checkExistingPrivate(target);
  const temporary = join(directory, `.${name}.${randomBytes(12).toString('hex')}`);
  const file = await open(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
  try {
    await file.writeFile(bytes);
    await file.sync();
    await file.close();
    await rename(temporary, target);
  } catch (error) {
    await file.close().catch(() => {});
    await unlink(temporary).catch(() => {});
    throw error;
  }
}

export function makeStatus(input, state, reason = null, now = Date.now() / 1000) {
  if (!STATES.has(state) || (reason !== null && !REASONS.has(reason))) fail();
  if (['failed', 'expired'].includes(state) !== (reason !== null)) fail();
  const value = { nonce: input.nonce, state, reason, updated_at: Math.floor(now) };
  if (state === 'authorized') {
    value.group_code = input.group_code;
    value.verified_group_codes = [...input.group_codes];
  }
  return value;
}

export function groupPermission(response) {
  // These fields are the current official frontend's group permission gate.
  // An absent/changed schema is not proof of membership or write permission.
  if (!response || response.retcode !== 0) return 'unverified';
  const value = response.data?.get_rsp;
  if (!value || !Number.isInteger(value.cmduin_privilege) || ![0, 1, 2].includes(value.cmduin_privilege)
      || typeof value.shutup_flag !== 'boolean' || typeof value.shutup_flag_me !== 'boolean') return 'unverified';
  return value.shutup_flag_me || (value.cmduin_privilege === 0 && value.shutup_flag) ? 'denied' : 'allowed';
}

export function filterBrowserState(state, now = Date.now() / 1000) {
  if (!Array.isArray(state?.cookies) || state.cookies.length > 2000) fail();
  const cookies = state.cookies.filter((cookie) => {
    if (!['docs.qq.com', '.docs.qq.com', '.qq.com'].includes(cookie.domain)) return false;
    if (typeof cookie.name !== 'string' || !cookie.name || /[^!#$%&'*+.^_`|~0-9a-zA-Z-]/.test(cookie.name)
        || typeof cookie.value !== 'string' || /[\x00-\x20\x7f;]/.test(cookie.value)
        || cookie.name.length > 256 || cookie.value.length > 16384
        || typeof cookie.path !== 'string' || !cookie.path.startsWith('/') || /[\r\n]/.test(cookie.path)
        || !Number.isFinite(cookie.expires) || (cookie.expires !== -1 && cookie.expires <= now)
        || typeof cookie.httpOnly !== 'boolean' || typeof cookie.secure !== 'boolean'
        || !['Strict', 'Lax', 'None'].includes(cookie.sameSite)) return false;
    // Partitioned login cookies are not transferable into the host HTTP client.
    return !cookie.partitionKey;
  }).map(({ name, value, domain, path, expires, httpOnly, secure, sameSite }) =>
    ({ name, value, domain, path, expires, httpOnly, secure, sameSite }));
  if (!cookies.some((cookie) => cookie.name === 'TOK' && cookie.value)) fail();
  return { cookies, origins: [] };
}

export function hasLoginCookies(cookies) {
  return cookies.some((cookie) => cookie.name === 'TOK' && cookie.value)
    && cookies.some((cookie) => ['skey', 'p_skey', 'p_lskey'].includes(cookie.name) && cookie.value);
}

export async function isCancelled(directory, nonce) {
  try { return (await readPrivateFile(join(directory, 'cancel'), 128)).toString('utf8').trim() === nonce; }
  catch (error) { if (error.code === 'ENOENT') return false; throw error; }
}

async function loadRuntime(path) {
  const manifest = JSON.parse((await readPrivateFile(path, 32768)).toString('utf8'));
  for (const name of ['node_modules', 'chromium']) {
    if (typeof manifest[name] !== 'string' || !isAbsolute(manifest[name])) fail('browser_failed');
  }
  const module = join(manifest.node_modules, 'playwright', 'index.mjs');
  for (const candidate of [module, manifest.chromium]) {
    const file = await lstat(candidate);
    if (!file.isFile() || ![0, process.getuid()].includes(file.uid) || (file.mode & 0o022)) fail('browser_failed');
  }
  return { chromium: (await import(pathToFileURL(module).href)).chromium, executablePath: manifest.chromium };
}

function officialRequest(url) {
  try {
    const target = new URL(url);
    return target.protocol === 'https:' && !target.username && !target.password && (!target.port || target.port === '443')
      && ['qq.com', 'gtimg.com', 'gtimg.cn', 'qpic.cn'].some((domain) => target.hostname === domain || target.hostname.endsWith(`.${domain}`));
  } catch { return false; }
}

async function findQr(page) {
  for (const frame of page.frames()) {
    let host;
    try { host = new URL(frame.url()).hostname; } catch { continue; }
    if (!(host === 'ptlogin2.qq.com' || host.endsWith('.ptlogin2.qq.com'))) continue;
    const image = frame.locator('#qrlogin_img');
    try {
      if (await image.count() && await image.isVisible()
          && await image.evaluate((element) => element.complete && element.naturalWidth > 80)) return image;
    } catch (error) { if (!frame.isDetached()) throw error; }
  }
  return null;
}

export async function checkGroups(page, groupCodes, stopCheck) {
  await page.goto('https://docs.qq.com/mall/m/index/doc?padtype=0', { waitUntil: 'domcontentloaded', timeout: 30000 });
  for (const groupCode of groupCodes) {
    await stopCheck();
    const response = await page.evaluate(async (code) => {
      const cookie = (name) => document.cookie.split('; ').find((value) => value.startsWith(`${name}=`))?.slice(name.length + 1) || '';
      const xsrf = cookie('TOK');
      if (!xsrf || location.origin !== 'https://docs.qq.com') return null;
      let bkn = 5381;
      for (const character of cookie('skey')) bkn += (bkn << 5) + character.charCodeAt(0);
      const url = new URL('/cgi-bin/group_online_docs/group_info_get', location.origin);
      url.search = new URLSearchParams({ group_code: code, bkn: String(bkn & 2147483647), xsrf });
      try {
        const result = await fetch(url, { credentials: 'same-origin', redirect: 'error', signal: AbortSignal.timeout(20000) });
        if (!result.ok || Number(result.headers.get('content-length')) > 65536) return null;
        const reader = result.body.getReader(); const chunks = []; let bytes = 0;
        for (;;) {
          const { done, value } = await reader.read(); if (done) break;
          bytes += value.length;
          if (bytes > 65536) { await reader.cancel(); return null; }
          chunks.push(value);
        }
        const buffer = new Uint8Array(bytes); let offset = 0;
        for (const chunk of chunks) { buffer.set(chunk, offset); offset += chunk.length; }
        return JSON.parse(new TextDecoder().decode(buffer));
      } catch { return null; }
    }, groupCode);
    const permission = groupPermission(response);
    if (permission === 'denied') fail('group_access_denied');
    if (permission !== 'allowed') return false;
  }
  return true;
}

export async function runLogin(flowDirectory, runtimePath) {
  process.umask(0o077);
  await checkFlowDirectory(flowDirectory);
  const raw = JSON.parse((await readPrivateFile(join(flowDirectory, 'input.json'), 32768)).toString('utf8'));
  let input;
  // A stale but otherwise valid flow gets an explicit terminal status.
  try { input = validateInput(raw); }
  catch (error) {
    if (error.reason === 'login_expired') {
      await writePrivateFile(flowDirectory, 'status.json', JSON.stringify(makeStatus(raw, 'expired', 'login_expired')));
    }
    throw error;
  }
  let browser; let finished = false; let authorized = false; let monitor; let signalCancelled = false;
  const onSignal = () => { signalCancelled = true; };
  process.on('SIGTERM', onSignal); process.on('SIGINT', onSignal);
  const update = async (state, reason = null) => writePrivateFile(flowDirectory, 'status.json', JSON.stringify(makeStatus(input, state, reason)));
  const stopCheck = async () => {
    if (finished || signalCancelled) fail('cancelled');
    if (Date.now() / 1000 >= input.expires_at) fail('login_expired');
    if (await isCancelled(flowDirectory, input.nonce)) fail('cancelled');
  };
  await update('starting');
  try {
    await stopCheck();
    const runtime = await loadRuntime(runtimePath);
    const session = async () => {
      browser = await runtime.chromium.launch({ executablePath: runtime.executablePath, headless: true,
        timeout: Math.min(30000, Math.max(1, input.expires_at * 1000 - Date.now())),
        args: ['--disable-dev-shm-usage', '--disable-extensions', '--disable-background-networking', '--no-pings'] });
      if (finished) { await browser.close(); return; }
      await stopCheck();
      const context = await browser.newContext({ viewport: { width: 1280, height: 900 }, locale: 'zh-CN',
        timezoneId: 'Asia/Shanghai', permissions: [], acceptDownloads: false, serviceWorkers: 'block' });
      await context.route('**/*', (route) => officialRequest(route.request().url()) ? route.continue() : route.abort('blockedbyclient'));
      await context.routeWebSocket('**/*', (socket) => socket.close());
      const page = await context.newPage();
      page.setDefaultTimeout(15000);
      page.on('dialog', (dialog) => dialog.dismiss().catch(() => {}));
      await page.goto('https://docs.qq.com/desktop/', { waitUntil: 'domcontentloaded', timeout: 30000 });
      // The desktop page currently opens the login modal automatically.
      await page.getByText('QQ登录', { exact: true }).first().click();
      try { await page.getByText('同意', { exact: true }).first().click({ timeout: 5000 }); }
      catch (error) { if (error.name !== 'TimeoutError') throw error; }
      let qr; const qrDeadline = Math.min(input.expires_at * 1000, Date.now() + 30000);
      while (!qr && Date.now() < qrDeadline) {
        await stopCheck(); qr = await findQr(page); if (!qr) await sleep(500);
      }
      if (!qr) fail('browser_failed');
      const snapshot = await qr.screenshot({ type: 'png', timeout: 5000 });
      if (snapshot.length > 1024 * 1024) fail('browser_failed');
      await writePrivateFile(flowDirectory, 'qr.png', snapshot);
      await update('waiting_scan');
      let previousFingerprint = ''; let nextCheck = 0; let failures = 0; let verificationPage;
      while (!finished) {
        await stopCheck();
        // The QQ OAuth marker can be scoped to graph.qq.com. It is used only
        // to notice the completed login, never forwarded into saved credentials.
        const cookies = await context.cookies();
        if (hasLoginCookies(cookies)) {
          const fingerprint = createHash('sha256').update(JSON.stringify(cookies)).digest('hex');
          if (Date.now() >= nextCheck) {
            if (fingerprint !== previousFingerprint) { previousFingerprint = fingerprint; failures = 0; }
            verificationPage ||= await context.newPage();
            if (await checkGroups(verificationPage, input.group_codes, stopCheck)) {
              const state = filterBrowserState(await context.storageState());
              await stopCheck();
              await writePrivateFile(flowDirectory, 'browser-state.json', JSON.stringify(state));
              await stopCheck(); await update('authorized'); authorized = true; return;
            }
            if (++failures >= 3) fail('login_failed');
            nextCheck = Date.now() + 15000;
          }
        }
        await sleep(1000);
      }
    };
    monitor = (async () => {
      while (!finished) { await stopCheck(); await sleep(250); }
    })();
    await Promise.race([session(), monitor]);
  } catch (error) {
    const reason = error instanceof LoginError ? error.reason : 'browser_failed';
    await update(reason === 'login_expired' ? 'expired' : 'failed', reason);
  } finally {
    finished = true;
    await browser?.close().catch(() => {});
    await monitor?.catch(() => {});
    process.off('SIGTERM', onSignal); process.off('SIGINT', onSignal);
    await unlink(join(flowDirectory, 'qr.png')).catch(() => {});
    if (!authorized) await unlink(join(flowDirectory, 'browser-state.json')).catch(() => {});
  }
  return authorized;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  // The manager supplies absolute local paths; never print browser errors/URLs.
  try {
    const args = process.argv.slice(2);
    if (args.length !== 4 || args[0] !== '--flow-dir' || args[2] !== '--runtime') fail();
    process.exitCode = await runLogin(args[1], args[3]) ? 0 : 1;
  } catch { process.stderr.write('QQ document login failed.\n'); process.exitCode = 1; }
}
