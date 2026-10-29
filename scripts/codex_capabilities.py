"""Bounded, non-executing discovery of transferable local Codex capabilities.

Discovery never runs MCP commands or header helpers, and never reads auth.json.
Private source details stay in records used by the administrator backend only.
"""
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import stat
import tomllib
import urllib.parse

NAME = re.compile(r'[a-z0-9][a-z0-9_-]{0,31}\Z')
TOOL = re.compile(r'[A-Za-z0-9_.-]{1,128}\Z')
HOOKS_LIMIT = 65536
HOOK_EVENTS = {'PreToolUse', 'PermissionRequest', 'PostToolUse', 'PreCompact',
               'PostCompact', 'SessionStart', 'SessionEnd', 'UserPromptSubmit',
               'SubagentStart', 'SubagentStop', 'Stop', 'Interrupt'}
PUBLIC_FIELDS = ('id', 'kind', 'name', 'description', 'personal_account', 'available',
                 'reason', 'tools', 'registration_id', 'registered', 'enabled')


def source_read(path, limit):
    """Read an operator-owned regular file, refusing links and writable ancestors."""
    path = Path(path).absolute()
    for parent in [*reversed(path.parents), path]:
        info = parent.lstat()
        sticky_root = stat.S_ISDIR(info.st_mode) and info.st_uid == 0 and info.st_mode & stat.S_ISVTX
        if stat.S_ISLNK(info.st_mode) or info.st_uid not in (0, os.getuid()) or (info.st_mode & 0o022 and not sticky_root):
            raise ValueError('untrusted capability source')
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > limit:
            raise ValueError('invalid capability source')
        value = stream.read(limit + 1)
    if len(value) > limit:
        raise ValueError('capability source too large')
    return value


def public_endpoint(value):
    if not isinstance(value, str) or len(value) > 2048 or value.strip() != value or any(ord(c) < 32 for c in value):
        return False
    try:
        url = urllib.parse.urlsplit(value)
        host = (url.hostname or '').lower().rstrip('.')
        if url.scheme != 'https' or url.port not in (None, 443) or url.username or url.password or url.query or url.fragment:
            return False
        if not host or any(host == suffix or host.endswith('.' + suffix) for suffix in
                           ('localhost', 'local', 'internal', 'home.arpa', 'lan', 'onion')):
            return False
        try:
            return ipaddress.ip_address(host).is_global
        except ValueError:
            return '.' in host and bool(re.fullmatch(r'[a-z0-9.-]+', host))
    except ValueError:
        return False


def safe_name(value, fallback):
    return value if isinstance(value, str) and 0 < len(value) <= 80 and not any(ord(c) < 32 or c in '/\\?@=' for c in value) else fallback


def entry(kind, source, name):
    digest = hashlib.sha256((kind + ':' + source).encode()).hexdigest()[:20]
    return {'id': kind + '-' + digest, 'kind': kind, 'name': name, 'description': '',
            'personal_account': True, 'available': False, 'reason': '', 'tools': [],
            'registration_id': 'import-' + digest, 'registered': False, 'enabled': False}


def hooks_record():
    record = entry('hooks', 'user-hooks', 'Codex 命令 Hooks')
    record.update(personal_account=False, description='可选的任务事件命令；导入后保持停用，启用后作用于后续原生任务。')
    return record


