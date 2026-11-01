"""Management boundary tests; only temporary files and simulated external services.

Run beside clawmingle_admin.py. A worktree may use PYTHONPATH to point to the
backend under review without copying or modifying that backend.
"""

from contextlib import ExitStack
from email.message import Message
import fcntl
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import tomllib
import unittest
from urllib.parse import urlunsplit
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import clawmingle_admin as admin


class AdminFixture(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        for target in ('subprocess.run', 'subprocess.Popen', 'os.killpg',
                       'urllib.request.urlopen', 'urllib.request.build_opener'):
            self.stack.enter_context(mock.patch(
                'clawmingle_admin.' + target,
                side_effect=AssertionError('Unexpected external operation: ' + target)))
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory(
            prefix='clawmingle-admin-test-')))
        self.config_dir = self.directory('configuration')
        self.home = self.directory('owner-home')
        self.data = self.directory('state')
        self.runtime = self.directory('state/runtime/codex')
        self.directory('state/skills')
        self.directory('state/groups')
        self.directory('state/private/groups')
        self.directory('owner-home/.codex')
        self.assets = self.directory('assets')
        for name in ('index.html', 'app.js', 'app.css'):
            self.write(self.assets / name, 'public fixture ' + name)
        self.credential_values = {
            'qq': 'synthetic-' + 'qq-value',
            'jev': 'synthetic-' + 'jev-value',
            'provider': 'synthetic-' + 'provider-value',
            'personal': 'synthetic-' + 'personal-access',
        }
        self.qq = self.config_dir / 'qq.env'
        self.jev = self.config_dir / 'jev.env'
        self.provider = self.config_dir / 'model.env'
        self.write(self.qq, admin.env_dump({'QQ_APP_ID': '123456', 'QQ_APP_SECRET': self.credential_values['qq']}))
        self.write(self.jev, admin.env_dump({'TYPESAFE_API_KEY': self.credential_values['jev']}))
        self.write(self.provider, admin.env_dump({'OPENAI_API_KEY': self.credential_values['provider']}))
        self.write(self.runtime / 'config.toml', admin.toml_dump({
            'model': 'fixture-model', 'model_reasoning_effort': 'high', 'web_search': 'disabled'}))
        self.write(self.runtime / 'auth.json', '{}')
        self.personal_auth = self.home / '.codex/auth.json'
        self.write(self.personal_auth, json.dumps({'tokens': {'access_token': self.credential_values['personal']}}))
        self.write(self.home / '.codex/config.toml', 'model = "personal-only-model"\n')
        self.write(self.home / '.codex/history.jsonl', '{"personal_history":true}\n')
        self.config_path = self.config_dir / 'clawmingle.toml'
        self.config = {
            'data_dir': str(self.data), 'qq_credentials_file': str(self.qq),
            'allowed_groups': ['test-group'], 'mode': 'mode-1', 'private_enabled': True,
            'private_allowed_users': [], 'command_admins': [], 'max_private_sessions': 16,
            'quiet_ms': 1200, 'max_wait_ms': 4000,
            'model_presets': {'sol': {'model': 'fixture-model', 'reasoning_effort': 'high'}},
            'jev': {'credentials_file': str(self.jev), 'model': 'jev-latest'},
            'codex': {'credentials_file': str(self.provider), 'model': 'fixture-model', 'reasoning_effort': 'high'},
        }
        self.write(self.config_path, admin.toml_dump(self.config))
        self.lock_path = self.data / 'operator.lock'
        self.write(self.lock_path, '')
        self.manager = admin.Manager(self.config_path, self.root / 'fixture-binary', self.assets, self.home)
        self.running = True
        self.actions = []
        self.validation_fails = False
        self.models_probe = None
        self.stack.enter_context(mock.patch.object(self.manager, 'run', side_effect=self.simulate_run))

    def directory(self, relative):
        path = self.root / relative
        path.mkdir(mode=0o700, parents=True, exist_ok=True)
        for parent in [path, *path.parents]:
            if parent == self.root.parent:
                break
            parent.chmod(0o700)
        return path

    @staticmethod
    def write(path, content):
        path = Path(path)
        path.write_bytes(content if isinstance(content, bytes) else content.encode())
        path.chmod(0o600)

    @staticmethod
    def behavior_fixture():
        return {
            'response_instructions': '按问题复杂度选择详略。',
            'formatting_instructions': '适当使用列表和加粗。',
            'categories': {key: {'name': key, 'instructions': '根据请求回答。'} for key in
                           ('casual', 'factual', 'explanation', 'comparison', 'troubleshooting', 'task', 'emotional', 'clarification')},
            'modes': {key: {'name': key, 'participation_instructions': '结合上下文判断。' if key in {'mode-2', 'mode-3'} else '',
                            'reply_instructions': '按请求回答。'} for key in admin.MODES},
        }

    def behavior_body(self):
        return {'section': 'behavior', 'behavior': self.behavior_fixture(), 'max_bubbles': 3, 'max_bubble_chars': 2000,
                'context_windows': {'jev_messages': 25, 'codex_messages': 40, 'jev_bot_messages': 8, 'codex_bot_messages': 12}}

    def simulate_run(self, args, timeout=30):
        if args[:3] == ['systemctl', '--user', 'show']:
            return (f'ActiveState={"active" if self.running else "inactive"}\n'
                    f'SubState={"running" if self.running else "dead"}\n'
                    'NRestarts=0\nMemoryCurrent=1024\n').encode()
        if args[:2] == ['systemctl', '--user'] and args[2] in {'start', 'stop', 'restart'}:
            self.actions.append(args[2])
            self.running = args[2] != 'stop'
            return b''
        if args[0] == str(self.manager.binary) and args[1] == 'check':
            if self.validation_fails:
                raise admin.AdminError('Fixture validation refused.', 502)
            return b'{}'
        if args[0] == str(self.manager.binary) and args[1] == 'behavior':
            config = tomllib.loads(self.config_path.read_text())
            configured_soul = config.get('soul', '')
            default_soul = '统一人格默认测试文案'
            files = config.get('persona_files', False)
            identity = ''
            if files:
                configured_soul = admin.private_read(self.data / 'persona/SOUL.md', 32000).decode()
                identity = admin.private_read(self.data / 'persona/IDENTITY.md', 8000).decode()
            return json.dumps({'current': config.get('behavior', self.behavior_fixture()), 'defaults': self.behavior_fixture(),
                               'persona': {'files_enabled': files, 'identity': identity,
                                           'identity_source': 'persona/IDENTITY.md' if files else 'legacy_soul',
                                           'soul_source': 'persona/SOUL.md' if files else 'social.toml:soul'},
                               'soul': {'default': default_soul, 'effective': configured_soul if configured_soul.strip() else default_soul,
                                        'uses_default': not files and (not configured_soul.strip() or configured_soul == default_soul)},
                               'context_windows': config.get('context_windows', self.behavior_body()['context_windows']),
                               'max_bubbles': config.get('max_bubbles', 3), 'max_bubble_chars': config.get('max_bubble_chars', 1000)}).encode()
        if args[0] == str(self.manager.binary) and args[1] == 'models':
            if self.models_probe:
                self.models_probe()
            return json.dumps({'models': [{
                'id': 'fixture-model', 'model': 'fixture-model', 'display_name': 'Fixture model',
                'reasoning_efforts': ['low', 'medium', 'high'], 'default_effort': 'high'}]}).encode()
        raise AssertionError('Unexpected simulated command')

    def document(self, *, private=False, status='completed', message_status='bypassed'):
        scope = 'private/groups' if private else 'groups'
        name = 'test-person' if private else 'test-group'
        path = self.data / scope / (name + '.json')
        value = {'group_id': name, 'state': {
            'next_seq': 2, 'thread_id': 'fixture-thread', 'context_start_seq': 0,
            'messages': [{'seq': 1, 'status': message_status,
                          'message': {'id': 'fixture-message', 'timestamp_ms': 1000}}],
            'requests': {'1': {'id': 1, 'status': status, 'revision': 1,
                               'bubbles': [], 'source_seqs': [1]}},
        }}
        self.write(path, json.dumps(value))
        return path

    def request(self, method, path, *, authorized=True, body=None, raw=None,
                content_type='application/json', length=None, extra_headers=None):
        handler_type = admin.handler_for(self.manager, b't' * 40)
        handler = handler_type.__new__(handler_type)
        handler.headers = Message()
        if authorized:
            handler.headers['X-CM-Dashboard-Proxy'] = (b't' * 40).decode()
        if method == 'POST':
            payload = raw if raw is not None else json.dumps(body).encode()
            handler.headers['Content-Type'] = content_type
            handler.headers['Content-Length'] = str(len(payload)) if length is None else str(length)
        else:
            payload = b''
        for name, value in (extra_headers or {}).items():
            handler.headers[name] = value
        handler.path = path
        handler.command = method
        handler.request_version = 'HTTP/1.1'
        handler.requestline = method + ' ' + path + ' HTTP/1.1'
        handler.rfile = io.BytesIO(payload)
        handler.wfile = io.BytesIO()
        getattr(handler, 'do_' + method)()
        head, payload = handler.wfile.getvalue().split(b'\r\n\r\n', 1)
        status = int(head.split(b'\r\n', 1)[0].split()[1])
        headers = dict(line.decode().split(': ', 1) for line in head.split(b'\r\n')[1:])
        data = json.loads(payload) if 'application/json' in headers.get('Content-Type', '') else payload
        return status, headers, data



class HooksCapabilityTests(AdminFixture):
    def local_hooks(self, value=None):
        value = value if value is not None else {'description': 'Private operator instructions', 'hooks': {
            'SessionStart': [{'matcher': 'startup', 'hooks': [
                {'type': 'command', 'command': 'printf private-fixture-command', 'timeout': 15,
                 'async': False, 'statusMessage': 'private-fixture-status', 'additionalContextLimit': 0}]}]}}
        raw = json.dumps(value, indent=2).encode() + b'\n'
        self.write(self.home / '.codex/hooks.json', raw)
        return raw

    def record(self):
        return next(item for item in self.manager.capability_catalog()['capabilities'] if item['kind'] == 'hooks')

    def act(self, action, record=None, **extra):
        record = record or self.record()
        return self.manager.capability_action({'id': record['id'], 'action': action, **extra})

    def snapshot(self):
        return {path: path.read_bytes() if path.exists() else None for path in
                [*self.manager.hooks_paths(), self.config_path, self.runtime / 'config.toml',
                 self.runtime / 'auth.json', self.personal_auth]}

    def test_discovery_never_executes_or_exposes_commands_descriptions_or_paths(self):
        self.local_hooks()
        result = self.manager.capability_catalog()
        encoded = json.dumps(result)
        for private in ['Private operator instructions', 'private-fixture-command', 'private-fixture-status', str(self.home)]:
            self.assertNotIn(private, encoded)
        self.assertTrue(self.record()['available'])
        self.assertFalse(self.record()['registered'])
        self.assertFalse(self.record()['enabled'])
        self.assertEqual(self.actions, [])
        self.assertFalse(any(path.exists() for path in self.manager.hooks_paths()))

    def test_import_disabled_then_enable_disable_preserves_exact_bytes_and_auth(self):
        raw = self.local_hooks()
        before = self.snapshot()
        record = self.record()
        self.act('import', record)
        active, disabled, marker = self.manager.hooks_paths()
        self.assertFalse(active.exists())
        self.assertEqual(disabled.read_bytes(), raw)
        self.assertEqual(marker.stat().st_mode & 0o777, 0o600)
        self.assertEqual(disabled.stat().st_mode & 0o777, 0o600)
        self.assertFalse(self.record()['enabled'])
        self.act('enable')
        self.assertEqual(active.read_bytes(), raw)
        self.assertFalse(disabled.exists())
        self.assertTrue(self.record()['enabled'])
        self.act('disable')
        self.assertFalse(active.exists())
        self.assertEqual(disabled.read_bytes(), raw)
        self.assertFalse(self.record()['enabled'])
        for path, contents in before.items():
            if path not in self.manager.hooks_paths():
                self.assertEqual(path.read_bytes(), contents)
        self.assertEqual(self.actions, ['stop', 'start'] * 3)

    def test_no_source_hooks_means_no_default_import_or_catalogue_item(self):
        catalog = self.manager.capability_catalog()
        self.assertFalse(any(item['kind'] == 'hooks' for item in catalog['capabilities']))
        self.assertFalse(any(path.exists() for path in self.manager.hooks_paths()))

    def test_imported_hooks_remain_manageable_without_personal_source(self):
        raw = self.local_hooks()
        self.act('import')
        (self.home / '.codex/hooks.json').unlink()
        self.assertTrue(self.record()['registered'])
        self.act('enable')
        self.assertEqual((self.runtime / 'hooks.json').read_bytes(), raw)
        self.act('disable')

    def test_refresh_digest_refuses_changed_source_without_creating_files(self):
        self.local_hooks()
        record = self.record()
        self.local_hooks({'hooks': {'Stop': [{'hooks': [{'type': 'command', 'command': 'true'}]}]}})
        before = self.snapshot()
        with self.assertRaisesRegex(admin.AdminError, '已变化'):
            self.act('import', record)
        self.assertEqual(self.snapshot(), before)

    def test_existing_unmanaged_names_are_never_overwritten(self):
        self.local_hooks()
        for path in self.manager.hooks_paths():
            with self.subTest(name=path.name):
                self.write(path, b'existing operator configuration')
                record = self.record()
                before = self.snapshot()
                self.assertFalse(record['available'])
                with self.assertRaises(admin.AdminError):
                    self.act('import', record)
                self.assertEqual(self.snapshot(), before)
                path.unlink()

    def test_conflict_created_after_catalogue_is_detected_inside_transaction(self):
        self.local_hooks()
        record = self.record()
        self.write(self.runtime / 'hooks.json', 'manual hook data')
        before = self.snapshot()
        with self.assertRaises(admin.AdminError):
            self.act('import', record)
        self.assertEqual(self.snapshot(), before)

    def test_manual_edits_to_managed_content_or_marker_are_not_overwritten(self):
        self.local_hooks()
        self.act('import')
        original = self.snapshot()
        for path in (self.runtime / 'hooks.disabled.json', self.runtime / 'hooks.import.json'):
            with self.subTest(name=path.name):
                self.write(path, '{}')
                before = self.snapshot()
                self.assertFalse(self.record()['available'])
                with self.assertRaises((admin.AdminError, ValueError)):
                    self.act('enable')
                self.assertEqual(self.snapshot(), before)
                self.write(path, original[path])

    def test_failed_config_check_rolls_back_import_enable_and_disable_exactly(self):
        self.local_hooks()
        for action in ('import', 'enable', 'disable'):
            with self.subTest(action=action):
                before = self.snapshot()
                self.validation_fails = True
                with self.assertRaisesRegex(admin.AdminError, 'validation refused'):
                    self.act(action)
                self.assertEqual(self.snapshot(), before)
                self.assertTrue(self.running)
                self.validation_fails = False
                self.act(action)

    def test_busy_or_locked_runtime_cannot_import_hooks(self):
        self.local_hooks()
        before = self.snapshot()
        with mock.patch.object(self.manager, 'idle', return_value=False):
            with self.assertRaises(admin.AdminError):
                self.act('import')
        lock = os.open(self.lock_path, os.O_RDWR)
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(admin.AdminError):
                self.act('import')
        finally:
            os.close(lock)
        self.assertEqual(self.snapshot(), before)

    def test_source_file_must_be_owner_only_regular_and_unlinked(self):
        raw = self.local_hooks()
        source = self.home / '.codex/hooks.json'
        source.chmod(0o644)
        self.assertFalse(self.record()['available'])
        source.chmod(0o600)
        with mock.patch.object(admin.codex_capabilities.os, 'getuid', return_value=os.getuid() + 1):
            records, _ = admin.codex_capabilities.discover_local(self.home)
            self.assertFalse(next(item for item in records if item['kind'] == 'hooks')['available'])
        source.unlink()
        source.symlink_to(self.personal_auth)
        self.assertFalse(self.record()['available'])
        source.unlink()
        self.write(source, raw)
        os.link(source, self.home / '.codex/hooks-alias.json')
        self.assertFalse(self.record()['available'])

    def test_runtime_symlink_hardlink_and_duplicate_active_backup_are_refused(self):
        self.local_hooks()
        record = self.record()
        active, disabled, _ = self.manager.hooks_paths()
        active.symlink_to(self.runtime / 'missing-hook-target')
        with self.assertRaises(admin.AdminError):
            self.act('import', record)
        self.assertTrue(active.is_symlink())
        active.unlink()
        self.act('import')
        alias = self.runtime / 'unmanaged-alias'
        os.link(disabled, alias)
        with self.assertRaises(admin.AdminError):
            self.act('enable')
        alias.unlink()
        self.write(active, disabled.read_bytes())
        before = self.snapshot()
        with self.assertRaises(admin.AdminError):
            self.act('disable')
        self.assertEqual(self.snapshot(), before)

    def test_unsupported_or_malformed_documents_are_unavailable(self):
        invalid_handlers = [
            {'type': kind, 'command': 'true'} for kind in ('prompt', 'agent', 'mcp_tool')]
        invalid_handlers += [{'type': 'command', 'command': 'true', **extra} for extra in (
            {'commandWindows': 'ignored'}, {'command_windows': 'ignored'}, {'env': {}},
            {'timeout': True}, {'timeout': 0}, {'timeout': 601}, {'async': 'yes'},
            {'additionalContextLimit': -1}, {'additionalContextLimit': None}, {'statusMessage': None},
            {'command': ''}, {'command': '\ud800'}, {'command': 'a' * 8193})]
        for handler in invalid_handlers:
            with self.subTest(handler=handler.get('type')):
                self.local_hooks({'hooks': {'Stop': [{'hooks': [handler]}]}})
                self.assertFalse(self.record()['available'])
        for document in ({'hooks': {}}, {'hooks': {'Unknown': []}}, {'hooks': {}, 'state': {}},
                         {'description': None, 'hooks': {}},
                         {'hooks': {'Stop': [{'matcher': None, 'hooks': [{'type': 'command', 'command': 'true'}]}]}},
                         {'hooks': {'Stop': [{'hooks': [{'type': 'command', 'command': 'true'}] * 33}]}}):
            self.local_hooks(document)
            self.assertFalse(self.record()['available'])
        for raw in (b'{"hooks":{},"hooks":{}}', b'{"hooks":NaN}', b'[' * 2000,
                    '{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}'.encode('utf-16'),
                    b' ' * (admin.codex_capabilities.HOOKS_LIMIT + 1)):
            self.write(self.home / '.codex/hooks.json', raw)
            self.assertFalse(self.record()['available'])

    def test_hooks_action_rejects_paths_commands_tool_selection_and_connection_check(self):
        self.local_hooks()
        record = self.record()
        before = self.snapshot()
        for extra in ({'allowed_tools': ['search']}, {'path': '/tmp/hooks.json'}, {'command': 'true'}):
            with self.assertRaises(admin.AdminError):
                self.act('import', record, **extra)
        with self.assertRaises(admin.AdminError):
            self.act('check', record)
        self.assertEqual(self.snapshot(), before)


