"""Authenticated first-run setup. No service control or personal auth import."""

import asyncio
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import re
import secrets
import shlex
import shutil
import stat
import subprocess
import tempfile
import threading
import time

import qq_smoke


class SetupError(Exception):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def require(condition, message, status=400):
    if not condition:
        raise SetupError(message, status)


def _exists(path):
    return os.path.lexists(path)


def _safe_path(path):
    require(path.is_absolute() and '..' not in path.parts, '初始化路径必须是绝对路径。')
    for item in [path, *path.parents]:
        require(not item.is_symlink(), '初始化路径不能经过符号链接。')
        require(not (item / '.git').exists(), '首次配置和数据目录必须位于 Git 仓库之外。')


def _identifier(value):
    require(isinstance(value, str) and re.fullmatch(r'[A-Za-z0-9_-]{1,256}', value)
            and not value.startswith('YOUR_'), '请填写平台 OpenID，不能填写占位值。')
    return value


def _secret(value):
    require(isinstance(value, str) and 0 < len(value) <= 8192
            and not any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in value), '凭据不能为空或包含空白和控制字符。')
    return value


def _new_file(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data.encode())
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        path.unlink(missing_ok=True)
        raise


def _env(values):
    return ''.join(key + '=' + shlex.quote(value) + '\n' for key, value in values.items())


def _toml(values):
    # This initializer only writes scalar strings, bools, and string lists.
    return ''.join(key + ' = ' + json.dumps(value, ensure_ascii=False) + '\n'
                   for key, value in values.items())