def hooks_document(raw):
    """Validate the Linux command subset without evaluating commands or regexes."""
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError('duplicate hooks key')
            result[key] = value
        return result
    def string(value, limit):
        return (isinstance(value, str) and len(value) <= limit and '\0' not in value
                and not any(0xd800 <= ord(character) <= 0xdfff for character in value))
    def check(valid):
        if not valid:
            raise ValueError('unsupported command hooks document')
    check(isinstance(raw, bytes) and len(raw) <= HOOKS_LIMIT)
    value = json.loads(raw.decode('utf-8'), object_pairs_hook=unique)
    check(isinstance(value, dict) and set(value) <= {'description', 'hooks'})
    check('description' not in value or string(value['description'], 4096))
    events = value.get('hooks')
    check(isinstance(events, dict) and set(events) <= HOOK_EVENTS)
    count = 0
    for groups in events.values():
        check(isinstance(groups, list) and len(groups) <= 32)
        for group in groups:
            check(isinstance(group, dict) and set(group) <= {'matcher', 'hooks'})
            check('matcher' not in group or string(group['matcher'], 2048))
            handlers = group.get('hooks')
            check(isinstance(handlers, list) and len(handlers) <= 32)
            for handler in handlers:
                check(isinstance(handler, dict) and set(handler) <= {
                    'type', 'command', 'timeout', 'async', 'statusMessage', 'additionalContextLimit'})
                check(handler.get('type') == 'command' and string(handler.get('command'), 8192)
                      and bool(handler['command'].strip()))
                if 'timeout' in handler:
                    check(type(handler['timeout']) is int and 1 <= handler['timeout'] <= 600)
                if 'async' in handler:
                    check(type(handler['async']) is bool)
                check('statusMessage' not in handler or string(handler['statusMessage'], 512))
                if 'additionalContextLimit' in handler:
                    check(type(handler['additionalContextLimit']) is int
                          and 0 <= handler['additionalContextLimit'] <= (1 << 63) - 1)
                count += 1
                check(count <= 32)
    check(count > 0)
    return value


def hooks_read(path):
    raw = source_read(path, HOOKS_LIMIT)
    info = Path(path).lstat()
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
        raise ValueError('hooks must be owner-only')
    hooks_document(raw)
    return raw


def discover_local(home):
    home = Path(home)
    records, notices = [], []
    try:
        config = tomllib.loads(source_read(home / '.codex/config.toml', 1024 * 1024).decode())
    except FileNotFoundError:
        config = {}
    except (ValueError, OSError, UnicodeError):
        config = {}
        notices.append('本机 Codex 配置无法安全读取，未导入其中的连接。')
    servers = config.get('mcp_servers', {})
    if not isinstance(servers, dict):
        servers = {}
    for index, (key, server) in enumerate(servers.items()):
        if index >= 64:
            break
        if not isinstance(server, dict):
            continue
        record = entry('mcp', key, safe_name(key, '本机 MCP'))
        record['description'] = '本机 Codex 中登记的 MCP；可选择工具后导入。'
        record['_server'] = server
        record['tools'] = [{'name': name, 'description': ''} for name in server.get('enabled_tools', [])
                           if isinstance(name, str) and TOOL.fullmatch(name)][:32] if isinstance(server.get('enabled_tools', []), list) else []
        headers = server.get('http_headers', {})
        env_headers = server.get('env_http_headers', {})
        has_helper = any(server.get(key) for key in ('http_headers_helper', 'headers_helper'))
        if server.get('command'):
            record['reason'] = '此连接需要本机进程；当前自动迁移支持公共 HTTPS MCP。'
        elif has_helper:
            record['reason'] = '此连接依赖自定义鉴权程序；请使用对应的专用连接设置。'
        elif not public_endpoint(server.get('url')):
            record['reason'] = '只支持不含账号、查询参数的公共 HTTPS 地址。'
        elif not isinstance(headers, dict) or not isinstance(env_headers, dict) or env_headers or set(headers) - {'Authorization'}:
            record['reason'] = '此连接的请求头方式暂不支持自动迁移。'
        elif headers and (not isinstance(headers.get('Authorization'), str) or not headers['Authorization'].startswith('Bearer ')):
            record['reason'] = '此连接的鉴权方式暂不支持自动迁移。'
        elif server.get('bearer_token_env_var') and not re.fullmatch(r'[A-Z][A-Z0-9_]{0,127}', str(server['bearer_token_env_var'])):
            record['reason'] = '此连接的凭据环境变量无效。'
        elif server.get('bearer_token_env_var') and not os.environ.get(server['bearer_token_env_var']):
            record['reason'] = '管理服务尚未配置该连接所需的凭据环境变量。'
        else:
            record['available'] = True
            record['personal_account'] = bool(headers or server.get('bearer_token_env_var'))
        records.append(record)
    for root in (home / '.agents/skills', home / '.codex/skills'):
        if not root.exists() or root.is_symlink():
            continue
        try:
            folders = sorted(root.iterdir())[:128]
        except OSError:
            continue
        for folder in folders:
            if not NAME.fullmatch(folder.name) or not folder.is_dir() or folder.is_symlink():
                continue
            record = entry('skill', str(folder), folder.name)
            record['registration_id'] = folder.name
            record['description'] = '本机 Codex Skill；导入后在 Skills 中选择启用范围。'
            record['_path'] = folder / 'SKILL.md'
            try:
                raw = source_read(record['_path'], 16384)
                raw.decode()
                if any(item.name != 'SKILL.md' for item in folder.iterdir()):
                    record['reason'] = '包含脚本或资源的技能包请通过固定版本的技能包安装流程导入。'
                elif not raw.strip():
                    record['reason'] = 'Skill 内容为空。'
                else:
                    record['available'] = True
                    record['_digest'] = hashlib.sha256(raw).hexdigest()
            except (OSError, ValueError, UnicodeError):
                record['reason'] = 'Skill 文件不符合可迁移文件要求。'
            records.append(record)
    hooks = home / '.codex/hooks.json'
    if os.path.lexists(hooks):
        record = hooks_record()
        record['_path'] = hooks
        try:
            raw = hooks_read(hooks)
            record.update(available=True, _digest=hashlib.sha256(raw).hexdigest())
        except (OSError, ValueError, UnicodeError, RecursionError):
            record['reason'] = '只支持本人所有且权限为 0600 的命令 Hooks；最多 32 条，不接受其他类型、Windows 命令或未知字段。'
        records.append(record)
    return records, notices