class CapabilityTests(AdminFixture):
    def setUp(self):
        super().setUp()
        self.apps = {'apps': [{'id': 'connector_fixture', 'name': 'Fixture Notes',
                              'tool_namespace': 'fixture_notes', 'tools': [
                                  {'name': 'search', 'description': 'Search notes'},
                                  {'name': 'fetch', 'description': 'Fetch note'}]}]}
        original = self.simulate_run
        def run(args, timeout=30):
            if args[:2] == [str(self.manager.binary), 'capability-catalog']:
                self.assertEqual(args[2:], ['--config', str(self.config_path)])
                self.assertEqual(timeout, 100)
                return json.dumps(self.apps).encode()
            if args[:2] == [str(self.manager.binary), 'integration-check']:
                return b'{"ok":true,"tool_count":1}'
            return original(args, timeout)
        self.manager.run.side_effect = run
        self.manager.hosted_capabilities = admin.codex_capabilities.discover_apps(self.apps)
        self.manager.hosted_capabilities_notice = ''

    def await_refresh(self):
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            status = self.manager.post('/api/config', {'section': 'capabilities', 'action': 'status'})
            if status['state'] != 'running':
                return status
            threading.Event().wait(0.01)
        self.fail('Capability refresh did not finish')

    def local_mcp(self, **values):
        self.write(self.home / '.codex/config.toml', admin.toml_dump({
            'mcp_servers': {'fixture_docs': {'url': 'https://example.com/mcp',
                                            'enabled_tools': ['search', 'fetch'], **values}}}))

    def catalog_entry(self, kind):
        catalog = self.manager.capability_catalog()
        return next(item for item in catalog['capabilities'] if item['kind'] == kind)

    def test_catalogue_omits_auth_urls_paths_and_does_not_execute_helpers(self):
        self.local_mcp(http_headers_helper='python3 /private/helper.py --secret synthetic-helper-value')
        value = self.manager.capability_catalog()
        encoded = json.dumps(value)
        for hidden in ['example.com', 'synthetic-helper-value', '/private/helper.py', str(self.home),
                       self.credential_values['personal'], 'personal-only-model']:
            self.assertNotIn(hidden, encoded)
        mcp = next(item for item in value['capabilities'] if item['kind'] == 'mcp')
        self.assertFalse(mcp['available'])
        self.assertEqual(self.actions, [])

    def test_account_app_import_is_disabled_and_does_not_grant_any_conversation(self):
        group = self.document()
        before = group.read_bytes()
        auth_before = self.personal_auth.read_bytes()
        app = self.catalog_entry('app')
        self.assertTrue(app['personal_account'])
        self.assertFalse(app['enabled'])
        self.manager.capability_action({'id': app['id'], 'action': 'import', 'allowed_tools': ['search']})
        plugin = self.manager.config['integrations']['plugins'][app['registration_id']]
        self.assertFalse(plugin['enabled'])
        self.assertTrue(plugin['personal_account'])
        self.assertEqual(plugin['allowed_tools'], ['search'])
        self.assertEqual(plugin['endpoint'], 'https://chatgpt.com/')
        self.assertEqual(plugin['app_id'], 'connector_fixture')
        self.assertEqual(group.read_bytes(), before)
        self.assertEqual(self.personal_auth.read_bytes(), auth_before)
        self.assertEqual(admin.private_read(self.runtime / 'auth.json'), b'{}')
        self.manager.capability_action({'id': app['id'], 'action': 'enable'})
        self.assertTrue(self.manager.config['integrations']['plugins'][app['registration_id']]['enabled'])
        self.assertEqual(group.read_bytes(), before)
        self.manager.capability_action({'id': app['id'], 'action': 'disable'})
        self.assertFalse(self.manager.config['integrations']['plugins'][app['registration_id']]['enabled'])

    def test_forged_apps_tools_and_config_fields_are_refused(self):
        app = self.catalog_entry('app')
        before = self.config_path.read_bytes()
        for body in [
            {'id': 'app-unknown', 'action': 'import', 'allowed_tools': ['search']},
            {'id': app['id'], 'action': 'import', 'allowed_tools': ['write_all']},
            {'id': app['id'], 'action': 'import', 'allowed_tools': ['search'], 'app_id': 'other'},
            {'id': app['id'], 'action': 'import', 'allowed_tools': []},
            {'id': app['id'], 'action': 'import', 'allowed_tools': ['search', 'search']},
        ]:
            with self.assertRaises(admin.AdminError):
                self.manager.capability_action(body)
            self.assertEqual(self.config_path.read_bytes(), before)

    def test_native_raw_app_tool_names_import_without_an_invented_namespace(self):
        self.apps['apps'][0].update(tool_namespace=None, tools=[
            {'name': 'capture_file_upload'}, {'name': 'fixture_notes.search'}])
        self.manager.hosted_capabilities = admin.codex_capabilities.discover_apps(self.apps)
        app = self.catalog_entry('app')
        self.assertTrue(app['available'])
        self.manager.capability_action({'id': app['id'], 'action': 'import',
                                       'allowed_tools': ['capture_file_upload', 'fixture_notes.search']})
        plugin = self.manager.config['integrations']['plugins'][app['registration_id']]
        self.assertNotIn('tool_namespace', plugin)
        self.assertEqual(plugin['allowed_tools'], ['capture_file_upload', 'fixture_notes.search'])
        self.assertFalse(plugin['enabled'])
        persisted = tomllib.loads(self.config_path.read_text())
        self.assertEqual(persisted['integrations']['plugins'][app['registration_id']], plugin)

    def test_http_import_copies_only_selected_bearer_and_rolls_back_failed_validation(self):
        token = 'synthetic-capability-bearer'
        self.local_mcp(http_headers={'Authorization': 'Bearer ' + token})
        mcp = self.catalog_entry('mcp')
        self.assertNotIn(token, json.dumps(self.manager.capability_catalog()))
        body = {'id': mcp['id'], 'action': 'import', 'allowed_tools': ['search']}
        before = self.config_path.read_bytes()
        secret = self.config_dir / (mcp['registration_id'] + '.key')
        self.validation_fails = True
        with self.assertRaises(admin.AdminError):
            self.manager.capability_action(body)
        self.assertFalse(secret.exists())
        self.assertEqual(self.config_path.read_bytes(), before)
        self.validation_fails = False
        self.manager.capability_action(body)
        self.assertEqual(admin.private_read(secret), (token + '\n').encode())
        plugin = self.manager.config['integrations']['plugins'][mcp['registration_id']]
        self.assertFalse(plugin['enabled'])
        self.assertEqual(plugin['allowed_tools'], ['search'])
        self.assertNotIn(token, self.config_path.read_text())

    def test_changed_http_source_and_expired_selection_require_refresh(self):
        self.local_mcp()
        mcp = self.catalog_entry('mcp')
        body = {'id': mcp['id'], 'action': 'import', 'allowed_tools': ['search']}
        self.local_mcp(url='https://other.example.com/mcp')
        with self.assertRaises(admin.AdminError):
            self.manager.capability_action(body)
        self.manager.capabilities_refreshed = 0
        with self.assertRaises(admin.AdminError):
            self.manager.capability_action(body)
        self.assertNotIn('integrations', self.manager.config)

    def test_registered_connection_remains_controllable_without_local_source(self):
        self.local_mcp()
        mcp = self.catalog_entry('mcp')
        self.manager.capability_action({'id': mcp['id'], 'action': 'import', 'allowed_tools': ['search']})
        self.write(self.home / '.codex/config.toml', '')
        registered = self.catalog_entry('mcp')
        self.assertTrue(registered['registered'])
        self.manager.capability_action({'id': registered['id'], 'action': 'enable'})
        result = self.manager.capability_action({'id': registered['id'], 'action': 'check'})
        self.assertTrue(result['ok'])

    def test_text_skill_import_is_disabled_and_does_not_copy_profile_or_other_files(self):
        folder = self.directory('owner-home/.codex/skills/fixture-method')
        raw = '---\nname: fixture-method\ndescription: Test method\n---\nRead the input carefully.'
        self.write(folder / 'SKILL.md', raw)
        skill = self.catalog_entry('skill')
        self.assertTrue(skill['available'])
        self.manager.capability_action({'id': skill['id'], 'action': 'import'})
        self.assertEqual(admin.private_read(self.data / 'skills/fixture-method/SKILL.md').decode(), raw)
        self.assertNotIn('fixture-method', self.manager.config.get('enabled_skills', []))
        self.assertEqual(list((self.data / 'skills/fixture-method').iterdir()), [self.data / 'skills/fixture-method/SKILL.md'])
        with self.assertRaises(admin.AdminError):
            self.manager.capability_action({'id': skill['id'], 'action': 'import'})

    def test_skill_packages_and_symlink_sources_are_not_silently_partially_imported(self):
        folder = self.directory('owner-home/.codex/skills/fixture-method')
        self.write(folder / 'SKILL.md', 'Instructions')
        self.write(folder / 'helper.py', 'raise RuntimeError("must never execute")')
        skill = self.catalog_entry('skill')
        self.assertFalse(skill['available'])
        with self.assertRaises(admin.AdminError):
            self.manager.capability_action({'id': skill['id'], 'action': 'import'})
        (folder / 'SKILL.md').unlink()
        (folder / 'SKILL.md').symlink_to(self.personal_auth)
        self.assertFalse(self.catalog_entry('skill')['available'])

    def test_local_config_cannot_inject_processes_private_endpoints_or_headers(self):
        fixture_user, fixture_password = 'EXAMPLE_USER', 'EXAMPLE_PASSWORD'
        for server in [
            {'command': 'sh', 'args': ['-c', 'exit 1']},
            {'url': 'http://example.com/mcp'}, {'url': 'https://127.0.0.1/mcp'},
            {'url': 'https://192.0.2.1/mcp'}, {'url': 'https://[::1]/mcp'},
            {'url': urlunsplit(('https', f'{fixture_user}:{fixture_password}@example.com', '/mcp', '', ''))},
            {'url': 'https://example.com/mcp?key=secret'},
            {'url': 'https://example.com:8443/mcp'}, {'url': 'https://host.internal/mcp'},
            {'http_headers': {'X-Api-Key': 'EXAMPLE_API_KEY'}},
            {'bearer_token_env_var': 'MISSING_FIXTURE_CAPABILITY_CREDENTIAL'},
        ]:
            with self.subTest(server=server):
                self.local_mcp(**server)
                self.assertFalse(self.catalog_entry('mcp')['available'])

    def test_unavailable_account_directory_does_not_break_local_discovery(self):
        self.local_mcp()
        original = self.manager.run.side_effect
        def run(args, timeout=30):
            if args[:2] == [str(self.manager.binary), 'capability-catalog']:
                raise admin.AdminError('unavailable', 502)
            return original(args, timeout)
        self.manager.run.side_effect = run
        self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})
        self.assertEqual(self.await_refresh()['state'], 'failed')
        catalog = self.manager.capability_catalog()
        self.assertTrue(catalog['notices'])
        self.assertTrue(any(item['kind'] == 'mcp' for item in catalog['capabilities']))
        self.assertTrue(self.running)
        self.assertEqual(self.actions, ['stop', 'start'])

    def test_opening_directory_never_stops_bot_or_launches_hosted_probe(self):
        self.manager.hosted_capabilities = []
        self.local_mcp()
        self.assertEqual(self.request('GET', '/api/capabilities')[0], 200)
        self.assertEqual(self.request('POST', '/api/config', body={'section': 'capabilities', 'action': 'catalog'})[0], 200)
        self.assertEqual(self.actions, [])
        self.assertFalse(any(call.args[0][:2] == [str(self.manager.binary), 'capability-catalog']
                             for call in self.manager.run.call_args_list))

    def test_refresh_releases_operator_lock_restores_service_and_blocks_mutations_without_blocking_status(self):
        entered, release = threading.Event(), threading.Event()
        original = self.manager.run.side_effect
        def run(args, timeout=30):
            if args[:2] == [str(self.manager.binary), 'capability-catalog']:
                fd = os.open(self.lock_path, os.O_RDWR)
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                finally:
                    os.close(fd)
                entered.set()
                if not release.wait(2):
                    raise RuntimeError('test release timed out')
            return original(args, timeout)
        self.manager.run.side_effect = run
        first = self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})
        try:
            self.assertTrue(entered.wait(1))
            self.assertEqual(self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})['id'], first['id'])
            self.assertEqual(self.manager.post('/api/config', {'section': 'capabilities', 'action': 'status'})['state'], 'running')
            self.assertEqual(self.request('GET', '/api/state')[0], 409)
            with self.assertRaises(admin.AdminError) as result:
                self.manager.post('/api/service', {'action': 'start'})
            self.assertEqual(result.exception.status, 409)
        finally:
            release.set()
        self.assertEqual(self.await_refresh()['state'], 'completed')
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertTrue(self.running)

    def test_refresh_while_busy_does_not_stop_bot(self):
        self.document(message_status='running')
        self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})
        status = self.await_refresh()
        self.assertEqual(status['state'], 'failed')
        self.assertIn('正在处理消息', status['message'])
        self.assertEqual(self.actions, [])

    def test_refresh_timeout_restores_bot_and_does_not_expose_command_output(self):
        original = self.manager.run.side_effect
        def run(args, timeout=30):
            if args[:2] == [str(self.manager.binary), 'capability-catalog']:
                raise admin.subprocess.TimeoutExpired('/private/credential-path', timeout,
                                                      output=b'synthetic-private-output')
            return original(args, timeout)
        self.manager.run.side_effect = run
        self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})
        status = self.await_refresh()
        self.assertEqual(status['state'], 'failed')
        self.assertNotIn('private', json.dumps(status))
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertTrue(self.running)
        self.manager.run.side_effect = original
        self.running = False
        self.actions.clear()
        self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})
        self.assertEqual(self.await_refresh()['state'], 'completed')
        self.assertEqual(self.actions, [])
        self.assertFalse(self.running)

    def test_unavailable_apps_display_actionable_reasons_and_cannot_import(self):
        self.apps['unavailable'] = [{'id': 'connector_missing', 'name': 'Missing Notes',
                                     'reason': 'read_only_namespace_unavailable'},
                                    {'id': 'connector_unknown', 'name': 'Unknown Notes',
                                     'reason': 'secret-private-path-must-not-leak'}]
        self.manager.post('/api/config', {'section': 'capabilities', 'action': 'refresh'})
        catalog = self.await_refresh()['catalog']
        missing = next(item for item in catalog['capabilities'] if item['name'] == 'Missing Notes')
        self.assertFalse(missing['available'])
        self.assertIn('只读工具', missing['reason'])
        self.assertNotIn('secret-private-path', json.dumps(catalog))
        with self.assertRaises(admin.AdminError):
            self.manager.capability_action({'id': missing['id'], 'action': 'import', 'allowed_tools': ['search']})

    def test_browser_uses_safe_native_manifest_and_respects_explicit_disable(self):
        manifest = self.data / 'runtime/native-tools.json'
        self.write(manifest, json.dumps({'version': 1, 'browser_ready': True}))
        self.assertTrue(self.manager.integrations_public()['browser']['configured'])
        self.manager.config['integrations'] = {'browser': {'enabled': False}}
        self.assertFalse(self.manager.integrations_public()['browser']['configured'])
        self.manager.config['integrations'] = {}
        for value in [{'version': 2, 'browser_ready': True}, {'version': True, 'browser_ready': True},
                      {'version': 1, 'browser_ready': False}, {'version': 1, 'browser_ready': 'yes'}, []]:
            self.write(manifest, json.dumps(value))
            self.assertFalse(self.manager.integrations_public()['browser']['configured'])
        self.write(manifest, json.dumps({'version': 1, 'browser_ready': True}))
        manifest.chmod(0o644)
        self.assertFalse(self.manager.integrations_public()['browser']['configured'])
        manifest.unlink()
        manifest.symlink_to(self.personal_auth)
        self.assertFalse(self.manager.integrations_public()['browser']['configured'])

    def test_capability_routes_require_proxy_authorization(self):
        self.assertEqual(self.request('GET', '/api/capabilities', authorized=False)[0], 401)
        self.assertEqual(self.request('POST', '/api/capability', authorized=False,
                                      body={'id': 'unknown', 'action': 'enable'})[0], 401)
        status, headers, value = self.request('GET', '/api/capabilities')
        self.assertEqual(status, 200)
        self.assertEqual(headers['Cache-Control'], 'no-store')
        app = next(item for item in value['capabilities'] if item['kind'] == 'app')
        status, _, result = self.request('POST', '/api/capability',
                                         body={'id': app['id'], 'action': 'import', 'allowed_tools': ['search']})
        self.assertEqual((status, result['ok']), (200, True))


