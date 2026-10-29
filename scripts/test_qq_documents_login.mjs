import test from 'node:test';
import assert from 'node:assert/strict';
import { chmod, link, lstat, mkdtemp, readFile, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  checkFlowDirectory, checkGroups, filterBrowserState, groupPermission, hasLoginCookies,
  isCancelled, makeStatus, readPrivateFile, runLogin, validateInput, writePrivateFile,
} from './clawmingle_qq_documents_login.mjs';

const nonce = 'a'.repeat(64);
const input = (now = 1000) => ({ nonce, group_code: '123456', group_codes: ['123456', '234567'], expires_at: now + 600 });
const permission = (fields = {}) => ({ retcode: 0, data: { get_rsp: {
  cmduin_privilege: 1, shutup_flag: false, shutup_flag_me: false, ...fields,
} } });
const cookie = (fields = {}) => ({ name: 'TOK', value: 'fixture-value', domain: 'docs.qq.com',
  path: '/', expires: -1, httpOnly: false, secure: true, sameSite: 'Lax', ...fields });

async function privateDirectory(t) {
  const directory = await mkdtemp(join(tmpdir(), 'qq-documents-login-test-'));
  await chmod(directory, 0o700);
  t.after(() => rm(directory, { recursive: true, force: true }));
  return directory;
}

test('login input requires target membership, bounded groups and a deadline within fifteen minutes', () => {
  assert.deepEqual(validateInput(input(), 1000), input());
  for (const patch of [
    { nonce: 'short' }, { nonce: `${nonce}\n` }, { group_code: '12345\n', group_codes: ['12345\n'] }, { group_code: 123456 },
    { group_codes: [] }, { group_codes: ['234567'] }, { group_codes: ['123456', 'invalid'] },
    { group_codes: Array(101).fill('123456') }, { expires_at: 1901 }, { expires_at: 1600.5 },
  ]) assert.throws(() => validateInput({ ...input(), ...patch }, 1000));
  assert.throws(() => validateInput(input(), 1700), { message: 'login_expired' });
  assert.deepEqual(validateInput({ ...input(), group_codes: ['123456', '123456'] }, 1000).group_codes, ['123456']);
});

test('permission requires complete positive evidence and respects group mute rules', () => {
  assert.equal(groupPermission(permission()), 'allowed');
  assert.equal(groupPermission(permission({ cmduin_privilege: 0 })), 'allowed');
  assert.equal(groupPermission(permission({ cmduin_privilege: 0, shutup_flag: true })), 'denied');
  assert.equal(groupPermission(permission({ shutup_flag_me: true })), 'denied');
  assert.equal(groupPermission(permission({ cmduin_privilege: 1, shutup_flag: true })), 'allowed');
  for (const response of [null, {}, { retcode: '0' }, { retcode: 1 }, { retcode: 0 },
    permission({ cmduin_privilege: 3 }), permission({ shutup_flag_me: undefined }), permission({ shutup_flag: 0 })]) {
    assert.equal(groupPermission(response), 'unverified');
  }
});

test('QQ scoped login cookies work without literal skey', () => {
  assert.equal(hasLoginCookies([cookie(), cookie({ name: 'p_lskey', domain: '.qq.com' })]), true);
  assert.equal(hasLoginCookies([cookie(), cookie({ name: 'p_skey', domain: '.qq.com' })]), true);
  assert.equal(hasLoginCookies([cookie(), cookie({ name: 'p_skey', domain: '.graph.qq.com' })]), true);
  assert.equal(hasLoginCookies([cookie()]), false);
  assert.equal(hasLoginCookies([cookie({ name: 'skey' })]), false);
});

test('every existing and target group must pass before login can be authorized', async () => {
  const checked = []; let checks = 0;
  const page = {
    goto: async (url) => assert.equal(url, 'https://docs.qq.com/mall/m/index/doc?padtype=0'),
    evaluate: async (_, group) => { checked.push(group); return permission(); },
  };
  assert.equal(await checkGroups(page, ['123456', '234567'], async () => { checks++; }), true);
  assert.deepEqual(checked, ['123456', '234567']); assert.equal(checks, 2);
  page.evaluate = async (_, group) => group === '123456' ? permission() : { retcode: 50000 };
  assert.equal(await checkGroups(page, ['123456', '234567'], async () => {}), false);
  page.evaluate = async () => permission({ shutup_flag_me: true });
  await assert.rejects(checkGroups(page, ['123456'], async () => {}), { message: 'group_access_denied' });
});

