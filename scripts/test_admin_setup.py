"""Offline first-run setup tests; never contact QQ or start a bot/service."""

import asyncio
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import threading
import tomllib
import unittest
from contextlib import redirect_stdout
from unittest.mock import AsyncMock, patch

import admin_setup as setup
import qq_smoke


class SetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.config_dir = self.home / '.config/clawmingle'
        self.config_dir.mkdir(parents=True, mode=0o700)
        self.config = self.config_dir / 'social.toml'
        self.binary = self.home / 'clawmingle'
        self.binary.write_text('#!/bin/sh\nexit 0\n')
        self.binary.chmod(0o700)
        self.app = setup.Setup(self.config, self.binary, self.home / 'assets', self.home)
        self.addCleanup(self.app.close)

    def settings(self, **changes):
        self.app.post({'action': 'save-settings', 'app_id': '123', 'app_secret': 'EXAMPLE_QQ_SECRET',
                       'jev_key': 'EXAMPLE_JEV_KEY', 'model': 'example-model',
                       'reasoning_effort': 'medium', 'codex_binary': '/example/bin/codex', **changes})

    def ready(self, **changes):
        self.settings(**changes)
        self.app.post({'action': 'group-manual', 'group_openid': 'test_group', 'admin_openid': 'test_member'})

    def finish(self, returncode=0):
        with patch.object(setup.subprocess, 'run', return_value=subprocess.CompletedProcess([], returncode)) as run:
            result = self.app.post({'action': 'finish'})
        return result, run

    def test_draft_is_in_memory_and_never_echoes_credentials(self):
        self.settings()
        state = json.dumps(self.app.state())
        self.assertNotIn('EXAMPLE_QQ_SECRET', state)
        self.assertNotIn('EXAMPLE_JEV_KEY', state)
        self.assertEqual(self.app.state()['settings']['app_id'], '123')
        self.assertEqual(list(self.config_dir.iterdir()), [])
        self.assertFalse(self.app.data.exists())

    def test_empty_keys_preserve_existing_and_model_edit_preserves_group(self):
        self.ready()
        self.settings(app_secret='', jev_key='', model='other-model')
        self.assertEqual(self.app.settings['app_secret'], 'EXAMPLE_QQ_SECRET')
        self.assertEqual(self.app.settings['jev_key'], 'EXAMPLE_JEV_KEY')
        self.assertEqual(self.app.registration['state'], 'matched')
        with self.assertRaises(setup.SetupError):
            self.settings(app_id='456', app_secret='')

    def test_settings_reject_injection_controls_and_invalid_model(self):
        for values in ({'app_id': '123\n'}, {'app_secret': chr(10)},
                       {'model': 'x"\n[other]'}, {'reasoning_effort': {}},
                       {'codex_binary': 'relative/codex'}, {'jev_key': []}):
            with self.subTest(values=values), self.assertRaises(setup.SetupError):
                self.settings(**values)
        self.assertEqual(list(self.config_dir.iterdir()), [])

    def test_manual_ids_not_nicknames_and_both_required(self):
        self.settings()
        for group, member in [('nickname name', 'test'), ('test', ''), ('YOUR_GROUP_OPENID', 'test'), ('test', '../x')]:
            with self.assertRaises(setup.SetupError):
                self.app.post({'action': 'group-manual', 'group_openid': group, 'admin_openid': member})
        with self.assertRaises(setup.SetupError):
            self.finish()

    def test_finish_checks_config_then_publishes_private_isolated_files(self):
        self.ready()
        result, run = self.finish()
        self.assertFalse(result['setupRequired'])
        args = run.call_args.args[0]
        self.assertEqual(args[:2], [str(self.binary), 'check'])
        self.assertNotIn('EXAMPLE_QQ_SECRET', str(args))
        cfg = tomllib.loads(self.config.read_text())
        self.assertEqual(cfg['allowed_groups'], ['test_group'])
        self.assertEqual(cfg['command_admins'], ['test_member'])
        self.assertEqual(cfg['mode'], 'mode-1')
        self.assertFalse(cfg['private_enabled'])
        self.assertTrue(Path(cfg['data_dir']).is_absolute())
        self.assertEqual(qq_smoke.load_credentials(self.config_dir / 'qq.env')['QQ_APP_ID'], '123')
        runtime = tomllib.loads((self.app.data / 'runtime/codex/config.toml').read_text())
        self.assertEqual(runtime['model'], 'example-model')
        self.assertEqual(runtime['cli_auth_credentials_store'], 'file')
        self.assertTrue((self.app.data / 'operator.lock').is_file())
        self.assertFalse((self.app.data / 'runtime/codex/auth.json').exists())
        for path in [self.config, self.config_dir / 'qq.env', self.config_dir / 'jev.env',
                     *self.app.data.rglob('*')]:
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700 if path.is_dir() else 0o600)
        self.assertIsNone(self.app.settings)
        with self.assertRaises(setup.SetupError):
            self.app.post({'action': 'finish'})

    def test_jev_is_optional_and_personal_auth_not_read(self):
        personal = self.home / '.codex'
        personal.mkdir()
        (personal / 'auth.json').write_text('private-marker-never-copy')
        self.ready(jev_key='')
        self.finish()
        self.assertFalse((self.config_dir / 'jev.env').exists())
        self.assertFalse((self.app.data / 'runtime/codex/auth.json').exists())

    def test_validation_failure_never_writes_credentials_or_data(self):
        self.ready()
        with self.assertRaises(setup.SetupError):
            self.finish(returncode=1)
        self.assertEqual(list(self.config_dir.iterdir()), [])
        self.assertFalse(self.app.data.exists())
        self.assertTrue(self.app.pending)

    def test_timeout_error_does_not_expose_process_or_credentials(self):
        self.ready()
        with patch.object(setup.subprocess, 'run', side_effect=subprocess.TimeoutExpired('private-marker', 20)):
            with self.assertRaises(setup.SetupError) as caught:
                self.app.post({'action': 'finish'})
        self.assertNotIn('private-marker', str(caught.exception))
        self.assertEqual(list(self.config_dir.iterdir()), [])

    def test_existing_config_credentials_and_data_never_overwritten(self):
        self.ready()
        for path in (self.config, self.config_dir / 'qq.env', self.config_dir / 'jev.env'):
            path.write_text('preserve')
            with self.assertRaises(setup.SetupError):
                self.finish()
            self.assertEqual(path.read_text(), 'preserve')
            path.unlink()
        self.app.data.mkdir(parents=True)
        (self.app.data / 'user.txt').write_text('preserve')
        with self.assertRaises(setup.SetupError):
            self.finish()
        self.assertEqual((self.app.data / 'user.txt').read_text(), 'preserve')

    def test_racing_writer_after_validation_preserved(self):
        self.ready()
        def check(*args, **kwargs):
            self.config.write_text('concurrent-config')
            return subprocess.CompletedProcess([], 0)
        with patch.object(setup.subprocess, 'run', side_effect=check):
            with self.assertRaises(setup.SetupError):
                self.app.post({'action': 'finish'})
        self.assertEqual(self.config.read_text(), 'concurrent-config')
        self.assertFalse(self.app.data.exists())

    def test_publish_failure_rolls_back_only_created_files(self):
        self.ready()
        with patch.object(setup.os, 'link', side_effect=OSError('private-path')):
            with self.assertRaises(setup.SetupError) as caught:
                self.finish()
        self.assertNotIn('private-path', str(caught.exception))
        self.assertFalse(self.app.data.exists())
        self.assertFalse(self.config.exists())
        self.assertFalse((self.config_dir / 'qq.env').exists())
        self.assertFalse((self.config_dir / 'jev.env').exists())

    def test_failed_file_write_removes_its_partial_file(self):
        path = self.config_dir / 'partial'
        with patch.object(setup.os, 'fsync', side_effect=OSError('disk failure')):
            with self.assertRaises(OSError):
                setup._new_file(path, 'private-marker')
        self.assertFalse(path.exists())

    def test_rollback_preserves_unexpected_new_runtime_file(self):
        self.ready()
        def link(*args):
            (self.app.data / 'new-user-file').write_text('keep')
            raise OSError('interrupted')
        with patch.object(setup.os, 'link', side_effect=link):
            with self.assertRaises(setup.SetupError):
                self.finish()
        self.assertEqual((self.app.data / 'new-user-file').read_text(), 'keep')
        self.assertFalse((self.app.data / 'operator.lock').exists())

    def test_paths_symlinks_repository_and_parent_permissions_rejected(self):
        target = self.home / 'linked'
        target.symlink_to(self.config_dir, target_is_directory=True)
        with self.assertRaises(setup.SetupError):
            setup.Setup(target / 'social.toml', self.binary, self.home, self.home)
        (self.home / '.git').mkdir()
        with self.assertRaises(setup.SetupError):
            setup.Setup(self.config, self.binary, self.home, self.home)
        (self.home / '.git').rmdir()
        self.ready()
        self.config_dir.chmod(0o755)
        with self.assertRaises(setup.SetupError):
            self.finish()

    def test_missing_registration_dependency_actionable_no_network(self):
        self.settings()
        with patch.object(setup.importlib.util, 'find_spec', side_effect=ModuleNotFoundError):
            with self.assertRaises(setup.SetupError) as caught:
                self.app.post({'action': 'registration-start'})
        self.assertIn('requirements-qq-smoke.txt', str(caught.exception))
        self.assertIsNone(self.app.probe_thread)

    def test_registration_matches_group_and_sender_without_writing_or_printing(self):
        self.settings()
        commands = []
        async def probe(credentials, seconds, **kwargs):
            self.assertEqual(seconds, 300)
            self.assertEqual(credentials['QQ_APP_ID'], '123')
            commands.append(kwargs['registration_nonce'])
            kwargs['registration_callback']('test_group', 'test_member')
        with patch.object(setup.importlib.util, 'find_spec', return_value=object()), patch.object(qq_smoke, 'run_probe', probe), redirect_stdout(io.StringIO()) as output:
            self.app.post({'action': 'registration-start'})
            self.app.probe_thread.join(timeout=2)
        self.assertFalse(self.app.probe_thread.is_alive())
        self.assertEqual(self.app.state()['registration']['source'], 'qq')
        self.assertEqual(self.app.state()['registration']['admin_openid'], 'test_member')
        self.assertRegex(commands[0], r'^[0-9a-f]{32}$')
        self.assertEqual(output.getvalue(), '')
        self.assertEqual(list(self.config_dir.iterdir()), [])

    def test_cancel_rejects_late_completion_and_stops_probe(self):
        self.settings()
        started = threading.Event()
        async def probe(credentials, seconds, **kwargs):
            started.set()
            try:
                await asyncio.Event().wait()
            except asyncio.CancelledError:
                kwargs['registration_callback']('late_group', 'late_member')
                raise
        with patch.object(setup.importlib.util, 'find_spec', return_value=object()), patch.object(qq_smoke, 'run_probe', probe):
            self.app.post({'action': 'registration-start'})
            self.assertTrue(started.wait(2))
            with self.assertRaises(setup.SetupError):
                self.app.post({'action': 'registration-start'})
            self.app.post({'action': 'registration-cancel'})
            self.app.probe_thread.join(timeout=2)
        self.assertEqual(self.app.registration['state'], 'idle')
        self.assertNotIn('group_openid', self.app.registration)