class HttpBoundaryTests(AdminFixture):
    def test_proxy_authorization_covers_assets_health_state_and_mutations(self):
        with mock.patch.object(self.manager, 'state') as state, mock.patch.object(self.manager, 'post') as post:
            for path in ('/', '/assets/app.js', '/assets/app.css', '/api/state', '/health'):
                with self.subTest(path=path):
                    self.assertEqual(self.request('GET', path, authorized=False)[0], 401)
            self.assertEqual(self.request('POST', '/api/service', authorized=False, body={'action': 'stop'})[0], 401)
            state.assert_not_called()
            post.assert_not_called()
        status, _, data = self.request('GET', '/health')
        self.assertEqual((status, data), (200, {'ok': True}))
        self.assertEqual(self.request('GET', '/assets/app.js')[0], 200)

    def test_wrong_proxy_token_is_denied(self):
        # Duplicate request headers cannot turn an incorrect first value into authorization.
        self.assertEqual(self.request('GET', '/api/state', authorized=False,
                                     extra_headers={'X-CM-Dashboard-Proxy': 'incorrect'})[0], 401)

    def test_routes_are_fixed_and_do_not_accept_traversal_or_queries(self):
        for path in ('/assets/../configuration/qq.env', '/assets/%2e%2e/qq.env',
                     '/api/state?file=qq.env', '/assets/app.js/../../qq.env', '/unknown'):
            with self.subTest(path=path):
                self.assertEqual(self.request('GET', path)[0], 404)
        self.assertEqual(self.request('POST', '/api/unknown', body={})[0], 404)
        self.assertEqual(self.actions, [])

    def test_get_does_not_execute_a_service_mutation(self):
        self.assertEqual(self.request('GET', '/api/service')[0], 404)
        self.assertEqual(self.actions, [])

    def test_authorized_service_controls_accept_only_known_actions(self):
        self.assertEqual(self.request('POST', '/api/service', body={'action': 'stop'})[0], 200)
        self.assertEqual(self.actions, ['stop'])
        self.assertEqual(self.request('POST', '/api/service', body={'action': 'arbitrary-operation'})[0], 400)
        self.assertEqual(self.actions, ['stop'])

    def test_post_rejects_wrong_type_transfer_encoding_and_nonobject_json(self):
        self.assertEqual(self.request('POST', '/api/service', body={}, content_type='text/plain')[0], 415)
        self.assertEqual(self.request('POST', '/api/service', body={},
                                     extra_headers={'Transfer-Encoding': 'chunked'})[0], 400)
        self.assertEqual(self.request('POST', '/api/service', body=[])[0], 400)
        self.assertEqual(self.actions, [])

    def test_payload_limits_and_malformed_inputs_are_client_errors(self):
        for length in (0, -1, 65537):
            with self.subTest(length=length):
                self.assertEqual(self.request('POST', '/api/service', body={}, length=length)[0], 413)
        self.assertEqual(self.request('POST', '/api/service', body={}, length='invalid')[0], 400)
        self.assertEqual(self.request('POST', '/api/service', raw=b'{')[0], 400)
        self.assertEqual(self.actions, [])

    def test_state_exposes_credential_presence_without_credential_values(self):
        status, headers, data = self.request('GET', '/api/state')
        self.assertEqual(status, 200)
        self.assertEqual(data['credentials'], {'qq': True, 'jev': True, 'provider': True})
        self.assertEqual(data['config']['qq_app_id'], '123456')
        serialized = json.dumps(data)
        for value in self.credential_values.values():
            self.assertNotIn(value, serialized)
        self.assertEqual(headers['Cache-Control'], 'no-store')
        self.assertEqual(headers['X-Content-Type-Options'], 'nosniff')

    def test_internal_errors_do_not_reflect_private_values(self):
        hidden = self.credential_values['personal']
        with mock.patch.object(self.manager, 'state', side_effect=RuntimeError(hidden)):
            status, _, data = self.request('GET', '/api/state')
        self.assertEqual(status, 500)
        self.assertNotIn(hidden, json.dumps(data))
        self.assertNotIn('RuntimeError', json.dumps(data))

    def test_unknown_body_fields_cannot_select_arbitrary_commands(self):
        status, _, _ = self.request('POST', '/api/config', body={
            'section': 'general', 'binary': '/not-a-managed-binary'})
        self.assertEqual(status, 400)
        self.assertNotIn('binary', tomllib.loads(self.config_path.read_text()))

    def test_bad_submitted_credentials_are_not_reflected_in_error_responses(self):
        submitted = 'synthetic-' + 'credential with spaces'
        status, _, data = self.request('POST', '/api/config', body={
            'section': 'jev', 'api_key': submitted})
        self.assertEqual(status, 400)
        self.assertNotIn(submitted, json.dumps(data))
        self.assertEqual(admin.env_read(self.jev)['TYPESAFE_API_KEY'], self.credential_values['jev'])


class ProtectedFileTests(AdminFixture):
    def test_private_read_rejects_file_and_parent_permission_changes(self):
        self.qq.chmod(0o644)
        with self.assertRaises(admin.AdminError):
            admin.private_read(self.qq)
        self.qq.chmod(0o600)
        self.config_dir.chmod(0o755)
        with self.assertRaises(admin.AdminError):
            admin.private_read(self.qq)

    def test_private_read_honors_the_limit_before_returning_file_contents(self):
        path = self.config_dir / 'oversized.env'
        self.write(path, b'x' * 128)
        with self.assertRaises(admin.AdminError):
            admin.private_read(path, limit=64)

    def test_private_read_rejects_a_directory_owned_by_another_user(self):
        with mock.patch.object(admin.os, 'getuid', return_value=os.getuid() + 1):
            with self.assertRaises(admin.AdminError):
                admin.private_read(self.qq)

    def test_private_read_rejects_symlinked_file_and_parent(self):
        linked = self.config_dir / 'linked.env'
        linked.symlink_to(self.qq)
        with self.assertRaises((admin.AdminError, OSError)):
            admin.private_read(linked)
        folder = self.root / 'linked-configuration'
        folder.symlink_to(self.config_dir, target_is_directory=True)
        with self.assertRaises((admin.AdminError, OSError)):
            admin.private_read(folder / self.qq.name)

    def test_atomic_write_rejects_an_unsafe_new_file_parent(self):
        unsafe = self.directory('unsafe')
        unsafe.chmod(0o755)
        path = unsafe / 'new.env'
        with self.assertRaises((admin.AdminError, OSError)):
            admin.atomic_write(path, 'not-a-real-credential')
        self.assertFalse(path.exists())

    def test_atomic_write_rejects_symlinked_new_file_parent(self):
        destination = self.directory('outside')
        link = self.root / 'linked-parent'
        link.symlink_to(destination, target_is_directory=True)
        with self.assertRaises((admin.AdminError, OSError)):
            admin.atomic_write(link / 'new.env', 'fixture')
        self.assertFalse((destination / 'new.env').exists())

    def test_atomic_write_rejects_dangling_destination_symlink(self):
        path = self.config_dir / 'dangling.env'
        path.symlink_to(self.config_dir / 'absent-target')
        with self.assertRaises((admin.AdminError, OSError)):
            admin.atomic_write(path, 'fixture')
        self.assertTrue(path.is_symlink())


class TransactionTests(AdminFixture):
    def test_success_stops_validates_and_restores_the_previously_running_service(self):
        self.manager.update_config({'section': 'general', 'mode': 'mode-3'})
        self.assertEqual(tomllib.loads(self.config_path.read_text())['mode'], 'mode-3')
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertTrue(self.running)

    def test_success_does_not_start_a_previously_stopped_service(self):
        self.running = False
        self.manager.update_config({'section': 'general', 'mode': 'mode-3'})
        self.assertEqual(self.actions, [])
        self.assertFalse(self.running)

    def test_model_aliases_follow_the_existing_rust_configuration_contract(self):
        presets = {name: {'model': 'fixture-model', 'reasoning_effort': 'high'}
                   for name in ('Sol', '6fast', '_fast', '__proto__')}
        self.manager.update_config({'section': 'general', 'model_presets': presets})
        self.assertEqual(tomllib.loads(self.config_path.read_text())['model_presets'], presets)

    def test_busy_work_in_either_scope_prevents_a_stop_or_write(self):
        before = self.config_path.read_bytes()
        for private in (False, True):
            path = self.document(private=private, status='generating')
            with self.subTest(private=private), self.assertRaises(admin.AdminError) as failure:
                self.manager.update_config({'section': 'general', 'mode': 'mode-3'})
            self.assertEqual(failure.exception.status, 409)
            self.assertEqual(self.config_path.read_bytes(), before)
            self.assertEqual(self.actions, [])
            path.unlink()

    def test_scheduled_execution_and_delivery_prevent_admin_restart(self):
        folder = self.directory('state/tasks')
        for status in ('running', 'sending'):
            self.write(folder / 'g_fixture.json', json.dumps({'runs': {'1': {'status': status}}}))
            self.assertFalse(self.manager.idle())
            with self.assertRaises(admin.AdminError):
                with self.manager.transaction():
                    self.fail('Active scheduled task must prevent mutation')
            self.assertEqual(self.actions, [])
        self.write(folder / 'g_fixture.json', json.dumps({'runs': {'1': {'status': 'deferred'}}}))
        self.assertTrue(self.manager.idle())

    def test_new_inbound_message_during_stop_is_caught_by_second_idle_check(self):
        before = self.config_path.read_bytes()
        with mock.patch.object(self.manager, 'idle', side_effect=[True, False]):
            with self.assertRaises(admin.AdminError) as failure:
                self.manager.update_config({'section': 'general', 'mode': 'mode-3'})
        self.assertEqual(failure.exception.status, 409)
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertEqual(self.config_path.read_bytes(), before)

    def test_stop_error_after_a_state_change_recovers_the_original_service(self):
        original_control = self.manager.control
        before = self.config_path.read_bytes()
        def stop_then_fail(action):
            if action == 'stop':
                self.actions.append(action)
                self.running = False
                raise admin.AdminError('Fixture stop completion was not confirmed.', 502)
            return original_control(action)
        with mock.patch.object(self.manager, 'control', side_effect=stop_then_fail):
            with self.assertRaises(admin.AdminError):
                self.manager.update_config({'section': 'general', 'mode': 'mode-3'})
        self.assertEqual(self.config_path.read_bytes(), before)
        self.assertTrue(self.running)
        self.assertEqual(self.actions, ['stop', 'start'])

    def test_existing_operator_lock_prevents_the_mutation(self):
        descriptor = os.open(self.lock_path, os.O_RDWR)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(admin.AdminError) as failure:
                self.manager.update_config({'section': 'general', 'mode': 'mode-3'})
            self.assertEqual(failure.exception.status, 409)
            self.assertEqual(self.actions, ['stop', 'start'])
        finally:
            os.close(descriptor)

    def test_failed_validation_restores_both_config_and_changed_credentials(self):
        before = {path: path.read_bytes() for path in (self.config_path, self.qq)}
        self.validation_fails = True
        replacement = 'synthetic-' + 'replacement'
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({'section': 'qq', 'app_id': '654321',
                                        'app_secret': replacement})
        for path, content in before.items():
            self.assertEqual(path.read_bytes(), content)
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertTrue(self.running)

    def test_rollback_removes_new_files_and_restores_existing_files(self):
        before = self.config_path.read_bytes()
        new = self.config_dir / 'new.env'
        with self.assertRaises(RuntimeError):
            with self.manager.transaction() as save:
                save(new, 'fixture')
                save(self.config_path, admin.toml_dump({**self.config, 'mode': 'mode-2'}))
                raise RuntimeError('simulated failure')
        self.assertFalse(new.exists())
        self.assertEqual(self.config_path.read_bytes(), before)
        self.assertTrue(self.running)

    def test_blank_credential_fields_preserve_saved_values(self):
        self.manager.update_config({'section': 'qq', 'app_id': '654321', 'app_secret': ''})
        values = admin.env_read(self.qq)
        self.assertEqual(values['QQ_APP_ID'], '654321')
        self.assertEqual(values['QQ_APP_SECRET'], self.credential_values['qq'])
        self.manager.update_config({'section': 'jev', 'model': 'jev-latest', 'api_key': ''})
        self.assertEqual(admin.env_read(self.jev)['TYPESAFE_API_KEY'], self.credential_values['jev'])

    def test_model_query_releases_operator_lock_before_running_the_binary(self):
        probes = []
        def probe():
            descriptor = os.open(self.lock_path, os.O_RDWR)
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                probes.append(True)
            finally:
                os.close(descriptor)
        self.models_probe = probe
        self.assertTrue(self.manager.check({'target': 'models'})['ok'])
        self.assertEqual(probes, [True])
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertEqual(self.manager.state()['model_list_source'], 'live')

    def test_provider_url_must_not_contain_credentials_or_plain_http(self):
        before = (self.runtime / 'config.toml').read_bytes()
        credential_url = 'https://' + 'name:password' + '@api.example.com/v1'
        for url in ('http://api.example.com/v1', credential_url,
                    'https://api.example.com/v1?credential=fixture'):
            with self.subTest(url=url), self.assertRaises(admin.AdminError):
                self.manager.update_config({'section': 'provider', 'id': 'custom',
                                            'base_url': url, 'api_key': self.credential_values['provider'],
                                            'model': 'fixture-model', 'reasoning_effort': 'high'})
            self.assertEqual((self.runtime / 'config.toml').read_bytes(), before)

    def test_temporary_query_failure_preserves_the_previous_model_cache(self):
        cache = self.config_dir / 'models-cache.json'
        self.write(cache, json.dumps({'provider': 'openai', 'models': [{'model': 'old-fixture-model'}]}))
        before = cache.read_bytes()
        def fail_query():
            raise admin.AdminError('Fixture model query unavailable.', 502)
        self.models_probe = fail_query
        with self.assertRaises(admin.AdminError):
            self.manager.check({'target': 'models'})
        self.assertEqual(cache.read_bytes(), before)
        self.assertEqual(self.actions, ['stop', 'start'])


