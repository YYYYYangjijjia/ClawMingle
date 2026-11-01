import { test, expect } from '@playwright/test';
import { readFile } from 'node:fs/promises';
import { stateFixture, memoryFixture } from './fixture.mjs';

test.use({ viewport: { width: 1360, height: 1000 }, reducedMotion: 'reduce', serviceWorkers: 'block' });

async function openAdmin(page, { base = '/', legacyPersona = false } = {}) {
  const state = stateFixture();
  if (legacyPersona) state.config.persona = { identity: '', files_enabled: false };
  const views = new Map(), requests = [], writes = [], errors = [];
  let stateReads = 0, conflict = true, failView = false, failArchive = false, heldView = null;
  const memoryFor = scope => {
    const key = `${scope.private}:${scope.target}`;
    if (!views.has(key)) {
      const view = memoryFixture(scope);
      if (scope.target === 'group-second') { view.ledger.drafts = []; view.ledger.records = []; }
      views.set(key, view);
    }
    return views.get(key);
  };
  page.on('pageerror', error => errors.push(error.message));
  await page.route('**/*', async route => {
    const request = route.request(), url = new URL(request.url());
    // Every request is fulfilled or rejected here: no real listener, API or login.
    if (url.origin !== 'http://admin.invalid') {
      errors.push(`Unexpected origin: ${url.origin}`);
      return route.abort();
    }
    const path = url.pathname;
    if (path === '/api/bootstrap') return route.fulfill({ json: { csrfToken: 'fixture-csrf' } });
    if (path === `${base}api/state`) { stateReads++; return route.fulfill({ json: state }); }
    if (path === `${base}api/config` || path === `${base}api/memory`) {
      expect(request.method()).toBe('POST');
      expect(request.headers()['x-csrf-token']).toBe('fixture-csrf');
      const body = request.postDataJSON();
      if (path.endsWith('/config')) {
        writes.push(body);
        if (body.section === 'memory') {
          const { section, ...settings } = body;
          state.config.memory = settings;
        } else if (body.section === 'behavior') {
          state.behavior = { ...state.behavior, current: body.behavior, context_windows: body.context_windows,
            max_bubbles: body.max_bubbles, max_bubble_chars: body.max_bubble_chars };
        } else if (body.section === 'persona') {
          if (!body.identity.trim() || !body.soul.trim()) return route.fulfill({ status: 400, json: { error: '身份和性格不能为空。' } });
          state.config.persona = { identity: body.identity, files_enabled: true };
          state.config.soul = body.soul;
        } else {
          errors.push(`Unexpected configuration section: ${body.section}`);
          return route.fulfill({ status: 400, json: { error: 'Unexpected test request' } });
        }
        return route.fulfill({ json: { ok: true } });
      }
      requests.push(body);
      const view = memoryFor({ target: body.target, private: body.private });
      if (body.action === 'view' && failView) return route.fulfill({ status: 503, json: { error: '测试读取失败' } });
      if (body.action === 'archive' && failArchive) return route.fulfill({ status: 409, json: { error: '测试维护正忙' } });
      if (body.action === 'view' && body.target === 'group-fixture' && heldView) {
        const notify = heldView;
        heldView = null;
        await new Promise(resolve => notify(resolve));
      }
      const entry = view.ledger.drafts.find(row => row.id === body.id);
      if (body.action === 'edit') {
        if (conflict) {
          conflict = false; entry.revision = '3';
          return route.fulfill({ status: 409, json: { error: '版本已经更新，请刷新核对。' } });
        }
        if (entry.revision !== body.revision) return route.fulfill({ status: 409, json: { error: '版本不一致' } });
        entry.content = body.content; entry.revision = String(Number(entry.revision) + 1);
      } else if (body.action === 'correct') {
        view.ledger.drafts.push({ ...structuredClone(view.ledger.records.find(row => row.id === body.id)),
          id: '43', revision: '1', status: 'buffered', content: body.content,
          operation: { action: 'rewrite', record_id: body.id, revision: body.revision } });
      } else if (body.action === 'apply') {
        entry.status = 'applied';
        if (entry.operation?.action === 'rewrite') {
          const record = view.ledger.records.find(row => row.id === entry.operation.record_id);
          record.content = entry.content; record.revision = String(Number(record.revision) + 1);
        }
      } else if (!['view', 'archive'].includes(body.action)) {
        errors.push(`Unexpected memory action: ${body.action}`);
        return route.fulfill({ status: 400, json: { error: 'Unexpected test request' } });
      }
      return route.fulfill({ json: view });
    }
    const assets = new Map([[base, 'index.html'], [`${base}assets/app.js`, 'app.js'], [`${base}assets/app.css`, 'app.css']]);
    if (assets.has(path)) {
      const file = assets.get(path);
      return route.fulfill({ body: await readFile(new URL(`../../admin/${file}`, import.meta.url)),
        contentType: file.endsWith('.html') ? 'text/html' : file.endsWith('.js') ? 'text/javascript' : 'text/css' });
    }
    errors.push(`Unexpected path: ${path}`);
    return route.fulfill({ status: 404, body: '' });
  });
  await page.goto(`http://admin.invalid${base}`);
  await expect(page.locator('#memory-group-select option')).toHaveCount(state.groups.length);
  await expect(page.locator('#memory-group-select')).toHaveValue('group-fixture');
  return { state, requests, writes, errors, stateReads: () => stateReads,
    failMemoryRead: value => { failView = value; }, failArchive: value => { failArchive = value; },
    holdNextGroupRead: () => new Promise(resolve => { heldView = resolve; }) };
}

