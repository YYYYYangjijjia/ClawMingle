'use strict';
(() => {
  const $ = id => document.getElementById(id);
  let csrf = '', state = null, busy = false, initialized = false;
  function notice(message, error = false) {
    $('notice').textContent = message;
    $('notice').classList.toggle('error', error);
  }
  async function api(path, body) {
    const response = await fetch(path, {credentials: 'same-origin', cache: 'no-store',
      headers: body ? {'Content-Type': 'application/json', 'X-CSRF-Token': csrf} : {},
      method: body ? 'POST' : 'GET', body: body ? JSON.stringify(body) : undefined});
    if (response.status === 401) { location.replace('/login'); throw new Error('请重新登录。'); }
    const value = await response.json();
    if (!response.ok) throw new Error(value.error || '操作未完成，请稍后重试。');
    return value;
  }
  function render(value) {
    state = value;
    if (!value.setupRequired) { location.replace('/'); return; }
    if (!initialized) {
      const settings = {...value.defaults, ...value.settings};
      for (const [key, field] of Object.entries(settings)) {
        const input = $('settings-form').elements.namedItem(key);
        if (input && input.type === 'checkbox') input.checked = Boolean(field);
        else if (input) input.value = field;
      }
      initialized = true;
    }
    const registration = value.registration;
    const active = ['connecting', 'waiting'].includes(registration.state);
    $('registration-start').disabled = busy || !value.configured || active || !value.prerequisites.qqRegistrationAvailable;
    $('registration-cancel').disabled = busy || !active;
    $('manual-save').disabled = busy || !value.configured;
    $('finish').disabled = busy || registration.state !== 'matched' || !value.prerequisites.binaryAvailable;
    $('registration-command').hidden = !active;
    $('command').textContent = registration.command || '';
    $('expires').textContent = registration.expiresAt ? `本次口令有效至 ${new Date(registration.expiresAt * 1000).toLocaleTimeString()}；取消或重试后旧口令失效。` : '';
    $('registration-status').textContent = registration.message;
    $('matched').hidden = registration.state !== 'matched';
    $('matched').textContent = registration.state === 'matched' ? `群：${registration.group_openid} · 命令管理员：${registration.admin_openid}` : '';
    $('paths').textContent = `配置：${value.configPath}；数据：${value.dataPath}。`;
    $('credentials-note').textContent = value.configured ? `QQ 凭据已暂存；Jev ${value.credentials.jev ? '密钥已暂存' : '尚未设置'}。留空可保留已输入的密钥，重启管理台后需重新填写。` : '完成初始化前，凭据仅保存在本次管理进程中；重启管理台后需重新填写。';
    if (!value.prerequisites.binaryAvailable) notice('未找到 ClawMingle 程序，请先完成安装，再刷新本页。', true);
    else if (!value.prerequisites.qqRegistrationAvailable) notice('自动登记需要安装 QQ 连接依赖（scripts/requirements-qq-smoke.txt）；也可以手动填写 OpenID。', true);
  }
  async function action(body) {
    if (busy) return;
    busy = true;
    if (state) render(state);
    try {
      const next = await api('/api/setup', body);
      notice(body.action === 'finish' ? '初始化完成，正在进入管理台。' : '已更新。');
      render(next);
      return true;
    } catch (error) { notice(error.message, true); }
    finally { busy = false; if (state) render(state); }
  }
  $('settings-form').addEventListener('submit', async event => {
    event.preventDefault();
    const saved = await action({action: 'save-settings', ...Object.fromEntries(new FormData(event.target)),
      browser_enabled: event.target.elements.browser_enabled.checked});
    if (saved) {
      event.target.elements.app_secret.value = '';
      event.target.elements.jev_key.value = '';
    }
  });
  $('manual-form').addEventListener('submit', event => {
    event.preventDefault();
    action({action: 'group-manual', ...Object.fromEntries(new FormData(event.target))});
  });
  $('registration-start').addEventListener('click', () => action({action: 'registration-start'}));
  $('registration-cancel').addEventListener('click', () => action({action: 'registration-cancel'}));
  $('finish').addEventListener('click', () => action({action: 'finish'}));
  async function start() {
    try {
      const bootstrap = await api('/api/bootstrap');
      csrf = bootstrap.csrfToken;
      if (!bootstrap.setupRequired) { location.replace('/'); return; }
      render(await api('/api/setup'));
      notice('准备好了。连接设置仅在最后一步检查通过后写入本机私有文件。');
    } catch (error) { notice(error.message, true); }
  }
  setInterval(async () => {
    if (busy || !state || !['connecting', 'waiting'].includes(state.registration.state)) return;
    try { render(await api('/api/setup')); }
    catch (error) { notice(error.message, true); }
  }, 2000);
  start();
})();