class Setup:
    def __init__(self, config, binary, assets, home=None):
        self.config_path = Path(config)
        self.binary = Path(binary)
        self.assets = Path(assets)
        self.home = Path(home or Path.home())
        self.data = self.home / '.local/share/clawmingle'
        self.mutex = threading.RLock()
        self.settings = None
        self.registration = {'state': 'idle', 'message': '尚未登记群聊。'}
        self.generation = 0
        self.probe_thread = None
        self.probe_loop = None
        self.probe_task = None
        _safe_path(self.config_path)
        _safe_path(self.data)

    @property
    def pending(self):
        return not _exists(self.config_path)

    def state(self):
        with self.mutex:
            try:
                websocket = importlib.util.find_spec('websockets.asyncio.client') is not None
            except (ImportError, ModuleNotFoundError):
                websocket = False
            return {'setupRequired': self.pending, 'configured': self.settings is not None,
                    'configPath': str(self.config_path), 'dataPath': str(self.data),
                    'defaults': {'model': 'gpt-6.1-sol', 'reasoning_effort': 'high',
                                 'codex_binary': shutil.which('codex') or '/opt/codex/current/bin/codex'},
                    'settings': ({key: self.settings[key] for key in ('app_id', 'model', 'reasoning_effort', 'codex_binary')} if self.settings else {}),
                    'credentials': {'qq': bool(self.settings), 'jev': bool(self.settings and self.settings['jev_key'])},
                    'prerequisites': {'qqRegistrationAvailable': websocket,
                                      'binaryAvailable': self.binary.is_file() and os.access(self.binary, os.X_OK)},
                    'registration': dict(self.registration)}

    def post(self, body):
        with self.mutex:
            require(self.pending, '已存在配置，不能重复初始化。', 409)
            require(isinstance(body, dict), '初始化请求无效。')
            action = body.get('action')
            if action == 'save-settings':
                self._settings(body)
            elif action == 'registration-start':
                self._start()
            elif action == 'registration-cancel':
                self._cancel()
            elif action == 'group-manual':
                require(self.settings is not None, '请先保存连接设置。')
                group, member = _identifier(body.get('group_openid')), _identifier(body.get('admin_openid'))
                self._cancel()
                self.registration = {'state': 'matched', 'source': 'manual', 'group_openid': group,
                                     'admin_openid': member, 'message': '已填写群和管理员 OpenID；它们尚未经过 QQ 在线验证。'}
            elif action == 'finish':
                self._finish()
            else:
                raise SetupError('未知的初始化操作。')
            return self.state()

    def _settings(self, body):
        require(set(body) <= {'action', 'app_id', 'app_secret', 'jev_key', 'model',
                              'reasoning_effort', 'codex_binary'}, '连接设置包含未知字段。')
        app_id = body.get('app_id')
        require(isinstance(app_id, str) and re.fullmatch(r'[0-9]{1,32}', app_id), 'QQ AppID 必须是平台提供的数字。')
        secret = body.get('app_secret')
        if not secret and self.settings and app_id == self.settings['app_id']:
            secret = self.settings['app_secret']
        secret = _secret(secret)
        jev = body.get('jev_key', '')
        if not jev and self.settings:
            jev = self.settings['jev_key']
        if jev:
            _secret(jev)
        require(isinstance(jev, str), 'Jev 密钥格式无效。')
        model = body.get('model', 'gpt-6.1-sol')
        require(isinstance(model, str) and re.fullmatch(r'[A-Za-z0-9._/-]{1,128}', model), '模型名称格式无效。')
        effort = body.get('reasoning_effort', 'high')
        require(isinstance(effort, str) and effort in {'low', 'medium', 'high', 'xhigh'}, '推理强度无效。')
        codex = body.get('codex_binary', shutil.which('codex') or '/opt/codex/current/bin/codex')
        require(isinstance(codex, str) and 0 < len(codex) <= 4096 and '\x00' not in codex
                and Path(codex).is_absolute(), '请填写 Codex 可执行文件的绝对路径。')
        registration = dict(self.registration) if self.settings and self.settings['app_id'] == app_id and self.settings['app_secret'] == secret and self.registration.get('state') == 'matched' else None
        self._cancel()
        self.settings = {'app_id': app_id, 'app_secret': secret, 'jev_key': jev,
                         'model': model, 'reasoning_effort': effort, 'codex_binary': codex}
        self.registration = registration or {'state': 'idle', 'message': '连接设置已暂存，请登记群聊。'}

    def _start(self):
        require(self.settings is not None, '请先保存连接设置。')
        require(self.state()['prerequisites']['qqRegistrationAvailable'],
                '自动登记需要在管理台的 Python 环境安装 scripts/requirements-qq-smoke.txt。也可以手动填写 OpenID。')
        require(not self.probe_thread or not self.probe_thread.is_alive(), '上一次登记连接尚未退出，请稍后再试。', 409)
        self.generation += 1
        generation = self.generation
        nonce = secrets.token_hex(16)
        self.registration = {'state': 'connecting', 'command': '/clawmingle-register ' + nonce,
                             'expiresAt': time.time() + 300, 'message': '正在连接 QQ，登记有效期为五分钟。'}
        credentials = {'QQ_APP_ID': self.settings['app_id'], 'QQ_APP_SECRET': self.settings['app_secret']}

        def update(**values):
            with self.mutex:
                if generation == self.generation:
                    self.registration.update(values)

        def matched(group, member):
            update(state='matched', source='qq', group_openid=group, admin_openid=member,
                   message='已确认群聊；发送登记口令的成员将成为命令管理员。')

        async def run():
            with self.mutex:
                if generation != self.generation:
                    return
                self.probe_loop = asyncio.get_running_loop()
                self.probe_task = asyncio.current_task()
            try:
                async with asyncio.timeout(300):
                    await qq_smoke.run_probe(credentials, 300, registration_nonce=nonce,
                                             registration_callback=matched,
                                             status_callback=lambda message: update(message=message))
            except asyncio.CancelledError:
                pass
            except TimeoutError:
                update(state='expired', message='五分钟内未完成登记，连接已关闭；可以重新开始。')
            except qq_smoke.ProbeError as error:
                update(state='error', message=str(error))
            except Exception:
                # Never expose HTTP details, event payloads, or credentials.
                update(state='error', message='登记连接未成功，未保存任何凭据或群设置。请检查依赖和网络后重试。')
            finally:
                with self.mutex:
                    self.probe_loop = self.probe_task = None

        self.probe_thread = threading.Thread(target=lambda: asyncio.run(run()), daemon=True,
                                             name='clawmingle-first-group')
        self.probe_thread.start()

    def _cancel(self):
        self.generation += 1
        if self.probe_loop is not None and self.probe_task is not None:
            self.probe_loop.call_soon_threadsafe(self.probe_task.cancel)
        self.registration = {'state': 'idle', 'message': '登记已取消；可以重新开始。'}

    def close(self):
        with self.mutex:
            self._cancel()
            self.settings = None

    def _finish(self):
        require(self.settings is not None and self.registration.get('state') == 'matched', '请先完成连接设置和群登记。')
        require(self.state()['prerequisites']['binaryAvailable'], '未找到可执行的 ClawMingle 程序，请先完成安装。')
        _safe_path(self.config_path)
        _safe_path(self.data)
        parent = self.config_path.parent
        require(parent.is_dir() and parent.stat().st_uid == os.getuid()
                and stat.S_IMODE(parent.stat().st_mode) == 0o700, '配置目录必须属于当前用户且权限为 0700。')
        qq_file, jev_file = parent / 'qq.env', parent / 'jev.env'
        require(not any(_exists(p) for p in [self.config_path, qq_file, jev_file, self.data]),
                '已存在配置、凭据或运行数据。初始化不会覆盖它们，请使用已有配置进入管理台。', 409)
        settings = self.settings
        config = _toml({'data_dir': str(self.data), 'qq_credentials_file': str(qq_file),
                        'allowed_groups': [self.registration['group_openid']],
                        'command_admins': [self.registration['admin_openid']],
                        'mode': 'mode-1', 'private_enabled': False})
        config += '\n[jev]\n' + _toml({'credentials_file': str(jev_file)})
        config += '\n[codex]\n' + _toml({'binary': settings['codex_binary'], 'model': settings['model'],
                                          'reasoning_effort': settings['reasoning_effort']})
        runtime = _toml({'model': settings['model'], 'model_reasoning_effort': settings['reasoning_effort'],
                         'web_search': 'live', 'cli_auth_credentials_store': 'file'})
        created_files, created_dirs = [], []
        # Validate a private candidate before publishing the config. The read-only
        # check command never starts QQ, a model, a service, or a browser.
        with tempfile.TemporaryDirectory(prefix='.setup-', dir=parent) as temporary:
            staged = Path(temporary) / 'social.toml'
            _new_file(staged, config)
            try:
                result = subprocess.run([str(self.binary), 'check', '--config', str(staged)],
                                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                        stderr=subprocess.DEVNULL, timeout=20, check=False)
            except (OSError, subprocess.TimeoutExpired):
                raise SetupError('配置检查未完成；未安装配置或启动机器人。', 502) from None
            require(result.returncode == 0, 'ClawMingle 配置检查未通过；未安装配置或启动机器人。', 400)
            # Serialize separate server instances without locking the bot's future
            # operator.lock. Never truncate any existing lock file.
            fd = os.open(parent / '.setup.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
            try:
                require(stat.S_ISREG(os.fstat(fd).st_mode) and os.fstat(fd).st_uid == os.getuid(), '初始化锁不可用。', 409)
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                require(not any(_exists(p) for p in [self.config_path, qq_file, jev_file, self.data]),
                        '初始化目标已存在，未覆盖。', 409)
                def mkdir(path):
                    if not path.exists():
                        mkdir(path.parent)
                        path.mkdir(mode=0o700)
                        created_dirs.append(path)
                def write(path, text):
                    _new_file(path, text)
                    created_files.append(path)
                try:
                    mkdir(self.data)
                    for relative in ('groups', 'private/groups', 'tasks', 'skills', 'runtime/codex', 'runtime/sqlite', 'runtime/workspace'):
                        mkdir(self.data / relative)
                    write(self.data / 'operator.lock', '')
                    write(self.data / 'runtime/codex/config.toml', runtime)
                    write(qq_file, _env({'QQ_APP_ID': settings['app_id'], 'QQ_APP_SECRET': settings['app_secret']}))
                    if settings['jev_key']:
                        write(jev_file, _env({'TYPESAFE_API_KEY': settings['jev_key']}))
                    # Hard-link is atomic and fails if another writer created the
                    # final config. It never replaces existing config contents.
                    os.link(staged, self.config_path)
                    created_files.append(self.config_path)
                    directory_fd = os.open(parent, os.O_RDONLY | os.O_DIRECTORY)
                    try:
                        os.fsync(directory_fd)
                    finally:
                        os.close(directory_fd)
                except Exception:
                    for path in reversed(created_files):
                        path.unlink(missing_ok=True)
                    for path in reversed(created_dirs):
                        try:
                            path.rmdir()
                        except OSError:
                            pass  # Preserve unexpected new files; never recursive-delete.
                    raise
            except BlockingIOError:
                raise SetupError('另一个初始化操作正在进行，请稍后再试。', 409) from None
            except OSError:
                raise SetupError('初始化文件无法保存，已撤回本次创建的文件；未覆盖已有数据。', 500) from None
            finally:
                os.close(fd)
        self.settings = None
        self.registration = {'state': 'complete', 'message': '配置已建立。请在管理台连接机器人的独立 Codex 账号，再按部署说明启动服务。'}