test('memory edits survive refresh, conflict and scope changes; applying requires confirmation', async ({ page }) => {
  const api = await openAdmin(page, { base: '/apps/clawmingle/' });
  expect(api.requests).toHaveLength(0);
  await page.locator('[data-view=memory]').click();
  const draft = page.locator('[data-memory-kind=draft][data-memory-id="42"]');
  await expect(draft).toBeVisible();
  await expect(page.locator('#memory-content img')).toHaveCount(0);
  await expect(page.locator('[data-memory-id="8"]')).toContainText('已过期');
  await draft.getByRole('button', { name: '编辑草稿', exact: true }).click();
  await draft.locator('textarea').fill('未保存的修正正文');
  const before = api.stateReads();
  await page.evaluate(() => document.dispatchEvent(new Event('visibilitychange')));
  await expect.poll(api.stateReads).toBeGreaterThan(before);
  await expect(draft.locator('textarea')).toHaveValue('未保存的修正正文');
  await expect(draft.locator('textarea')).toBeFocused();
  await draft.getByRole('button', { name: '保存草稿', exact: true }).click();
  await expect(page.locator('#memory-error')).toBeVisible();
  await expect(draft.locator('textarea')).toHaveValue('未保存的修正正文');
  expect(api.requests.at(-1).revision).toBe('2');
  await page.locator('#memory-refresh').click();
  await draft.getByRole('button', { name: '核对并采用最新版本', exact: true }).click();
  await page.locator('#confirm-accept').click();
  await draft.getByRole('button', { name: '保存草稿', exact: true }).click();
  await expect(draft.locator('textarea')).toHaveCount(0);
  expect(api.requests.at(-1)).toMatchObject({ revision: '3', content: '未保存的修正正文' });

  await draft.getByRole('button', { name: '编辑草稿', exact: true }).click();
  await draft.locator('textarea').fill('仅保留在群聊编辑中');
  await page.locator('#memory-kind-select').selectOption('private');
  await expect(page.locator('#memory-group-label')).toHaveText('测试私聊');
  await expect(draft.locator('textarea')).toHaveCount(0);
  expect(api.requests.at(-1)).toMatchObject({ private: true, target: 'private-fixture' });
  await page.locator('#memory-kind-select').selectOption('group');
  await expect(draft.locator('textarea')).toHaveValue('仅保留在群聊编辑中');
  api.failMemoryRead(true);
  await page.locator('#memory-refresh').click();
  await expect(page.locator('#memory-error')).toContainText('测试读取失败');
  await expect(draft.locator('textarea')).toHaveValue('仅保留在群聊编辑中');
  api.failMemoryRead(false);
  await draft.getByRole('button', { name: '收起编辑', exact: true }).click();
  await page.locator('#confirm-accept').click();
  await draft.getByRole('button', { name: '确认入库', exact: true }).click();
  await expect(page.locator('#confirm-message')).toContainText('未保存的修正正文');
  await expect(page.locator('#confirm-message')).toContainText('版本 4');
  expect(api.requests.some(row => row.action === 'apply')).toBe(false);
  await page.locator('#confirm-cancel').click();
  expect(api.requests.some(row => row.action === 'apply')).toBe(false);
  await draft.getByRole('button', { name: '确认入库', exact: true }).click();
  await page.locator('#confirm-accept').click();
  await expect(draft).toContainText('已入库');

  const record = page.locator('[data-memory-kind=record][data-memory-id="7"]');
  await record.getByRole('button', { name: '提出更正', exact: true }).click();
  await record.locator('textarea').fill('正式资料的新版本');
  await record.getByRole('button', { name: '创建更正草稿', exact: true }).click();
  const rewrite = page.locator('[data-memory-kind=draft][data-memory-id="43"]');
  await expect(rewrite).toBeVisible();
  await expect(record.locator('.memory-entry-text')).toHaveText('原正式资料');
  await rewrite.getByRole('button', { name: '确认入库', exact: true }).click();
  await expect(page.locator('#confirm-message')).toContainText('改写正式记录 #7');
  await page.locator('#confirm-accept').click();
  await expect(record.locator('.memory-entry-text')).toHaveText('正式资料的新版本');
  await page.locator('#memory-maintain').click();
  await expect.poll(() => api.requests.at(-1).action).toBe('archive');
  await page.locator('.memory-settings>summary').click();
  await page.locator('#memory-settings-form [name=archive_hour]').fill('5');
  await page.locator('#memory-settings-form button[type=submit]').click();
  await expect(page.locator('#memory-settings-form')).toHaveAttribute('data-dirty', 'false');
  expect(api.writes.at(-1)).toMatchObject({ section: 'memory', archive_hour: 5 });
  await expect(page.locator('#memory-schedule-title')).toContainText('05:00');
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect(api.errors).toEqual([]);
});