class SkillsAndLoginTests(AdminFixture):
    def save_skill(self, enabled=True):
        return self.manager.update_config({
            'section': 'skill', 'action': 'save', 'name': 'test-summary',
            'description': 'A temporary text-only fixture.', 'instructions': 'Summarize confirmed points.',
            'enabled': enabled})

    def test_skill_save_enable_disable_and_delete_are_local_and_archived(self):
        self.save_skill()
        self.assertEqual(self.manager.skill_list()[0]['instructions'], 'Summarize confirmed points.')
        self.assertTrue(self.manager.skill_list()[0]['enabled'])
        self.save_skill(False)
        self.assertFalse(self.manager.skill_list()[0]['enabled'])
        self.manager.update_config({'section': 'skill', 'action': 'delete', 'name': 'test-summary'})
        self.assertEqual(self.manager.skill_list(), [])
        archives = list((self.config_dir / 'skill-archive').glob('*.md'))
        self.assertEqual(len(archives), 1)
        self.assertIn(b'Summarize confirmed points.', admin.private_read(archives[0]))
        self.assertNotIn('test-summary', self.manager.config.get('enabled_skills', []))

    def test_empty_skill_directories_do_not_break_state_or_new_skill_creation(self):
        self.directory('state/skills/leftover-empty')
        self.assertEqual(self.manager.state()['skills'], [])
        self.save_skill()
        self.assertEqual([skill['name'] for skill in self.manager.skill_list()], ['test-summary'])

    def test_inactive_skill_directories_do_not_hide_later_active_skills(self):
        for index in range(32):
            self.directory(f'state/skills/empty-{index:02d}')
        self.save_skill()
        self.assertEqual([skill['name'] for skill in self.manager.skill_list()], ['test-summary'])

    def test_recreating_a_deleted_skill_cannot_bypass_the_active_skill_limit(self):
        for index in range(32):
            path = self.directory(f'state/skills/live-{index:02d}') / 'SKILL.md'
            self.write(path, '---\ndescription: Fixture.\n---\nText-only instructions.')
        removed = self.directory('state/skills/test-summary') / 'SKILL.md'
        self.write(removed, '---\ndescription: 已移除\n---\n')
        before = removed.read_bytes()
        with self.assertRaises(admin.AdminError):
            self.save_skill(False)
        self.assertEqual(removed.read_bytes(), before)

    def test_frontmatter_quotes_roundtrip_description_as_text(self):
        description = 'Keep "quoted": labels, including 中文.'
        self.manager.update_config({
            'section': 'skill', 'action': 'save', 'name': 'test-summary',
            'description': description, 'instructions': 'Text-only instructions.', 'enabled': False})
        skill = self.manager.skill_list()[0]
        self.assertEqual(skill['description'], description)
        self.assertEqual(skill['instructions'], 'Text-only instructions.')
        self.assertFalse(skill['enabled'])
        raw = admin.private_read(self.data / 'skills/test-summary/SKILL.md').decode()
        self.assertIn('name: test-summary\n', raw)
        self.assertIn('description: ' + json.dumps(description, ensure_ascii=False), raw)

    def test_skill_path_traversal_is_rejected(self):
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({'section': 'skill', 'action': 'delete', 'name': '../outside'})
        self.assertFalse((self.data / 'outside').exists())

    def test_skill_archive_symlink_cannot_redirect_a_delete_write(self):
        self.save_skill()
        outside = self.directory('outside-archive')
        (self.config_dir / 'skill-archive').symlink_to(outside, target_is_directory=True)
        before = (self.data / 'skills/test-summary/SKILL.md').read_bytes()
        with self.assertRaises((admin.AdminError, OSError)):
            self.manager.update_config({'section': 'skill', 'action': 'delete', 'name': 'test-summary'})
        self.assertEqual(list(outside.iterdir()), [])
        self.assertEqual((self.data / 'skills/test-summary/SKILL.md').read_bytes(), before)

    def test_auth_reuse_copies_only_auth_and_never_writes_personal_state(self):
        personal_before = {path.name: path.read_bytes() for path in (self.home / '.codex').iterdir()}
        runtime_config = (self.runtime / 'config.toml').read_bytes()
        self.manager.login({'action': 'reuse'})
        self.assertEqual((self.runtime / 'auth.json').read_bytes(), personal_before['auth.json'])
        self.assertEqual((self.runtime / 'config.toml').read_bytes(), runtime_config)
        self.assertFalse((self.runtime / 'history.jsonl').exists())
        self.assertEqual({path.name: path.read_bytes() for path in (self.home / '.codex').iterdir()}, personal_before)
        self.assertEqual(self.actions, ['stop', 'start'])

    def test_logout_stops_only_the_bot_and_leaves_personal_auth_unchanged(self):
        before = self.personal_auth.read_bytes()
        self.manager.login({'action': 'logout'})
        self.assertEqual(json.loads((self.runtime / 'auth.json').read_bytes()), {})
        self.assertEqual(self.personal_auth.read_bytes(), before)
        self.assertFalse(self.running)
        self.assertEqual(self.actions, ['stop'])

    def test_failed_auth_reuse_rolls_back_without_touching_personal_state(self):
        before = (self.runtime / 'auth.json').read_bytes()
        personal_before = self.personal_auth.read_bytes()
        self.validation_fails = True
        with self.assertRaises(admin.AdminError):
            self.manager.login({'action': 'reuse'})
        self.assertEqual((self.runtime / 'auth.json').read_bytes(), before)
        self.assertEqual(self.personal_auth.read_bytes(), personal_before)
        self.assertTrue(self.running)

    def test_device_status_returns_only_the_public_url_code_and_status(self):
        process = mock.Mock()
        process.poll.return_value = None
        self.manager.device = {'process': process, 'directory': self.data / 'fixture-device',
                               'started': admin.time.monotonic(), 'status': 'waiting'}
        for code in ('ABCD-EFGHI', 'ABCD-EFGH'):
            self.manager.device_output = ('\x1b[32mhttps://auth.openai.com/codex/device\x1b[0m\n'
                                          + code + '\n' + self.credential_values['personal'])
            value = self.manager.device_state()
            self.assertEqual(value, {'status': 'waiting', 'url': 'https://auth.openai.com/codex/device', 'code': code})
            self.assertNotIn(self.credential_values['personal'], json.dumps(value))

    def test_conversation_reset_changes_context_without_removing_history(self):
        path = self.document(status='waiting_for_user')
        before = json.loads(path.read_text())
        bound = json.loads(path.read_text())
        bound['state']['requests']['1']['codex_thread_id'] = 'task-thread'
        bound['state']['requests']['1']['codex_context'] = {'thread_id': 'task-thread'}
        self.write(path, json.dumps(bound))
        self.manager.conversation({'scope': 'group', 'id': 'test-group', 'action': 'reset'})
        current = json.loads(path.read_text())['state']
        self.assertEqual(current['messages'], before['state']['messages'])
        self.assertEqual(current['requests'], before['state']['requests'])
        self.assertIsNone(current['thread_id'])
        self.assertEqual(current['context_start_seq'], 2)

    def test_conversation_reset_refuses_unknown_delivery(self):
        path = self.document(status='unknown_delivery')
        before = path.read_bytes()
        with self.assertRaises(admin.AdminError) as failure:
            self.manager.conversation({'scope': 'group', 'id': 'test-group', 'action': 'reset'})
        self.assertEqual(failure.exception.status, 409)
        self.assertEqual(path.read_bytes(), before)


class GroupProfileTests(AdminFixture):
    def body(self, **changes):
        return {'scope': 'group', 'id': 'test-group', 'action': 'profile',
                'expected_revision': '0', 'profile': dict(zip(admin.GROUP_PROFILE_FIELDS, ['工作群', '同事', '直接', '不打断'])), **changes}

    def test_profile_is_scoped_versioned_preserves_state_and_survives_context_reset(self):
        path = self.document(status='waiting_for_user')
        private = self.document(private=True)
        private_before = private.read_bytes()
        before = json.loads(path.read_text())['state']
        self.manager.conversation(self.body())
        saved = json.loads(path.read_text())['state']
        self.assertEqual(saved['group_profile']['fields'], self.body()['profile'])
        self.assertEqual(saved['group_profile']['revision'], 1)
        self.assertEqual(saved['group_profile']['source_seq'], 1)
        self.assertEqual({k:v for k,v in saved.items() if k != 'group_profile'}, before)
        self.assertEqual(private.read_bytes(), private_before)
        public = self.manager.state()['groups'][0]['group_profile']
        self.assertEqual(public['revision'], '1')
        self.assertNotIn('updated_by', public)
        self.manager.conversation({'scope': 'group', 'id': 'test-group', 'action': 'reset'})
        self.assertEqual(json.loads(path.read_text())['state']['group_profile'], saved['group_profile'])
        self.manager.conversation({'scope': 'group', 'id': 'test-group', 'action': 'profile_reset', 'expected_revision': '1'})
        reset = json.loads(path.read_text())['state']['group_profile']
        self.assertEqual(reset['revision'], 2)
        self.assertEqual(reset['source_seq'], 1)
        self.assertFalse(any(reset['fields'].values()))

    def test_stale_or_malformed_profile_cannot_change_state(self):
        path = self.document()
        self.manager.conversation(self.body())
        before = path.read_bytes()
        bad = [self.body(), self.body(scope='private'), self.body(expected_revision=True),
               self.body(expected_revision='18446744073709551615'), self.body(expected_revision='1', mode='mode-all'),
               self.body(expected_revision='1', profile={'purpose':'partial'}),
               self.body(expected_revision='1', profile=dict.fromkeys(admin.GROUP_PROFILE_FIELDS, '字'*600)),
               self.body(expected_revision='1', profile={**self.body()['profile'], 'style':'a\0b'}),
               self.body(expected_revision='1', profile={**self.body()['profile'], 'style':'a\u202eb'})]
        for body in bad:
            with self.subTest(body=body), self.assertRaises(admin.AdminError):
                self.manager.conversation(body)
            self.assertEqual(path.read_bytes(), before)

    def test_unbound_busy_and_failed_validation_leave_profile_unchanged(self):
        path = self.document(status='generating')
        before = path.read_bytes()
        with self.assertRaises(admin.AdminError): self.manager.conversation(self.body())
        self.assertEqual(path.read_bytes(), before)
        self.document()
        before = path.read_bytes()
        original = self.manager.run.side_effect
        self.manager.run.side_effect = admin.AdminError('synthetic validation failure', 409)
        with self.assertRaises(admin.AdminError): self.manager.conversation(self.body())
        self.assertEqual(path.read_bytes(), before)
        self.manager.run.side_effect = original
        self.manager.conversation({'scope':'group','id':'test-group','action':'unbind'})
        with self.assertRaises(admin.AdminError): self.manager.conversation(self.body())
        self.assertEqual(path.read_bytes(), before)


