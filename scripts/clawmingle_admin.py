"""Private loopback management service; local login or an authenticated proxy.

Standard library only. Does not expose an arbitrary command/file API. All bot
mutations stop an idle bot, acquire its operator lock, validate, and restart it.
"""
import argparse
import base64
from contextlib import contextmanager
import fcntl
import hashlib
import hmac
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path, PurePosixPath
import re
import secrets
import shlex
import signal
import shutil
import stat
import subprocess
import tempfile
import threading
import time
import tomllib
import urllib.parse
import urllib.request
import codex_capabilities
import admin_auth

UNIT = 'clawmingle.service'
MODES = {'mode-1', 'mode-2', 'mode-3', 'mode-all'}
EFFORTS = {'low', 'medium', 'high', 'xhigh'}
ID = re.compile(r'[A-Za-z0-9_-]{1,256}\Z')
NAME = re.compile(r'[a-z0-9][a-z0-9_-]{0,31}\Z')
TOOLS = ['workspace_exec', 'time_now', 'weather_current', 'public_web_read', 'message_lookup', 'messages_list', 'tasks_manage', 'qq_info', 'group_documents', 'memory_manage', 'task_update', 'public_browser', 'plugin_tools', 'plugin_call', 'sticker_lookup', 'sticker_manage', 'identity_manage', 'group_profile_manage']
GROUP_PROFILE_FIELDS = ('purpose', 'role', 'style', 'boundaries')


def profile_fields(value):
    require(isinstance(value, dict) and set(value) == set(GROUP_PROFILE_FIELDS), '群档案需要用途、角色、风格和边界四项。')
    require(all(isinstance(v, str) for v in value.values()), '群档案各项必须为文字。')
    fields = {k: value[k].strip() for k in GROUP_PROFILE_FIELDS}
    require(all(len(v) <= 600 and not any((ord(c) < 32 and c not in '\n\t')
                or 127 <= ord(c) <= 159 or 0x202a <= ord(c) <= 0x202e
                or 0x2066 <= ord(c) <= 0x2069 for c in v) for v in fields.values())
            and sum(map(len, fields.values())) <= 2000, '群档案每项最多 600 字，合计最多 2000 字，不能包含控制字符。')
    return fields


def public_group_profile(state):
    profile = state.get('group_profile') or {}
    return {'fields': {k: profile.get('fields', {}).get(k, '') for k in GROUP_PROFILE_FIELDS},
            'revision': str(profile.get('revision', 0)), 'updated_at_ms': profile.get('updated_at_ms', 0)}


MEMORY_DEFAULTS = {'enabled': True, 'automatic_capture': True, 'archive_hour': 4,
                   'archive_minute': 0, 'archive_every_days': 1, 'short_term_days': 14}
INTERACTION_DEFAULTS = {'enabled': True, 'long_timeout_seconds': 0, 'tool_calls': 48,
                        'long_max_bubbles': 4, 'long_bubble_chars': 2000,
                        'feedback_mode': 'standard', 'ack_delay_seconds': 5, 'heartbeat_interval_seconds': 90,
                        'max_progress_messages': 2, 'progress_interval_seconds': 45, 'workspace_timeout_seconds': 120}
STICKER_DEFAULTS = {'enabled': True, 'automatic': True, 'auto_collect': True, 'cooldown_seconds': 0, 'usage_instructions': ''}
TINYFISH_ENDPOINT = 'https://agent.tinyfish.ai/mcp'
TINYFISH_TOOLS = ['search', 'fetch_content']
TIKHUB_TOOLS = {
    'xiaohongshu': ['xiaohongshu_app_v2_' + name for name in (
        'search_notes', 'search_images', 'search_users', 'get_user_info', 'get_user_posted_notes',
        'get_image_note_detail', 'get_video_note_detail', 'get_note_comments', 'get_note_sub_comments')],
    'weibo': ['weibo_web_v2_' + name for name in (
        'fetch_hot_search', 'fetch_advanced_search', 'fetch_realtime_search', 'fetch_user_search',
        'fetch_post_detail', 'fetch_post_comments', 'fetch_post_sub_comments', 'fetch_user_info', 'fetch_user_posts')],
    'twitter': ['twitter_web_' + name for name in (
        'fetch_search_timeline', 'fetch_tweet_detail', 'fetch_post_comments', 'fetch_user_profile',
        'fetch_user_post_tweet', 'fetch_user_media', 'fetch_user_tweet_replies', 'fetch_trending')],
}
TIKHUB_NAMES = {'xiaohongshu': '小红书', 'weibo': '微博', 'twitter': 'Twitter / X'}


class AdminError(Exception):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def require(condition, message='提交内容无效。', status=400):
    if not condition:
        raise AdminError(message, status)


def private_read(path, limit=65536):
    path = Path(path)
    parent = path.parent.lstat()
    require(stat.S_ISDIR(parent.st_mode) and parent.st_uid == os.getuid()
            and stat.S_IMODE(parent.st_mode) == 0o700, '私有目录权限异常。', 500)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, 'rb') as file:
        info = os.fstat(file.fileno())
        require(stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o600 and info.st_size <= limit,
                '私有文件权限或大小异常。', 500)
        data = file.read(limit + 1)
    require(len(data) <= limit, '文件超过大小限制。', 500)
    return data