test('identity and soul stay independent; behavior save/reset preserves archived replies', async ({ page }) => {
  const api = await openAdmin(page);
  await page.locator('[data-view=persona]').click();
  await expect(page.locator('[name=identity]')).toHaveValue('测试助手');
  await expect(page.locator('[name=soul]')).toHaveValue('当前测试性格');
  await expect(page.locator('#identity-source-hint')).toContainText('IDENTITY.md');
  await expect(page.locator('#soul-source-hint')).toContainText('SOUL.md');
  const archive = page.locator('details').filter({ hasText: '旧版分模式回复文案 · 只读存档' });
  await archive.locator('summary').click();
  await expect(archive.locator('textarea[readonly]')).toHaveCount(4);
  await expect(page.locator('[name^="mode_"][name$="_reply"]')).toHaveCount(0);
  const original = structuredClone(api.state.behavior.current.modes);
  const mode = page.locator('details').filter({ has: page.locator('[name="mode_mode-2_name"]') });
  await mode.locator('summary').click();
  await mode.locator('[name="mode_mode-2_name"]').fill('仅修改参与模式名称');
  await page.locator('#behavior-form button[type=submit]').click();
  await expect(page.locator('#behavior-form')).toHaveAttribute('data-dirty', 'false');
  for (const [id, value] of Object.entries(original)) expect(api.writes.at(-1).behavior.modes[id].reply_instructions).toBe(value.reply_instructions);
  await page.locator('#behavior-reset').click();
  await page.locator('#confirm-accept').click();
  await page.locator('#behavior-form button[type=submit]').click();
  await expect(page.locator('#behavior-form')).toHaveAttribute('data-dirty', 'false');
  for (const [id, value] of Object.entries(original)) expect(api.writes.at(-1).behavior.modes[id].reply_instructions).toBe(value.reply_instructions);
  await expect(page.locator('[name=soul]')).toHaveValue('当前测试性格');
  await page.locator('[name=identity]').fill('新的测试名字');
  await page.locator('#persona-form button[type=submit]').click();
  await expect(page.locator('#persona-form')).toHaveAttribute('data-dirty', 'false');
  expect(api.writes.at(-1)).toMatchObject({ identity: '新的测试名字', soul: '当前测试性格' });
  await page.locator('[name=soul]').fill('新的测试性格');
  await page.locator('#persona-form button[type=submit]').click();
  await expect(page.locator('#persona-form')).toHaveAttribute('data-dirty', 'false');
  expect(api.writes.at(-1)).toMatchObject({ identity: '新的测试名字', soul: '新的测试性格' });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
  expect(api.errors).toEqual([]);
});