class InteractionAndStickerTests(AdminFixture):
    def test_feedback_defaults_fill_legacy_config_without_overwriting_intervals(self):
        self.config['interactions'] = {'progress_interval_seconds': 73, 'long_timeout_seconds': 0}
        self.write(self.config_path, admin.toml_dump(self.config))
        value = self.manager.state()['config']['interactions']
        self.assertEqual(value['feedback_mode'], 'standard')
        self.assertEqual(value['ack_delay_seconds'], 5)
        self.assertEqual(value['heartbeat_interval_seconds'], 90)
        self.assertEqual(value['progress_interval_seconds'], 73)
        self.assertEqual(value['long_timeout_seconds'], 0)
        self.assertEqual(self.actions, [])

    def test_feedback_settings_roundtrip_preserves_mode_and_other_settings(self):
        self.config['mode'] = 'mode-3'
        self.config['enabled_tools'] = ['time_now']
        self.write(self.config_path, admin.toml_dump(self.config))
        for mode in ['quiet', 'standard', 'detailed']:
            settings = {**admin.INTERACTION_DEFAULTS, 'feedback_mode': mode,
                        'ack_delay_seconds': 7, 'heartbeat_interval_seconds': 120,
                        'progress_interval_seconds': 55}
            with self.subTest(mode=mode):
                self.manager.update_config({'section': 'interactions', **settings})
                saved = tomllib.loads(self.config_path.read_text())
                self.assertEqual(saved['interactions'], settings)
                self.assertEqual(self.manager.state()['config']['interactions'], settings)
                self.assertEqual(saved['mode'], 'mode-3')
                self.assertEqual(saved['enabled_tools'], ['time_now'])
                self.assertEqual(saved['interactions']['long_timeout_seconds'], 0)
                self.assertTrue(self.running)

    def test_feedback_validation_rejects_unknown_modes_and_invalid_times(self):
        for changed in [{'feedback_mode': 'chatty'}, {'feedback_mode': []}, {'feedback_mode': True},
                        {'ack_delay_seconds': 0}, {'ack_delay_seconds': 31}, {'ack_delay_seconds': True},
                        {'heartbeat_interval_seconds': -1}, {'heartbeat_interval_seconds': 1},
                        {'heartbeat_interval_seconds': 29}, {'heartbeat_interval_seconds': 1801},
                        {'heartbeat_interval_seconds': True}, {'heartbeat_interval_seconds': '90'}]:
            with self.subTest(changed=changed), self.assertRaises(admin.AdminError):
                admin.Manager.validate_interactions({**admin.INTERACTION_DEFAULTS, **changed})
        for value in [0, 30, 1800]:
            admin.Manager.validate_interactions({**admin.INTERACTION_DEFAULTS, 'heartbeat_interval_seconds': value})
        for value in [1, 30]:
            admin.Manager.validate_interactions({**admin.INTERACTION_DEFAULTS, 'ack_delay_seconds': value})

    def test_feedback_rejected_update_keeps_existing_config_and_restores_service(self):
        before = self.config_path.read_bytes()
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({'section': 'interactions', **admin.INTERACTION_DEFAULTS,
                                        'heartbeat_interval_seconds': 2})
        self.assertEqual(self.config_path.read_bytes(), before)
        self.assertTrue(self.running)

    def test_sticker_library_uses_existing_admin_proxy_route_without_service_restart(self):
        result = mock.Mock(returncode=0, stdout=b'{"catalogue":{"revision":0,"assets":[]}}')
        body = {'section': 'sticker_library', 'action': 'list'}
        with mock.patch('clawmingle_admin.subprocess.run', return_value=result) as run:
            self.assertEqual(self.request('POST', '/api/config', authorized=False, body=body)[0], 401)
            run.assert_not_called()
            status, _, value = self.request('POST', '/api/config', body=body)
        self.assertEqual(status, 200)
        self.assertEqual(value['catalogue']['assets'], [])
        self.assertEqual(json.loads(run.call_args.kwargs['input']), {'action': 'list'})
        self.assertEqual(self.actions, [])

    def test_large_nonlibrary_configuration_remains_rejected(self):
        status, _, _ = self.request('POST', '/api/config', body={'section': 'persona', 'soul': 'x' * 65536})
        self.assertEqual(status, 413)
        self.assertEqual(self.actions, [])

    def test_interaction_limits_allow_unlimited_total_but_bound_individual_tools(self):
        admin.Manager.validate_interactions(dict(admin.INTERACTION_DEFAULTS))
        for limit in [0, 60, 900, 3600]:
            admin.Manager.validate_interactions({**admin.INTERACTION_DEFAULTS, 'long_timeout_seconds': limit})
        for changed in [{'tool_calls': True}, {'long_timeout_seconds': 3601},
                        {'long_timeout_seconds': True}, {'long_timeout_seconds': -1}, {'long_timeout_seconds': 59},
                        {'workspace_timeout_seconds': 301}, {'long_max_bubbles': 9},
                        {'progress_interval_seconds': 0}]:
            with self.subTest(changed=changed), self.assertRaises(admin.AdminError):
                admin.Manager.validate_interactions({**admin.INTERACTION_DEFAULTS, **changed})
        with self.assertRaises(admin.AdminError):
            admin.Manager.validate_interactions({'enabled': True})

    def test_integrations_catalogue_omits_endpoints_credentials_and_disabled_services(self):
        self.manager.config['integrations'] = {
            'browser': {'enabled': True, 'runtime': {'node': '/private/runtime'}},
            'plugins': {
                'docs': {'enabled': True, 'name': 'Public docs', 'endpoint': 'https://example.com/mcp',
                         'credential_file': '/private/credentials', 'allowed_tools': ['search']},
                'disabled': {'enabled': False, 'name': 'Unavailable'},
            },
        }
        result = self.manager.integrations_public()
        self.assertEqual(result, {'browser': {'enabled': True, 'configured': True}, 'plugins': [
            {'id': 'docs', 'name': 'Public docs', 'tools': ['search'], 'read_only': True}]})
        self.manager.config['integrations']['browser'].pop('runtime')
        self.assertFalse(self.manager.integrations_public()['browser']['configured'])

    def test_browser_toggle_preserves_runtime_and_uses_operator_transaction(self):
        self.config['integrations'] = {'browser': {'enabled': False, 'runtime': {'node': '/trusted/node'}}}
        self.write(self.config_path, admin.toml_dump(self.config))
        self.manager.update_config({'section': 'browser', 'enabled': True})
        persisted = tomllib.loads(self.config_path.read_text())
        self.assertEqual(persisted['integrations']['browser'], {'enabled': True, 'runtime': {'node': '/trusted/node'}})
        self.assertEqual(self.actions, ['stop', 'start'])
        for value in ('true', 1, None):
            with self.assertRaises(admin.AdminError):
                self.manager.update_config({'section': 'browser', 'enabled': value})

    def test_conversation_grants_are_explicit_and_reject_unregistered_services(self):
        path = self.document()
        cfg = tomllib.loads(self.config_path.read_text())
        cfg['integrations'] = {'browser': {'enabled': True, 'runtime': {'node': '/trusted/node'}},
                               'plugins': {'docs': {'enabled': True, 'name': 'Docs'},
                                           'disabled': {'enabled': False, 'name': 'Unavailable'}}}
        self.write(self.config_path, admin.toml_dump(cfg))
        body = {'scope': 'group', 'id': 'test-group', 'action': 'settings',
                'mode': 'mode-2', 'model_alias': '', 'browser_enabled': True, 'plugin_grants': ['docs']}
        self.manager.conversation(body)
        current = json.loads(path.read_text())['state']
        self.assertTrue(current['browser_enabled'])
        self.assertEqual(current['plugin_grants'], ['docs'])
        for grants in [['unknown'], ['disabled'], ['docs', 'docs']]:
            before = path.read_bytes()
            with self.subTest(grants=grants), self.assertRaises(admin.AdminError):
                self.manager.conversation({**body, 'plugin_grants': grants})
            self.assertEqual(path.read_bytes(), before)
        self.manager.conversation({**body, 'browser_enabled': False, 'plugin_grants': []})
        current = json.loads(path.read_text())['state']
        self.assertFalse(current['browser_enabled'])
        self.assertEqual(current['plugin_grants'], [])

    def codex_tikhub_fixture(self):
        config = self.home / '.codex/config.toml'
        servers = {'tikhub-' + platform: {
            'url': f'https://mcp.tikhub.io/{platform}/mcp',
            'http_headers_helper': 'python3 ' + str(self.home / '.codex/tikhub/http_headers.py'),
        } for platform in admin.TIKHUB_TOOLS}
        self.write(config, admin.toml_dump({'mcp_servers': servers}))
        folder = self.directory('owner-home/.codex/secrets')
        path = folder / 'tikhub_api_key'
        self.write(path, 'synthetic-dedicated-tikhub-value\n')
        return path

    def test_tikhub_reuses_only_selected_codex_connections_without_copying_key(self):
        source = self.codex_tikhub_fixture()
        before = source.read_bytes()
        self.manager.update_config({'section': 'tikhub', 'source': 'codex', 'enabled': True})
        plugins = self.manager.config['integrations']['plugins']
        self.assertEqual(set(plugins), {'tikhub-' + p for p in admin.TIKHUB_TOOLS})
        self.assertTrue(all(p['credential_file'] == str(source) for p in plugins.values()))
        self.assertEqual(source.read_bytes(), before)
        self.assertFalse((self.config_dir / 'tikhub-api.key').exists())
        state = self.manager.state()
        self.assertTrue(state['config']['connections']['tikhub']['configured'])
        self.assertEqual(state['config']['connections']['tikhub']['source'], 'codex')
        self.assertNotIn(before.decode().strip(), json.dumps(state))
        self.assertNotIn(str(source), json.dumps(state))

    def test_tikhub_reuse_never_executes_unrecognized_header_helper(self):
        self.codex_tikhub_fixture()
        path = self.home / '.codex/config.toml'
        config = tomllib.loads(path.read_text())
        config['mcp_servers']['tikhub-weibo']['http_headers_helper'] = 'untrusted-command'
        self.write(path, admin.toml_dump(config))
        before = self.config_path.read_bytes()
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({'section': 'tikhub', 'source': 'codex', 'enabled': True})
        self.assertEqual(before, self.config_path.read_bytes())

    def test_integration_keys_are_private_preserved_when_blank_and_clear_without_deleting_source(self):
        original = self.codex_tikhub_fixture()
        for service in ('tinyfish', 'tikhub'):
            with self.subTest(service=service):
                body = {'section': service, 'source': 'api_key', 'enabled': True, 'api_key': 'YOUR_MANAGED_INTEGRATION_KEY'}
                self.manager.update_config(body)
                path = self.config_dir / (service + '-api.key')
                self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                self.assertEqual(path.read_text(), body['api_key'] + '\n')
                self.manager.update_config({**body, 'api_key': ''})
                self.assertEqual(path.read_text(), body['api_key'] + '\n')
                state = self.manager.state()
                self.assertNotIn(body['api_key'], json.dumps(state))
                self.assertNotIn(str(path), json.dumps(state))
                self.manager.update_config({'section': service, 'source': 'api_key', 'enabled': True, 'remove_api_key': True})
                self.assertEqual(path.read_text(), '')
                self.assertFalse(self.manager.integration_connection(service)['enabled'])
                self.assertFalse(self.manager.integration_connection(service)['configured'])
                self.assertEqual(self.manager.integration_connection(service)['source'], 'api_key')
                with self.assertRaises(admin.AdminError):
                    self.manager.update_config({'section': service, 'source': 'api_key', 'enabled': True})
                self.assertEqual(original.read_text(), 'synthetic-dedicated-tikhub-value\n')

    def test_integration_validation_failure_rolls_back_configuration_and_new_key(self):
        self.validation_fails = True
        before = self.config_path.read_bytes()
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({'section': 'tikhub', 'source': 'api_key', 'enabled': True,
                                        'api_key': 'YOUR_ROLLBACK_API_KEY'})
        self.assertEqual(before, self.config_path.read_bytes())
        self.assertFalse((self.config_dir / 'tikhub-api.key').exists())
        self.assertTrue(self.running)

    def test_integration_cannot_choose_endpoint_path_tools_or_enable_without_auth(self):
        for service in ('tinyfish', 'tikhub'):
            body = {'section': service, 'source': 'api_key', 'enabled': True}
            for changed in ({}, {'endpoint': 'https://example.com/mcp'}, {'credential_file': '/outside'},
                            {'allowed_tools': ['write']}, {'api_key': 'YOUR_API_KEY' + '\nvalue'},
                            {'api_key': 'YOUR_CONFLICTING_API_KEY', 'remove_api_key': True}):
                before = self.config_path.read_bytes()
                with self.subTest(service=service, changed=list(changed)), self.assertRaises(admin.AdminError):
                    self.manager.update_config({**body, **changed})
                self.assertEqual(before, self.config_path.read_bytes())
                self.assertFalse((self.config_dir / (service + '-api.key')).exists())

    def test_tinyfish_uses_existing_bot_login_without_writing_credentials(self):
        self.write(self.runtime / 'auth.json', json.dumps({'tokens': {'access_token': 'YOUR_BOT_ACCESS_TOKEN'}}))
        self.manager.update_config({'section': 'tinyfish', 'source': 'codex', 'enabled': True})
        plugin = self.manager.config['integrations']['plugins']['tinyfish']
        self.assertTrue(plugin['codex_app'])
        self.assertNotIn('credential_file', plugin)
        self.assertEqual(plugin['allowed_tools'], ['search', 'fetch_content'])
        self.assertFalse((self.config_dir / 'tinyfish-api.key').exists())

    def test_tinyfish_empty_auth_file_does_not_enable_hosted_connection(self):
        self.assertFalse(self.manager.integration_connection('tinyfish')['codex_available'])
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({'section': 'tinyfish', 'source': 'codex', 'enabled': True})

    def test_tikhub_can_be_disabled_when_existing_codex_source_is_unavailable(self):
        source = self.codex_tikhub_fixture()
        self.manager.update_config({'section': 'tikhub', 'source': 'codex', 'enabled': True})
        source.unlink()
        self.manager.update_config({'section': 'tikhub', 'source': 'codex', 'enabled': False})
        self.assertFalse(self.manager.integration_connection('tikhub')['enabled'])

    def test_disabled_connection_grants_can_be_retained_or_revoked_but_not_newly_added(self):
        path = self.document()
        document = json.loads(path.read_text())
        document['state']['plugin_grants'] = ['tinyfish']
        self.write(path, json.dumps(document))
        config = tomllib.loads(self.config_path.read_text())
        config['integrations'] = {'plugins': {'tinyfish': {'enabled': False}}}
        self.write(self.config_path, admin.toml_dump(config))
        body = {'scope': 'group', 'id': 'test-group', 'action': 'settings', 'plugin_grants': ['tinyfish']}
        self.manager.conversation(body)
        self.assertEqual(json.loads(path.read_text())['state']['plugin_grants'], ['tinyfish'])
        self.manager.conversation({**body, 'plugin_grants': []})
        with self.assertRaises(admin.AdminError):
            self.manager.conversation(body)

    def test_integration_check_uses_bounded_cli_without_query_or_exposing_catalogue(self):
        self.manager.update_config({'section': 'tinyfish', 'source': 'api_key', 'enabled': True,
                                    'api_key': 'YOUR_CHECK_API_KEY'})
        original_run = self.manager.run
        def run(args, timeout=30):
            if args[0] == str(self.manager.binary) and args[1] == 'integration-check':
                self.assertEqual(args[2:4], ['--plugin', 'tinyfish'])
                self.assertNotIn('YOUR_CHECK_API_KEY', ' '.join(args))
                self.assertEqual(timeout, 50)
                return b'{"ok":true,"tool_count":2}'
            return original_run(args, timeout)
        with mock.patch.object(self.manager, 'run', side_effect=run):
            self.assertTrue(self.manager.check({'target': 'tinyfish'})['ok'])

    def test_sticker_bridge_uses_fixed_command_stdin_and_lossless_revisions(self):
        result = mock.Mock(returncode=0, stdout=json.dumps({'asset': {'revision': 18446744073709551615}}).encode())
        with mock.patch('clawmingle_admin.subprocess.run', return_value=result) as run:
            value = self.manager.stickers({'action': 'preview', 'id': 'sticker-1', 'revision': '18446744073709551615'})
        self.assertEqual(value['asset']['revision'], '18446744073709551615')
        args, kwargs = run.call_args
        self.assertEqual(args[0], [str(self.manager.binary), 'stickers', '--config', str(self.config_path)])
        self.assertEqual(json.loads(kwargs['input'])['revision'], 18446744073709551615)
        self.assertEqual(kwargs['stderr'], admin.subprocess.DEVNULL)

    def test_sticker_invalid_identity_revision_and_fields_never_invoke_binary(self):
        for payload in [
            {'action': 'preview', 'id': '../../outside', 'revision': '1'},
            {'action': 'remove', 'id': 'one', 'revision': True},
            {'action': 'remove', 'id': 'one', 'revision': '18446744073709551616'},
            {'action': 'list', 'path': '/private'},
        ]:
            with self.subTest(payload=payload), self.assertRaises(admin.AdminError):
                self.manager.stickers(payload)

    def test_sticker_import_size_is_checked_before_external_operation(self):
        payload = {'action': 'import', 'name': 'Test', 'tags': [], 'description': '',
                   'when_to_use': '', 'avoid_when': '', 'enabled': True,
                   'data_base64': 'A' * 2796205}
        with self.assertRaises(admin.AdminError) as failure:
            self.manager.stickers(payload)
        self.assertEqual(failure.exception.status, 413)

    def test_sticker_series_move_and_taxonomy_forward_only_validated_fields(self):
        result = mock.Mock(returncode=0, stdout=b'{"asset":{"revision":3}}')
        operations = [
            {'action': 'series_create', 'series_id': 'cat-friends', 'name': '猫猫朋友'},
            {'action': 'move', 'id': 'sticker-1', 'revision': '2', 'series_id': 'cat-friends'},
            {'action': 'taxonomy'},
        ]
        for body in operations:
            with self.subTest(body=body), mock.patch('clawmingle_admin.subprocess.run', return_value=result) as run:
                self.manager.stickers(body)
                expected = dict(body)
                if 'revision' in expected:
                    expected['revision'] = int(expected['revision'])
                self.assertEqual(json.loads(run.call_args.kwargs['input']), expected)
        self.assertEqual(self.actions, [])

    def test_sticker_taxonomy_and_series_validation_rejects_injection_and_duplicates(self):
        base = {'action': 'import', 'name': '猫猫', 'tags': [], 'description': '',
                'when_to_use': '', 'avoid_when': '', 'enabled': True, 'data_base64': 'AAAA'}
        bad = [
            {**base, 'series_id': '../outside'}, {**base, 'styles': ['cute', 'cute']},
            {**base, 'styles': ['unknown']}, {**base, 'emotions': [['happy']]},
            {**base, 'emotions': ['happy', 'sad', 'speechless', 'amused', 'calm', 'angry', 'surprised']},
            {**base, 'metadata_status': 'ready'}, {**base, 'asset_type': 'static'},
            {**base, 'summary': 'x' * 301},
            {'action': 'series_create', 'series_id': 'Upper', 'name': 'Unsafe'},
            {'action': 'move', 'id': 'one', 'revision': '2', 'series_id': 'default', 'path': '/private'},
        ]
        for body in bad:
            with self.subTest(body=body), self.assertRaises(admin.AdminError):
                self.manager.stickers(body)
        with mock.patch('clawmingle_admin.subprocess.run', return_value=mock.Mock(returncode=0, stdout=b'{"duplicate":false,"asset":{}}')) as run:
            self.manager.stickers({**base, 'series_id': 'cat-friends', 'summary': '开心小猫', 'styles': ['cute', 'animal'], 'emotions': ['happy']})
            self.assertEqual(json.loads(run.call_args.kwargs['input'])['styles'], ['cute', 'animal'])
            self.manager.stickers({**base, 'summary': '尴尬地翻白眼', 'styles': ['teasing'], 'emotions': ['embarrassed']})
            self.assertEqual(json.loads(run.call_args.kwargs['input'])['styles'], ['teasing'])

    def test_sticker_preferences_are_scoped_and_operator_events_are_unique(self):
        self.document()
        result = mock.Mock(returncode=0, stdout=b'{"scores":[]}')
        body = {'action': 'preference', 'target': 'test-group', 'private': False,
                'dimension': 'style', 'value': 'cute', 'operation': 'avoid'}
        with mock.patch('clawmingle_admin.subprocess.run', return_value=result) as run:
            self.manager.stickers(body)
            first = json.loads(run.call_args.kwargs['input'])
            self.manager.stickers(body)
            second = json.loads(run.call_args.kwargs['input'])
        self.assertTrue(first['source_event_id'].startswith('admin:'))
        self.assertNotEqual(first['source_event_id'], second['source_event_id'])
        self.assertEqual({key: value for key, value in first.items() if key != 'source_event_id'}, body)
        self.assertEqual(self.actions, [])
        for override in [{'target': 'unbound'}, {'private': True}, {'source_event_id': 'pretend'},
                         {'scope': 'g_guess'}, {'dimension': 'emotion'}, {'operation': []}]:
            with self.subTest(override=override), self.assertRaises(admin.AdminError):
                self.manager.stickers({**body, **override})

    def test_sticker_auto_collection_setting_is_separate_from_automatic_sending(self):
        body = {'section': 'stickers', **admin.STICKER_DEFAULTS, 'automatic': False, 'auto_collect': True}
        self.manager.update_config(body)
        cfg = tomllib.loads(self.config_path.read_text())
        self.assertFalse(cfg['stickers']['automatic'])
        self.assertTrue(cfg['stickers']['auto_collect'])
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({**body, 'auto_collect': 1})

    def test_sticker_bridge_does_not_leak_invalid_cli_response(self):
        result = mock.Mock(returncode=0, stdout=b'private-response-marker')
        with mock.patch('clawmingle_admin.subprocess.run', return_value=result), self.assertRaises(admin.AdminError) as failure:
            self.manager.stickers({'action': 'list'})
        self.assertNotIn('private-response-marker', str(failure.exception))

    def test_sticker_expression_policy_is_editable_and_resettable(self):
        body = {'section': 'stickers', **admin.STICKER_DEFAULTS, 'usage_instructions': '接梗但不嘲讽低落的人。'}
        self.manager.update_config(body)
        self.assertEqual(tomllib.loads(self.config_path.read_text())['stickers']['usage_instructions'], body['usage_instructions'])
        self.manager.update_config({**body, 'usage_instructions': ''})
        self.assertEqual(tomllib.loads(self.config_path.read_text())['stickers']['usage_instructions'], '')
        for invalid in [None, 1, 'x' * 4001, '\0']:
            with self.subTest(invalid=type(invalid).__name__), self.assertRaises(admin.AdminError):
                self.manager.update_config({**body, 'usage_instructions': invalid})


class SkillBudgetTests(AdminFixture):
    def fixture_skill(self, name, repetitions=1):
        path = self.directory('state/skills/' + name) / 'SKILL.md'
        self.write(path, '---\nname: ' + name + '\ndescription: "Fixture."\n---\n' + '中' * repetitions)
        return path

    def scoped_skills(self, names, private=False):
        path = self.document(private=private)
        stored = json.loads(path.read_text())
        stored['state']['enabled_skills'] = names
        self.write(path, json.dumps(stored))
        return path

    def select_global(self, names):
        self.config['enabled_skills'] = names
        self.write(self.config_path, admin.toml_dump(self.config))
        self.manager.load()

    def test_utf8_limit_rejects_chinese_before_saving_new_or_existing_skill(self):
        existing = self.fixture_skill('existing-skill')
        before = existing.read_bytes()
        config_before = self.config_path.read_bytes()
        instructions = '中' * 5500  # Valid character count, but over 16 KiB in UTF-8.
        for name in ('existing-skill', 'new-skill'):
            with self.subTest(name=name), self.assertRaises(admin.AdminError):
                self.manager.update_config({
                    'section': 'skill', 'action': 'save', 'name': name,
                    'description': 'Fixture.', 'instructions': instructions, 'enabled': False})
            self.assertEqual(self.config_path.read_bytes(), config_before)
        self.assertEqual(existing.read_bytes(), before)
        self.assertFalse((self.data / 'skills/new-skill/SKILL.md').exists())

    def test_empty_soul_budget_includes_the_effective_default_and_rolls_back(self):
        names = ['default-budget-a', 'default-budget-b', 'default-budget-c']
        for name in names:
            self.fixture_skill(name, 5000)
        self.select_global(names)
        original = self.config_path.read_bytes()
        with mock.patch.object(self.manager, 'behavior_settings', return_value={'soul': {'default': 'default-persona-' * 300}}) as settings:
            with self.assertRaises(admin.AdminError):
                self.manager.update_config({'section': 'persona', 'soul': '', 'web_search': 'disabled'})
        self.assertEqual(settings.call_count, 1)
        self.assertEqual(self.config_path.read_bytes(), original)

    def test_persona_and_skill_budget_failure_roll_back_all_written_files(self):
        paths = [self.fixture_skill('budget-a', 5100),
                 self.fixture_skill('budget-b', 5100),
                 self.fixture_skill('budget-c', 4500)]
        self.select_global(['budget-a', 'budget-b'])
        scoped = self.scoped_skills(['budget-c'])
        paths += [self.config_path, self.runtime / 'config.toml', scoped]
        before = {path: path.read_bytes() for path in paths}
        payloads = [
            {'section': 'persona', 'soul': '中' * 2500, 'web_search': 'live'},
            {'section': 'skill', 'action': 'save', 'name': 'budget-c',
             'description': 'Fixture.', 'instructions': '中' * 5400, 'enabled': False},
        ]
        for body in payloads:
            with self.subTest(section=body['section']), self.assertRaises(admin.AdminError):
                self.manager.update_config(body)
            for path, content in before.items():
                self.assertEqual(path.read_bytes(), content)
            self.assertTrue(self.running)

    def test_global_and_scoped_selection_deduplicates_then_enforces_sixteen_limit(self):
        names = [f'slot-{index:02d}' for index in range(16)]
        for name in names:
            self.fixture_skill(name)
        self.select_global(names[:15])
        scoped = self.scoped_skills([names[14], names[15]])
        # The overlap is one skill, so this is sixteen, not seventeen.
        self.manager.update_config({'section': 'persona', 'soul': 'Fixture persona.', 'web_search': 'disabled'})
        before = {path: path.read_bytes() for path in (self.config_path, scoped)}
        with self.assertRaises(admin.AdminError):
            self.manager.update_config({
                'section': 'skill', 'action': 'save', 'name': 'slot-extra',
                'description': 'Fixture.', 'instructions': 'Text-only instructions.', 'enabled': True})
        for path, content in before.items():
            self.assertEqual(path.read_bytes(), content)
        self.assertFalse((self.data / 'skills/slot-extra/SKILL.md').exists())

    def test_skill_delete_cleans_group_and_private_selections_but_preserves_history(self):
        self.fixture_skill('removed-skill')
        self.fixture_skill('retained-skill')
        self.select_global(['removed-skill'])
        documents = [self.scoped_skills(['removed-skill', 'retained-skill', 'removed-skill'], private)
                     for private in (False, True)]
        before = {path: json.loads(path.read_text()) for path in documents}
        self.manager.update_config({'section': 'skill', 'action': 'delete', 'name': 'removed-skill'})
        self.assertNotIn('removed-skill', self.manager.config.get('enabled_skills', []))
        for path in documents:
            current = json.loads(path.read_text())
            expected = before[path]
            expected['state']['enabled_skills'] = ['retained-skill']
            self.assertEqual(current, expected)
        self.assertEqual([skill['name'] for skill in self.manager.skill_list()], ['retained-skill'])


