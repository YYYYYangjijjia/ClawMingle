"""Offline deployment contract tests; all homes, executables and services are fake."""

from contextlib import ExitStack
import io
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import install_clawmingle as install


@unittest.skipIf(os.getuid() == 0, 'Installer intentionally requires a non-root deployment user')
class InstallationTests(unittest.TestCase):
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        original_umask = os.umask(0o077)
        self.addCleanup(os.umask, original_umask)
        self.base = Path(self.stack.enter_context(tempfile.TemporaryDirectory(prefix='clawmingle-install-test-')))
        self.home = self.base / 'home'
        self.home.mkdir(mode=0o700)
        self.repository = self.base / 'repo'
        self.commands = []
        self.fail_at = None
        self.dependencies = {}
        for name in ('codex', 'bwrap', 'systemctl', 'cargo', 'node'):
            path = self.base / 'tools' / name
            self.write(path, b'\x7fELFfixture', 0o700)
            self.dependencies[name] = str(path)
        for name in install.BINARIES:
            self.write(self.repository / 'target/debug' / name, b'\x7fELFfixture-' + name.encode(), 0o700)
        for name in install.SCRIPTS:
            self.write(self.repository / 'scripts' / name, '# fixture ' + name)
        for name in install.ASSETS:
            self.write(self.repository / 'admin' / name, 'fixture ' + name)
        self.write(self.repository / 'scripts/requirements-qq-smoke.txt', 'websockets==17.3\n')
        self.write(self.repository / 'config/clawmingle.service.example',
                   '[Unit]\nDescription=fixture bot\n[Service]\nExecStart=%h/.local/lib/clawmingle/clawmingle run\n')
        self.write(self.repository / 'config/clawmingle-admin.service.example',
                   '[Unit]\nDescription=fixture admin\n[Service]\n'
                   'ExecStart=%h/.local/lib/clawmingle/admin-venv/bin/python --standalone --init\n')
        self.stack.enter_context(mock.patch.dict(os.environ, {}, clear=True))
        self.stack.enter_context(mock.patch.object(install.shutil, 'which', side_effect=self.which))
        self.stack.enter_context(mock.patch.object(install, 'run_command', side_effect=self.command))
        self.stack.enter_context(mock.patch.object(install.subprocess, 'run', side_effect=AssertionError('Unexpected real process')))
        self.installer = install.Installer(self.repository, self.home)

    @staticmethod
    def write(path, data, mode=0o600):
        path.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
        path.write_bytes(data.encode() if isinstance(data, str) else data)
        path.chmod(mode)

    def which(self, name, path=None):
        return self.dependencies.get(name)

    def command(self, arguments, *, cwd=None, env=None):
        self.commands.append((arguments, cwd, env))
        if self.fail_at and self.fail_at(arguments):
            raise install.InstallError('fixture command failure')
        if arguments[1:3] == ['-m', 'venv']:
            root = Path(arguments[3])
            self.write(root / 'pyvenv.cfg', 'fixture venv')
            self.write(root / 'bin/python', b'\x7fELFfixture', 0o700)
        return mock.Mock(returncode=0)

    def snapshot(self, path):
        return {str(item.relative_to(path)): (item.lstat().st_mode,
                item.read_bytes() if item.is_file() and not item.is_symlink() else None)
                for item in path.rglob('*')}

    def fresh(self, **kwargs):
        return install.Installer(self.repository, self.home, **kwargs)

    def test_default_plan_is_read_only_without_starting_processes(self):
        before = self.snapshot(self.base)
        result = self.installer.plan()
        self.assertTrue(result['ready'], result)
        self.assertEqual(before, self.snapshot(self.base))
        self.assertEqual([], self.commands)
        self.assertFalse(self.installer.root.exists())

    def test_first_install_preserves_personal_auth_and_creates_no_runtime_config(self):
        personal = self.home / '.codex/auth.json'
        self.write(personal, '{"fixture":"personal state"}')
        result = self.installer.apply()
        self.assertTrue(result['changed'])
        self.assertFalse(result['bot_started'])
        self.assertEqual('{"fixture":"personal state"}', personal.read_text())
        self.assertFalse((self.home / '.local/share/clawmingle').exists())
        self.assertFalse((self.home / '.config/clawmingle').exists())
        for name in install.BINARIES:
            self.assertTrue((self.installer.root / name).is_file())
        for name in install.ASSETS:
            self.assertTrue((self.installer.root / 'admin/assets' / name).is_file())
        self.assertTrue(self.installer.manifest_path.is_file())
        self.assertFalse(any('start' in cmd or 'enable' in cmd for cmd, _, _ in self.commands))
        self.assertEqual(0o700, stat.S_IMODE(self.installer.root.stat().st_mode))
        self.assertEqual(0o600, stat.S_IMODE(self.installer.manifest_path.stat().st_mode))

    def test_identical_rerun_does_not_reinstall_rewrite_or_reload(self):
        self.installer.apply()
        before = self.snapshot(self.home)
        self.commands.clear()
        result = self.fresh().apply()
        self.assertFalse(result['changed'])
        self.assertEqual(before, self.snapshot(self.home))
        self.assertEqual([['systemctl', '--user', 'show', '--property=Version', '--value']],
                         [cmd for cmd, _, _ in self.commands])

    def test_explicit_start_and_enable_only_touch_admin(self):
        self.installer.apply(start_admin=True, enable_admin=True)
        actions = [cmd for cmd, _, _ in self.commands if 'start' in cmd or 'enable' in cmd]
        self.assertEqual([['systemctl', '--user', 'enable', 'clawmingle-admin.service'],
                          ['systemctl', '--user', 'start', 'clawmingle-admin.service']], actions)

    def test_changed_source_refuses_upgrade_without_writing(self):
        self.installer.apply()
        before = self.snapshot(self.home)
        self.write(self.repository / 'admin/app.js', 'new version')
        with self.assertRaisesRegex(install.InstallError, 'automatic upgrades'):
            self.fresh().apply()
        self.assertEqual(before, self.snapshot(self.home))

    def test_edited_installed_file_is_preserved(self):
        self.installer.apply()
        edited = self.installer.root / 'admin/app_custom.py'
        self.write(edited, 'user customization')
        known = self.installer.root / 'admin/clawmingle_admin.py'
        self.write(known, 'modified known file')
        with self.assertRaisesRegex(install.InstallError, 'Installed file changed'):
            self.fresh().apply()
        self.assertEqual('modified known file', known.read_text())
        self.assertEqual('user customization', edited.read_text())

    def test_unknown_existing_installation_is_preserved(self):
        sentinel = self.installer.root / 'unmanaged'
        self.write(sentinel, 'existing deployment')
        with self.assertRaisesRegex(install.InstallError, 'no installer manifest'):
            self.installer.apply()
        self.assertEqual('existing deployment', sentinel.read_text())
        self.assertEqual([], self.commands)

    def test_existing_unit_without_manifest_is_preserved(self):
        unit = self.installer.units / 'clawmingle.service'
        self.write(unit, 'existing service')
        with self.assertRaisesRegex(install.InstallError, 'another installation'):
            self.installer.apply()
        self.assertEqual('existing service', unit.read_text())
        self.assertFalse(self.installer.root.exists())

    def test_symlink_destination_or_parent_is_rejected(self):
        outside = self.base / 'outside'
        outside.mkdir(mode=0o700)
        (self.home / '.local').symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(install.InstallError, 'symlink'):
            self.installer.apply()
        self.assertEqual([], list(outside.iterdir()))

    def test_world_writable_parent_is_rejected_without_chmod(self):
        local = self.home / '.local'
        local.mkdir(mode=0o700)
        local.chmod(0o777)
        with self.assertRaisesRegex(install.InstallError, 'writable'):
            self.installer.apply()
        self.assertEqual(0o777, stat.S_IMODE(local.stat().st_mode))

    def test_pip_failure_rolls_back_only_new_paths(self):
        unrelated = self.home / '.config/systemd/user/another.service'
        self.write(unrelated, 'unrelated service')
        auth = self.home / '.config/clawmingle/existing-private-file'
        self.write(auth, 'existing data')
        before = self.snapshot(self.home)
        self.fail_at = lambda cmd: 'pip' in cmd
        with self.assertRaisesRegex(install.InstallError, 'fixture command failure'):
            self.installer.apply()
        self.assertEqual(before, self.snapshot(self.home))
        self.assertFalse(any('daemon-reload' in cmd for cmd, _, _ in self.commands))

    def test_admin_start_failure_does_not_remove_completed_installation(self):
        self.fail_at = lambda cmd: 'start' in cmd
        with self.assertRaisesRegex(install.InstallError, 'fixture command failure'):
            self.installer.apply(start_admin=True)
        self.assertTrue(self.installer.manifest_path.exists())
        self.assertTrue((self.installer.root / 'clawmingle').exists())
        self.fail_at = None
        self.assertFalse(self.fresh().apply(start_admin=True)['changed'])

    def test_missing_user_systemd_session_prevents_filesystem_changes(self):
        self.fail_at = lambda cmd: '--property=Version' in cmd
        before = self.snapshot(self.home)
        with self.assertRaises(install.InstallError):
            self.installer.apply()
        self.assertEqual(before, self.snapshot(self.home))

    def test_missing_native_codex_reports_dependency_without_executing_launcher(self):
        self.write(Path(self.dependencies['codex']), '#!/bin/sh\necho pretend\n', 0o700)
        plan = self.installer.plan()
        self.assertFalse(plan['ready'])
        self.assertTrue(any('Codex native executable' in item for item in plan['blockers']))
        self.assertEqual([], self.commands)

    def test_official_npm_launcher_resolves_vendor_native_binary(self):
        package = self.base / '@openai/codex'
        launcher = package / 'bin/codex.js'
        self.write(launcher, '// fixture npm wrapper', 0o700)
        native = package / 'node_modules/@openai/codex-linux-x64/vendor/x86_64-unknown-linux-musl/codex/codex'
        self.write(native, b'\x7fELFfixture', 0o700)
        with mock.patch.object(install.platform, 'machine', return_value='x86_64'):
            self.assertEqual(native, install.codex_native(str(launcher)))
        self.assertEqual([], self.commands)

    def test_build_is_explicit_locked_all_bins_and_bounded(self):
        with mock.patch.dict(os.environ, {'CARGO_BUILD_JOBS': '8', 'CARGO_PROFILE_DEV_DEBUG': '0'}):
            self.fresh(build=True).apply()
        builds = [(cmd, cwd, env) for cmd, cwd, env in self.commands if cmd[0] == 'cargo']
        self.assertEqual(1, len(builds))
        cmd, cwd, env = builds[0]
        self.assertEqual(['cargo', 'build', '--bins', '--locked'], cmd)
        self.assertEqual(self.repository, cwd)
        self.assertEqual('2', env['CARGO_BUILD_JOBS'])
        self.assertEqual('0', env['CARGO_PROFILE_DEV_DEBUG'])

    def test_plan_build_accepts_missing_binary_but_does_not_build(self):
        for name in install.BINARIES:
            (self.repository / 'target/debug' / name).unlink()
        self.assertTrue(self.fresh(build=True).plan()['ready'])
        self.assertFalse(self.fresh().plan()['ready'])
        self.assertEqual([], self.commands)

    def test_missing_assets_fail_before_build_or_install(self):
        (self.repository / 'admin/setup.js').unlink()
        with self.assertRaisesRegex(install.InstallError, 'setup.js'):
            self.fresh(build=True).apply()
        self.assertFalse(self.installer.root.exists())
        self.assertEqual([], self.commands)

    def test_private_proxy_template_cannot_be_installed_as_standalone(self):
        self.write(self.repository / 'config/clawmingle-admin.service.example',
                   '[Service]\nExecStart=python admin.py --token-file private.token\n')
        with self.assertRaisesRegex(install.InstallError, 'standalone first enrollment'):
            self.installer.apply()

    def test_service_path_is_minimal_escaped_and_stable_on_rerun(self):
        tools = self.base / 'strange%path"name'
        tools.mkdir(mode=0o700)
        command = tools / 'node'
        self.write(command, b'\x7fELFfixture', 0o700)
        self.dependencies['node'] = str(command)
        initial = self.fresh()
        with mock.patch.dict(os.environ, {'PATH': '/unrelated/private/directory'}):
            initial.apply()
        unit = (initial.units / 'clawmingle-admin.service').read_text()
        self.assertNotIn('/unrelated/private/directory', unit)
        self.assertIn('strange%%path\\"name', unit)
        self.dependencies['node'] = str(self.base / 'other-tools/node')
        self.assertFalse(self.fresh().apply()['changed'])

    def test_invalid_manifest_paths_do_not_escape_destination(self):
        self.installer.apply()
        data = json.loads(self.installer.manifest_path.read_text())
        data['files']['../../unrelated'] = {'mode': 0o600, 'sha256': 'unknown'}
        self.write(self.installer.manifest_path, json.dumps(data))
        with self.assertRaisesRegex(install.InstallError, 'automatic upgrades'):
            self.fresh().apply()

    def test_explicit_binary_directory_and_cargo_target_dir(self):
        custom = self.base / 'release'
        for name in install.BINARIES:
            self.write(custom / name, b'\x7fELFfixture', 0o700)
        self.assertTrue(self.fresh(binaries_dir=custom).plan()['ready'])
        with mock.patch.dict(os.environ, {'CARGO_TARGET_DIR': 'custom-target'}):
            self.assertEqual(self.repository / 'custom-target/debug', self.fresh().binaries)

    def test_cli_requires_apply_for_service_actions_and_disallows_ambiguous_build(self):
        for args in (['--start-admin'], ['--enable-admin'], ['--build', '--binaries-dir', '/tmp/fixture']):
            with self.subTest(args=args), mock.patch('sys.stderr', new_callable=io.StringIO):
                with self.assertRaises(SystemExit) as caught:
                    install.main(args)
                self.assertEqual(2, caught.exception.code)
        self.assertEqual([], self.commands)


if __name__ == '__main__':
    unittest.main()