test('legacy persona displays a migration hint and saves separate identity/soul fields', async ({ page }) => {
  const api = await openAdmin(page, { legacyPersona: true });
  await page.locator('[data-view=persona]').click();
  await expect(page.locator('#identity-source-hint')).toContainText('旧 Soul');
  await page.locator('[name=identity]').fill('迁移后的测试身份');
  await page.locator('#persona-form button[type=submit]').click();
  await expect(page.locator('#persona-form')).toHaveAttribute('data-dirty', 'false');
  expect(api.writes.at(-1)).toMatchObject({ section: 'persona', identity: '迁移后的测试身份', soul: '当前测试性格' });
  await expect(page.locator('#identity-source-hint')).toContainText('IDENTITY.md');
  await expect(page.locator('#soul-source-hint')).toContainText('SOUL.md');
  expect(api.errors).toEqual([]);
});

test('late memory responses cannot replace a new scope, and failed maintenance preserves records', async ({ page }) => {
  const api = await openAdmin(page);
  await page.locator('[data-view=memory]').click();
  await expect(page.locator('[data-memory-id="42"]')).toBeVisible();
  api.failArchive(true);
  await page.locator('#memory-maintain').click();
  await expect(page.locator('#memory-error')).toContainText('测试维护正忙');
  await expect(page.locator('#memory-facts')).toHaveText('2');
  await expect(page.locator('[data-memory-id="42"]')).toContainText('缓冲待归档');
  api.failArchive(false);
  await page.locator('#memory-maintain').click();
  await expect(page.locator('#memory-error')).toBeHidden();
  await page.locator('#memory-group-select').selectOption('group-second');
  await expect(page.locator('#memory-group-label')).toHaveText('另一个测试群');
  await expect(page.locator('#memory-facts')).toHaveText('0');
  const waiting = api.holdNextGroupRead();
  await page.locator('#memory-group-select').selectOption('group-fixture');
  const release = await waiting;
  await page.locator('#memory-group-select').selectOption('group-second');
  await expect(page.locator('#memory-group-label')).toHaveText('另一个测试群');
  const response = page.waitForResponse(reply => reply.url().endsWith('/api/memory') && reply.request().postDataJSON().target === 'group-fixture');
  release();
  await (await response).finished();
  // Drain rendering after the old body arrives, so this assertion cannot pass
  // merely because it ran before the stale fetch continuation.
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await expect(page.locator('#memory-group-label')).toHaveText('另一个测试群');
  await expect(page.locator('#memory-facts')).toHaveText('0');
  await page.locator('[data-view=persona]').click();
  const reads = api.requests.length;
  const states = api.stateReads();
  await page.locator('#refresh-state').click();
  await expect.poll(api.stateReads).toBeGreaterThan(states);
  expect(api.requests).toHaveLength(reads);
  const skillName = page.locator('#skill-form input[name=name]');
  await skillName.fill('123-summary');
  expect(await skillName.evaluate(input => input.checkValidity())).toBe(true);
  for (const invalid of ['_summary', '-summary', 'Summary']) {
    await skillName.fill(invalid);
    expect(await skillName.evaluate(input => input.checkValidity())).toBe(false);
  }
  expect(api.errors).toEqual([]);
});