class PersonaFilesTests(AdminFixture):
    fixture_skill = SkillBudgetTests.fixture_skill
    select_global = SkillBudgetTests.select_global

    def body(self, **changes):
        return {'section': 'persona', 'identity': '你叫星河，是 AI 群友。',
                'soul': ' 坦率、有主见。\n', 'web_search': 'disabled', **changes}

    def test_migration_has_one_file_source_and_preserves_other_configuration(self):
        original = tomllib.loads(self.config_path.read_text())
        self.manager.update_config(self.body())
        stored = tomllib.loads(self.config_path.read_text())
        self.assertTrue(stored.pop('persona_files'))
        self.assertNotIn('soul', stored)
        original.pop('soul', None)
        self.assertEqual(stored, original)
        for name, expected in [('IDENTITY.md', self.body()['identity']), ('SOUL.md', self.body()['soul'])]:
            path = self.data / 'persona' / name
            self.assertEqual(path.read_text(), expected)
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertEqual((self.data / 'persona').stat().st_mode & 0o777, 0o700)
        state = self.manager.state()['config']
        self.assertEqual(state['persona']['identity'], self.body()['identity'])
        self.assertEqual(state['persona']['soul_source'], 'persona/SOUL.md')
        self.assertEqual(state['soul'], self.body()['soul'])

    def test_legacy_client_updates_file_soul_without_changing_identity(self):
        self.manager.update_config(self.body())
        self.manager.update_config({'section': 'persona', 'soul': '新性格', 'web_search': 'live'})
        self.assertEqual((self.data / 'persona/IDENTITY.md').read_text(), self.body()['identity'])
        self.assertEqual((self.data / 'persona/SOUL.md').read_text(), '新性格')
        self.assertNotIn('soul', tomllib.loads(self.config_path.read_text()))

    def test_configuration_failure_rolls_back_both_new_and_existing_persona_files(self):
        for existing in (False, True):
            if existing:
                self.manager.update_config(self.body())
            paths = [self.config_path, self.runtime / 'config.toml',
                     self.data / 'persona/IDENTITY.md', self.data / 'persona/SOUL.md']
            before = {p: p.read_bytes() if p.exists() else None for p in paths}
            self.validation_fails = True
            with self.assertRaises(admin.AdminError):
                self.manager.update_config(self.body(identity='新身份', soul='新性格', web_search='live'))
            self.validation_fails = False
            for path, content in before.items():
                self.assertEqual(path.read_bytes() if path.exists() else None, content)
            self.assertTrue(self.running)

    def test_empty_oversized_and_symlink_persona_inputs_are_rejected(self):
        before = self.config_path.read_bytes()
        for changes in [{'identity': ' '}, {'identity': '字' * 2001}, {'soul': '\n'}, {'soul': '字' * 8001}, {'identity': '无\0效'}]:
            with self.subTest(changes=next(iter(changes))), self.assertRaises(admin.AdminError):
                self.manager.update_config(self.body(**changes))
            self.assertEqual(self.config_path.read_bytes(), before)
        directory = self.directory('state/persona')
        link = directory / 'IDENTITY.md'
        link.symlink_to(self.root / 'missing-target')
        with self.assertRaises(admin.AdminError):
            self.manager.update_config(self.body())
        self.assertTrue(link.is_symlink())
        self.assertFalse((directory / 'SOUL.md').exists())

    def test_budget_counts_both_files_and_rolls_back_the_pair(self):
        names = ['persona-budget-a', 'persona-budget-b', 'persona-budget-c']
        for name in names:
            self.fixture_skill(name, 4600)
        self.select_global(names)
        original = self.config_path.read_bytes()
        with self.assertRaises(admin.AdminError):
            self.manager.update_config(self.body(identity='字' * 2000, soul='性格'))
        self.assertEqual(self.config_path.read_bytes(), original)
        self.assertFalse((self.data / 'persona/IDENTITY.md').exists())
        self.assertFalse((self.data / 'persona/SOUL.md').exists())


class DeviceCleanupTests(AdminFixture):
    def test_device_start_uses_validated_runtime_binary_with_independent_home(self):
        resolved = str(self.root / 'native-codex')
        process = mock.Mock()
        process.stdout = io.BytesIO(b'')
        process.poll.return_value = None
        personal = self.personal_auth.read_bytes()
        with mock.patch.object(self.manager, 'run', return_value=json.dumps({'binary': resolved}).encode()) as resolve, \
             mock.patch('clawmingle_admin.subprocess.Popen', return_value=process) as spawn:
            result = self.manager.login({'action': 'device_start'})
        resolve.assert_called_once_with([str(self.manager.binary), 'codex-binary', '--config', str(self.config_path)], 15)
        self.assertEqual(result['device']['status'], 'waiting')
        self.assertEqual(spawn.call_args.args[0], [resolved, 'login', '--device-auth'])
        staging = self.manager.device['directory']
        self.assertEqual(spawn.call_args.kwargs['cwd'], staging)
        self.assertEqual(spawn.call_args.kwargs['env']['HOME'], str(staging))
        self.assertEqual(spawn.call_args.kwargs['env']['CODEX_HOME'], str(staging))
        self.assertNotEqual(staging, self.home)
        self.assertEqual(self.personal_auth.read_bytes(), personal)

    def test_device_start_resolution_failure_creates_no_login_directory(self):
        before = set(self.data.iterdir())
        with mock.patch.object(self.manager, 'run', side_effect=admin.AdminError('Unavailable fixture', 502)):
            with self.assertRaises(admin.AdminError):
                self.manager.login({'action': 'device_start'})
        self.assertEqual(set(self.data.iterdir()), before)
        self.assertIsNone(self.manager.device)

    def test_device_start_spawn_failure_removes_only_new_empty_directory(self):
        before = set(self.data.iterdir())
        personal = self.personal_auth.read_bytes()
        with mock.patch.object(self.manager, 'run', return_value=b'{"binary":"/missing/native-codex"}'), \
             mock.patch('clawmingle_admin.subprocess.Popen', side_effect=OSError('fixture failure')):
            with self.assertRaises(admin.AdminError):
                self.manager.login({'action': 'device_start'})
        self.assertEqual(set(self.data.iterdir()), before)
        self.assertEqual(self.personal_auth.read_bytes(), personal)
        self.assertIsNone(self.manager.device)

    def staged_device(self, suffix, returncode=0):
        directory = self.directory('state/clawmingle-login-' + suffix)
        staged = json.dumps({'tokens': {'access_token': self.credential_values['personal']}}).encode()
        self.write(directory / 'auth.json', staged)
        process = mock.Mock()
        process.pid = 12345
        process.returncode = returncode
        process.poll.return_value = returncode
        process.wait.return_value = returncode
        device = {'process': process, 'directory': directory,
                  'started': admin.time.monotonic(), 'status': 'waiting'}
        self.manager.device = device
        self.manager.device_output = 'https://auth.openai.com/codex/device\nABCD-EFGHI'
        return device, staged

    @staticmethod
    def finish_process(process, result):
        process.returncode = result
        process.poll.return_value = result
        process.wait.return_value = result

    def test_success_copies_auth_before_removing_only_the_staging_directory(self):
        device, staged = self.staged_device('success')
        original_cleanup = self.manager.cleanup_device
        def cleanup_after_transfer(value):
            self.assertEqual((self.runtime / 'auth.json').read_bytes(), staged)
            self.assertTrue((device['directory'] / 'auth.json').exists())
            original_cleanup(value)
        with mock.patch.object(self.manager, 'cleanup_device', side_effect=cleanup_after_transfer):
            result = self.manager.login({'action': 'device_poll'})
        self.assertEqual(result['device'], {'status': 'completed', 'url': '', 'code': ''})
        self.assertFalse(device['directory'].exists())
        self.assertEqual((self.runtime / 'auth.json').read_bytes(), staged)
        self.assertEqual(self.manager.device_output, '')
        self.assertEqual(self.actions, ['stop', 'start'])
        self.assertTrue(self.personal_auth.exists())

    def test_failed_auth_transfer_preserves_staged_credentials_for_retry(self):
        device, staged = self.staged_device('retry')
        previous_auth = (self.runtime / 'auth.json').read_bytes()
        self.validation_fails = True
        with self.assertRaises(admin.AdminError):
            self.manager.login({'action': 'device_poll'})
        self.assertEqual((device['directory'] / 'auth.json').read_bytes(), staged)
        self.assertEqual((self.runtime / 'auth.json').read_bytes(), previous_auth)
        self.assertEqual(self.manager.device['status'], 'ready')
        self.assertTrue(self.running)

    def test_cancel_removes_an_exited_staging_process_without_sending_a_signal(self):
        device, _ = self.staged_device('already-exited', -15)
        previous_auth = (self.runtime / 'auth.json').read_bytes()
        self.assertTrue(self.manager.login({'action': 'device_cancel'})['ok'])
        self.assertFalse(device['directory'].exists())
        self.assertIsNone(self.manager.device)
        self.assertEqual(self.manager.device_output, '')
        self.assertEqual((self.runtime / 'auth.json').read_bytes(), previous_auth)
        self.assertEqual(self.actions, [])

    def test_cancel_waits_for_the_process_to_exit_before_removing_its_directory(self):
        device, _ = self.staged_device('running-cancel', None)
        process = device['process']
        def terminate(pid, signal):
            self.assertTrue(device['directory'].exists())
            self.assertEqual((pid, signal), (process.pid, admin.signal.SIGTERM))
            self.finish_process(process, -15)
        with mock.patch.object(admin.os, 'killpg', side_effect=terminate) as signal:
            self.manager.login({'action': 'device_cancel'})
        signal.assert_called_once()
        process.wait.assert_called_once_with(timeout=3)
        self.assertFalse(device['directory'].exists())

    def test_cancel_handles_process_disappearance_between_poll_and_signal(self):
        device, _ = self.staged_device('signal-race', None)
        def disappeared(*_):
            self.finish_process(device['process'], 0)
            raise ProcessLookupError('Fixture process already exited.')
        with mock.patch.object(admin.os, 'killpg', side_effect=disappeared):
            self.assertTrue(self.manager.login({'action': 'device_cancel'})['ok'])
        self.assertFalse(device['directory'].exists())
        self.assertIsNone(self.manager.device)

    def test_expired_state_is_persistent_and_clears_directory_and_code(self):
        device, _ = self.staged_device('expired', None)
        device['started'] -= 901
        with mock.patch.object(admin.os, 'killpg',
                               side_effect=lambda *_: self.finish_process(device['process'], -15)) as signal:
            first = self.manager.device_state()
            second = self.manager.device_state()
        self.assertEqual(first, {'status': 'expired', 'url': '', 'code': ''})
        self.assertEqual(second, first)
        self.assertEqual(device['status'], 'expired')
        self.assertEqual(signal.call_count, 1)
        self.assertFalse(device['directory'].exists())
        self.assertEqual(self.manager.device_output, '')

    def test_stubborn_process_is_killed_and_reaped_before_cleanup(self):
        device, _ = self.staged_device('forced-exit', None)
        process = device['process']
        process.wait.side_effect = [admin.subprocess.TimeoutExpired('fixture-device', 3), -9]
        def signal_process(_, signal):
            if signal == admin.signal.SIGKILL:
                process.returncode = -9
                process.poll.return_value = -9
        with mock.patch.object(admin.os, 'killpg', side_effect=signal_process) as signal:
            self.manager.login({'action': 'device_cancel'})
        self.assertEqual(signal.call_args_list, [
            mock.call(process.pid, admin.signal.SIGTERM),
            mock.call(process.pid, admin.signal.SIGKILL)])
        self.assertEqual(process.wait.call_count, 2)
        self.assertFalse(device['directory'].exists())

    def test_cleanup_refuses_a_process_that_is_still_running(self):
        device, staged = self.staged_device('not-exited', None)
        with self.assertRaises(admin.AdminError) as failure:
            self.manager.cleanup_device(device)
        self.assertEqual(failure.exception.status, 409)
        self.assertEqual((device['directory'] / 'auth.json').read_bytes(), staged)

    def test_cleanup_refuses_wrong_parent_name_permissions_or_symlink(self):
        device, _ = self.staged_device('safe-source')
        outside = self.directory('outside/clawmingle-login-outside')
        wrong_name = self.directory('state/personal-profile')
        nested = self.directory('state/nested/clawmingle-login-nested')
        unsafe = self.directory('state/clawmingle-login-unsafe')
        unsafe.chmod(0o755)
        actual = self.directory('state/retained-profile')
        linked = self.data / 'clawmingle-login-linked'
        linked.symlink_to(actual, target_is_directory=True)
        self.write(actual / 'keep.txt', 'retained fixture')
        for directory in (outside, wrong_name, nested, unsafe, linked):
            with self.subTest(directory=directory.name), self.assertRaises(admin.AdminError):
                self.manager.cleanup_device({**device, 'directory': directory})
            self.assertTrue(directory.exists())
        self.assertEqual((actual / 'keep.txt').read_text(), 'retained fixture')

    def test_cleanup_refuses_a_staging_directory_with_a_different_owner(self):
        device, staged = self.staged_device('wrong-owner')
        with mock.patch.object(admin.os, 'getuid', return_value=os.getuid() + 1):
            with self.assertRaises(admin.AdminError):
                self.manager.cleanup_device(device)
        self.assertEqual((device['directory'] / 'auth.json').read_bytes(), staged)


class ModelReferenceTests(AdminFixture):
    def test_partial_general_update_preserves_rust_default_presets(self):
        del self.config['model_presets']
        self.write(self.config_path, admin.toml_dump(self.config))
        self.manager.update_config({'section': 'general', 'reply_format': 'markdown'})
        saved = tomllib.loads(admin.private_read(self.config_path).decode())
        self.assertEqual(saved['reply_format'], 'markdown')
        self.assertEqual(saved['model_presets']['sol']['model'], 'gpt-6.1-sol')

    def test_removing_a_default_alias_is_refused_and_original_config_restored(self):
        path = self.document()
        stored = json.loads(admin.private_read(path))
        stored['state']['model_alias'] = 'sol'
        self.write(path, json.dumps(stored))
        before = admin.private_read(self.config_path)
        with self.assertRaises(admin.AdminError) as error:
            self.manager.update_config({'section': 'general', 'model_presets': {
                'other': {'model': 'fixture-other', 'reasoning_effort': 'medium'}}})
        self.assertEqual(error.exception.status, 409)
        self.assertEqual(admin.private_read(self.config_path), before)
        self.assertTrue(self.running)

    def test_completed_request_alias_does_not_block_removal(self):
        path = self.document()
        stored = json.loads(admin.private_read(path))
        stored['state']['requests']['1']['model_alias'] = 'sol'
        self.write(path, json.dumps(stored))
        self.manager.update_config({'section': 'general', 'model_presets': {
            'other': {'model': 'fixture-other', 'reasoning_effort': 'medium'}}})
        saved = tomllib.loads(admin.private_read(self.config_path).decode())
        self.assertNotIn('sol', saved['model_presets'])