def atomic_write(path, data):
    path = Path(path)
    parent = path.parent.lstat()
    require(stat.S_ISDIR(parent.st_mode) and parent.st_uid == os.getuid()
            and stat.S_IMODE(parent.st_mode) == 0o700, '私有目录权限异常。', 500)
    require(not path.is_symlink(), '私有文件不能是链接。', 500)
    if path.exists():
        private_read(path, 64 * 1024 * 1024)
    fd, temporary = tempfile.mkstemp(prefix='.admin-', dir=path.parent)
    try:
        with os.fdopen(fd, 'wb') as file:
            file.write(data if isinstance(data, bytes) else data.encode())
            file.flush()
            os.fsync(file.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def toml_dump(data):
    """Serialize this application's bounded TOML config without new dependencies."""
    def value(item):
        if isinstance(item, str):
            return json.dumps(item, ensure_ascii=False)
        if type(item) is bool:
            return 'true' if item else 'false'
        if type(item) is int:
            return str(item)
        if isinstance(item, list):
            return '[' + ', '.join(value(i) for i in item) + ']'
        raise AdminError('配置包含不支持的数据类型。')
    lines = []
    def table(mapping, parts):
        if parts:
            lines.append('[' + '.'.join(json.dumps(p) for p in parts) + ']')
        for key, item in mapping.items():
            if not isinstance(item, dict):
                lines.append(json.dumps(key) + ' = ' + value(item))
        lines.append('')
        for key, item in mapping.items():
            if isinstance(item, dict):
                table(item, parts + [key])
    table(data, [])
    output = '\n'.join(lines)
    require(tomllib.loads(output) == data, '配置校验失败。')
    return output


def env_read(path):
    if not Path(path).exists():
        return {}
    result = {}
    for line in private_read(path).decode().splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        key, raw = line.split('=', 1)
        tokens = shlex.split(raw)
        require(re.fullmatch(r'[A-Z][A-Z0-9_]*', key) and len(tokens) == 1
                and key not in result, '凭据格式异常。', 500)
        result[key] = tokens[0]
    return result


def env_dump(values):
    return ''.join(key + '=' + shlex.quote(value) + '\n' for key, value in values.items())


def safe_secret(value):
    require(isinstance(value, str) and 0 < len(value) <= 8192
            and not any(c.isspace() or c == '\0' for c in value), '凭据格式不正确。')
    return value


def identifier(value):
    require(isinstance(value, str) and ID.fullmatch(value), '群或成员标识格式不正确。')
    return value


def alias(value):
    require(isinstance(value, str) and NAME.fullmatch(value), '别名须为小写字母、数字、短横线或下划线。')
    return value


class Manager:
    def __init__(self, config, binary, assets, home=None):
        self.config_path = Path(config)
        self.binary = Path(binary)
        self.assets = Path(assets)
        self.home = Path(home or Path.home())
        self.mutex = threading.RLock()
        self.checks = {}
        self.device = None
        self.device_output = ''
        self.capability_sources = {}
        self.capabilities_refreshed = 0
        self.hosted_capabilities = []
        self.hosted_capabilities_notice = '账号应用尚未读取；点击“刷新账号应用目录”后检查。'
        self.capability_job_lock = threading.Lock()
        self.capability_job = {'state': 'idle'}
        self.qq_document_login = None
        self.load()

    def load(self):
        self.config = tomllib.loads(private_read(self.config_path).decode())
        self.data = Path(self.config.get('data_dir', str(self.home / '.local/share/clawmingle'))).expanduser()
        self.runtime = self.data / 'runtime/codex'
        return self.config

    def path(self, key, default):
        return Path(self.config.get(key, default)).expanduser()

    def run(self, args, timeout=30):
        result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                timeout=timeout, check=False)
        require(result.returncode == 0, '操作未成功，请检查配置或服务状态。', 502)
        return result.stdout

    def service(self):
        raw = self.run(['systemctl', '--user', 'show', UNIT, '-p', 'ActiveState',
                        '-p', 'SubState', '-p', 'NRestarts', '-p', 'MemoryCurrent'], 10).decode()
        fields = dict(line.split('=', 1) for line in raw.splitlines() if '=' in line)
        number = lambda key: int(fields.get(key, '0')) if fields.get(key, '').isdigit() else 0
        return {'active': fields.get('ActiveState', 'unknown'),
                'substate': fields.get('SubState', 'unknown'),
                'restarts': number('NRestarts'), 'memory_bytes': number('MemoryCurrent')}

    def control(self, action):
        require(action in {'start', 'stop', 'restart'}, '服务操作无效。')
        self.run(['systemctl', '--user', action, UNIT], 30)
        return {'ok': True, 'service': self.service()}

    def documents(self, private=False):
        folder = self.data / ('private/groups' if private else 'groups')
        if not folder.exists():
            return []
        results = []
        for path in sorted(folder.glob('*.json'))[:64]:
            identifier(path.stem)
            stored = json.loads(private_read(path, 64 * 1024 * 1024))
            require(stored.get('group_id') == path.stem, '会话标识不一致。', 500)
            results.append((path, stored))
        return results

    def idle(self):
        for scope in (False, True):
            for _, document in self.documents(scope):
                state = document['state']
                if any(m.get('status') in {'pending', 'running'} for m in state.get('messages', [])):
                    return False
                if any(r.get('status') in {'queued', 'generating', 'ready', 'sending'}
                       for r in state.get('requests', {}).values()):
                    return False
        task_dir = self.data / 'tasks'
        if task_dir.exists():
            require(not task_dir.is_symlink() and task_dir.is_dir(), '任务目录不可用。', 500)
            for path in task_dir.glob('*.json'):
                task_state = json.loads(private_read(path, 4 * 1024 * 1024))
                if any(run.get('status') in {'running', 'sending'}
                       for run in task_state.get('runs', {}).values()):
                    return False
        return True

    @contextmanager
    def transaction(self, hold_lock=True, restart=True):
        with self.mutex:
            running = self.service()['active'] in {'active', 'activating'}
            require(self.idle(), '机器人正在处理消息，请稍后再保存。', 409)
            lock = None
            previous = {}
            committed = False
            try:
                if running:
                    self.control('stop')
                lock = os.open(self.data / 'operator.lock', os.O_RDWR | os.O_NOFOLLOW)
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    raise AdminError('机器人仍在运行，未修改配置。', 409) from None
                require(self.idle(), '刚收到新消息，请等待处理完成。', 409)
                if not hold_lock:
                    os.close(lock)
                    lock = None
                def save(path, content):
                    path = Path(path)
                    require(not path.is_symlink(), '私有文件不能是链接。', 500)
                    if path not in previous:
                        previous[path] = private_read(path, 64 * 1024 * 1024) if path.exists() else None
                    if content is None:
                        path.unlink(missing_ok=True)
                    else:
                        atomic_write(path, content)
                yield save
                self.run([str(self.binary), 'check', '--config', str(self.config_path)], 15)
                committed = True
            except Exception:
                for path, data in previous.items():
                    if data is None:
                        path.unlink(missing_ok=True)
                    else:
                        atomic_write(path, data)
                raise
            finally:
                if lock is not None:
                    os.close(lock)
                self.load()
                if running and (restart or not committed):
                    self.control('start')

    def login_state(self):
        auth = self.runtime / 'auth.json'
        state = {'authenticated': False, 'kind': 'none',
                 'personal_available': (self.home / '.codex/auth.json').is_file()}
        if auth.exists():
            value = json.loads(private_read(auth))
            state['authenticated'] = bool(value.get('OPENAI_API_KEY') or value.get('tokens', {}).get('access_token'))
            state['kind'] = 'api_key' if value.get('OPENAI_API_KEY') else 'chatgpt'
        if self.device:
            state['device'] = self.device_state()
        return state

    def skill_list(self):
        skills = []
        scopes = {}
        for private in (False, True):
            for index, (_, stored) in enumerate(self.documents(private), 1):
                for selected in stored['state'].get('enabled_skills', []):
                    scopes.setdefault(selected, []).append(('私聊' if private else '群聊') + str(index))
        directory = self.data / 'skills'
        if directory.exists():
            for folder in sorted(directory.iterdir()):
                if not NAME.fullmatch(folder.name) or not folder.is_dir() or folder.is_symlink():
                    continue
                path = folder / 'SKILL.md'
                if not path.exists():
                    continue
                raw = private_read(path, 16384).decode()
                if raw == '---\ndescription: 已移除\n---\n':
                    continue
                lines = raw.splitlines()
                end = lines.index('---', 1) if lines and lines[0] == '---' and '---' in lines[1:] else -1
                description = next((line[len('description: '):] for line in lines[1:end] if line.startswith('description: ')), '')
                if description.startswith(chr(34)):
                    description = json.loads(description)
                companions = any(item.name != 'SKILL.md' for item in folder.iterdir())
                package = None
                if companions:
                    package = {'valid': False, 'source': '', 'commit': '', 'files': 0}
                    try:
                        manifest = json.loads(private_read(folder / '.clawmingle-package.json', 32768))
                        source, commit = manifest.get('source'), manifest.get('commit')
                        require(manifest.get('schema_version') == 1 and manifest.get('name') == folder.name
                                and isinstance(source, str) and len(source) <= 2048
                                and source.startswith('https://raw.githubusercontent.com/')
                                and not any(c in source for c in ('?', '#', '@'))
                                and isinstance(commit, str) and re.fullmatch(r'[0-9a-fA-F]{40}', commit)
                                and isinstance(manifest.get('files'), list) and len(manifest['files']) <= 64,
                                '技能包版本记录无效。')
                        package = {'valid': True, 'source': source, 'commit': commit,
                                   'files': len(manifest['files'])}
                    except (AdminError, OSError, ValueError, TypeError):
                        pass
                skills.append({'name': folder.name, 'description': description,
                               'instructions': '\n'.join(lines[end + 1:]) if end >= 1 else raw,
                               'enabled': folder.name in self.config.get('enabled_skills', []),
                               'read_only': companions, 'package': package,
                               'scopes': scopes.get(folder.name, [])})
                if len(skills) == 32:
                    break
        return skills

    def behavior_settings(self):
        return json.loads(self.run([str(self.binary), 'behavior', '--config', str(self.config_path)], 15))

    @staticmethod
    def validate_behavior(body):
        require(set(body) == {'section', 'behavior', 'context_windows', 'max_bubbles', 'max_bubble_chars'})
        behavior = body['behavior']
        require(isinstance(behavior, dict) and set(behavior) == {'response_instructions', 'formatting_instructions', 'categories', 'modes'})
        def prompt(value):
            return isinstance(value, str) and bool(value.strip()) and len(value) <= 2000 and '\0' not in value
        def name(value):
            return isinstance(value, str) and bool(value.strip()) and len(value) <= 40 and not any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value)
        require(prompt(behavior['response_instructions']) and prompt(behavior['formatting_instructions']), '全局表达与排版规则须为 1–2000 字。')
        categories = behavior['categories']
        require(isinstance(categories, dict) and set(categories) == {'casual', 'factual', 'explanation', 'comparison', 'troubleshooting', 'task', 'emotional', 'clarification'})
        for category in categories.values():
            require(isinstance(category, dict) and set(category) == {'name', 'instructions'}
                    and name(category['name']) and prompt(category['instructions']), '分类名称最多 40 字，规则须为 1–2000 字。')
        modes = behavior['modes']
        require(isinstance(modes, dict) and set(modes) == MODES)
        for key, mode in modes.items():
            require(isinstance(mode, dict) and {'name', 'participation_instructions'} <= set(mode)
                    and set(mode) <= {'name', 'participation_instructions', 'reply_instructions'}
                    and name(mode['name']), '模式名称或参与规则不正确。')
            legacy = mode.get('reply_instructions', '')
            require(isinstance(legacy, str) and len(legacy) <= 2000 and '\0' not in legacy,
                    '旧版回复文案须为最多 2000 字的文字，仅保留作存档。')
            require(prompt(mode['participation_instructions']) if key in {'mode-2', 'mode-3'} else mode['participation_instructions'] == '',
                    '仅中间两种模式支持 Jev 判断提示；仅 @ 和全部回应的路由规则固定。')
        require(len(json.dumps(behavior, ensure_ascii=False, separators=(',', ':')).encode()) <= 24000, '表达规则总长度超过 24 KB。')
        for key, maximum in [('max_bubbles', 3), ('max_bubble_chars', 2000)]:
            require(type(body[key]) is int and 1 <= body[key] <= maximum, '消息长度或气泡数量超出范围。')
        windows = body['context_windows']
        bounds = {'jev_messages': 40, 'codex_messages': 80, 'jev_bot_messages': 20, 'codex_bot_messages': 40}
        require(isinstance(windows, dict) and set(windows) == set(bounds))
        require(all(type(windows[k]) is int and 1 <= windows[k] <= maximum for k, maximum in bounds.items()), '上下文窗口超出范围。')

    @staticmethod
    def validate_memory_settings(settings):
        require(isinstance(settings, dict) and set(settings) == set(MEMORY_DEFAULTS), '记忆设置字段不完整或包含不支持的选项。')
        require(all(type(settings[key]) is bool for key in ('enabled', 'automatic_capture')), '记忆开关必须为布尔值。')
        for key, low, high in [('archive_hour', 0, 23), ('archive_minute', 0, 59),
                               ('archive_every_days', 1, 30), ('short_term_days', 1, 90)]:
            require(type(settings[key]) is int and low <= settings[key] <= high, '记忆归档时间、周期或短期有效天数超出范围。')

    @staticmethod
    def validate_interactions(settings):
        require(isinstance(settings, dict) and set(settings) == set(INTERACTION_DEFAULTS), '长任务设置字段不完整。')
        require(type(settings['enabled']) is bool, '进展交互开关格式不正确。')
        require(isinstance(settings['feedback_mode'], str) and settings['feedback_mode'] in {'quiet', 'standard', 'detailed'}, '请选择安静、标准或详细反馈。')
        require(type(settings['ack_delay_seconds']) is int and 1 <= settings['ack_delay_seconds'] <= 30, '接收确认等待时间须为 1–30 秒。')
        heartbeat = settings['heartbeat_interval_seconds']
        require(type(heartbeat) is int and (heartbeat == 0 or 30 <= heartbeat <= 1800), '状态提醒间隔须为 30–1800 秒，或 0 表示关闭。')
        for key, low, high in [('long_timeout_seconds', 60, 3600), ('tool_calls', 1, 128),
                               ('long_max_bubbles', 1, 8), ('long_bubble_chars', 100, 2000),
                               ('max_progress_messages', 1, 3), ('progress_interval_seconds', 10, 300), ('workspace_timeout_seconds', 1, 300)]:
            require(type(settings[key]) is int and (low <= settings[key] <= high or (key == 'long_timeout_seconds' and settings[key] == 0)), '长任务预算超出允许范围（总时限 0 表示不限）。')

    def integrations_public(self):
        config = self.config.get('integrations', {})
        browser = config.get('browser', {})
        browser_ready = bool(browser.get('runtime'))
        if not browser_ready:
            try:
                manifest = json.loads(private_read(self.data / 'runtime/native-tools.json', 4096))
                browser_ready = type(manifest.get('version')) is int and manifest['version'] == 1 and manifest.get('browser_ready') is True
            except (AdminError, OSError, ValueError, TypeError, AttributeError):
                pass
        return {'browser': {'enabled': browser.get('enabled', True),
                            'configured': bool(browser.get('enabled', True) and browser_ready)},
                'plugins': [{'id': key, 'name': value.get('name', key),
                             'tools': value.get('allowed_tools', []), 'read_only': True}
                            for key, value in config.get('plugins', {}).items() if value.get('enabled', False)]}

    def capability_busy(self):
        with self.capability_job_lock:
            return self.capability_job['state'] == 'running'

    def capability_job_status(self):
        with self.capability_job_lock:
            return dict(self.capability_job)

    def start_capability_refresh(self):
        with self.capability_job_lock:
            if self.capability_job['state'] == 'running':
                return dict(self.capability_job)
            self.capability_job = {'state': 'running', 'id': secrets.token_hex(8)}
        def work():
            status = {'state': 'completed', 'message': '账号应用目录已刷新。'}
            try:
                # The CLI takes operator.lock itself. Stop an idle bot, release
                # our lock before the child, and restore it in transaction.finally.
                with self.transaction(hold_lock=False):
                    raw = self.run([str(self.binary), 'capability-catalog', '--config', str(self.config_path)], 100)
                    require(len(raw) <= 1024 * 1024, 'Codex 能力目录过大。', 502)
                    hosted = codex_capabilities.discover_apps(json.loads(raw))
                with self.mutex:
                    self.hosted_capabilities = hosted
                    self.hosted_capabilities_notice = ''
                    status['catalog'] = self.capability_catalog()
            except Exception as error:
                message = str(error) if isinstance(error, AdminError) and error.status == 409 else '账号应用目录未能读取，请检查机器人服务、独立 Codex 登录与应用授权后重试。'
                status = {'state': 'failed', 'message': message}
                with self.mutex:
                    self.hosted_capabilities_notice = message
                    for record in self.hosted_capabilities:
                        record.update(available=False, reason='账号应用目录尚未重新验证，请刷新后再启用。')
                    for record in self.capability_sources.values():
                        if record['kind'] == 'app':
                            record.update(available=False, reason='账号应用目录尚未重新验证，请刷新后再启用。')
            finally:
                with self.capability_job_lock:
                    self.capability_job.update(status)
        worker = threading.Thread(target=work, name='codex-capability-catalog', daemon=True)
        try:
            worker.start()
        except Exception:
            with self.capability_job_lock:
                self.capability_job = {'state': 'failed', 'message': '未能开始刷新，请重试。'}
            raise
        return self.capability_job_status()

    def capability_catalog(self):
        self.load()
        records, notices = codex_capabilities.discover_local(self.home)
        records.extend(dict(record) for record in self.hosted_capabilities)
        if self.hosted_capabilities_notice:
            notices.append(self.hosted_capabilities_notice)
        plugins = self.config.get('integrations', {}).get('plugins', {})
        for record in records:
            if record['kind'] == 'app':
                existing = next((key for key, plugin in plugins.items()
                                 if plugin.get('app_id') == record.get('_app_id') and plugin.get('codex_app')), None)
                if existing:
                    record['registration_id'] = existing
        skills = {skill['name']: skill for skill in self.skill_list()}
        hooks = next((record for record in records if record['kind'] == 'hooks'), None)
        if hooks is None and any(os.path.lexists(path) for path in self.hooks_paths()):
            hooks = codex_capabilities.hooks_record()
            records.append(hooks)
        if hooks is not None:
            try:
                managed = self.managed_hooks()
                if managed:
                    hooks.update(registered=True, enabled=managed['enabled'], available=True, reason='')
            except (OSError, ValueError, AdminError, RecursionError):
                hooks.update(available=False, reason='机器人已有同名或被手工修改的 Hooks 文件，未覆盖；请先由管理员核对。')
        self.capability_sources = {record['id']: record for record in records}
        self.capabilities_refreshed = time.monotonic()
        for record in records:
            if record['kind'] == 'hooks':
                continue
            registered = skills.get(record['registration_id']) if record['kind'] == 'skill' else plugins.get(record['registration_id'])
            if registered is not None:
                record.update(registered=True, enabled=bool(registered.get('enabled', False)))
                if record['kind'] != 'skill':
                    record['tools'] = [{'name': name, 'description': ''} for name in registered.get('allowed_tools', [])]
        # Keep registrations manageable when the original local source disappears.
        known = {record['registration_id'] for record in records if record['kind'] in {'app', 'mcp'}}
        for plugin_id, plugin in plugins.items():
            if plugin_id in known:
                continue
            record = {'id': 'registered-' + plugin_id, 'kind': 'app' if plugin.get('codex_app') else 'mcp',
                      'name': plugin.get('name', plugin_id), 'description': '已登记的连接；会话授权独立保留。',
                      'personal_account': plugin.get('personal_account', bool(plugin.get('codex_app') or plugin.get('credential_file'))),
                      'available': True, 'reason': '', 'registration_id': plugin_id,
                      'registered': True, 'enabled': bool(plugin.get('enabled', False)),
                      'tools': [{'name': name, 'description': ''} for name in plugin.get('allowed_tools', [])]}
            records.append(record)
            self.capability_sources[record['id']] = record
        return {'capabilities': [codex_capabilities.public_record(record) for record in records],
                'notices': notices,
                'native': {'name': 'Codex 通用工作环境',
                           'description': '启动时自动准备原生命令、文件、Skills、多代理和浏览器环境；Hooks 可选导入，搜索与浏览器遵循各自开关。'}}

    def hooks_paths(self):
        return tuple(self.runtime / name for name in ('hooks.json', 'hooks.disabled.json', 'hooks.import.json'))

    def managed_hooks(self):
        active, disabled, marker = self.hooks_paths()
        present = [os.path.lexists(path) for path in (active, disabled, marker)]
        if not any(present):
            return None
        require(present[2] and present[0] != present[1], '已有同名 Hooks 文件，未覆盖原内容。', 409)
        for path, exists in zip((active, disabled, marker), present):
            if exists:
                info = path.lstat()
                require(stat.S_ISREG(info.st_mode) and info.st_nlink == 1,
                        'Hooks 文件不能是链接或特殊文件。', 409)
        metadata = json.loads(private_read(marker, 2048))
        require(isinstance(metadata, dict) and set(metadata) == {'version', 'source_id', 'sha256'}
                and type(metadata['version']) is int and metadata['version'] == 1
                and metadata['source_id'] == codex_capabilities.hooks_record()['id']
                and isinstance(metadata['sha256'], str) and re.fullmatch(r'[a-f0-9]{64}', metadata['sha256']),
                'Hooks 管理记录不匹配，未修改。', 409)
        raw = private_read(active if present[0] else disabled, codex_capabilities.HOOKS_LIMIT)
        require(hashlib.sha256(raw).hexdigest() == metadata['sha256'], 'Hooks 内容已被手工修改，未覆盖。', 409)
        codex_capabilities.hooks_document(raw)
        return {'enabled': present[0], 'raw': raw}

    def hooks_action(self, body, record):
        require(set(body) == {'id', 'action'} and body['action'] in {'import', 'enable', 'disable'})
        action = body['action']
        with self.transaction() as save:
            self.load()
            try:
                managed = self.managed_hooks()
            except (OSError, ValueError, RecursionError):
                raise AdminError('Hooks 文件无法安全校验，未修改。', 409) from None
            active, disabled, marker = self.hooks_paths()
            if action == 'import':
                require(managed is None, 'Hooks 已导入，请使用启用或停用。', 409)
                require(record['available'] and '_path' in record, record['reason'] or 'Hooks 暂不可导入。')
                try:
                    raw = codex_capabilities.hooks_read(record['_path'])
                except (OSError, ValueError, UnicodeError, RecursionError):
                    raise AdminError('本机 Hooks 无法安全读取，请刷新后重试。', 409) from None
                require(hashlib.sha256(raw).hexdigest() == record['_digest'], '本机 Hooks 已变化，请刷新后重新选择。', 409)
                save(disabled, raw)
                save(marker, json.dumps({'version': 1, 'source_id': record['id'], 'sha256': record['_digest']}) + '\n')
            else:
                require(managed is not None, 'Hooks 尚未导入。', 409)
                enabled = action == 'enable'
                if managed['enabled'] != enabled:
                    save(active if enabled else disabled, managed['raw'])
                    save(disabled if enabled else active, None)
        record.update(registered=True, enabled=action == 'enable', available=True, reason='')
        return {'ok': True, 'message': 'Hooks 已导入并保持停用；请核对命令依赖后选择启用。' if action == 'import'
                else 'Hooks 已启用，将用于后续原生任务。' if action == 'enable' else 'Hooks 已停用，原内容已保留。'}

    def capability_action(self, body):
        require(set(body) <= {'id', 'action', 'allowed_tools'})
        require(isinstance(body.get('id'), str) and body.get('action') in {'import', 'enable', 'disable', 'check'})
        require(time.monotonic() - self.capabilities_refreshed <= 600, '请先刷新能力目录。', 409)
        record = self.capability_sources.get(body['id'])
        require(record is not None, '能力目录已变化，请重新刷新。', 409)
        action = body['action']
        plugin_id = record['registration_id']
        if record['kind'] == 'hooks':
            return self.hooks_action(body, record)
        if action == 'check':
            require(record['kind'] != 'skill' and record['registered'], '请先导入此连接。')
            with self.transaction(hold_lock=False):
                plugin = self.load().get('integrations', {}).get('plugins', {}).get(plugin_id)
                require(plugin and plugin.get('enabled'), '请先启用此连接。')
                result = json.loads(self.run([str(self.binary), 'integration-check', '--plugin', plugin_id,
                                             '--config', str(self.config_path)], 50))
                require(result.get('ok') is True and type(result.get('tool_count')) is int and result['tool_count'] > 0,
                        '连接未通过，请核对授权或允许的工具。', 502)
            return {'ok': True, 'message': '允许的工具目录验证通过，未执行内容查询。'}
        if record['kind'] == 'skill':
            require(action == 'import' and not record['registered'], '已导入的 Skill 请在 Skills 编辑器中启停。')
            require(record['available'], record['reason'] or '此 Skill 暂不支持导入。')
            raw = codex_capabilities.source_read(record['_path'], 16384)
            require(hashlib.sha256(raw).hexdigest() == record['_digest'], '本机 Skill 已变化，请刷新后重新选择。', 409)
            with self.transaction() as save:
                cfg = self.load()
                require(len(self.skill_list()) < 32, '最多管理 32 个 Skill。')
                folder = self.data / 'skills' / alias(plugin_id)
                require(not folder.is_symlink() and (not folder.exists() or (folder.is_dir() and not any(folder.iterdir()))),
                        '已有同名 Skill，未覆盖原内容。', 409)
                require(not folder.parent.is_symlink(), 'Skill 根目录不能是链接。')
                folder.parent.mkdir(mode=0o700, exist_ok=True)
                folder.mkdir(mode=0o700, exist_ok=True)
                save(folder / 'SKILL.md', raw)
                save(self.config_path, toml_dump(cfg))
            record.update(registered=True, enabled=False)
            return {'ok': True, 'message': 'Skill 已导入并保持停用，请在 Skills 中选择启用。'}
        with self.transaction() as save:
            cfg = self.load()
            plugins = cfg.setdefault('integrations', {}).setdefault('plugins', {})
            if action == 'import':
                require(record['available'], record['reason'] or '此能力暂不支持导入。')
                require(plugin_id not in plugins, '此能力已导入，请使用启用或停用。', 409)
                tools = body.get('allowed_tools')
                require(isinstance(tools, list) and 1 <= len(tools) <= 32
                        and all(isinstance(name, str) and codex_capabilities.TOOL.fullmatch(name) for name in tools)
                        and len(set(tools)) == len(tools), '请选择 1–32 个不重复的工具名称。')
                plugin = {'enabled': False, 'name': record['name'], 'allowed_tools': tools,
                          'personal_account': record['personal_account']}
                if record['kind'] == 'app':
                    require(set(tools) <= {tool['name'] for tool in record['tools']}, '所选工具不在账号应用目录中。')
                    plugin.update(codex_app=True, app_id=record['_app_id'], endpoint='https://chatgpt.com/')
                    if record['_namespace'] is not None:
                        plugin['tool_namespace'] = record['_namespace']
                else:
                    current, _ = codex_capabilities.discover_local(self.home)
                    source = next((item for item in current if item['id'] == record['id']), None)
                    require(source and source['available'] and source['_server'] == record['_server'],
                            '本机 MCP 配置已变化，请刷新后重新选择。', 409)
                    server = source['_server']
                    plugin['endpoint'] = server['url']
                    credential = server.get('http_headers', {}).get('Authorization', '')[7:]
                    if server.get('bearer_token_env_var'):
                        credential = os.environ.get(server['bearer_token_env_var'], '')
                    if credential:
                        secret = self.config_path.parent / (plugin_id + '.key')
                        save(secret, safe_secret(credential) + '\n')
                        plugin['credential_file'] = str(secret)
                plugins[plugin_id] = plugin
            else:
                require(plugin_id in plugins, '此连接尚未导入。', 409)
                require(action != 'enable' or record['available'], record['reason'] or '应用当前没有可用工具，请刷新账号应用目录并检查授权。')
                plugins[plugin_id]['enabled'] = action == 'enable'
            save(self.config_path, toml_dump(cfg))
        record.update(registered=True, enabled=plugins[plugin_id]['enabled'])
        record['tools'] = [{'name': name, 'description': ''} for name in plugins[plugin_id]['allowed_tools']]
        return {'ok': True, 'message': '连接已导入并保持停用；启用后，在会话管理中选择授权。' if action == 'import'
                else '连接已启用，会话授权独立设置。' if action == 'enable' else '连接已停用，已有会话授权保留。'}

    def codex_tikhub_source(self):
        # Import only these known public endpoints and the existing dedicated
        # TikHub key. Never execute arbitrary header helpers from Codex config.
        config = tomllib.loads(private_read(self.home / '.codex/config.toml', 1024 * 1024).decode())
        servers = config.get('mcp_servers', {})
        for platform in TIKHUB_TOOLS:
            server = servers.get('tikhub-' + platform, {})
            require(server.get('enabled', True) and server.get('url') == f'https://mcp.tikhub.io/{platform}/mcp'
                    and server.get('http_headers_helper') == 'python3 ' + str(self.home / '.codex/tikhub/http_headers.py'),
                    'Codex 中的 TikHub 配置不符合当前受支持的连接方式。')
        path = self.home / '.codex/secrets/tikhub_api_key'
        safe_secret(private_read(path, 8192).decode().strip())
        return path

    def integration_connection(self, service):
        plugins = self.config.get('integrations', {}).get('plugins', {})
        managed = self.config_path.parent / (service + '-api.key')
        saved_key = bool(managed.exists() and private_read(managed, 8192).strip())
        if service == 'tinyfish':
            plugin = plugins.get(service, {})
            source = 'codex' if plugin.get('codex_app', not plugin) else 'api_key'
            login = self.login_state()
            available = bool(login['authenticated'] and login['kind'] == 'chatgpt')
            configured = available if source == 'codex' else saved_key
            enabled = bool(plugin.get('enabled', False))
        else:
            selected = [plugins.get('tikhub-' + platform, {}) for platform in TIKHUB_TOOLS]
            source = 'api_key' if any(p.get('credential_file') == str(managed) for p in selected) else 'codex'
            enabled = all(p.get('enabled', False) for p in selected)
            try:
                self.codex_tikhub_source()
                available = True
            except (AdminError, OSError, ValueError, UnicodeError):
                available = False
            configured = available if source == 'codex' else saved_key
        return {'enabled': enabled, 'configured': configured, 'source': source,
                'codex_available': available, 'api_key_configured': saved_key}

    def configure_integration(self, cfg, body, save):
        service = body['section']
        require(set(body) <= {'section', 'source', 'enabled', 'api_key', 'remove_api_key'})
        require(body.get('source') in {'codex', 'api_key'}, '请选择连接来源。')
        require(type(body.get('enabled')) is bool and type(body.get('remove_api_key', False)) is bool)
        require(isinstance(body.get('api_key', ''), str), '凭据格式不正确。')
        require(not (body.get('remove_api_key') and body.get('api_key')), '更换和清除密钥不能同时提交。')
        require(not body.get('api_key') or body['source'] == 'api_key', '请在独立 API Key 模式下填写密钥。')
        managed = self.config_path.parent / (service + '-api.key')
        if body.get('api_key'):
            key = safe_secret(body['api_key'])
            require(len(key.encode()) < 8192, '凭据格式不正确。')
            save(managed, key + '\n')
        if body.get('remove_api_key') and managed.exists():
            save(managed, '')
        enabled = body['enabled'] and not (body.get('remove_api_key') and body['source'] == 'api_key')
        credential = None
        if body['source'] == 'api_key':
            configured = managed.exists() and bool(private_read(managed, 8192).strip())
            require(not enabled or configured, '请先填写 API Key。')
            # Retain the chosen source even after clearing its key. Re-enabling
            # must not silently switch back to the owner's Codex credentials.
            credential = str(managed)
        elif service == 'tikhub':
            credential = str(self.codex_tikhub_source() if enabled else self.home / '.codex/secrets/tikhub_api_key')
        else:
            login = self.login_state()
            require(not enabled or (login['authenticated'] and login['kind'] == 'chatgpt'), '请先完成机器人独立 Codex 登录。')
        plugins = cfg.setdefault('integrations', {}).setdefault('plugins', {})
        for platform in (TIKHUB_TOOLS if service == 'tikhub' else ['tinyfish']):
            key = 'tikhub-' + platform if service == 'tikhub' else 'tinyfish'
            plugin = {'enabled': enabled,
                      'name': 'TikHub · ' + TIKHUB_NAMES[platform] if service == 'tikhub' else 'TinyFish 网页搜索与提取',
                      'endpoint': f'https://mcp.tikhub.io/{platform}/mcp' if service == 'tikhub' else TINYFISH_ENDPOINT,
                      'allowed_tools': list(TIKHUB_TOOLS[platform] if service == 'tikhub' else TINYFISH_TOOLS)}
            if service == 'tinyfish':
                plugin['codex_app'] = body['source'] == 'codex'
            if credential:
                plugin['credential_file'] = credential
            plugins[key] = plugin
        self.checks.pop(service, None)

    def state(self):
        self.load()
        qq = env_read(self.path('qq_credentials_file', str(self.home / '.config/clawmingle/qq.env')))
        jev = env_read(Path(self.config.get('jev', {}).get('credentials_file', str(self.home / '.config/clawmingle/jev.env'))).expanduser())
        codex = self.config.get('codex', {})
        provider = tomllib.loads(private_read(self.runtime / 'config.toml').decode())
        provider_id = codex.get('provider', provider.get('model_provider', 'openai'))
        provider_detail = provider.get('model_providers', {}).get(provider_id, {})
        environment = env_read(Path(codex['credentials_file']).expanduser()) if codex.get('credentials_file') else {}
        presets = self.config.get('model_presets', {'sol': {'model': 'gpt-6.1-sol', 'reasoning_effort': 'high'}, 'luna': {'model': 'gpt-6-luna', 'reasoning_effort': 'medium'}})
        models = []
        cache = self.config_path.parent / 'models-cache.json'
        if cache.exists():
            cached = json.loads(private_read(cache, 1024 * 1024))
            if cached.get('provider') == provider_id:
                models = cached.get('models', [])
        source = 'live' if models else 'configured'
        if not models:
            for preset in presets.values():
                if not any(m['model'] == preset['model'] for m in models):
                    models.append({'id': preset['model'], 'model': preset['model'], 'display_name': preset['model'],
                                   'reasoning_efforts': sorted(EFFORTS), 'default_effort': preset['reasoning_effort']})
        def chats(private):
            stored = {p.stem: d['state'] for p, d in self.documents(private)}
            ids = stored.keys() if private else self.config.get('allowed_groups', [])
            result = []
            for index, subject in enumerate(ids):
                current = stored.get(subject, {})
                messages = current.get('messages', [])
                requests = current.get('requests', {}).values()
                result.append({'id': subject, 'name': ('私聊 ' if private else '测试群 ') + str(index + 1),
                    'mode': 'mode-all' if private else (current.get('mode_override') or self.config.get('mode', 'mode-1')),
                    'model_alias': current.get('model_alias') or '', 'message_count': len(messages),
                    'pending': sum(r.get('status') in {'queued', 'generating', 'ready', 'sending'} for r in requests),
                    'waiting_for_user': sum(r.get('status') == 'waiting_for_user' for r in current.get('requests', {}).values()),
                    'awaiting_delivery': sum(r.get('status') == 'awaiting_delivery' for r in current.get('requests', {}).values()),
                    'group_profile': None if private else public_group_profile(current),
                    'browser_enabled': current.get('browser_enabled', False),
                    'plugin_grants': current.get('plugin_grants', []),
                    'failed': sum(r.get('status') in {'failed', 'unknown_delivery'} for r in current.get('requests', {}).values()),
                    'last_activity': max((m.get('message', {}).get('timestamp_ms', 0) for m in messages), default=0)})
            return result
        behavior = self.behavior_settings()
        return {'service': self.service(), 'behavior': behavior, 'config': {
            'mode': self.config.get('mode', 'mode-1'), 'private_enabled': self.config.get('private_enabled', True),
            'max_private_sessions': self.config.get('max_private_sessions', 16),
            'private_allowed_users': self.config.get('private_allowed_users', []),
            'command_admins': self.config.get('command_admins', []), 'quiet_ms': self.config.get('quiet_ms', 1200),
            'max_wait_ms': self.config.get('max_wait_ms', 4000), 'model_presets': presets,
            'qq_app_id': qq.get('QQ_APP_ID', ''), 'jev_model': self.config.get('jev', {}).get('model', 'jev-1.14.0'),
            'soul': behavior['soul']['effective'], 'soul_uses_default': behavior['soul']['uses_default'],
            'persona': behavior['persona'],
            'web_search': provider.get('web_search', 'disabled'),
            'reply_format': self.config.get('reply_format', 'text'),
            'memory': {**MEMORY_DEFAULTS, **self.config.get('memory', {})},
            'interactions': {**INTERACTION_DEFAULTS, **self.config.get('interactions', {})},
            'stickers': {**STICKER_DEFAULTS, **self.config.get('stickers', {})},
            'integrations': self.integrations_public(),
            'documents': self.documents_public(),
            'connections': {name: self.integration_connection(name) for name in ('tinyfish', 'tikhub')},
            'enabled_tools': self.config.get('enabled_tools', TOOLS),
            'provider': {'id': 'openai' if provider_id == 'openai' else 'custom',
                         'base_url': provider_detail.get('base_url', ''), 'configured': bool(environment),
                         'model': codex.get('model', 'gpt-6.1-sol'), 'reasoning_effort': codex.get('reasoning_effort', 'high')}},
            'credentials': {'qq': bool(qq.get('QQ_APP_SECRET')), 'jev': bool(jev.get('TYPESAFE_API_KEY')), 'provider': bool(environment)},
            'login': self.login_state(), 'groups': chats(False), 'private_chats': chats(True),
            'models': models, 'model_list_source': source, 'skills': self.skill_list(), 'health': self.checks}

    def update_config(self, body):
        section = body.get('section')
        if section == 'skill':
            folder = self.data / 'skills' / alias(body.get('name'))
            require(not folder.parent.is_symlink() and not folder.is_symlink(), 'Skill 目录不能是链接。')
            if folder.exists():
                require(folder.is_dir() and not any(item.name != 'SKILL.md' for item in folder.iterdir()),
                        '完整技能包为只读：请在对应会话启停。')
        with self.transaction() as save:
            cfg = self.load()
            runtime = tomllib.loads(private_read(self.runtime / 'config.toml').decode())
            if section == 'general':
                allowed = {'mode', 'private_enabled', 'max_private_sessions', 'private_allowed_users', 'command_admins', 'quiet_ms', 'max_wait_ms', 'model_presets', 'reply_format'}
                require(set(body) <= allowed | {'section'})
                for key, value in body.items():
                    if key != 'section':
                        cfg[key] = value
                require(cfg.get('mode', 'mode-1') in MODES)
                require(cfg.get('reply_format', 'text') in {'text', 'markdown'})
                require(type(cfg.get('private_enabled', True)) is bool)
                for key, maximum in [('private_allowed_users', 64), ('command_admins', 32)]:
                    values = cfg.get(key, [])
                    require(isinstance(values, list) and len(values) <= maximum)
                    for value in values:
                        identifier(value)
                for key, low, high in [('max_private_sessions', 1, 32), ('quiet_ms', 100, 10000), ('max_wait_ms', 100, 30000)]:
                    value = cfg.get(key, low)
                    require(type(value) is int and low <= value <= high)
                presets = cfg.setdefault('model_presets', {
                    'sol': {'model': 'gpt-6.1-sol', 'reasoning_effort': 'high'},
                    'luna': {'model': 'gpt-6-luna', 'reasoning_effort': 'medium'}})
                require(isinstance(presets, dict) and 1 <= len(presets) <= 16)
                for name, preset in presets.items():
                    require(isinstance(name, str) and re.fullmatch(r'[A-Za-z0-9_-]{1,32}', name))
                    require(set(preset) == {'model', 'reasoning_effort'})
                    self.validate_model(preset['model'], preset['reasoning_effort'])
                for private in (False, True):
                    for _, document in self.documents(private):
                        state = document['state']
                        references = [state.get('model_alias')]
                        references.extend(v.get('alias') for v in state.get('next_models', {}).values()
                                          if v.get('expires_at_ms', 0) > time.time() * 1000)
                        references.extend(r.get('model_alias') for r in state.get('requests', {}).values()
                                          if r.get('status') not in {'completed', 'cancelled'})
                        require(all(ref is None or ref in presets for ref in references),
                                '模型预设仍被会话或未完成请求使用，请先切换或取消相关请求。', 409)
            elif section == 'browser':
                require(set(body) == {'section', 'enabled'} and type(body.get('enabled')) is bool)
                cfg.setdefault('integrations', {}).setdefault('browser', {})['enabled'] = body['enabled']
            elif section in {'tinyfish', 'tikhub'}:
                self.configure_integration(cfg, body, save)
            elif section in {'qq', 'jev'}:
                require(set(body) <= ({'section', 'app_id', 'app_secret'} if section == 'qq' else {'section', 'api_key', 'model'}))
                if section == 'qq':
                    path = self.path('qq_credentials_file', str(self.config_path.parent / 'qq.env'))
                    values = env_read(path)
                    if body.get('app_id'):
                        require(isinstance(body['app_id'], str) and body['app_id'].isascii() and body['app_id'].isdigit())
                        values['QQ_APP_ID'] = body['app_id']
                    if body.get('app_secret'):
                        values['QQ_APP_SECRET'] = safe_secret(body['app_secret'])
                    require(set(values) == {'QQ_APP_ID', 'QQ_APP_SECRET'})
                else:
                    settings = cfg.setdefault('jev', {})
                    path = Path(settings.get('credentials_file', str(self.config_path.parent / 'jev.env'))).expanduser()
                    values = env_read(path)
                    if body.get('api_key'):
                        values['TYPESAFE_API_KEY'] = safe_secret(body['api_key'])
                    if body.get('model'):
                        require(re.fullmatch(r'jev-[a-zA-Z0-9.-]{1,40}', body['model']))
                        settings['model'] = body['model']
                    require(set(values) == {'TYPESAFE_API_KEY'})
                save(path, env_dump(values))
            elif section == 'provider':
                require(set(body) <= {'section', 'id', 'base_url', 'api_key', 'model', 'reasoning_effort'})
                require(body.get('id') in {'openai', 'custom'})
                self.validate_model(body.get('model'), body.get('reasoning_effort'))
                settings = cfg.setdefault('codex', {})
                settings.update(model=body['model'], reasoning_effort=body['reasoning_effort'])
                runtime.update(model=body['model'], model_reasoning_effort=body['reasoning_effort'])
                path = self.config_path.parent / 'model.env'
                old = env_read(path)
                key = 'OPENAI_API_KEY' if body['id'] == 'openai' else 'CLAWMINGLE_MODEL_API_KEY'
                credential = safe_secret(body['api_key']) if body.get('api_key') else old.get(key)
                if body['id'] == 'custom':
                    parsed = urllib.parse.urlsplit(body.get('base_url', ''))
                    require(parsed.scheme == 'https' and parsed.hostname and not parsed.username
                            and not parsed.password and not parsed.query and not parsed.fragment,
                            '自定义接口必须为不含凭据的 HTTPS 地址。')
                    settings['provider'] = 'clawmingle_custom'
                    runtime['model_provider'] = 'clawmingle_custom'
                    runtime.setdefault('model_providers', {})['clawmingle_custom'] = {
                        'name': 'ClawMingle custom', 'base_url': body['base_url'].rstrip('/'),
                        'env_key': key, 'wire_api': 'responses', 'requires_openai_auth': False}
                    require(credential, '请填写此服务的 API 密钥。')
                else:
                    settings.pop('provider', None)
                    runtime.pop('model_provider', None)
                if credential:
                    save(path, env_dump({key: credential}))
                    settings['credentials_file'] = str(path)
                else:
                    settings.pop('credentials_file', None)
                save(self.runtime / 'config.toml', toml_dump(runtime))
            elif section == 'behavior':
                self.validate_behavior(body)
                updated = dict(body, behavior=json.loads(json.dumps(body['behavior'])))
                for key, mode in updated['behavior']['modes'].items():
                    previous = cfg.get('behavior', {}).get('modes', {}).get(key, {})
                    if 'reply_instructions' in previous:
                        # Read-only migration archive: saving/resetting current rules
                        # never overwrites a previously stored per-mode personality.
                        mode['reply_instructions'] = previous['reply_instructions']
                self.validate_behavior(updated)
                for key in ('behavior', 'context_windows', 'max_bubbles', 'max_bubble_chars'):
                    cfg[key] = updated[key]
            elif section == 'memory':
                require(set(body) == {'section', *MEMORY_DEFAULTS}, '记忆设置包含不支持的字段。')
                settings = {key: body[key] for key in MEMORY_DEFAULTS}
                self.validate_memory_settings(settings)
                cfg['memory'] = settings
            elif section == 'interactions':
                require(set(body) == {'section', *INTERACTION_DEFAULTS}, '长任务设置包含不支持的字段。')
                settings = {key: body[key] for key in INTERACTION_DEFAULTS}
                self.validate_interactions(settings)
                cfg['interactions'] = settings
            elif section == 'stickers':
                require(set(body) == {'section', *STICKER_DEFAULTS}, '表情包设置字段不正确。')
                require(type(body['enabled']) is bool and type(body['automatic']) is bool and type(body['auto_collect']) is bool
                        and type(body['cooldown_seconds']) is int and 0 <= body['cooldown_seconds'] <= 86400,
                        '表情包开关或冷却时间不正确。')
                require(isinstance(body['usage_instructions'], str) and len(body['usage_instructions']) <= 4000
                        and '\0' not in body['usage_instructions'], '表达策略须为不超过 4000 字的文字。')
                cfg['stickers'] = {key: body[key] for key in STICKER_DEFAULTS}
            elif section == 'persona':
                require(set(body) <= {'section', 'identity', 'soul', 'web_search', 'enabled_tools'})
                require(isinstance(body.get('soul'), str) and len(body['soul']) <= 8000)
                require(body.get('web_search') in {'live', 'cached', 'disabled'})
                if 'identity' in body or cfg.get('persona_files', False):
                    directory = self.data / 'persona'
                    directory.mkdir(mode=0o700, exist_ok=True)
                    require(not directory.is_symlink()
                            and not any((directory / name).is_symlink() for name in ('IDENTITY.md', 'SOUL.md')),
                            '身份与人格文件及目录不能是链接。')
                    identity = body.get('identity') if 'identity' in body else private_read(directory / 'IDENTITY.md', 8000).decode()
                    require(isinstance(identity, str) and 1 <= len(identity) <= 2000
                            and identity.strip() and '\0' not in identity, 'Identity 须为 1–2000 字的身份说明，不能留空。')
                    require(body['soul'].strip() and '\0' not in body['soul'], 'Soul 不能为空。请保留或填写人格说明。')
                    save(directory / 'IDENTITY.md', identity)
                    save(directory / 'SOUL.md', body['soul'])
                    cfg['persona_files'] = True
                    cfg.pop('soul', None)
                else:
                    # Older clients keep their inline behavior until explicitly migrated.
                    cfg['soul'] = body['soul']
                runtime['web_search'] = body['web_search']
                if 'enabled_tools' in body:
                    selected = body['enabled_tools']
                    require(isinstance(selected, list) and len(selected) <= len(TOOLS)
                            and all(isinstance(tool, str) and tool in TOOLS for tool in selected)
                            and len(set(selected)) == len(selected), '工具列表不正确。')
                    cfg['enabled_tools'] = selected
                save(self.runtime / 'config.toml', toml_dump(runtime))
            elif section == 'skill':
                require(set(body) <= {'section', 'action', 'name', 'description', 'instructions', 'enabled'})
                name = alias(body.get('name'))
                require(body.get('action') in {'save', 'delete'})
                directory = self.data / 'skills' / name
                directory.parent.mkdir(mode=0o700, exist_ok=True)
                require(not directory.parent.is_symlink(), 'Skill 根目录不能是链接。')
                directory.mkdir(mode=0o700, exist_ok=True)
                require(not directory.is_symlink(), 'Skill 目录不能是链接。')
                require(not any(item.name != 'SKILL.md' for item in directory.iterdir()),
                        '完整技能包为只读：此编辑器不能覆盖、删除脚本资源或版本记录；请在对应会话启停。')
                path = directory / 'SKILL.md'
                enabled = cfg.setdefault('enabled_skills', [])
                if body['action'] == 'save':
                    require(isinstance(body.get('instructions'), str) and 1 <= len(body['instructions']) <= 10000)
                    require(isinstance(body.get('description'), str) and len(body['description']) <= 200
                            and '\n' not in body['description'] and type(body.get('enabled')) is bool)
                    existing_skills = self.skill_list()
                    require(len(existing_skills) < 32 or any(s['name'] == name for s in existing_skills), '最多管理 32 个 Skill。')
                    content = '---\nname: ' + name + '\ndescription: ' + json.dumps(body['description'], ensure_ascii=False) + '\n---\n' + body['instructions']
                    require(len(content.encode()) <= 16384, 'Skill 内容超过 16 KiB 限制。')
                    save(path, content)
                    if body['enabled'] and name not in enabled:
                        enabled.append(name)
                    if not body['enabled'] and name in enabled:
                        enabled.remove(name)
                else:
                    require(path.exists(), 'Skill 不存在。', 404)
                    # Preserve removed instructions in a protected archive; never run them.
                    archive = self.config_path.parent / 'skill-archive'
                    archive.mkdir(mode=0o700, exist_ok=True)
                    save(archive / f'{name}-{time.time_ns()}.md', private_read(path, 16384))
                    save(path, '---\ndescription: 已移除\n---\n')
                    if name in enabled:
                        enabled.remove(name)
                    for private in (False, True):
                        for state_path, stored in self.documents(private):
                            selected = stored['state'].get('enabled_skills', [])
                            if name in selected:
                                stored['state']['enabled_skills'] = [s for s in selected if s != name]
                                save(state_path, json.dumps(stored, ensure_ascii=False))
                save(self.config_path, toml_dump(cfg))
            else:
                raise AdminError('未知的配置分类。')
            if section in {'persona', 'skill'}:
                selections = [cfg.get('enabled_skills', [])]
                selections.extend(list(dict.fromkeys(cfg.get('enabled_skills', []) + d['state'].get('enabled_skills', [])))
                                  for private in (False, True) for _, d in self.documents(private))
                if cfg.get('persona_files', False):
                    persona_size = sum(len(private_read(self.data / 'persona' / name, limit))
                                       for name, limit in [('IDENTITY.md', 8000), ('SOUL.md', 32000)])
                else:
                    soul = cfg.get('soul', '')
                    if not soul.strip():
                        soul = self.behavior_settings()['soul']['default']
                    persona_size = len(soul.encode())
                for selected in selections:
                    require(len(selected) <= 16, '每个会话最多启用 16 个 Skill。')
                    size = persona_size
                    for name in selected:
                        alias(name)
                        size += len(private_read(self.data / 'skills' / name / 'SKILL.md', 16384)) + 128
                    require(size <= 47000, '人格和 Skill 指令总量过大，请先停用部分 Skill。')
            save(self.config_path, toml_dump(cfg))
        return {'ok': True}

    def validate_model(self, model, effort):
        require(isinstance(model, str) and re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.:/-]{0,127}', model))
        require(effort in EFFORTS, '当前执行器支持 low、medium、high、xhigh。')

    def conversation(self, body):
        require(set(body) <= {'scope', 'id', 'action', 'mode', 'model_alias', 'browser_enabled', 'plugin_grants', 'profile', 'expected_revision'})
        private = body.get('scope') == 'private'
        require(body.get('scope') in {'group', 'private'})
        subject = identifier(body.get('id'))
        action = body.get('action')
        require(action in {'bind', 'unbind', 'settings', 'reset', 'profile', 'profile_reset'})
        if action in {'profile', 'profile_reset'}:
            require(not private, '群档案仅支持群聊。')
            require(set(body) == {'scope', 'id', 'action', 'expected_revision'} | ({'profile'} if action == 'profile' else set()))
            expected = body.get('expected_revision')
            require(isinstance(expected, str) and re.fullmatch(r'0|[1-9][0-9]{0,19}', expected)
                    and int(expected) < 2**64 - 1, '档案版本无效；请重新读取。')
            fields = profile_fields(body['profile']) if action == 'profile' else dict.fromkeys(GROUP_PROFILE_FIELDS, '')
        else:
            require(not ({'profile', 'expected_revision'} & set(body)))
        with self.transaction() as save:
            cfg = self.load()
            groups = cfg.setdefault('allowed_groups', [])
            if action in {'bind', 'unbind'}:
                require(not private, '私聊接入请通过私聊白名单管理。')
                if action == 'bind':
                    require(subject in groups or len(groups) < 32, '绑定群数量已达上限。')
                    if subject not in groups:
                        groups.append(subject)
                else:
                    require(subject in groups, '此群尚未绑定。')
                    groups.remove(subject)
                    cfg.get('documents', {}).get('bindings', {}).pop(subject, None)
                    self.qq_documents_unbind(cfg, subject)
                save(self.config_path, toml_dump(cfg))
            else:
                path = self.data / ('private/groups' if private else 'groups') / (subject + '.json')
                require(path.exists(), '会话尚未产生记录；请先发送一条消息。', 409)
                stored = json.loads(private_read(path, 64 * 1024 * 1024))
                require(stored.get('group_id') == subject)
                state = stored['state']
                if action in {'profile', 'profile_reset'}:
                    require(subject in groups, '此群尚未绑定。')
                    previous = state.get('group_profile') or {}
                    require(previous.get('revision', 0) == int(expected), '群档案已被修改；请重新读取后再保存，当前草稿尚未保存。', 409)
                    state['group_profile'] = {'fields': fields, 'revision': int(expected) + 1,
                        'source_seq': max(0, state.get('next_seq', 1) - 1), 'source_message_id': '',
                        'updated_by': 'operator', 'updated_at_ms': int(time.time() * 1000)}
                elif action == 'settings':
                    mode = body.get('mode', '')
                    selected = body.get('model_alias', '')
                    require(mode == '' or mode in MODES)
                    require(selected == '' or selected in cfg.get('model_presets', {'sol': {}, 'luna': {}}))
                    state['mode_override'] = None if private or not mode else mode
                    state['model_alias'] = selected or None
                    if 'browser_enabled' in body:
                        require(type(body['browser_enabled']) is bool, '浏览器授权必须为布尔值。')
                        require(not body['browser_enabled'] or self.integrations_public()['browser']['configured'], '公共浏览器尚未配置。')
                        state['browser_enabled'] = body['browser_enabled']
                    if 'plugin_grants' in body:
                        grants = body['plugin_grants']
                        available = cfg.get('integrations', {}).get('plugins', {})
                        require(isinstance(grants, list) and len(grants) <= 16
                                and all(isinstance(name, str) and name in available
                                        and (available[name].get('enabled', False) or name in state.get('plugin_grants', [])) for name in grants)
                                and len(set(grants)) == len(grants), '只能授权已登记的插件。')
                        state['plugin_grants'] = grants
                else:
                    require(not any(r.get('status') == 'unknown_delivery' for r in state.get('requests', {}).values()),
                            '有发送结果未知的消息，请先核对。', 409)
                    state['thread_id'] = None
                    for request in state.get('requests', {}).values():
                        request.pop('codex_thread_id', None)
                        request.pop('codex_context', None)
                    state['context_start_seq'] = state.get('next_seq', 1)
                    state['next_models'] = {}
                save(path, json.dumps(stored, ensure_ascii=False))
        return {'ok': True}

    def login(self, body):
        require(set(body) == {'action'})
        action = body['action']
        if action == 'device_start':
            require(not self.device or self.device['process'].poll() is not None, '已有登录正在进行。', 409)
            # The runtime resolver validates native/npm installs and expands
            # configured paths before the isolated login HOME is created.
            resolved = json.loads(self.run([str(self.binary), 'codex-binary', '--config', str(self.config_path)], 15))
            binary = resolved.get('binary')
            require(isinstance(binary, str) and Path(binary).is_absolute(), 'Codex 安装路径不可用。', 502)
            directory = Path(tempfile.mkdtemp(prefix='clawmingle-login-', dir=self.data))
            environment = {'HOME': str(directory), 'CODEX_HOME': str(directory), 'PATH': '/usr/bin:/bin', 'LANG': 'C.UTF-8'}
            try:
                process = subprocess.Popen([binary, 'login', '--device-auth'], cwd=directory, env=environment,
                                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True)
            except OSError:
                # Popen did not start Codex; only remove this newly-created empty directory.
                directory.rmdir()
                raise AdminError('Codex 登录无法启动，请检查安装路径和权限。', 502) from None
            self.device = {'process': process, 'directory': directory, 'started': time.monotonic(), 'status': 'waiting'}
            self.device_output = ''
            def collect():
                while chunk := process.stdout.read(1):
                    with self.mutex:
                        self.device_output = (self.device_output + chunk.decode('utf-8', 'ignore'))[-16384:]
            threading.Thread(target=collect, daemon=True).start()
            return {'ok': True, 'device': self.device_state()}
        if action == 'device_cancel':
            if self.device:
                self.stop_device(self.device)
                self.cleanup_device(self.device)
            self.device = None
            self.device_output = ''
            return {'ok': True}
        if action == 'device_poll':
            require(self.device is not None, '没有进行中的登录。', 409)
            status = self.device_state()
            if status['status'] == 'ready':
                with self.transaction() as save:
                    save(self.runtime / 'auth.json', private_read(self.device['directory'] / 'auth.json'))
                self.device['status'] = 'completed'
                status['status'] = 'completed'
                self.cleanup_device(self.device)
                status.update(url='', code='')
            return {'ok': True, 'device': status}
        require(action in {'reuse', 'logout'})
        with self.transaction(restart=action != 'logout') as save:
            if action == 'reuse':
                auth = private_read(self.home / '.codex/auth.json')
                parsed = json.loads(auth)
                require(parsed.get('tokens', {}).get('access_token') or parsed.get('OPENAI_API_KEY'), '本机登录态不可用。')
                save(self.runtime / 'auth.json', auth)
            else:
                # Keep a valid, empty auth file; never touch the personal account.
                save(self.runtime / 'auth.json', '{}')
        return {'ok': True}

    def device_state(self):
        device = self.device
        status = device['status']
        process = device['process']
        if time.monotonic() - device['started'] > 900 and process.poll() is None:
            self.stop_device(device)
            status = 'expired'
        elif status not in {'completed', 'expired'} and process.poll() is not None:
            status = 'ready' if process.returncode == 0 and (device['directory'] / 'auth.json').exists() else 'failed'
        raw = re.sub(r'\x1b\[[0-9;]*m', '', self.device_output)
        urls = re.findall(r'https://auth\.openai\.com/[A-Za-z0-9/_-]+', raw)
        codes = re.findall(r'\b[A-Z0-9]{4}-[A-Z0-9]{5}\b|\b[A-Z0-9]{4}-[A-Z0-9]{4}\b', raw)
        device['status'] = status
        if status in {'expired', 'failed', 'completed'}:
            self.cleanup_device(device)
            return {'status': status, 'url': '', 'code': ''}
        return {'status': status, 'url': urls[0] if urls else '', 'code': codes[0] if codes else ''}

    def stop_device(self, device):
        process = device['process']
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=3)

    def cleanup_device(self, device):
        # Only this service's newly-created, disposable login staging tree.
        # Successful auth is copied before cleanup. Never delete a Codex home.
        path = device['directory']
        if not path.exists():
            return
        require(device['process'].poll() is not None, '登录进程尚未退出。', 409)
        info = path.lstat()
        require(path.parent.resolve() == self.data.resolve()
                and path.name.startswith('clawmingle-login-') and not path.is_symlink()
                and stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
                and stat.S_IMODE(info.st_mode) == 0o700, '登录临时目录检查失败。', 500)
        shutil.rmtree(path)
        self.device_output = ''

    def check(self, body):
        require(set(body) == {'target'})
        target = body['target']
        require(target in {'qq', 'jev', 'codex', 'models', 'tinyfish', 'tikhub'})
        if target in {'tinyfish', 'tikhub'}:
            with self.transaction(hold_lock=False):
                require(self.integration_connection(target)['enabled'], '请先保存并开启此连接。')
                names = ['tinyfish'] if target == 'tinyfish' else ['tikhub-' + name for name in TIKHUB_TOOLS]
                args = [str(self.binary), 'integration-check']
                for name in names:
                    args.extend(['--plugin', name])
                raw = self.run([*args, '--config', str(self.config_path)], 50)
                value = json.loads(raw)
                require(value.get('ok') is True and type(value.get('tool_count')) is int and value['tool_count'] > 0,
                        '工具连接未通过，请核对授权或密钥。', 502)
            message = '连接通过，已验证允许工具的目录；未执行内容查询或模型回复。'
        elif target in {'codex', 'models'}:
            with self.transaction(hold_lock=False) as save:
                # Read-only model/list query in the same isolated profile; no turn.
                raw = self.run([str(self.binary), 'models', '--config', str(self.config_path)], 45)
                value = json.loads(raw)
                require(isinstance(value.get('models'), list) and value['models'], '模型目录为空。', 502)
                provider = self.config.get('codex', {}).get('provider', 'openai')
                save(self.config_path.parent / 'models-cache.json', json.dumps({'provider': provider, 'models': value['models']}))
            message = '独立 Codex 连接正常，已刷新可用模型与思考强度；未发起回复。'
        elif target == 'qq':
            from qq_smoke import load_credentials, http_json, API_BASE
            values = load_credentials(self.path('qq_credentials_file', str(self.config_path.parent / 'qq.env')))
            result = http_json(API_BASE + '/app/getAppAccessToken', body={'appId': values['QQ_APP_ID'], 'clientSecret': values['QQ_APP_SECRET']}, stage='QQ 检查')
            require(bool(result.get('access_token')), 'QQ 鉴权未成功。', 502)
            message = 'QQ 鉴权通过；实际收发请使用聊天中的 /ping。'
        else:
            from qq_smoke import NoRedirect
            values = env_read(Path(self.config.get('jev', {}).get('credentials_file', str(self.config_path.parent / 'jev.env'))).expanduser())
            payload = {'model': self.config.get('jev', {}).get('model', 'jev-1.14.0'), 'state': {'connection_test': True},
                       'questions': {'connected': {'type': 'choice', 'instructions': 'Select yes for this connection test.', 'criteria': {'yes': 'Connection test', 'no': 'Other'}}}}
            request = urllib.request.Request('https://api.typesafe.ai/v1/systemone', data=json.dumps(payload).encode(),
                headers={'Content-Type': 'application/json', 'Authorization': 'Bearer ' + values['TYPESAFE_API_KEY']})
            with urllib.request.build_opener(NoRedirect()).open(request, timeout=15) as response:
                result = json.loads(response.read(65536))
            require(isinstance(result.get('answers'), dict), 'Jev 测试响应无效。', 502)
            message = 'Jev 连接正常；已完成一次最小判断测试。'
        self.checks[target] = {'ok': True, 'checked_at': int(time.time() * 1000), 'message': message}
        return {'ok': True, 'message': message}

    def memory_cli(self, payload):
        # Operator content and raw scope identifiers travel through stdin only.
        raw = json.dumps(payload, ensure_ascii=False, separators=(',', ':')).encode()
        require(len(raw) <= 32768, '记忆操作内容过长。')
        result = subprocess.run([str(self.binary), 'memory-operation', '--config', str(self.config_path)],
                                input=raw, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                timeout=20, check=False)
        require(result.returncode == 0,
                '记忆操作未完成，记录可能已经更新。请刷新核对版本后重试。' if payload['action'] in {'edit', 'reject', 'apply', 'correct'}
                else '记忆读取或归档未完成，请稍后刷新重试。',
                409 if payload['action'] in {'edit', 'reject', 'apply', 'correct'} else 502)
        require(len(result.stdout) <= 4 * 1024 * 1024, '记忆结果过大。', 502)
        try:
            return json.loads(result.stdout)
        except (ValueError, UnicodeError):
            raise AdminError('记忆返回格式异常。', 502) from None

    def stickers(self, body):
        action = body.get('action')
        require(isinstance(action, str), '表情包操作类型不正确。')
        metadata = {'name', 'tags', 'description', 'when_to_use', 'avoid_when', 'enabled'}
        fields = {'list': set(), 'taxonomy': set(), 'series_create': {'series_id', 'name'},
                  'import': metadata | {'data_base64'}, 'edit': metadata | {'id', 'revision'},
                  'preview': {'id', 'revision'}, 'remove': {'id', 'revision'},
                  'move': {'id', 'revision', 'series_id'},
                  'preferences': {'target', 'private'},
                  'preference': {'target', 'private', 'dimension', 'value', 'operation'}}
        optional = {'summary', 'styles', 'emotions'} if action in {'import', 'edit'} else set()
        if action == 'import':
            optional.add('series_id')
        require(action in fields and fields[action] | {'action'} <= set(body)
                and set(body) <= fields[action] | optional | {'action'}, '表情包操作字段不正确。')
        payload = dict(body)
        if action in {'preferences', 'preference'}:
            target = identifier(body.get('target'))
            require(type(body.get('private')) is bool, '请明确选择群聊或私聊。')
            self.load()
            known = [path.stem for path, _ in self.documents(True)] if body['private'] else self.config.get('allowed_groups', [])
            require(target in known, '该私聊尚无本地记录。' if body['private'] else '该群尚未绑定。', 404)
            if action == 'preference':
                require(isinstance(body['dimension'], str) and body['dimension'] in {'asset', 'series', 'style'}
                        and isinstance(body['operation'], str) and body['operation'] in {'prefer', 'avoid', 'reset'}, '偏好操作类型不正确。')
                require(isinstance(body['value'], str) and re.fullmatch(r'[a-zA-Z0-9_-]{1,80}', body['value']), '偏好目标不正确。')
                payload['source_event_id'] = 'admin:' + secrets.token_hex(16)
        if 'id' in body:
            require(isinstance(body['id'], str) and re.fullmatch(r'[a-zA-Z0-9_-]{1,80}', body['id']), '表情编号不正确。')
        if 'series_id' in body:
            require(isinstance(body['series_id'], str) and re.fullmatch(r'[a-z0-9_-]{1,48}', body['series_id']), '系列 ID 仅支持小写字母、数字、短横线和下划线，最多 48 字符。')
        if 'revision' in body:
            value = body['revision']
            require((type(value) is int and 1 <= value <= 9007199254740991)
                    or (isinstance(value, str) and re.fullmatch(r'[0-9]{1,20}', value)), '版本号不正确。')
            payload['revision'] = int(value)
            require(1 <= payload['revision'] <= 18446744073709551615, '版本号超出范围。')
        if action == 'series_create':
            require(isinstance(body['name'], str) and 0 < len(body['name'].strip()) <= 60
                    and '\0' not in body['name'], '请填写不超过 60 字的系列名称。')
        if action in {'import', 'edit'}:
            for key, limit in [('name', 60), ('description', 300), ('when_to_use', 300), ('avoid_when', 300), ('summary', 300)]:
                if key in body:
                    require(isinstance(body[key], str) and len(body[key]) <= limit and '\0' not in body[key], '表情说明过长或包含无效字符。')
            require(bool(body['name'].strip()) and type(body['enabled']) is bool, '请填写名称与启用状态。')
            require(isinstance(body['tags'], list) and len(body['tags']) <= 12
                    and all(isinstance(tag, str) and 0 < len(tag) <= 30 and '\0' not in tag for tag in body['tags']), '标签最多 12 个，每个最多 30 字。')
            taxonomies = {
                'styles': {'cute', 'goofy', 'meme', 'ordinary', 'animal', 'person', 'cartoon', 'text', 'teasing'},
                'emotions': {'happy', 'sad', 'speechless', 'amused', 'calm', 'angry', 'surprised', 'confused',
                             'affectionate', 'encouraging', 'tired', 'embarrassed', 'celebratory', 'neutral'},
            }
            for key, allowed in taxonomies.items():
                if key in body:
                    values = body[key]
                    require(isinstance(values, list) and len(values) <= (6 if key == 'emotions' else len(allowed))
                            and all(isinstance(value, str) and value in allowed for value in values)
                            and len(set(values)) == len(values), '请选择有效且不重复的风格或情绪。')
        if action == 'import':
            require(isinstance(body['data_base64'], str) and 0 < len(body['data_base64']) <= 2796204,
                    '原图须不超过 2 MiB。', 413)
        result = subprocess.run([str(self.binary), 'stickers', '--config', str(self.config_path)],
                                input=json.dumps(payload, ensure_ascii=False).encode(),
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, timeout=30, check=False)
        require(result.returncode == 0, '表情包操作未完成；请检查格式、大小或刷新版本后重试。', 409 if action not in {'list', 'taxonomy', 'preferences'} else 502)
        require(len(result.stdout) <= 8 * 1024 * 1024, '表情库响应过大。', 502)
        try:
            value = json.loads(result.stdout)
        except (ValueError, UnicodeError):
            raise AdminError('表情库响应无效。', 502) from None
        require(isinstance(value, dict), '表情库响应无效。', 502)
        def wire(item):
            if isinstance(item, dict):
                return {key: str(val) if key == 'revision' and type(val) is int else wire(val) for key, val in item.items()}
            if isinstance(item, list):
                return [wire(row) for row in item]
            return item
        return wire(value)

    def memory(self, body):
        legacy_request = 'group_id' in body
        if legacy_request:
            require(set(body) == {'action', 'group_id'} and body.get('action') in {'view', 'maintain'}, '旧版记忆接口仅支持查看和归档。')
            body = {'action': 'archive' if body['action'] == 'maintain' else 'view',
                    'target': body['group_id'], 'private': False}
        action = body.get('action')
        require(action in {'view', 'archive', 'edit', 'reject', 'apply', 'correct'}, '记忆操作无效。')
        expected = {'action', 'target', 'private'}
        if action in {'edit', 'reject', 'apply', 'correct'}:
            expected |= {'id', 'revision'}
        if action in {'edit', 'correct'}:
            expected.add('content')
        require(set(body) == expected, '记忆操作字段不完整或包含不支持的选项。')
        target = identifier(body.get('target'))
        require(type(body.get('private')) is bool, '请明确选择群聊或私聊。')
        private = body['private']
        self.load()
        known = [path.stem for path, _ in self.documents(True)] if private else self.config.get('allowed_groups', [])
        require(target in known, '该私聊尚无本地记录。' if private else '该群尚未绑定。', 404)
        payload = {'action': action, 'target': target, 'private': private}
        for key in ('id', 'revision'):
            if key not in body:
                continue
            value = body[key]
            require((type(value) is int and 1 <= value <= 9007199254740991)
                    or (isinstance(value, str) and re.fullmatch(r'[0-9]{1,20}', value) is not None), '记录编号与版本必须为正整数；大编号请使用十进制文字。')
            value = int(value)
            require(1 <= value <= 18446744073709551615, '记录编号或版本超出范围。')
            payload[key] = value
        if 'content' in body:
            content = body['content']
            require(isinstance(content, str) and bool(content.strip()) and len(content) <= 500 and '\0' not in content,
                    '记忆内容须为 1–500 字的文字。')
            payload['content'] = content
        result = self.memory_cli(payload)
        require(isinstance(result, dict) and all(isinstance(result.get(key), dict) for key in ('legacy', 'ledger', 'layout', 'settings')),
                '记忆返回格式异常。', 502)
        ledger = result['ledger']
        require(all(isinstance(ledger.get(key), list) for key in ('drafts', 'records', 'reviews'))
                and isinstance(ledger.get('maintenance'), dict) and type(ledger.get('revision')) is int,
                '记忆账本格式异常。', 502)
        for entry in ledger['drafts'] + ledger['records']:
            require(isinstance(entry, dict) and type(entry.get('id')) is int and type(entry.get('revision')) is int
                    and isinstance(entry.get('content'), str), '记忆条目格式异常。', 502)
        self.validate_memory_settings(result['settings'])
        layout = result['layout']
        require(set(layout) == {'canonical', 'legacy_authority', 'drafts_json', 'drafts_markdown', 'revisions_json', 'categories'}
                and isinstance(layout['categories'], list), '记忆文件布局格式异常。', 502)
        paths = [layout[key] for key in layout if key != 'categories']
        for row in layout['categories']:
            require(isinstance(row, dict) and set(row) == {'category', 'path'}, '记忆文件布局格式异常。', 502)
            paths.append(row['path'])
        require(all(isinstance(path, str) and path and not PurePosixPath(path).is_absolute()
                    and not any(part in {'..', '.'} for part in path.split('/'))
                    and not any(c in path for c in ('\\', '\0', ':')) and not path.startswith('~')
                    for path in paths), '记忆布局必须为安全的相对路径。', 502)
        # The CLI returns paths relative to its scoped data root. Present every
        # path relative to the main application data root in the admin UI.
        if private:
            for key in layout:
                if key != 'categories':
                    layout[key] = 'private/' + layout[key]
            for row in layout['categories']:
                row['path'] = 'private/' + row['path']
        def wire(value, key=''):
            if isinstance(value, dict):
                return {k: wire(v, k) for k, v in value.items()}
            if isinstance(value, list):
                return [wire(item) for item in value]
            if type(value) is int and (key in {'id', 'revision', 'authority_revision', 'supersedes', 'superseded_by'} or key.endswith('_id') or key.endswith('_revision')):
                return str(value)
            return value
        result = wire(result)
        index = known.index(target) + 1
        result.update({'target': target, 'private': private, 'name': ('私聊 ' if private else '群聊 ') + str(index),
                       'logical_layout': [
                           {'role': 'workspace', 'path': 'workspaces/' + ('p_' if private else 'g_') + hashlib.sha1(target.encode()).hexdigest(),
                            'note': '当前会话的隔离工作区；不是正式记忆。'},
                           {'role': 'archive', 'path': ('private/' if private else '') + 'history/archive.sqlite',
                            'note': '聊天历史存储；查询始终限定当前会话，不提供数据库浏览。'}]})
        if legacy_request:
            legacy = result['legacy']; summary = legacy.get('summary', {})
            result.update({'group_id': target, 'authority': {'revision': summary.get('authority_revision', '0'),
                'entries': [{'id': r.get('fact_id'), 'subject_alias': r.get('subject'), 'text': r.get('text'),
                             'revision': r.get('revision'), 'source_timestamp': r.get('source', {}).get('timestamp')} for r in legacy.get('entries', [])]},
                'changes': [{'id': r.get('change_id'), 'subject_alias': r.get('subject'), 'text': r.get('text'),
                             'revision': r.get('revision'), 'status': r.get('status'), 'source_timestamp': r.get('source', {}).get('timestamp')}
                            for r in legacy.get('changes', [])],
                'summary': {'facts': summary.get('facts', 0), 'pending': summary.get('pending', 0), 'confirmed': summary.get('confirmed', 0),
                            'revision': summary.get('authority_revision', '0'), 'last_maintained_at': legacy.get('last_maintained_at')},
                'schedule': {'time_zone': 'Asia/Shanghai', 'daily_time': f"{result['settings']['archive_hour']:02}:{result['settings']['archive_minute']:02}",
                             'active': result['settings']['enabled']}})
        return result

    def documents_public(self):
        cfg = self.config.get('documents', {})
        credential = cfg.get('credential_file')
        configured = bool(credential and Path(credential).is_file())
        return {'enabled': cfg.get('enabled', False), 'authorized': configured,
                'bindings': cfg.get('bindings', {}), 'provider': 'Tencent Docs',
                'workspace_policy': {'task_folders': True, 'reuse_existing': True, 'delivery': 'online_first',
                                     'attachment_fallback': 'ask_user', 'revision_attribution': 'local_journal',
                                     'native_revision_note_supported': False, 'qq_nested_folders_verified': False,
                                     'automatic_archive_enabled': False, 'retention_classes': ['standard', 'keep_online', 'memory'],
                                     'archive_exempt_classes': ['keep_online', 'memory']},
                'body_types': ['smartcanvas', 'doc', 'sheet', 'slide', 'smartsheet', 'form', 'flowchart'],
                'qq_group_online_list_integration': bool(self.qq_documents_public()['active_groups']),
                'qq_native': self.qq_documents_public(),
                'document_types': [
                    {'id': 'smartcanvas', 'name': '智能文档', 'level': '正文读写',
                     'operations': '创建、分页读取、块搜索、追加和修改正文', 'note': ''},
                    {'id': 'doc', 'name': '普通文档', 'level': '正文读写',
                     'operations': '创建、读取和编辑正文、格式、图片与表格', 'note': ''},
                    {'id': 'sheet', 'name': '表格', 'level': '结构读写',
                     'operations': '创建、单元格与公式读写、样式、图表和工作表管理', 'note': ''},
                    {'id': 'slide', 'name': '幻灯片', 'level': '结构读写',
                     'operations': '创建、页面、文字、图片、形状、表格与图表编辑', 'note': ''},
                    {'id': 'smartsheet', 'name': '智能表格', 'level': '结构读写',
                     'operations': '创建、记录、字段、视图与仪表盘管理', 'note': '不开放自动化任务与账号管理。'},
                    {'id': 'form', 'name': '收集表', 'level': '草稿与结果',
                     'operations': '创建草稿、编辑问题与设置、读取提交结果和统计',
                     'note': '需要在腾讯文档中人工发布；读取提交结果仍受账号权限限制。'},
                    {'id': 'flowchart', 'name': '流程图', 'level': '结构读写',
                     'operations': '创建、读取、增删改和移动图形节点', 'note': ''},
                    {'id': 'mind', 'name': '思维导图', 'level': '有限支持',
                     'operations': '从 Markdown 创建、查看文件信息、改名和回收',
                     'note': '当前官方接口未提供专用节点读取与编辑。'},
                    {'id': 'board', 'name': '智能白板', 'level': '有限支持',
                     'operations': '创建空白白板、查看文件信息、改名和回收',
                     'note': '当前官方接口未提供画布内容读取与编辑。'},
                    {'id': 'pdf', 'name': 'PDF', 'level': '文件导入导出',
                     'operations': '导入 PDF、查询导入结果、导出文件',
                     'note': 'PDF 导入已验证，不支持在线修改正文；导出接口已接入，但当前实测下载地址无文件，尚未验收可用。'},
                ]}

    def qq_documents_public(self):
        cfg = self.config.get('documents', {}).get('qq_native', {})
        configured = False
        if cfg.get('browser_state_file'):
            try:
                self.qq_documents_browser_state(Path(cfg['browser_state_file']))
                configured = True
            except (AdminError, OSError, ValueError):
                pass
        groups = {group: code for group, code in cfg.get('groups', {}).items()
                  if group in self.config.get('allowed_groups', [])}
        active = [group for group in groups if group in self.config.get('documents', {}).get('bindings', {})] if configured and cfg.get('enabled') and self.config.get('documents', {}).get('enabled') else []
        return {'enabled': bool(cfg.get('enabled')), 'credential_saved': configured,
                'groups': groups, 'active_groups': active,
                'verified_types': ['doc', 'smartcanvas', 'sheet', 'slide', 'smartsheet', 'form', 'mind', 'flowchart', 'board'],
                'login_validity': 'not_checked', 'flow': self.qq_documents_current_flow()}

    @staticmethod
    def qq_documents_unbind(cfg, group):
        native = cfg.get('documents', {}).get('qq_native')
        if native is not None:
            native.get('groups', {}).pop(group, None)
            if not native.get('groups'):
                native['enabled'] = False

    @staticmethod
    def qq_documents_browser_state(path):
        raw = private_read(path, 256 * 1024)
        value = json.loads(raw)
        require(isinstance(value, dict) and set(value) <= {'cookies', 'origins'}
                and isinstance(value.get('cookies'), list) and 0 < len(value['cookies']) <= 128
                and value.get('origins', []) == [], 'QQ 网页登录态格式不正确。')
        for cookie in value['cookies']:
            require(isinstance(cookie, dict) and cookie.get('domain') in {'docs.qq.com', '.docs.qq.com', 'qq.com', '.qq.com'}
                    and isinstance(cookie.get('name'), str) and isinstance(cookie.get('value'), str)
                    and 0 < len(cookie['name']) <= 256 and len(cookie['value']) <= 16384,
                    'QQ 网页登录态格式不正确。')
        return raw

    def qq_documents_flow_root(self, create=False):
        root = self.config_path.parent / 'qq-documents-login'
        if create:
            root.mkdir(mode=0o700, exist_ok=True)
        if root.exists() or root.is_symlink():
            info = root.lstat()
            require(stat.S_ISDIR(info.st_mode) and info.st_uid == os.getuid()
                    and stat.S_IMODE(info.st_mode) == 0o700, 'QQ 登录目录权限异常。', 500)
        return root

    def qq_documents_current_flow(self):
        current = self.qq_documents_flow_root() / 'current.json'
        if not current.exists():
            return None
        value = json.loads(private_read(current))
        return self.qq_documents_flow(value.get('flow_id'))[2]

    def qq_documents_flow(self, flow_id):
        require(isinstance(flow_id, str) and re.fullmatch(r'[a-f0-9]{32}', flow_id), '登录流程标识无效。')
        folder = self.qq_documents_flow_root() / ('flow_' + flow_id)
        metadata = json.loads(private_read(folder / 'metadata.json'))
        require(metadata.get('flow_id') == flow_id and isinstance(metadata.get('nonce'), str)
                and re.fullmatch(r'[a-f0-9]{64}', metadata['nonce'])
                and isinstance(metadata.get('group'), str) and ID.fullmatch(metadata['group'])
                and type(metadata.get('expires_at')) is int, '登录流程记录不正确。', 409)
        status = json.loads(private_read(folder / 'status.json'))
        require(isinstance(status, dict) and status.get('nonce') == metadata['nonce']
                and status.get('state') in {'starting', 'waiting_scan', 'authorized', 'expired', 'failed'},
                '登录状态未通过校验。', 409)
        stage = status['state']
        if metadata.get('finished'):
            stage = 'saved'
        elif (folder / 'cancel').exists():
            stage = 'cancelled'
        elif metadata['expires_at'] <= int(time.time()) or metadata['group'] not in self.config.get('allowed_groups', []):
            stage = 'expired'
        if stage == 'authorized':
            require(status.get('group_code') == metadata['group_code']
                    and sorted(status.get('verified_group_codes', [])) == sorted(metadata['group_codes']),
                    '登录账号的群权限未通过校验。', 409)
        reasons = {'group_access_denied': '登录账号无法访问已绑定的某个群，请检查账号或先解除该群登记。',
                   'login_expired': '二维码已过期，请重新登录。', 'cancelled': '已取消本次登录。',
                   'browser_failed': '登录页面暂不可用，请稍后重新登录。',
                   'login_failed': '登录未完成，请重新登录。'}
        public = {'flow_id': flow_id, 'state': stage, 'group': metadata['group'],
                  'expires_at': metadata['expires_at'], 'has_qr': stage == 'waiting_scan' and (folder / 'qr.png').is_file(),
                  'message': reasons.get(status.get('reason'), '登录未完成，请重新登录。') if stage == 'failed' else ''}
        return folder, metadata, public

    def qq_documents_qr(self, flow_id):
        folder, _, state = self.qq_documents_flow(flow_id)
        require(state['state'] == 'waiting_scan', '当前登录流程没有可用二维码。', 409)
        raw = private_read(folder / 'qr.png', 512 * 1024)
        require(raw.startswith(b'\x89PNG\r\n\x1a\n'), '登录二维码格式不正确。', 500)
        return raw

    def qq_documents_action(self, body):
        action = body.get('action')
        self.load()
        if action == 'native_auth_start':
            require(set(body) == {'action', 'group', 'group_code'})
            group = identifier(body['group'])
            require(group in self.config.get('allowed_groups', [])
                    and group in self.config.get('documents', {}).get('bindings', {}), '请先登记该群并绑定腾讯文档目录。')
            code = body['group_code']
            require(isinstance(code, str) and re.fullmatch(r'[1-9][0-9]{4,11}', code), '请填写实际 QQ 群号。')
            old = self.qq_documents_current_flow()
            require(not old or old['state'] in {'saved', 'failed', 'expired', 'cancelled'}, '请先完成或取消当前登录。', 409)
            if self.qq_document_login is not None:
                require(self.qq_document_login.poll() is not None, '正在关闭上一登录页面，请稍后重试。', 409)
            runtime = self.config.get('integrations', {}).get('browser', {}).get('runtime')
            require(isinstance(runtime, dict) and all(isinstance(runtime.get(key), str) for key in ('node', 'node_modules', 'chromium')), '请先安装公共网页浏览运行环境。')
            node = Path(runtime['node'])
            require(node.is_absolute() and node.is_file() and os.access(node, os.X_OK), '浏览运行环境不可用。')
            info = node.stat()
            require(info.st_uid in {0, os.getuid()} and not info.st_mode & 0o022, '浏览运行环境权限异常。')
            helper = Path(__file__).with_name('clawmingle_qq_documents_login.mjs')
            require(helper.is_file() and not helper.is_symlink(), 'QQ 登录组件尚未安装。', 503)
            info = helper.stat()
            require(info.st_uid == os.getuid() and not info.st_mode & 0o022, 'QQ 登录组件权限异常。', 500)
            root = self.qq_documents_flow_root(create=True)
            flow_id = secrets.token_hex(16)
            folder = root / ('flow_' + flow_id)
            folder.mkdir(mode=0o700)
            groups = self.config.get('documents', {}).get('qq_native', {}).get('groups', {})
            require(all(existing == group or other != code for existing, other in groups.items()), '该 QQ 群号已绑定其他会话。')
            group_codes = sorted(set([value for key, value in groups.items() if key != group] + [code]))
            require(len(group_codes) <= 64, '群列表登记一次最多验证 64 个群。')
            metadata = {'flow_id': flow_id, 'nonce': secrets.token_hex(32), 'group': group,
                        'group_code': code, 'group_codes': group_codes, 'expires_at': int(time.time()) + 900}
            atomic_write(folder / 'metadata.json', json.dumps(metadata))
            atomic_write(folder / 'input.json', json.dumps({key: metadata[key] for key in ('nonce', 'group_code', 'group_codes', 'expires_at')}))
            atomic_write(folder / 'runtime.json', json.dumps(runtime))
            atomic_write(folder / 'status.json', json.dumps({'state': 'starting', 'nonce': metadata['nonce']}))
            atomic_write(root / 'current.json', json.dumps({'flow_id': flow_id}))
            try:
                self.qq_document_login = subprocess.Popen([str(node), str(helper), '--flow-dir', str(folder), '--runtime', str(folder / 'runtime.json')],
                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    start_new_session=True, cwd=str(folder), env={'PATH': '/usr/bin:/bin', 'HOME': str(folder), 'TMPDIR': str(folder)})
            except OSError:
                atomic_write(folder / 'status.json', json.dumps({'state': 'failed', 'nonce': metadata['nonce'], 'reason': 'browser_failed'}))
                raise AdminError('QQ 登录页面未能启动，请检查浏览运行环境。', 502) from None
            return {'ok': True, 'flow': self.qq_documents_flow(flow_id)[2]}
        if action in {'native_auth_status', 'native_auth_cancel', 'native_auth_finish'}:
            require(set(body) == {'action', 'flow_id'})
            folder, metadata, public = self.qq_documents_flow(body['flow_id'])
            if action == 'native_auth_status':
                result = {'ok': True, 'flow': public}
                if public['has_qr']:
                    result['qr_data_url'] = 'data:image/png;base64,' + base64.b64encode(self.qq_documents_qr(body['flow_id'])).decode('ascii')
                return result
            if action == 'native_auth_cancel':
                atomic_write(folder / 'cancel', metadata['nonce'])
                return {'ok': True, 'flow': self.qq_documents_flow(body['flow_id'])[2]}
            require(public['state'] in {'authorized', 'saved'}, '请先完成扫码登录并通过群权限检查。', 409)
            if public['state'] == 'saved':
                return {'ok': True, 'message': '这次登录已保存。'}
            raw = self.qq_documents_browser_state(folder / 'browser-state.json')
            credential_dir = self.config_path.parent / 'qq-documents-browser'
            credential_dir.mkdir(mode=0o700, exist_ok=True)
            credential = credential_dir / 'browser-state.json'
            with self.transaction() as save:
                cfg = self.load()
                docs = cfg.setdefault('documents', {})
                require(metadata['group'] in cfg.get('allowed_groups', []) and metadata['group'] in docs.get('bindings', {}), '群绑定已变化，请重新登录。', 409)
                native = docs.setdefault('qq_native', {})
                groups = native.setdefault('groups', {})
                require(set(value for key, value in groups.items() if key != metadata['group']) <= set(metadata['group_codes'])
                        and metadata['expires_at'] > int(time.time())
                        and all(key == metadata['group'] or code != metadata['group_code'] for key, code in groups.items()),
                        '群绑定已变化或登录流程已过期，请重新登录。', 409)
                native.update({'enabled': True, 'browser_state_file': str(credential)})
                groups[metadata['group']] = metadata['group_code']
                save(credential, raw)
                save(self.config_path, toml_dump(cfg))
                metadata['finished'] = True
                save(folder / 'metadata.json', json.dumps(metadata))
            # The helper has finished; keep the credential only in its final protected location.
            (folder / 'browser-state.json').unlink(missing_ok=True)
            return {'ok': True, 'message': 'QQ 网页登录已保存，所选群已启用原生列表登记；九类在线文档已验证。'}
        require(action in {'native_enable', 'native_disable', 'native_unbind'})
        require(set(body) == ({'action', 'group'} if action == 'native_unbind' else {'action'}))
        with self.transaction() as save:
            cfg = self.load()
            native = cfg.setdefault('documents', {}).setdefault('qq_native', {})
            if action == 'native_unbind':
                require(body['group'] in cfg.get('allowed_groups', []), '只能解除已登记群的绑定。')
                self.qq_documents_unbind(cfg, body['group'])
            elif action == 'native_enable':
                require(native.get('groups') and native.get('browser_state_file'), '请先完成 QQ 网页登录和群绑定。')
                self.qq_documents_browser_state(Path(native['browser_state_file']))
                native['enabled'] = True
            else:
                native['enabled'] = False
            save(self.config_path, toml_dump(cfg))
        return {'ok': True, 'message': 'QQ 群列表登记设置已更新；已有群在线文档保持不变。'}

    def documents_action(self, body):
        action = body.get('action')
        if isinstance(action, str) and action.startswith('native_'):
            return self.qq_documents_action(body)
        require(action in {'auth_start', 'auth_finish', 'token', 'folders', 'create_folder', 'bind', 'unbind', 'disable'})
        self.load()
        pending = self.config_path.parent / 'tencent-docs-auth.json'
        credential = self.config_path.parent / 'tencent-docs.token'
        if action == 'auth_start':
            require(set(body) == {'action'})
            code = secrets.token_hex(8)
            atomic_write(pending, json.dumps({'code': code, 'expires': int(time.time()) + 300}))
            url = 'https://docs.qq.com/scenario/open-claw.html?' + urllib.parse.urlencode(
                {'nlc': '1', 'authType': '1', 'code': code, 'mcp_source': 'desktop'})
            return {'ok': True, 'url': url, 'expires_in': 300}
        if action in {'auth_finish', 'token'}:
            if action == 'auth_finish':
                require(set(body) == {'action'})
                require(pending.exists(), '请先开始腾讯文档授权。')
                flow = json.loads(private_read(pending))
                require(flow.get('expires', 0) > time.time(), '授权链接已过期，请重新开始。')
                from qq_smoke import NoRedirect
                url = 'https://docs.qq.com/oauth/v2/mcp/token/get?' + urllib.parse.urlencode({'code': flow['code']})
                try:
                    with urllib.request.build_opener(NoRedirect()).open(url, timeout=15) as response:
                        raw = response.read(32769)
                    require(len(raw) <= 32768, '授权响应过大。', 502)
                    value = json.loads(raw)
                except (OSError, ValueError):
                    raise AdminError('腾讯文档授权查询未成功，请稍后检查。', 502) from None
                token = value.get('data', {}).get('token') if isinstance(value, dict) and isinstance(value.get('data'), dict) else None
                require(isinstance(token, str) and token, '尚未完成授权或授权已过期。请完成官方页面操作后重试。')
            else:
                require(set(body) == {'action', 'token'})
                token = body['token']
            require(isinstance(token, str) and 1 <= len(token) <= 8192 and not any(c.isspace() for c in token), 'Token 格式无效。')
            with self.transaction() as save:
                cfg = self.load()
                save(credential, token)
                docs = cfg.setdefault('documents', {})
                docs['credential_file'] = str(credential)
                docs['enabled'] = True
                selected = cfg.setdefault('enabled_tools', list(TOOLS))
                if 'group_documents' not in selected:
                    selected.append('group_documents')
                save(self.config_path, toml_dump(cfg))
            if pending.exists():
                atomic_write(pending, '{}')
            return {'ok': True, 'message': '授权已保存在独立私有文件中；还需为群聊绑定专用文档目录。'}
        if action in {'folders', 'create_folder'}:
            allowed = {'action', 'parent', 'start'} if action == 'folders' else {'action', 'title'}
            require(set(body) <= allowed)
            require(self.documents_public()['authorized'], '请先完成腾讯文档授权。')
            # No chat/agent tool is exposed here. Only authenticated Dashboard
            # administrators may enumerate account folders or create a root folder.
            try:
                result = subprocess.run([str(self.binary), 'documents-admin', '--config', str(self.config_path)],
                    input=json.dumps(body).encode(), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                    timeout=60, check=False)
            except subprocess.TimeoutExpired:
                raise AdminError('文档服务等待超时；如在创建目录，请先刷新目录，避免重复创建。', 502) from None
            require(result.returncode == 0 and len(result.stdout) <= 512 * 1024,
                    '文档服务未确认操作，请检查授权；创建结果不明时请先刷新目录。', 502)
            return json.loads(result.stdout)
        if action in {'bind', 'unbind'}:
            require(set(body) == ({'action', 'group', 'folder'} if action == 'bind' else {'action', 'group'}))
            require(body['group'] in self.config.get('allowed_groups', []), '只能绑定已登记的群聊。')
            if action == 'bind':
                require(isinstance(body['folder'], str) and ID.fullmatch(body['folder']), '请选择或填写有效的专用目录 ID。')
                require(self.documents_public()['authorized'], '请先授权腾讯文档。')
        else:
            require(set(body) == {'action'})
        with self.transaction() as save:
            cfg = self.load()
            docs = cfg.setdefault('documents', {})
            if action == 'disable':
                docs['enabled'] = False
            else:
                bindings = docs.setdefault('bindings', {})
                if action == 'bind':
                    require(all(group == body['group'] or folder != body['folder'] for group, folder in bindings.items()),
                            '该目录已绑定其他群聊，请为每个群选择不同的专用目录。')
                    bindings[body['group']] = body['folder']
                    docs['enabled'] = True
                    selected = cfg.setdefault('enabled_tools', list(TOOLS))
                    if 'group_documents' not in selected:
                        selected.append('group_documents')
                else:
                    bindings.pop(body['group'], None)
                    self.qq_documents_unbind(cfg, body['group'])
            save(self.config_path, toml_dump(cfg))
        return {'ok': True, 'message': '群文档设置已更新。'}

    def post(self, path, body):
        require(isinstance(body, dict), '只接受 JSON 对象。')
        if path == '/api/config' and body.get('section') == 'capabilities':
            action = body.get('action')
            if action in {'refresh', 'status'}:
                require(set(body) == {'section', 'action'})
                return self.start_capability_refresh() if action == 'refresh' else self.capability_job_status()
            require(not self.capability_busy(), '正在刷新账号应用目录，请完成后再修改设置。', 409)
            with self.mutex:
                require(not self.capability_busy(), '正在刷新账号应用目录，请完成后再修改设置。', 409)
                if action == 'catalog':
                    require(set(body) == {'section', 'action'})
                    return self.capability_catalog()
                return self.capability_action({key: value for key, value in body.items() if key != 'section'})
        require(not self.capability_busy(), '正在刷新账号应用目录，请完成后再修改设置。', 409)
        with self.mutex:
            require(not self.capability_busy(), '正在刷新账号应用目录，请完成后再修改设置。', 409)
            handlers = {'/api/service': lambda b: self.control(b.get('action')),
                        '/api/config': self.update_config, '/api/conversation': self.conversation,
                        '/api/login': self.login, '/api/check': self.check, '/api/memory': self.memory,
                        '/api/documents': self.documents_action, '/api/capability': self.capability_action}
            require(path in handlers, '没有此管理接口。', 404)
            require(isinstance(body, dict), '只接受 JSON 对象。')
            # The established dashboard proxy already authorizes this admin route.
            # Library mutations use their own file lock, without restarting the bot.
            if path == '/api/config' and body.get('section') == 'sticker_library':
                return self.stickers({key: value for key, value in body.items() if key != 'section'})
            return handlers[path](body)