def discover_apps(value):
    if not isinstance(value, dict):
        raise ValueError('invalid app catalogue')
    records = []
    apps = value.get('apps', []) if isinstance(value, dict) else []
    if not isinstance(apps, list):
        raise ValueError('invalid app catalogue')
    for app in apps[:64]:
        if not isinstance(app, dict):
            continue
        app_id, namespace = app.get('id'), app.get('tool_namespace')
        if not isinstance(app_id, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,200}', app_id):
            continue
        record = entry('app', app_id, safe_name(app.get('name'), 'Codex 账号应用'))
        record['description'] = '通过机器人独立 Codex 登录使用；导入后仍需为会话授权。'
        record['_app_id'] = app_id
        record['_namespace'] = namespace
        record['tools'] = [{'name': tool['name'], 'description': ''} for tool in app.get('tools', [])
                           if isinstance(tool, dict) and isinstance(tool.get('name'), str) and TOOL.fullmatch(tool['name'])][:128]
        # Native app metadata may use bare or dotted original MCP tool names.
        # A null namespace means those names must be sent without a prefix.
        record['available'] = (namespace is None or isinstance(namespace, str) and bool(TOOL.fullmatch(namespace))) and bool(record['tools'])
        record['reason'] = '' if record['available'] else '此账号应用没有可迁移的只读工具，请检查机器人的账号连接与应用权限。'
        records.append(record)
    unavailable = value.get('unavailable', []) if isinstance(value, dict) else []
    if not isinstance(unavailable, list):
        raise ValueError('invalid unavailable app catalogue')
    reasons = {'metadata_unavailable': '账号应用信息尚不可用，请检查机器人登录后重试。',
               'app_unavailable': '此应用尚未在机器人账号中连接或授权，请先完成应用连接。',
               'read_only_namespace_unavailable': '此账号应用没有可迁移的只读工具，请检查账号权限；当前不会开放写入工具。',
               'namespace_ambiguous': '此应用的工具归属尚无法确定，暂不能安全导入。'}
    known = {record['_app_id'] for record in records}
    for app in unavailable[:64]:
        if not isinstance(app, dict) or not isinstance(app.get('id'), str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,200}', app['id']) or app['id'] in known:
            continue
        record = entry('app', app['id'], safe_name(app.get('name'), 'Codex 账号应用'))
        record['_app_id'] = app['id']
        record['description'] = '机器人账号中发现的可选应用。'
        record['reason'] = reasons.get(app.get('reason'), '此账号应用暂不可用，请检查机器人的账号连接与工具权限。')
        records.append(record)
    return records


def public_record(record):
    return {key: record[key] for key in PUBLIC_FIELDS}