class MemoryApiTests(AdminFixture):
    def setUp(self):
        super().setUp()
        self.config['allowed_groups'] = ['first-group', 'test-group', 'last-group']
        self.write(self.config_path, admin.toml_dump(self.config))
        self.write(self.data / 'private/groups/test-person.json', json.dumps({'group_id': 'test-person', 'state': {'messages': [], 'requests': {}}}))
        self.memory_calls = []
        self.memory_view = {
            'legacy': {'summary': {'authority_revision': 3, 'facts': 0, 'pending': 0, 'confirmed': 0}, 'entries': [], 'changes': []},
            'ledger': {'revision': 5, 'drafts': [{
                'id': 42, 'revision': 2, 'category': 'profile', 'subjects': ['Ufixture'],
                'content': '喜欢绿茶', 'disposition': 'retain', 'selection_reason': '长期偏好',
                'source_kind': 'automatic', 'certainty': 'explicit', 'evidence': [{'message_ref': 'M3', 'author': 'Ufixture', 'message_hash': 'fixture-hash', 'timestamp': 1700000000}],
                'expires_at': None, 'operation': {'action': 'append', 'supersedes': None},
                'created_at': 1700000000, 'updated_at': 1700000001, 'status': 'buffered', 'history': []}],
                'records': [], 'reviews': [], 'maintenance': {'history_cursor': 0, 'last_completed_day': None, 'last_error': None, 'updated_at': 1700000000}},
            'layout': {key: 'memory/groups/Gfixture/' + name for key, name in {
                'canonical': 'ledger.json', 'legacy_authority': 'authority.json', 'drafts_json': 'drafts.json',
                'drafts_markdown': 'drafts.md', 'revisions_json': 'revisions.json'}.items()},
            'settings': dict(admin.MEMORY_DEFAULTS),
        }
        self.memory_view['layout']['categories'] = [{'category': 'profile', 'path': 'memory/groups/Gfixture/profile.md'}]
        self.manager.memory_cli = mock.Mock(side_effect=self.simulate_memory_cli)

    def simulate_memory_cli(self, payload):
        self.memory_calls.append(payload)
        return json.loads(json.dumps(self.memory_view))

    def memory_request(self, action='view', **extra):
        return self.request('POST', '/api/memory', body={'action': action, 'target': 'test-group', 'private': False, **extra})

    def test_view_exposes_scoped_ledger_history_and_safe_layout_only_when_authenticated(self):
        status, headers, result = self.memory_request()
        self.assertEqual(status, 200)
        self.assertEqual(result['target'], 'test-group')
        self.assertFalse(result['private'])
        self.assertEqual(result['ledger']['drafts'][0]['id'], '42')
        self.assertEqual(result['ledger']['drafts'][0]['revision'], '2')
        self.assertEqual(result['ledger']['drafts'][0]['evidence'][0]['message_ref'], 'M3')
        self.assertEqual(result['layout']['canonical'], 'memory/groups/Gfixture/ledger.json')
        self.assertEqual(headers['Cache-Control'], 'no-store')
        self.assertEqual(self.memory_calls, [{'action': 'view', 'target': 'test-group', 'private': False}])
        self.assertEqual(self.request('POST', '/api/memory', authorized=False, body={'action': 'view', 'target': 'test-group', 'private': False})[0], 401)
        self.assertEqual(self.request('GET', '/api/memory')[0], 404)
        self.assertEqual(self.actions, [])

    def test_private_and_group_targets_must_be_known_in_their_own_scope(self):
        for target, private, expected in [('test-person', True, 200), ('test-person', False, 404),
                                          ('test-group', True, 404), ('unknown', False, 404)]:
            with self.subTest(target=target, private=private):
                status, _, result = self.memory_request(target=target, private=private)
                self.assertEqual(status, expected)
                if status == 200:
                    self.assertEqual(result['layout']['canonical'], 'private/memory/groups/Gfixture/ledger.json')
                    self.assertEqual(result['layout']['categories'][0]['path'], 'private/memory/groups/Gfixture/profile.md')
        self.assertEqual(self.memory_calls, [{'action': 'view', 'target': 'test-person', 'private': True}])

    def test_scopes_reload_and_do_not_depend_on_cli_group_indexes(self):
        self.assertEqual(self.memory_request()[0], 200)
        self.config['allowed_groups'] = ['test-group']
        self.write(self.config_path, admin.toml_dump(self.config))
        self.assertEqual(self.memory_request()[0], 200)
        self.assertTrue(all(call['target'] == 'test-group' for call in self.memory_calls))
        self.config['allowed_groups'] = []
        self.write(self.config_path, admin.toml_dump(self.config))
        self.assertEqual(self.memory_request()[0], 404)

    def test_all_mutations_propagate_exact_id_revision_and_content_without_lifecycle_changes(self):
        for action in ['edit', 'correct', 'reject', 'apply']:
            extra = {'id': '42', 'revision': '2'}
            if action in {'edit', 'correct'}:
                extra['content'] = '更正内容，保留来源。'
            with self.subTest(action=action):
                with mock.patch.object(self.manager, 'transaction', side_effect=AssertionError('No lifecycle change')):
                    self.assertEqual(self.memory_request(action, **extra)[0], 200)
                self.assertEqual(self.memory_calls[-1], {'action': action, 'target': 'test-group', 'private': False,
                                                       **{key: int(value) if key in {'id', 'revision'} else value for key, value in extra.items()}})
        self.assertEqual(self.memory_request('archive')[0], 200)
        self.assertEqual(self.actions, [])

    def test_legacy_group_view_and_maintain_map_to_new_fixed_operations(self):
        for action, expected in [('view', 'view'), ('maintain', 'archive')]:
            status, _, result = self.request('POST', '/api/memory', body={'action': action, 'group_id': 'test-group'})
            self.assertEqual(status, 200)
            self.assertIn('authority', result)
            self.assertIn('ledger', result)
            self.assertEqual(self.memory_calls[-1]['action'], expected)
        self.assertEqual(self.actions, [])

    def test_ids_remain_exact_above_javascript_integer_precision(self):
        large = 18446744073709551615
        self.memory_view['ledger']['drafts'][0]['id'] = large
        self.memory_view['ledger']['drafts'][0]['operation']['supersedes'] = large
        self.memory_view['ledger']['drafts'][0]['superseded_by'] = large
        status, _, result = self.memory_request('apply', id=str(large), revision='2')
        self.assertEqual(status, 200)
        self.assertEqual(result['ledger']['drafts'][0]['id'], str(large))
        self.assertEqual(result['ledger']['drafts'][0]['operation']['supersedes'], str(large))
        self.assertEqual(result['ledger']['drafts'][0]['superseded_by'], str(large))
        self.assertEqual(self.memory_calls[-1]['id'], large)
        for invalid in [large, '18446744073709551616', '0', True, '1.0', '-1', None]:
            with self.subTest(invalid=invalid):
                self.assertEqual(self.memory_request('apply', id=invalid, revision='2')[0], 400)

    def test_wrong_fields_paths_types_and_empty_content_never_invoke_cli(self):
        for extra in [{'target': '../test-group'}, {'target': 'test-group;stop'}, {'private': 1}, {'target': 'x' * 257},
                      {'path': '/arbitrary'}, {'id': 1}, {'author_id': 'someone'}, {'command': 'apply'}]:
            with self.subTest(extra=extra):
                self.assertEqual(self.memory_request(**extra)[0], 400)
        for content in ['', '  ', '字' * 501, 'bad\0content', None]:
            self.assertEqual(self.memory_request('edit', id='42', revision='2', content=content)[0], 400)
        for action in ['delete', 'rollback', 'confirm', 'add', None]:
            self.assertEqual(self.memory_request(action)[0], 400)
        self.assertEqual(self.memory_calls, [])

    def test_cli_uses_fixed_argv_and_json_stdin_never_a_shell_or_content_arguments(self):
        payload = {'action': 'edit', 'target': 'test-group', 'private': False, 'id': 42, 'revision': 2,
                   'content': '正文 $' + '(echo sensitive) `touch file` ; newline\n保留'}
        result = mock.Mock(returncode=0, stdout=json.dumps(self.memory_view).encode())
        with mock.patch('clawmingle_admin.subprocess.run', return_value=result) as run:
            response = admin.Manager.memory_cli(self.manager, payload)
        args, kwargs = run.call_args
        self.assertEqual(args[0], [str(self.manager.binary), 'memory-operation', '--config', str(self.config_path)])
        self.assertEqual(json.loads(kwargs['input']), payload)
        self.assertNotIn('shell', kwargs)
        self.assertNotIn(payload['content'], str(args))
        self.assertEqual(response['ledger']['revision'], 5)

    def test_failed_mutation_is_409_and_never_echoes_raw_errors_or_input_content(self):
        self.manager.memory_cli.side_effect = lambda payload: admin.Manager.memory_cli(self.manager, payload)
        result = mock.Mock(returncode=1, stdout=b'private fixture failure')
        with mock.patch('clawmingle_admin.subprocess.run', return_value=result):
            status, _, data = self.memory_request('edit', id='42', revision='1', content='private draft fixture')
        self.assertEqual(status, 409)
        self.assertNotIn('private', json.dumps(data))
        self.assertEqual(self.actions, [])

    def test_cli_failure_or_invalid_json_does_not_return_a_success_snapshot(self):
        for failed, stdout in [(True, b'private fixture'), (False, b'not json')]:
            self.manager.memory_cli.side_effect = lambda payload: admin.Manager.memory_cli(self.manager, payload)
            with mock.patch('clawmingle_admin.subprocess.run', return_value=mock.Mock(returncode=int(failed), stdout=stdout)):
                status, _, data = self.memory_request()
            self.assertEqual(status, 502)
            self.assertNotIn('ledger', data)
            self.assertNotIn('private fixture', json.dumps(data))

    def test_invalid_cli_schema_or_host_paths_are_rejected(self):
        for key, value in [('canonical', '/host/private.json'), ('canonical', '../private.json'), ('drafts_json', 'C:\\private.json')]:
            with self.subTest(value=value):
                self.memory_view['layout'][key] = value
                status, _, data = self.memory_request()
                self.assertEqual(status, 502)
                self.assertNotIn(value, json.dumps(data))
                self.memory_view['layout'][key] = 'memory/groups/Gfixture/valid.json'
        self.memory_view.pop('ledger')
        self.assertEqual(self.memory_request()[0], 502)


class MemorySettingsTests(AdminFixture):
    def test_defaults_and_valid_settings_are_public_and_tool_inventory_includes_memory(self):
        self.assertEqual(self.manager.state()['config']['memory'], admin.MEMORY_DEFAULTS)
        settings = dict(admin.MEMORY_DEFAULTS, enabled=False, automatic_capture=False, archive_hour=23, archive_minute=59, archive_every_days=30, short_term_days=90)
        self.manager.update_config({'section': 'memory', **settings})
        self.assertEqual(self.manager.state()['config']['memory'], settings)
        self.assertEqual(tomllib.loads(self.config_path.read_text())['memory'], settings)
        self.assertIn('memory_manage', admin.TOOLS)

    def test_memory_setting_bounds_types_and_unknown_fields_roll_back(self):
        original = self.config_path.read_bytes()
        for extra in [{'archive_hour': 24}, {'archive_minute': -1}, {'archive_every_days': 0}, {'short_term_days': 91},
                      {'enabled': 1}, {'automatic_capture': 'yes'}, {'time_zone': 'UTC'}, {'reminder': True}]:
            with self.subTest(extra=extra):
                with self.assertRaises(admin.AdminError):
                    self.manager.update_config({'section': 'memory', **admin.MEMORY_DEFAULTS, **extra})
                self.assertEqual(self.config_path.read_bytes(), original)
                self.assertTrue(self.running)


class PersonaToolSelectionTests(AdminFixture):
    def test_unknown_duplicate_and_invalid_tool_selections_are_rejected_without_changes(self):
        self.config['enabled_tools'] = ['time_now']
        self.write(self.config_path, admin.toml_dump(self.config))
        paths = [self.config_path, self.runtime / 'config.toml']
        before = {path: path.read_bytes() for path in paths}
        for selected in [['shell_exec'], ['time_now', 'time_now'],
                         ['public_web_read', 'unknown_tool'], 'time_now', None,
                         [True], [{'name': 'time_now'}]]:
            with self.subTest(selected=selected), self.assertRaises(admin.AdminError) as error:
                self.manager.update_config({'section': 'persona', 'soul': 'Changed persona.',
                                            'web_search': 'live', 'enabled_tools': selected})
            self.assertEqual(error.exception.status, 400)
            for path, content in before.items():
                self.assertEqual(path.read_bytes(), content)
            self.assertTrue(self.running)

    def test_legal_tool_save_and_legacy_persona_edits_preserve_selection_and_explicit_disable(self):
        self.assertEqual(set(admin.TOOLS), {'workspace_exec', 'time_now', 'weather_current', 'public_web_read', 'message_lookup', 'messages_list', 'tasks_manage', 'qq_info', 'group_documents', 'memory_manage', 'task_update', 'public_browser', 'plugin_tools', 'plugin_call', 'sticker_lookup', 'sticker_manage', 'identity_manage', 'group_profile_manage'})
        # Existing clients may omit the new field, including before it is stored.
        self.manager.update_config({'section': 'persona', 'soul': 'Legacy persona.',
                                    'web_search': 'disabled'})
        saved = tomllib.loads(admin.private_read(self.config_path).decode())
        self.assertNotIn('enabled_tools', saved)
        self.assertEqual(self.manager.state()['config']['enabled_tools'], admin.TOOLS)
        for selected in [admin.TOOLS, ['weather_current', 'time_now'], []]:
            with self.subTest(selected=selected):
                self.manager.update_config({'section': 'persona', 'soul': 'Selected tools.',
                                            'web_search': 'live', 'enabled_tools': selected})
                self.manager.update_config({'section': 'persona', 'soul': 'Legacy edit.',
                                            'web_search': 'cached'})
                saved = tomllib.loads(admin.private_read(self.config_path).decode())
                runtime = tomllib.loads(admin.private_read(self.runtime / 'config.toml').decode())
                self.assertEqual(saved['enabled_tools'], selected)
                self.assertEqual(saved['soul'], 'Legacy edit.')
                self.assertEqual(runtime['web_search'], 'cached')
                self.assertEqual(self.manager.state()['config']['enabled_tools'], selected)
                self.assertTrue(self.running)


class SkillPackageTests(AdminFixture):
    def package(self):
        folder = self.directory('state/skills/package-example')
        self.write(folder / 'SKILL.md', '---\nname: package-example\ndescription: "Complete fixture"\n---\nUse the isolated script.')
        scripts = self.directory('state/skills/package-example/scripts')
        self.write(scripts / 'run.py', 'print("fixture")')
        self.write(folder / '.clawmingle-package.json', json.dumps({
            'schema_version': 1, 'name': 'package-example',
            'source': 'https://raw.githubusercontent.com/example/repository/main/package-example/SKILL.md',
            'commit': 'a' * 40, 'installed_at': 0, 'files': [{'path': 'SKILL.md'}, {'path': 'scripts/run.py'}]}))
        return folder

    def test_packages_list_version_source_and_scopes_without_platform_ids(self):
        self.package()
        for private in (False, True):
            path = self.document(private=private)
            stored = json.loads(path.read_text())
            stored['state']['enabled_skills'] = ['package-example']
            self.write(path, json.dumps(stored))
        package = self.manager.skill_list()[0]
        self.assertTrue(package['read_only'])
        self.assertTrue(package['package']['valid'])
        self.assertEqual(package['package']['commit'], 'a' * 40)
        self.assertEqual(package['package']['files'], 2)
        self.assertEqual(package['scopes'], ['群聊1', '私聊1'])
        encoded = json.dumps(package)
        self.assertNotIn('test-group', encoded)
        self.assertNotIn('test-person', encoded)
        self.assertEqual(self.actions, [])

    def test_package_save_and_delete_cannot_overwrite_resources(self):
        folder = self.package()
        before = {str(path.relative_to(folder)): path.read_bytes()
                  for path in folder.rglob('*') if path.is_file()}
        for action in ('save', 'delete'):
            with self.assertRaises(admin.AdminError):
                self.manager.update_config({'section': 'skill', 'action': action,
                    'name': 'package-example', 'description': 'changed',
                    'instructions': 'changed', 'enabled': True})
        after = {str(path.relative_to(folder)): path.read_bytes()
                 for path in folder.rglob('*') if path.is_file()}
        self.assertEqual(before, after)
        self.assertEqual(self.actions, [])

    def test_invalid_package_manifest_is_visible_but_stays_read_only(self):
        folder = self.package()
        self.write(folder / '.clawmingle-package.json', '{}')
        package = self.manager.skill_list()[0]
        self.assertTrue(package['read_only'])
        self.assertFalse(package['package']['valid'])


class BehaviorSettingsTests(AdminFixture):
    def test_behavior_roundtrip_and_public_state_without_secrets(self):
        body = self.behavior_body()
        body['behavior']['modes']['mode-2']['name'] = '乐于帮助'
        self.manager.update_config(body)
        saved = self.manager.state()['behavior']
        self.assertEqual(saved['current'], body['behavior'])
        self.assertEqual(saved['context_windows'], body['context_windows'])
        self.assertEqual(saved['max_bubble_chars'], 2000)
        for secret in self.credential_values.values():
            self.assertNotIn(secret, json.dumps(saved))
        self.assertTrue(self.running)

    def test_optional_legacy_reply_text_loads_and_is_never_deleted_by_reset(self):
        body = self.behavior_body()
        self.manager.update_config(body)
        legacy = body['behavior']['modes']['mode-2']['reply_instructions']
        changed = self.behavior_body()
        for profile in changed['behavior']['modes'].values():
            profile.pop('reply_instructions')
        changed['behavior']['modes']['mode-2']['name'] = '只判断何时接话'
        self.manager.update_config(changed)
        saved = tomllib.loads(self.config_path.read_text())
        self.assertEqual(saved['behavior']['modes']['mode-2']['reply_instructions'], legacy)
        self.assertEqual(saved['behavior']['modes']['mode-2']['name'], '只判断何时接话')
        changed['behavior']['modes']['mode-2']['reply_instructions'] = ''
        self.manager.update_config(changed)
        self.assertEqual(tomllib.loads(self.config_path.read_text())['behavior']['modes']['mode-2']['reply_instructions'], legacy)

    def test_new_mode_profiles_accept_absent_or_empty_legacy_text(self):
        body = self.behavior_body()
        for index, profile in enumerate(body['behavior']['modes'].values()):
            if index % 2:
                profile.pop('reply_instructions')
            else:
                profile['reply_instructions'] = ''
        self.manager.update_config(body)
        current = self.manager.state()['behavior']['current']
        self.assertTrue(all(not profile.get('reply_instructions') for profile in current['modes'].values()))

    def test_soul_state_shows_effective_default_and_preserves_nonempty_custom_text(self):
        initial = self.manager.state()['config']
        self.assertEqual(initial['soul'], '统一人格默认测试文案')
        self.assertTrue(initial['soul_uses_default'])
        for soul in [' \n', ' 自定义人格，不开玩笑。 ']:
            self.manager.update_config({'section': 'persona', 'soul': soul, 'web_search': 'disabled'})
            current = self.manager.state()['config']
            self.assertEqual(current['soul'], soul if soul.strip() else '统一人格默认测试文案')
            self.assertEqual(current['soul_uses_default'], not soul.strip())
            self.assertEqual(tomllib.loads(self.config_path.read_text())['soul'], soul)

    def test_legacy_archive_still_rejects_invalid_or_oversize_values(self):
        original = self.config_path.read_bytes()
        for value in [None, 'x' * 2001, 'invalid\0archive']:
            body = self.behavior_body()
            body['behavior']['modes']['mode-2']['reply_instructions'] = value
            with self.assertRaises(admin.AdminError):
                self.manager.update_config(body)
            self.assertEqual(self.config_path.read_bytes(), original)

    def test_behavior_validation_rollback_preserves_original_config(self):
        original = self.config_path.read_bytes()
        invalid = [
            ('bypass', lambda b: b['behavior']['modes']['mode-1'].update(participation_instructions='always reply')),
            ('window', lambda b: b['context_windows'].update(jev_messages=41)),
            ('bool', lambda b: b.update(max_bubbles=True)),
            ('missing', lambda b: b['behavior']['categories'].pop('casual')),
            ('oversize', lambda b: b['behavior'].update(response_instructions='x' * 2001)),
            ('unknown', lambda b: b['behavior'].update(permissions='all')),
        ]
        for name, change in invalid:
            with self.subTest(name=name):
                body = self.behavior_body()
                change(body)
                with self.assertRaises(admin.AdminError):
                    self.manager.update_config(body)
                self.assertEqual(self.config_path.read_bytes(), original)
                self.assertTrue(self.running)
        self.validation_fails = True
        with self.assertRaises(admin.AdminError):
            self.manager.update_config(self.behavior_body())
        self.assertEqual(self.config_path.read_bytes(), original)
        self.assertTrue(self.running)



