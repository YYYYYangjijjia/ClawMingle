#!/usr/bin/env python3
"""Plan or install a standalone, user-owned ClawMingle administration service.

The default operation is read-only. This installer never configures a firewall,
uses sudo, starts the bot, or copies personal Codex credentials. Installation
supports a fresh destination or an identical rerun; upgrades are deliberate
operator work, not an implicit overwrite of an existing deployment.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import stat
import subprocess
import sys


BINARIES = ('clawmingle', 'clawmingle-hook-proxy', 'clawmingle-hook-input')
SCRIPTS = ('clawmingle_admin.py', 'admin_auth.py', 'admin_setup.py',
           'codex_capabilities.py', 'qq_smoke.py', 'qq_panels.py')
ASSETS = ('index.html', 'app.js', 'app.css', 'login.html', 'login.js',
          'login.css', 'setup.html', 'setup.js', 'setup.css')
UNITS = ('clawmingle.service', 'clawmingle-admin.service')
MANIFEST = 'installation.json'


class InstallError(Exception):
    pass


def digest(path):
    value = hashlib.sha256()
    with path.open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            value.update(chunk)
    return value.hexdigest()


def check_path(path, *, private=False, regular=False, source=False):
    """Reject links, foreign owners and writable ancestors without changing them."""
    if not path.is_absolute() or '..' in path.parts:
        raise InstallError('Expected an absolute path without parent traversal: ' + str(path))
    for item in (*reversed(path.parents), path):
        try:
            info = item.lstat()
        except FileNotFoundError:
            continue
        sticky_root = (stat.S_ISDIR(info.st_mode) and info.st_uid == 0
                       and info.st_mode & stat.S_ISVTX)
        if stat.S_ISLNK(info.st_mode) or info.st_uid not in (0, os.getuid()):
            raise InstallError('Refusing a symlink or foreign-owned path: ' + str(item))
        # A checkout already authorized for execution commonly uses umask 0002.
        # Accept group writes only in that user's source tree, never at the
        # installation destination or in root-owned/shared source ancestors.
        forbidden_writes = 0o002 if source and info.st_uid == os.getuid() else 0o022
        if info.st_mode & forbidden_writes and not sticky_root:
            raise InstallError('Refusing a group/other-writable path: ' + str(item))
        if item != path and not stat.S_ISDIR(info.st_mode):
            raise InstallError('Parent is not a directory: ' + str(item))
        if item == path:
            if info.st_uid != os.getuid():
                raise InstallError('Destination/source must belong to the current user: ' + str(item))
            if private and info.st_mode & 0o077:
                raise InstallError('Existing application directory must have mode 0700: ' + str(item))
            if regular and (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1):
                raise InstallError('Expected a regular, unlinked file: ' + str(item))


def native_binary(path, *, source=False):
    """Discover ELF files without executing a potentially JS-based Codex launcher."""
    try:
        resolved = path.resolve(strict=True)
        info = resolved.stat()
        forbidden_writes = 0o002 if source and info.st_uid == os.getuid() else 0o022
        if (not stat.S_ISREG(info.st_mode) or info.st_uid not in (0, os.getuid())
                or info.st_mode & forbidden_writes or not os.access(resolved, os.X_OK)):
            return None
        with resolved.open('rb') as stream:
            return resolved if stream.read(4) == b'\x7fELF' else None
    except OSError:
        return None


def codex_native(path):
    if not path:
        return None
    launcher = Path(path).resolve()
    direct = native_binary(launcher)
    if direct:
        return direct
    if (launcher.name != 'codex.js' or launcher.parent.name != 'bin'
            or launcher.parent.parent.name != 'codex'
            or launcher.parent.parent.parent.name != '@openai'):
        return None
    architecture = {'x86_64': ('x86_64', 'x64'), 'aarch64': ('aarch64', 'arm64')}
    pair = architecture.get(platform.machine())
    if not pair:
        return None
    target, package_arch = pair
    package = launcher.parent.parent
    vendor = Path('vendor') / (target + '-unknown-linux-musl') / 'codex/codex'
    candidates = (package / vendor,
                  package / 'node_modules/@openai' / ('codex-linux-' + package_arch) / vendor,
                  package.parent / ('codex-linux-' + package_arch) / vendor)
    return next((found for candidate in candidates if (found := native_binary(candidate))), None)


def run_command(arguments, *, cwd=None, env=None):
    try:
        result = subprocess.run(arguments, cwd=cwd, env=env, check=False,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                timeout=1800 if arguments[0] in ('cargo', sys.executable) else 300)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise InstallError('Could not complete command: ' + Path(arguments[0]).name) from error
    if result.returncode:
        # Tool output can contain credential-bearing package-index or proxy URLs.
        raise InstallError('Command failed: ' + Path(arguments[0]).name
                           + ' (exit ' + str(result.returncode) + '); inspect it separately.')
    return result


class Installer:
    def __init__(self, repository, home, *, binaries_dir=None, build=False):
        self.repository = Path(repository).absolute()
        self.home = Path(home).absolute()
        self.root = self.home / '.local/lib/clawmingle'
        self.venv = self.root / 'admin-venv'
        self.units = self.home / '.config/systemd/user'
        self.manifest_path = self.root / MANIFEST
        self.build = build
        target = Path(os.environ.get('CARGO_TARGET_DIR', str(self.repository / 'target')))
        if not target.is_absolute():
            target = self.repository / target
        self.binaries = Path(binaries_dir).absolute() if binaries_dir else target / 'debug'
        self.commands = {name: shutil.which(name) for name in ('cargo', 'codex', 'bwrap', 'systemctl', 'node')}
        self.manifest = None

    def load_manifest(self):
        check_path(self.home)
        check_path(self.root, private=True)
        check_path(self.units)
        if self.root.exists() and not self.root.is_dir():
            raise InstallError('Installation destination is not a directory.')
        if not self.root.exists():
            for name in UNITS:
                if (self.units / name).exists() or (self.units / name).is_symlink():
                    raise InstallError('An existing service belongs to another installation: ' + name)
            return
        if not self.manifest_path.exists():
            raise InstallError('Existing installation has no installer manifest; preserve it and use a reviewed migration.')
        check_path(self.manifest_path, regular=True)
        if self.manifest_path.stat().st_size > 262144:
            raise InstallError('Invalid installation manifest size.')
        try:
            self.manifest = json.loads(self.manifest_path.read_text())
        except (OSError, ValueError) as error:
            raise InstallError('Cannot read the installation manifest.') from error
        if (not isinstance(self.manifest, dict) or self.manifest.get('version') != 1
                or self.manifest.get('home') != str(self.home)
                or not isinstance(self.manifest.get('service_path'), list)
                or not isinstance(self.manifest.get('files'), dict)):
            raise InstallError('Invalid or incompatible installation manifest.')

    def service_path(self):
        if self.manifest:
            paths = self.manifest['service_path']
        else:
            paths = [str(self.venv / 'bin')]
            paths.extend(str(Path(self.commands[name]).parent) for name in ('codex', 'bwrap', 'node')
                         if self.commands[name])
            paths.extend(('/usr/local/bin', '/usr/bin', '/bin'))
        unique = []
        for value in paths:
            if (not isinstance(value, str) or not value.startswith('/') or ':' in value
                    or '..' in Path(value).parts or any(ord(char) < 32 or ord(char) == 127 for char in value)):
                raise InstallError('Invalid service executable search path.')
            if value not in unique:
                unique.append(value)
        if not unique:
            raise InstallError('Missing service executable search path.')
        return unique

    def unit_bytes(self, name):
        source = self.repository / 'config' / (name + '.example')
        data = source.read_text()
        if name == 'clawmingle-admin.service' and ('--standalone' not in data or '--init' not in data):
            raise InstallError('Admin service template must support standalone first enrollment.')
        value = ':'.join(self.service_path()).replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%')
        lines = [line for line in data.splitlines()
                 if not line.startswith(('Environment=PATH=', 'Environment="PATH='))]
        try:
            lines.insert(lines.index('[Service]') + 1, 'Environment="PATH=' + value + '"')
        except ValueError as error:
            raise InstallError('Service template is missing [Service]: ' + name) from error
        return ('\n'.join(lines) + '\n').encode()

    def artifacts(self, *, allow_missing_binaries=False):
        items = []
        for name in BINARIES:
            items.append((self.binaries / name, self.root / name, 0o700, None))
        for name in SCRIPTS:
            items.append((self.repository / 'scripts' / name, self.root / 'admin' / name, 0o600, None))
        for name in ASSETS:
            items.append((self.repository / 'admin' / name, self.root / 'admin/assets' / name, 0o600, None))
        requirement = self.repository / 'scripts/requirements-qq-smoke.txt'
        items.append((requirement, self.root / 'admin/requirements-qq-smoke.txt', 0o600, None))
        for name in UNITS:
            source = self.repository / 'config' / (name + '.example')
            items.append((source, self.units / name, 0o600, self.unit_bytes(name)))
        for source, destination, mode, _ in items:
            check_path(destination, regular=True)
            if allow_missing_binaries and mode == 0o700 and not source.exists():
                continue
            # Cargo may hardlink debug binaries to its deps directory.
            check_path(source, source=True)
            if not source.is_file() or (mode == 0o700 and not native_binary(source, source=True)):
                raise InstallError('Required artifact is missing or invalid: ' + str(source))
        return items

    def fingerprints(self, artifacts):
        return {str(destination.relative_to(self.home)): {
            'sha256': hashlib.sha256(contents).hexdigest() if contents is not None else digest(source),
            'mode': mode,
        } for source, destination, mode, contents in artifacts}

    def verify_existing(self, files):
        if not self.manifest:
            return
        if files != self.manifest['files']:
            raise InstallError('Source differs from installed version; automatic upgrades are not supported.')
        for relative, expected in files.items():
            path = self.home / relative
            check_path(path, regular=True)
            if (not path.exists() or digest(path) != expected['sha256']
                    or stat.S_IMODE(path.stat().st_mode) != expected['mode']):
                raise InstallError('Installed file changed; preserve and inspect it: ' + str(path))
        check_path(self.venv, private=True)
        check_path(self.venv / 'pyvenv.cfg', regular=True)
        if not (self.venv / 'pyvenv.cfg').is_file() or not (self.venv / 'bin/python').is_file():
            raise InstallError('Installed administration environment is incomplete.')

    def plan(self):
        blockers = []
        try:
            self.load_manifest()
            if self.manifest:
                search = ':'.join(self.service_path())
                for name in ('codex', 'bwrap', 'node'):
                    self.commands[name] = shutil.which(name, path=search)
        except (InstallError, OSError) as error:
            blockers.append(str(error))
        dependencies = [
            ('Linux', sys.platform == 'linux', 'Use a Linux host with user systemd.'),
            ('non-root user', os.getuid() != 0, 'Run as the deployment user, without sudo.'),
            ('Python >=3.11', sys.version_info >= (3, 11), 'Activate a Python 3.11+ environment.'),
            ('venv', importlib.util.find_spec('venv') is not None, 'Provide Python with venv support.'),
            ('pip bootstrap', importlib.util.find_spec('ensurepip') is not None, 'Provide Python with ensurepip support.'),
            ('Codex native executable', bool(codex_native(self.commands['codex'])),
             'Install official Codex for Linux and expose its native executable or npm launcher on PATH.'),
            ('Bubblewrap', bool(self.commands['bwrap'] and native_binary(Path(self.commands['bwrap']))),
             'Have the host operator install Bubblewrap and permit user namespaces.'),
            ('systemctl', bool(self.commands['systemctl']), 'Use a host with a working systemd user session.'),
        ]
        if self.build:
            dependencies.append(('cargo', bool(self.commands['cargo']), 'Activate the repository Rust build environment.'))
        for name, available, action in dependencies:
            if not available:
                blockers.append(name + ': ' + action)
        try:
            items = self.artifacts(allow_missing_binaries=self.build)
            if not self.build:
                self.verify_existing(self.fingerprints(items))
        except (InstallError, OSError) as error:
            blockers.append(str(error))
        return {
            'mode': 'plan', 'ready': not blockers, 'existing_installation': bool(self.manifest),
            'dependencies': [{'name': name, 'available': available} for name, available, _ in dependencies],
            'destinations': {'application': str(self.root), 'admin_environment': str(self.venv),
                             'user_units': [str(self.units / name) for name in UNITS]},
            'build': self.build, 'blockers': blockers,
            'notes': ['Plan does not write, contact services, or install dependencies.',
                      'Apply creates a private venv and installs the pinned WebSocket dependency via pip.',
                      'Bot remains stopped. Complete browser enrollment, setup and dedicated Codex login first.',
                      'No personal-dashboard, public listener, system packages, sudo, or personal auth imports.'],
        }

    def apply(self, *, start_admin=False, enable_admin=False):
        plan = self.plan()
        if not plan['ready']:
            raise InstallError('Installation preflight failed: ' + '; '.join(plan['blockers']))
        run_command(['systemctl', '--user', 'show', '--property=Version', '--value'])
        if self.build:
            environment = dict(os.environ)
            try:
                jobs = int(environment.get('CARGO_BUILD_JOBS', '2'))
            except ValueError as error:
                raise InstallError('CARGO_BUILD_JOBS must be a positive integer.') from error
            if jobs < 1:
                raise InstallError('CARGO_BUILD_JOBS must be a positive integer.')
            environment['CARGO_BUILD_JOBS'] = str(min(jobs, 2))
            run_command(['cargo', 'build', '--bins', '--locked'], cwd=self.repository, env=environment)
        items = self.artifacts()
        files = self.fingerprints(items)
        self.verify_existing(files)
        changed = not self.manifest
        if not self.manifest:
            self.install_new(items, files)
        # Do not roll back a completed installation after a service action fails.
        # It may already be serving or have new user enrollment/configuration.
        if changed:
            run_command(['systemctl', '--user', 'daemon-reload'])
        if enable_admin:
            run_command(['systemctl', '--user', 'enable', 'clawmingle-admin.service'])
        if start_admin:
            run_command(['systemctl', '--user', 'start', 'clawmingle-admin.service'])
        return {
            'mode': 'applied', 'application': str(self.root),
            'changed': changed,
            'admin_start_requested': start_admin, 'admin_enable_requested': enable_admin,
            'bot_started': False, 'admin_url': 'http://127.0.0.1:17430/',
            'next': 'Open the admin page locally or through an SSH tunnel; use the protected bootstrap code to enroll.',
            'bootstrap_file': str(self.home / '.config/clawmingle/admin-bootstrap.json'),
        }

    def install_new(self, items, files):
        created_dirs = []
        created_files = []
        venv_created = False

        def mkdir(path):
            if path.exists():
                check_path(path)
                return
            mkdir(path.parent)
            path.mkdir(mode=0o700)
            created_dirs.append(path)

        def write(path, content, mode):
            mkdir(path.parent)
            check_path(path, regular=True)
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, mode)
            created_files.append(path)
            with os.fdopen(fd, 'wb') as stream:
                stream.write(content)
                stream.flush()
                os.fsync(stream.fileno())

        try:
            for source, destination, mode, content in items:
                write(destination, content if content is not None else source.read_bytes(), mode)
            mkdir(self.venv)
            venv_created = True
            run_command([sys.executable, '-m', 'venv', str(self.venv)])
            run_command([str(self.venv / 'bin/python'), '-m', 'pip', '--disable-pip-version-check',
                         'install', '--no-input', '--no-cache-dir', '-r',
                         str(self.root / 'admin/requirements-qq-smoke.txt')])
            run_command([str(self.venv / 'bin/python'), '-B', '-c', 'import tomllib, websockets'])
            manifest = {'version': 1, 'home': str(self.home), 'service_path': self.service_path(),
                        'files': files,
                        'created_directories': [str(path.relative_to(self.home)) for path in created_dirs]}
            write(self.manifest_path, (json.dumps(manifest, indent=2) + '\n').encode(), 0o600)
            self.manifest = manifest
        except BaseException as error:
            # Only the newly-created, installer-owned virtualenv is recursive.
            # Application data and authentication directories are never touched.
            if venv_created:
                check_path(self.venv, private=True)
                shutil.rmtree(self.venv)
            for path in reversed(created_files):
                check_path(path, regular=True)
                path.unlink()
            for path in reversed(created_dirs):
                if path == self.venv and venv_created:
                    continue
                try:
                    path.rmdir()
                except OSError:
                    pass  # A directory with new/unrelated contents must survive.
            if isinstance(error, (InstallError, KeyboardInterrupt, SystemExit)):
                raise
            raise InstallError('Installation failed; new installation files were rolled back.') from error


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument('--plan', action='store_true', help='Read-only plan (default).')
    mode.add_argument('--apply', action='store_true', help='Install into the current user account.')
    parser.add_argument('--build', action='store_true', help='Build all debug binaries using the activated Cargo environment.')
    parser.add_argument('--binaries-dir', type=Path, help='Use existing binaries, e.g. target/release; incompatible with --build.')
    parser.add_argument('--start-admin', action='store_true', help='Start only the admin service after apply.')
    parser.add_argument('--enable-admin', action='store_true', help='Enable only the admin service after apply.')
    args = parser.parse_args(argv)
    if (args.start_admin or args.enable_admin) and not args.apply:
        parser.error('--start-admin and --enable-admin require --apply')
    if args.build and args.binaries_dir:
        parser.error('--build and --binaries-dir cannot be combined')
    installer = Installer(Path(__file__).resolve().parents[1], Path.home(),
                          binaries_dir=args.binaries_dir, build=args.build)
    try:
        result = (installer.apply(start_admin=args.start_admin, enable_admin=args.enable_admin)
                  if args.apply else installer.plan())
        print(json.dumps(result, indent=2))
        return 0 if result.get('ready', True) else 1
    except InstallError as error:
        print(json.dumps({'mode': 'error', 'error': str(error)}))
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