class RegistrationEvidenceTests(unittest.IsolatedAsyncioTestCase):
    async def listen(self, payload):
        ready = {'op': 0, 't': 'READY', 'd': {'user': {'id': 'bot-id'}}}
        websocket = AsyncMock()
        websocket.recv.side_effect = [json.dumps(ready), json.dumps(payload)]
        matches = []
        with redirect_stdout(io.StringIO()) as output:
            await qq_smoke.listen(websocket, {'QQ_APP_ID': '123'}, 'test-token', '/clawmingle-register nonce',
                                  registration_callback=lambda group, member: matches.append((group, member)),
                                  status_callback=lambda message: None)
        self.assertEqual(output.getvalue(), '')
        websocket.send.assert_not_called()
        return matches

    async def test_only_matching_real_mention_sender_gets_admin(self):
        payload = {'op': 0, 't': 'GROUP_AT_MESSAGE_CREATE', 'd': {
            'group_openid': 'test_group', 'id': 'message', 'content': '/clawmingle-register nonce',
            'author': {'member_openid': 'test_member', 'username': 'not-an-id'}}}
        self.assertEqual(await self.listen(payload), [('test_group', 'test_member')])
        payload['d']['author'] = {'username': 'test_member', 'id': 'unscoped-id'}
        with self.assertRaises(qq_smoke.ProbeError):
            await self.listen(payload)

    async def test_wrong_nonce_or_no_mention_never_registers(self):
        for kind, content in [('GROUP_MESSAGE_CREATE', '/clawmingle-register nonce'),
                              ('GROUP_AT_MESSAGE_CREATE', '/clawmingle-register old')]:
            payload = {'op': 0, 't': kind, 'd': {'group_openid': 'test_group', 'id': 'message',
                       'content': content, 'author': {'member_openid': 'test_member'}}}
            with self.assertRaises(StopAsyncIteration):
                await self.listen(payload)


if __name__ == '__main__':
    unittest.main()