def handler_for(manager, token, *, local_auth=None, setup=None, manager_factory=None):
    manager_lock = threading.Lock()

    def current_manager():
        nonlocal manager
        require(setup is None or not setup.pending, '请先完成首次初始化。', 409)
        with manager_lock:
            if manager is None:
                require(manager_factory is not None, '管理配置尚未就绪。', 409)
                manager = manager_factory()
        return manager

    class Handler(BaseHTTPRequestHandler):
        server_version = 'ClawMingle'
        def setup(self):
            super().setup()
            self.connection.settimeout(15)

        def log_message(self, *_):
            pass  # Do not put query strings, auth codes or credential data in logs.

        def send(self, status, data, content_type='application/json; charset=utf-8', headers=()):
            raw = json.dumps(data, ensure_ascii=False).encode() if isinstance(data, dict) else data
            self.send_response(status)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(raw)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('X-Content-Type-Options', 'nosniff')
            self.send_header('Referrer-Policy', 'no-referrer')
            self.send_header('Content-Security-Policy', "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'self'; base-uri 'self'; form-action 'self'")
            for name, value in headers:
                self.send_header(name, value)
            self.end_headers()
            self.wfile.write(raw)

        def authorized(self):
            if local_auth is not None:
                self.local_session = local_auth.session(self.headers)
                return self.local_session is not None
            given = self.headers.get('X-CM-Dashboard-Proxy', '')
            return bool(given) and hmac.compare_digest(given.encode(), token)

        def unauthorized(self):
            if local_auth is not None:
                return self.send(401, {'error': '请先登录管理台。', 'authMode': 'standalone', 'loginRequired': True})
            return self.send(401, {'error': '请先通过已配置的管理入口登录。'})

        def asset(self, name, kind):
            assets = manager.assets if manager is not None else setup.assets
            return self.send(200, (assets / name).read_bytes(), kind)

        def do_GET(self):
            try:
                if local_auth is not None:
                    local_auth.check_request(self.headers)
                    public_files = {'/login': ('login.html', 'text/html; charset=utf-8'),
                                    '/assets/login.js': ('login.js', 'text/javascript; charset=utf-8'),
                                    '/assets/login.css': ('login.css', 'text/css; charset=utf-8')}
                    if self.path == '/auth/status':
                        return self.send(200, local_auth.status())
                    if self.path in public_files:
                        return self.asset(*public_files[self.path])
                if not self.authorized():
                    if local_auth is not None and self.path in {'/', '/setup'}:
                        return self.send(302, b'', headers=[('Location', '/login')])
                    return self.unauthorized()
                if local_auth is not None:
                    if self.path == '/api/bootstrap':
                        return self.send(200, {'csrfToken': self.local_session['csrfToken'],
                                              'authMode': 'standalone', 'setupRequired': bool(setup and setup.pending)})
                    if self.path == '/api/setup':
                        require(setup is not None, '首次初始化不可用。', 404)
                        return self.send(200, setup.state())
                    if self.path == '/setup':
                        if setup is None or not setup.pending:
                            return self.send(302, b'', headers=[('Location', '/')])
                        return self.asset('setup.html', 'text/html; charset=utf-8')
                    if self.path in {'/assets/setup.js', '/assets/setup.css'}:
                        name = self.path.rsplit('/', 1)[1]
                        return self.asset(name, 'text/javascript; charset=utf-8' if name.endswith('.js') else 'text/css; charset=utf-8')
                    if self.path == '/' and setup and setup.pending:
                        return self.send(302, b'', headers=[('Location', '/setup')])
                active_manager = current_manager()
                if self.path == '/health':
                    return self.send(200, {'ok': True})
                if self.path == '/api/state':
                    require(not active_manager.capability_busy(), '正在刷新账号应用目录，请稍后读取状态。', 409)
                    with active_manager.mutex:
                        value = active_manager.state()
                    return self.send(200, value)
                if self.path == '/api/capabilities':
                    require(not active_manager.capability_busy(), '正在刷新账号应用目录，请使用操作状态轮询。', 409)
                    with active_manager.mutex:
                        value = active_manager.capability_catalog()
                    return self.send(200, value)
                files = {'/': ('index.html', 'text/html; charset=utf-8'),
                         '/assets/app.js': ('app.js', 'text/javascript; charset=utf-8'),
                         '/assets/app.css': ('app.css', 'text/css; charset=utf-8')}
                require(self.path in files, '没有此页面。', 404)
                file, kind = files[self.path]
                self.send(200, (active_manager.assets / file).read_bytes(), kind)
            except Exception as error:
                self.error(error)

        def do_POST(self):
            try:
                if local_auth is not None:
                    local_auth.check_request(self.headers, write=True)
                login = local_auth is not None and self.path in {'/auth/login', '/auth/enroll'}
                if not login:
                    if not self.authorized():
                        return self.unauthorized()
                    if local_auth is not None:
                        local_auth.check_csrf(self.headers, self.local_session)
                require(self.headers.get('Content-Type') == 'application/json', '只接受 JSON 请求。', 415)
                require(not self.headers.get('Transfer-Encoding'), '不接受分块请求。')
                try:
                    length = int(self.headers.get('Content-Length', '0'))
                except ValueError:
                    raise AdminError('请求长度无效。') from None
                limit = 3 * 1024 * 1024 if self.path == '/api/config' else 65536
                require(0 < length <= limit, '请求体大小不正确。', 413)
                try:
                    body = json.loads(self.rfile.read(length))
                except (ValueError, UnicodeError):
                    raise AdminError('JSON 格式无效。') from None
                if length > 65536:
                    require(isinstance(body, dict) and self.path == '/api/config'
                            and body.get('section') == 'sticker_library', '请求体大小不正确。', 413)
                if login:
                    session = local_auth.authenticate(body, enroll=self.path == '/auth/enroll')
                    return self.send(200, {'ok': True}, headers=[('Set-Cookie', local_auth.cookie(session))])
                if local_auth is not None and self.path == '/auth/logout':
                    require(body == {}, '退出请求无效。')
                    local_auth.logout(self.local_session)
                    return self.send(200, {'ok': True}, headers=[('Set-Cookie', local_auth.cookie('', clear=True))])
                if local_auth is not None and self.path == '/api/setup':
                    require(setup is not None, '首次初始化不可用。', 404)
                    return self.send(200, setup.post(body))
                self.send(200, current_manager().post(self.path, body))
            except Exception as error:
                self.error(error)

        def error(self, error):
            setup_errors = ()
            if setup is not None:
                from admin_setup import SetupError
                setup_errors = (SetupError,)
            if isinstance(error, (AdminError, admin_auth.AuthError, *setup_errors)):
                self.send(error.status, {'error': str(error)})
            else:
                self.send(500, {'error': '操作未完成；未展示可能含私有信息的底层错误。'})
    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path.home() / '.config/clawmingle/social.toml')
    authentication = parser.add_mutually_exclusive_group(required=True)
    authentication.add_argument('--token-file', type=Path, help='Trusted authentication proxy only.')
    authentication.add_argument('--standalone', action='store_true', help='Local/SSH-tunnel login; no external dashboard required.')
    parser.add_argument('--init', action='store_true', help='Allow first browser password setup using a private one-time pairing file.')
    parser.add_argument('--binary', type=Path, default=Path.home() / '.local/lib/clawmingle/clawmingle')
    source_assets = Path(__file__).resolve().parent / 'assets'
    if not source_assets.is_dir():
        source_assets = Path(__file__).resolve().parents[1] / 'admin'
    parser.add_argument('--assets', type=Path, default=source_assets)
    parser.add_argument('--port', type=int, default=17430)
    args = parser.parse_args()
    require(1024 <= args.port <= 65535)
    require(not args.init or args.standalone, '--init 仅用于独立管理台。')
    args.config = args.config.expanduser().absolute()
    args.binary = args.binary.expanduser().absolute()
    args.assets = args.assets.expanduser().absolute()
    setup = None
    if args.standalone:
        from admin_setup import Setup
        require(not any((parent / '.git').exists() for parent in args.config.resolve().parents),
                '管理配置和认证必须位于 Git 仓库之外。')
        setup = Setup(args.config, args.binary, args.assets)
        local_auth = admin_auth.LocalAuth(args.config.parent, args.port, initialize=args.init)
        factory = lambda: Manager(args.config, args.binary, args.assets)
        manager = None if setup.pending else factory()
        handler = handler_for(manager, None, local_auth=local_auth, setup=setup, manager_factory=factory)
    else:
        token = private_read(args.token_file.expanduser()).strip()
        require(len(token) >= 32, '代理认证令牌无效。')
        manager = Manager(args.config, args.binary, args.assets)
        handler = handler_for(manager, token)
    server = ThreadingHTTPServer(('127.0.0.1', args.port), handler)
    server.daemon_threads = True
    if args.standalone:
        print(f'ClawMingle 管理台：http://127.0.0.1:{args.port}', flush=True)
        if local_auth.credentials is None:
            print(f'首次配对文件（仅本人终端查看，不要发给 Agent 或贴到聊天）：{local_auth.bootstrap_path}', flush=True)
    try:
        server.serve_forever(poll_interval=0.5)
    finally:
        server.server_close()
        if setup is not None:
            setup.close()


if __name__ == '__main__':
    try:
        main()
    except Exception:
        raise SystemExit('ClawMingle 管理服务无法启动；请检查受保护的配置。') from None