test('saved state excludes other services, local storage, expired and malformed cookies', () => {
  const allowed = [cookie(), cookie({ name: 'p_skey', domain: '.qq.com' }), cookie({ name: 'ui', domain: '.docs.qq.com' })];
  const rejected = [
    cookie({ domain: 'qq.com' }), cookie({ domain: 'evil.qq.com' }), cookie({ domain: '.qq.com.evil.example' }),
    cookie({ domain: 'graph.qq.com' }), cookie({ domain: 'example.com' }), cookie({ expires: 999 }),
    cookie({ value: 'header\r\ninjection' }), cookie({ name: 'invalid name' }), cookie({ name: 'TOK\n' }), cookie({ path: '/\n' }),
    cookie({ secure: null }), cookie({ partitionKey: 'https://example.com' }),
  ];
  const result = filterBrowserState({ cookies: [...allowed, ...rejected], origins: [{ origin: 'https://docs.qq.com', localStorage: [{ name: 'private', value: 'discard' }] }] }, 1000);
  assert.deepEqual(result, { cookies: allowed, origins: [] });
  assert.throws(() => filterBrowserState({ cookies: rejected }, 1000));
});

test('status exposes only the fixed state contract and verified groups only after authorization', () => {
  assert.deepEqual(makeStatus(input(), 'waiting_scan', null, 1000), { nonce, state: 'waiting_scan', reason: null, updated_at: 1000 });
  assert.deepEqual(makeStatus(input(), 'authorized', null, 1000).verified_group_codes, ['123456', '234567']);
  assert.throws(() => makeStatus(input(), 'failed', 'remote cookie error'));
  assert.throws(() => makeStatus(input(), 'authorized', 'login_failed'));
  assert.throws(() => makeStatus(input(), 'failed'));
});

test('flow directory rejects broad permissions and symlinks', async (t) => {
  const directory = await privateDirectory(t);
  await checkFlowDirectory(directory);
  await chmod(directory, 0o755);
  await assert.rejects(checkFlowDirectory(directory));
  await chmod(directory, 0o700);
  await symlink(directory, join(directory, 'linked'));
  await assert.rejects(checkFlowDirectory(join(directory, 'linked')));
});

test('private reads reject shared mode, hardlinks, symlinks and oversized files', async (t) => {
  const directory = await privateDirectory(t); const path = join(directory, 'input.json');
  await writeFile(path, '{}', { mode: 0o600 });
  assert.equal((await readPrivateFile(path)).toString(), '{}');
  await chmod(path, 0o644); await assert.rejects(readPrivateFile(path)); await chmod(path, 0o600);
  await symlink(path, join(directory, 'linked')); await assert.rejects(readPrivateFile(join(directory, 'linked')));
  await link(path, join(directory, 'hardlinked')); await assert.rejects(readPrivateFile(path));
  await rm(join(directory, 'hardlinked'));
  await assert.rejects(readPrivateFile(path, 1));
});

test('atomic state updates retain 0600 and reject overwriting a symlink target', async (t) => {
  const directory = await privateDirectory(t);
  await writePrivateFile(directory, 'status.json', '{"state":"starting"}');
  await writePrivateFile(directory, 'status.json', '{"state":"waiting_scan"}');
  assert.equal((await lstat(join(directory, 'status.json'))).mode & 0o777, 0o600);
  assert.equal(JSON.parse(await readFile(join(directory, 'status.json'), 'utf8')).state, 'waiting_scan');
  await symlink(join(directory, 'status.json'), join(directory, 'browser-state.json'));
  await assert.rejects(writePrivateFile(directory, 'browser-state.json', 'overwrite'));
  assert.equal(JSON.parse(await readFile(join(directory, 'status.json'), 'utf8')).state, 'waiting_scan');
  await assert.rejects(writePrivateFile(directory, '../outside', 'bad'));
});

test('cancel marker must belong to this exact flow', async (t) => {
  const directory = await privateDirectory(t);
  assert.equal(await isCancelled(directory, nonce), false);
  await writeFile(join(directory, 'cancel'), 'b'.repeat(64), { mode: 0o600 });
  assert.equal(await isCancelled(directory, nonce), false);
  await writeFile(join(directory, 'cancel'), nonce);
  assert.equal(await isCancelled(directory, nonce), true);
});

test('pre-cancelled flow terminates without starting a browser or writing credentials', async (t) => {
  const directory = await privateDirectory(t);
  await writeFile(join(directory, 'input.json'), JSON.stringify(input(Math.floor(Date.now() / 1000))), { mode: 0o600 });
  await writeFile(join(directory, 'cancel'), nonce, { mode: 0o600 });
  assert.equal(await runLogin(directory, join(directory, 'missing-runtime.json')), false);
  const result = JSON.parse(await readFile(join(directory, 'status.json'), 'utf8'));
  assert.equal(result.state, 'failed'); assert.equal(result.reason, 'cancelled');
  await assert.rejects(lstat(join(directory, 'browser-state.json')), { code: 'ENOENT' });
});

test('expired flow reports expiry without reading runtime or starting a browser', async (t) => {
  const directory = await privateDirectory(t);
  await writeFile(join(directory, 'input.json'), JSON.stringify(input(1)), { mode: 0o600 });
  await assert.rejects(runLogin(directory, '/missing-runtime.json'), { message: 'login_expired' });
  assert.equal(JSON.parse(await readFile(join(directory, 'status.json'), 'utf8')).state, 'expired');
});