class DocumentAdminTests(AdminFixture):
    def test_document_capabilities_distinguish_body_editing_from_limited_types(self):
        public = self.manager.documents_public()
        self.assertEqual(set(public['body_types']),
                         {'smartcanvas', 'doc', 'sheet', 'slide', 'smartsheet', 'form', 'flowchart'})
        types = {row['id']: row for row in public['document_types']}
        self.assertEqual(len(types), 10)
        self.assertIn('人工发布', types['form']['note'])
        self.assertIn('未提供', types['mind']['note'])
        self.assertIn('未提供', types['board']['note'])
        self.assertIn('不支持', types['pdf']['note'])
        self.assertFalse(public['qq_group_online_list_integration'])
        self.assertFalse(public['authorized'])
        self.assertFalse(self.actions)

    def test_authorization_token_is_private_never_returned_and_enables_tool(self):
        value = 'synthetic-documents-token'
        result = self.manager.post('/api/documents', {'action': 'token', 'token': value})
        cfg = self.manager.load()
        path = Path(cfg['documents']['credential_file'])
        self.assertEqual(admin.private_read(path).decode(), value)
        self.assertIn('group_documents', cfg['enabled_tools'])
        self.assertNotIn(value, json.dumps(result))
        self.assertNotIn(value, json.dumps(self.manager.documents_public()))
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertTrue(self.running)

    def test_binding_is_group_only_and_cannot_share_another_groups_folder(self):
        self.manager.post('/api/documents', {'action': 'token', 'token': 'synthetic-documents-token'})
        with self.assertRaises(admin.AdminError):
            self.manager.post('/api/documents', {'action': 'bind', 'group': 'private-person', 'folder': 'folder1'})
        self.manager.post('/api/documents', {'action': 'bind', 'group': 'test-group', 'folder': 'folder1'})
        cfg = self.manager.load(); cfg['allowed_groups'].append('second-group')
        self.write(self.config_path, admin.toml_dump(cfg))
        with self.assertRaises(admin.AdminError):
            self.manager.post('/api/documents', {'action': 'bind', 'group': 'second-group', 'folder': 'folder1'})
        self.assertTrue(self.running)
        self.assertEqual(self.manager.load()['documents']['bindings'], {'test-group': 'folder1'})
        self.manager.post('/api/documents', {'action': 'unbind', 'group': 'test-group'})
        self.assertFalse(self.manager.load()['documents']['bindings'])

    def test_device_authorization_has_expiry_and_sends_no_network_request_until_check(self):
        result = self.manager.post('/api/documents', {'action': 'auth_start'})
        self.assertTrue(result['url'].startswith('https://docs.qq.com/scenario/open-claw.html?'))
        self.assertNotIn('token', result)
        pending = self.config_dir / 'tencent-docs-auth.json'
        self.assertEqual(pending.stat().st_mode & 0o777, 0o600)
        self.write(pending, '{}')
        with self.assertRaises(admin.AdminError):
            self.manager.post('/api/documents', {'action': 'auth_finish'})
        self.assertTrue(self.running)

    def test_failed_config_validation_rolls_back_new_credentials(self):
        self.validation_fails = True
        with self.assertRaises(admin.AdminError):
            self.manager.post('/api/documents', {'action': 'token', 'token': 'synthetic-documents-token'})
        self.assertNotIn('documents', self.manager.load())
        self.assertFalse((self.config_dir / 'tencent-docs.token').exists())
        self.assertTrue(self.running)


class QQNativeDocumentsTests(AdminFixture):
    def setUp(self):
        super().setUp()
        self.config['documents'] = {'enabled': True, 'credential_file': str(self.config_dir / 'docs.token'),
                                    'bindings': {'test-group': 'folder1'}}
        self.write(self.config_dir / 'docs.token', 'synthetic-documents-token')
        node = self.root / 'node'
        self.write(node, 'fixture'); node.chmod(0o700)
        self.config['integrations'] = {'browser': {'runtime': {'node': str(node),
            'node_modules': str(self.root / 'modules'), 'chromium': str(self.root / 'chromium')}}}
        self.write(self.config_path, admin.toml_dump(self.config))
        self.manager.load()
        self.write(self.root / 'clawmingle_qq_documents_login.mjs', 'fixture helper')
        self.stack.enter_context(mock.patch.object(admin, '__file__', str(self.root / 'clawmingle_admin.py')))
        self.process = mock.Mock(); self.process.poll.return_value = None
        self.spawn = self.stack.enter_context(mock.patch('clawmingle_admin.subprocess.Popen', return_value=self.process))

    def start(self):
        return self.manager.post('/api/documents', {'action': 'native_auth_start', 'group': 'test-group', 'group_code': '123456789'})['flow']

    def complete(self, flow):
        folder = self.config_dir / 'qq-documents-login' / ('flow_' + flow['flow_id'])
        meta = json.loads((folder / 'metadata.json').read_text())
        self.write(folder / 'status.json', json.dumps({'state': 'authorized', 'nonce': meta['nonce'],
            'group_code': meta['group_code'], 'verified_group_codes': meta['group_codes']}))
        self.write(folder / 'browser-state.json', json.dumps({'cookies': [
            {'name': 'fixture', 'value': 'synthetic-browser-secret', 'domain': '.docs.qq.com'}], 'origins': []}))
        self.process.poll.return_value = 0
        return folder

    def finish(self, flow):
        return self.manager.post('/api/documents', {'action': 'native_auth_finish', 'flow_id': flow['flow_id']})

    def test_start_is_nonblocking_isolated_and_returns_no_secret_or_paths(self):
        flow = self.start()
        folder, metadata, _ = self.manager.qq_documents_flow(flow['flow_id'])
        self.assertEqual(flow['state'], 'starting')
        self.assertEqual(folder.stat().st_mode & 0o777, 0o700)
        self.assertEqual((folder / 'input.json').stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads((folder / 'input.json').read_text())['group_codes'], ['123456789'])
        args, kwargs = self.spawn.call_args
        self.assertIn('--runtime', args[0]); self.assertEqual(kwargs['env']['HOME'], str(folder))
        self.assertNotIn(metadata['nonce'], json.dumps(flow))
        self.assertNotIn(str(self.root), json.dumps(flow))
        self.assertEqual(self.actions, [])
        with self.assertRaises(admin.AdminError): self.start()

    def test_unknown_groups_numbers_and_arbitrary_arguments_do_not_spawn(self):
        for body in [
            {'action': 'native_auth_start', 'group': 'private-person', 'group_code': '123456789'},
            {'action': 'native_auth_start', 'group': 'test-group', 'group_code': '../escape'},
            {'action': 'native_auth_start', 'group': 'test-group', 'group_code': '123456789', 'runtime': '/untrusted'},
            {'action': 'native_auth_cancel', 'flow_id': '/tmp/other'},
            {'action': 'native_auth_cancel', 'flow_id': '0' * 32, 'pid': 123},
        ]:
            with self.subTest(body=body), self.assertRaises(admin.AdminError):
                self.manager.post('/api/documents', body)
        self.spawn.assert_not_called()

    def test_qr_status_requires_dashboard_auth_and_private_nonce_checked_flow(self):
        flow = self.start(); folder, meta, _ = self.manager.qq_documents_flow(flow['flow_id'])
        self.write(folder / 'status.json', json.dumps({'state': 'waiting_scan', 'nonce': meta['nonce']}))
        self.write(folder / 'qr.png', b'\x89PNG\r\n\x1a\nfixture')
        body = {'action': 'native_auth_status', 'flow_id': flow['flow_id']}
        self.assertEqual(self.request('POST', '/api/documents', body=body, authorized=False)[0], 401)
        status, headers, result = self.request('POST', '/api/documents', body=body)
        self.assertEqual(status, 200)
        self.assertEqual(headers['Cache-Control'], 'no-store')
        self.assertTrue(result['qr_data_url'].startswith('data:image/png;base64,'))
        self.assertEqual(admin.base64.b64decode(result['qr_data_url'].split(',', 1)[1]), b'\x89PNG\r\n\x1a\nfixture')
        self.assertNotIn('qr_data_url', json.dumps(self.manager.qq_documents_public()))
        self.assertEqual(self.request('GET', '/assets/qr.png')[0], 404)
        self.assertEqual(self.request('GET', '/api/documents/native-qr/' + flow['flow_id'])[0], 404)
        self.write(folder / 'status.json', json.dumps({'state': 'waiting_scan', 'nonce': '0' * 64}))
        self.assertEqual(self.request('POST', '/api/documents', body=body)[0], 409)

    def test_qr_symlinks_and_public_permissions_are_rejected(self):
        flow = self.start(); folder, meta, _ = self.manager.qq_documents_flow(flow['flow_id'])
        self.write(folder / 'status.json', json.dumps({'state': 'waiting_scan', 'nonce': meta['nonce']}))
        outside = self.root / 'unrelated'; self.write(outside, b'\x89PNG\r\n\x1a\nfixture')
        (folder / 'qr.png').symlink_to(outside)
        body = {'action': 'native_auth_status', 'flow_id': flow['flow_id']}
        self.assertEqual(self.request('POST', '/api/documents', body=body)[0], 500)
        (folder / 'qr.png').unlink(); self.write(folder / 'qr.png', outside.read_bytes())
        (folder / 'qr.png').chmod(0o644)
        self.assertEqual(self.request('POST', '/api/documents', body=body)[0], 500)

    def test_expired_cancelled_or_wrong_group_authorization_never_saves(self):
        flow = self.start(); folder = self.complete(flow)
        status = json.loads((folder / 'status.json').read_text()); status['group_code'] = '987654321'
        self.write(folder / 'status.json', json.dumps(status))
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.complete(flow)
        self.manager.post('/api/documents', {'action': 'native_auth_cancel', 'flow_id': flow['flow_id']})
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.assertEqual(self.actions, [])
        self.assertFalse(self.manager.config['documents'].get('qq_native'))
        flow = self.start(); folder = self.complete(flow)
        metadata = json.loads((folder / 'metadata.json').read_text()); metadata['expires_at'] = int(time.time()) - 1
        self.write(folder / 'metadata.json', json.dumps(metadata))
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.assertEqual(self.actions, [])

    def test_finish_copies_only_private_browser_state_preserves_documents_and_is_idempotent(self):
        flow = self.start(); folder = self.complete(flow)
        result = self.finish(flow)
        cfg = self.manager.load(); native = cfg['documents']['qq_native']
        credential = Path(native['browser_state_file'])
        self.assertTrue(native['enabled']); self.assertEqual(native['groups'], {'test-group': '123456789'})
        self.assertEqual(cfg['documents']['bindings'], {'test-group': 'folder1'})
        self.assertEqual(cfg['documents']['credential_file'], self.config['documents']['credential_file'])
        self.assertEqual(credential.stat().st_mode & 0o777, 0o600)
        self.assertFalse((folder / 'browser-state.json').exists())
        public = self.manager.documents_public()
        self.assertTrue(public['qq_group_online_list_integration'])
        self.assertEqual(public['qq_native']['active_groups'], ['test-group'])
        self.assertEqual(public['qq_native']['login_validity'], 'not_checked')
        self.assertNotIn('synthetic-browser-secret', json.dumps([public, result]))
        self.assertNotIn(str(self.config_dir), json.dumps(public))
        before = list(self.actions); self.finish(flow); self.assertEqual(self.actions, before)
        self.assertTrue(self.running)

    def test_failed_config_validation_restores_old_profile_and_leaves_flow_retryable(self):
        flow = self.start(); folder = self.complete(flow)
        cred_dir = self.directory('configuration/qq-documents-browser')
        old = cred_dir / 'browser-state.json'; self.write(old, '{"old":true}')
        original = self.config_path.read_bytes()
        self.validation_fails = True
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.assertEqual(old.read_text(), '{"old":true}')
        self.assertEqual(self.config_path.read_bytes(), original)
        self.assertEqual(self.manager.qq_documents_flow(flow['flow_id'])[2]['state'], 'authorized')
        self.assertTrue((folder / 'browser-state.json').exists())
        self.assertTrue(self.running)

    def test_relogin_requires_access_to_all_bound_groups_and_detects_later_changes(self):
        self.config['allowed_groups'].append('other-group')
        self.config['documents']['qq_native'] = {'enabled': False, 'groups': {'other-group': '987654321'}}
        self.write(self.config_path, admin.toml_dump(self.config))
        flow = self.start(); folder = self.complete(flow)
        self.assertEqual(json.loads((folder / 'input.json').read_text())['group_codes'], ['123456789', '987654321'])
        status = json.loads((folder / 'status.json').read_text()); status['verified_group_codes'] = ['123456789']
        self.write(folder / 'status.json', json.dumps(status))
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.complete(flow)
        cfg = self.manager.load(); cfg['documents']['qq_native']['groups']['other-group'] = '876543210'
        self.write(self.config_path, admin.toml_dump(cfg))
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.assertFalse(self.manager.config['documents']['qq_native']['enabled'])

    def test_browser_state_rejects_unrelated_credentials_and_local_storage(self):
        flow = self.start(); folder = self.complete(flow)
        for value in [
            {'cookies': [{'name': 'fixture', 'value': 'synthetic', 'domain': '.example.com'}], 'origins': []},
            {'cookies': [], 'origins': []},
            {'cookies': [{'name': 'fixture', 'value': 'synthetic', 'domain': '.docs.qq.com'}], 'origins': [{'origin': 'https://docs.qq.com'}]},
        ]:
            self.write(folder / 'browser-state.json', json.dumps(value))
            with self.assertRaises(admin.AdminError): self.finish(flow)
        self.assertEqual(self.actions, [])

    def test_pause_enable_unbind_preserve_credentials_and_other_document_bindings(self):
        flow = self.start(); self.complete(flow); self.finish(flow)
        credential = Path(self.manager.config['documents']['qq_native']['browser_state_file'])
        before = credential.read_bytes()
        self.manager.post('/api/documents', {'action': 'native_disable'})
        self.assertFalse(self.manager.documents_public()['qq_group_online_list_integration'])
        self.manager.post('/api/documents', {'action': 'native_enable'})
        self.assertTrue(self.manager.documents_public()['qq_group_online_list_integration'])
        self.manager.post('/api/documents', {'action': 'native_unbind', 'group': 'test-group'})
        self.assertFalse(self.manager.documents_public()['qq_group_online_list_integration'])
        self.assertEqual(credential.read_bytes(), before)
        self.assertEqual(self.manager.config['documents']['bindings'], {'test-group': 'folder1'})

    def test_removed_conversation_expires_pending_flow_and_cannot_save(self):
        flow = self.start(); self.complete(flow)
        cfg = self.manager.load(); cfg['allowed_groups'] = []
        self.write(self.config_path, admin.toml_dump(cfg)); self.manager.load()
        self.assertEqual(self.manager.qq_documents_public()['flow']['state'], 'expired')
        with self.assertRaises(admin.AdminError): self.finish(flow)
        self.assertEqual(self.actions, [])

    def test_native_capabilities_report_the_nine_verified_types(self):
        self.assertEqual(set(self.manager.qq_documents_public()['verified_types']),
                         {'doc', 'smartcanvas', 'sheet', 'slide', 'smartsheet', 'form', 'mind', 'flowchart', 'board'})

    def test_unbinding_directory_removes_only_matching_native_registration(self):
        flow = self.start(); self.complete(flow); self.finish(flow)
        cfg = self.manager.load()
        cfg['allowed_groups'].append('other-group')
        cfg['documents']['bindings']['other-group'] = 'folder2'
        cfg['documents']['qq_native']['groups']['other-group'] = '987654321'
        self.write(self.config_path, admin.toml_dump(cfg))
        credential = Path(cfg['documents']['qq_native']['browser_state_file']); before = credential.read_bytes()
        self.manager.post('/api/documents', {'action': 'unbind', 'group': 'test-group'})
        docs = self.manager.load()['documents']
        self.assertEqual(docs['bindings'], {'other-group': 'folder2'})
        self.assertEqual(docs['qq_native']['groups'], {'other-group': '987654321'})
        self.assertTrue(docs['qq_native']['enabled'])
        self.assertEqual(credential.read_bytes(), before)
        self.manager.post('/api/documents', {'action': 'unbind', 'group': 'other-group'})
        docs = self.manager.load()['documents']
        self.assertEqual(docs['qq_native']['groups'], {})
        self.assertFalse(docs['qq_native']['enabled'])
        self.assertEqual(credential.read_bytes(), before)
        self.assertTrue(self.running)

    def test_removing_conversation_also_unbinds_directory_and_native_but_keeps_credentials_history(self):
        flow = self.start(); self.complete(flow); self.finish(flow)
        credential = Path(self.manager.config['documents']['qq_native']['browser_state_file'])
        before = credential.read_bytes()
        history = self.document()
        history_before = history.read_bytes()
        self.manager.post('/api/conversation', {'scope': 'group', 'id': 'test-group', 'action': 'unbind'})
        cfg = self.manager.load()
        self.assertEqual(cfg['allowed_groups'], [])
        self.assertEqual(cfg['documents']['bindings'], {})
        self.assertEqual(cfg['documents']['qq_native']['groups'], {})
        self.assertFalse(cfg['documents']['qq_native']['enabled'])
        self.assertEqual(credential.read_bytes(), before)
        self.assertEqual(history.read_bytes(), history_before)
        self.assertTrue(self.running)

    def test_excess_native_groups_cannot_start_or_spawn_login(self):
        cfg = self.manager.load()
        cfg['documents']['qq_native'] = {'groups': {'other-' + str(index): str(200000000 + index) for index in range(64)}}
        self.write(self.config_path, admin.toml_dump(cfg))
        with self.assertRaises(admin.AdminError) as raised: self.start()
        self.assertIn('64', str(raised.exception))
        self.spawn.assert_not_called()


if __name__ == '__main__':
    unittest.main()
