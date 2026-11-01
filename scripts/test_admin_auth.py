"""Offline local authentication and HTTP setup tests; no QQ/model/service calls."""
from concurrent.futures import ThreadPoolExecutor
from email.message import Message
import http.client
from http.cookies import SimpleCookie
from http.server import ThreadingHTTPServer
import json
from pathlib import Path
import stat
import subprocess
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

import admin_auth as auth
from admin_setup import Setup
import clawmingle_admin as admin


class LocalAuthTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / 'private'
        self.now = 1000
        self.app = auth.LocalAuth(self.directory, 17430, initialize=True, clock=lambda: self.now)
        self.password = 'EXAMPLE_LOCAL_PASSWORD'

    def enroll(self):
        return self.app.authenticate({'password': self.password, 'pairingCode': self.app.bootstrap['token']}, enroll=True)

    def session(self, token):
        return self.app.session({'Cookie': f'{self.app.cookie_name}={token}'})

    def test_enrollment_persists_only_salted_hash_and_consumes_pairing(self):
        token = self.enroll()
        value = json.loads(self.app.auth_path.read_text())
        self.assertNotIn(self.password, self.app.auth_path.read_text())
        self.assertEqual(value['iterations'], auth.ITERATIONS)
        self.assertEqual(len(bytes.fromhex(value['salt'])), 16)
        self.assertEqual(stat.S_IMODE(self.directory.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(self.app.auth_path.stat().st_mode), 0o600)
        self.assertFalse(self.app.bootstrap_path.exists())
        self.assertFalse(self.app.status()['enrollmentRequired'])
        self.assertIsNotNone(self.session(token))
        with self.assertRaises(auth.AuthError) as caught:
            self.app.authenticate({'password': self.password, 'pairingCode': 'x' * 40}, enroll=True)
        self.assertEqual(caught.exception.status, 409)

    def test_restart_retains_password_but_not_sessions_or_enrollment(self):
        old = self.enroll()
        before = self.app.auth_path.read_bytes()
        self.app = auth.LocalAuth(self.directory, 17430, initialize=True)
        self.assertFalse(self.app.bootstrap_path.exists())
        self.assertIsNone(self.session(old))
        self.assertIsNotNone(self.session(self.app.authenticate({'password': self.password})))
        self.assertEqual(before, self.app.auth_path.read_bytes())

    def test_pending_pairing_reused_until_expired_then_rotated(self):
        original = self.app.bootstrap['token']
        peer = auth.LocalAuth(self.directory, 17430, initialize=True, clock=lambda: self.now)
        self.assertEqual(peer.bootstrap['token'], original)
        self.now += auth.ENROLLMENT_SECONDS
        self.assertTrue(self.app.status()['enrollmentExpired'])
        with self.assertRaises(auth.AuthError):
            self.enroll()
        refreshed = auth.LocalAuth(self.directory, 17430, initialize=True, clock=lambda: self.now)
        self.assertNotEqual(refreshed.bootstrap['token'], original)

    def test_wrong_pairing_never_creates_password_and_throttles(self):
        for _ in range(10):
            with self.assertRaises(auth.AuthError) as caught:
                self.app.authenticate({'password': self.password, 'pairingCode': 'wrong'}, enroll=True)
            self.assertEqual(caught.exception.status, 401)
        with self.assertRaises(auth.AuthError) as caught:
            self.enroll()
        self.assertEqual(caught.exception.status, 429)
        self.assertFalse(self.app.auth_path.exists())
        self.now += 300
        self.enroll()

    def test_password_errors_never_create_session(self):
        self.enroll()
        count = len(self.app.sessions)
        for body in ({'password': 'EXAMPLE_WRONG_PASSWORD'}, {'password': 'EXAMPLE_X'},
                     {'password': self.password, 'extra': True}, {'password': []}, []):
            with self.assertRaises(auth.AuthError):
                self.app.authenticate(body)
        self.assertEqual(len(self.app.sessions), count)

    def test_expiry_logout_and_cookie_flags(self):
        token = self.enroll()
        cookie = SimpleCookie(self.app.cookie(token))[self.app.cookie_name]
        self.assertTrue(cookie['httponly'])
        self.assertEqual(cookie['samesite'], 'Strict')
        self.assertFalse(cookie['domain'])
        self.app.logout(self.session(token))
        self.assertIsNone(self.session(token))
        token = self.app.authenticate({'password': self.password})
        self.now += auth.SESSION_SECONDS
        self.assertIsNone(self.session(token))

    def test_host_origin_and_csrf_enforced(self):
        token = self.enroll()
        session = self.session(token)
        for host in ('evil.example:17430', '127.0.0.1', 'localhost:8080', ''):
            with self.assertRaises(auth.AuthError):
                self.app.check_request({'Host': host})
        headers = {'Host': 'localhost:17430', 'Origin': 'http://localhost:17430'}
        self.app.check_request(headers, write=True)
        for changes in ({'Origin': 'https://evil.example'}, {'Origin': ''}, {'Sec-Fetch-Site': 'cross-site'}):
            with self.assertRaises(auth.AuthError):
                self.app.check_request({**headers, **changes}, write=True)
        for csrf in ('', 'wrong'):
            with self.assertRaises(auth.AuthError):
                self.app.check_csrf({'X-CSRF-Token': csrf}, session)
        self.app.check_csrf({'X-CSRF-Token': session['csrfToken']}, session)

    def test_private_files_and_links_refused_without_rewriting(self):
        path = self.app.bootstrap_path
        before = path.read_bytes()
        path.chmod(0o644)
        with self.assertRaises(auth.AuthError):
            auth.LocalAuth(self.directory, 17430, initialize=True)
        self.assertEqual(before, path.read_bytes())
        path.chmod(0o600)
        target = self.directory / 'existing'
        path.rename(target)
        path.symlink_to(target)
        with self.assertRaises(OSError):
            auth.LocalAuth(self.directory, 17430, initialize=True)
        self.assertEqual(before, target.read_bytes())

    def test_invalid_auth_and_nonfinite_expiry_not_replaced(self):
        self.app.bootstrap_path.write_text(json.dumps({'token': 'x' * 40, 'expires_at': float('nan')}))
        with self.assertRaises(auth.AuthError):
            auth.LocalAuth(self.directory, 17430, initialize=True)
        auth.create_private(self.app.auth_path, {'version': 1})
        before = self.app.auth_path.read_bytes()
        with self.assertRaises(auth.AuthError):
            auth.LocalAuth(self.directory, 17430, initialize=True)
        self.assertEqual(before, self.app.auth_path.read_bytes())

    def test_failed_write_allows_enrollment_retry(self):
        with patch.object(auth.os, 'fsync', side_effect=OSError('fixture disk error')):
            with self.assertRaises(OSError):
                self.enroll()
        self.assertFalse(self.app.auth_path.exists())
        self.assertTrue(self.app.bootstrap_path.exists())
        self.assertIsNotNone(self.session(self.enroll()))

    def test_concurrent_enrollment_only_one_password_wins(self):
        code = self.app.bootstrap['token']
        def attempt(password):
            try:
                self.app.authenticate({'password': password, 'pairingCode': code}, enroll=True)
                return password
            except auth.AuthError:
                return None
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(attempt, ['EXAMPLE_FIRST_PASSWORD', 'EXAMPLE_SECOND_PASSWORD']))
        winners = [item for item in results if item]
        self.assertEqual(len(winners), 1)
        self.app.authenticate({'password': winners[0]})


class StandaloneHTTPTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        self.directory = self.home / '.config/clawmingle'
        self.assets = Path(__file__).resolve().parents[1] / 'admin'
        self.app = auth.LocalAuth(self.directory, 17430, initialize=True)
        (self.home / 'binary').write_text('#!/bin/sh\nexit 0\n')
        (self.home / 'binary').chmod(0o700)
        self.setup = Setup(self.directory / 'social.toml', self.home / 'binary', self.assets, self.home)
        self.addCleanup(self.setup.close)
        self.manager = Mock()
        self.manager.assets = self.assets
        self.manager.mutex = threading.Lock()
        self.manager.capability_busy.return_value = False
        self.manager.state.return_value = {'service': {'active': False}}
        self.manager.post.return_value = {'ok': True}
        self.factory = Mock(return_value=self.manager)
        handler = admin.handler_for(None, None, local_auth=self.app, setup=self.setup, manager_factory=self.factory)
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), handler)
        self.server.daemon_threads = True
        self.port = self.server.server_port
        self.app.hosts = {f'127.0.0.1:{self.port}', f'localhost:{self.port}'}
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={'poll_interval': 0.02})
        self.thread.start()
        self.addCleanup(self.stop)
        self.cookie = ''
        self.csrf = ''

    def stop(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def request(self, path, body=None, *, headers=None, raw=None):
        connection = http.client.HTTPConnection('127.0.0.1', self.port, timeout=5)
        values = {'Cookie': self.cookie}
        method = 'GET' if body is None and raw is None else 'POST'
        if method == 'POST':
            values.update({'Content-Type': 'application/json', 'Origin': f'http://127.0.0.1:{self.port}',
                           'X-CSRF-Token': self.csrf})
        values.update(headers or {})
        try:
            connection.request(method, path, body=raw if raw is not None else json.dumps(body).encode() if method == 'POST' else None, headers=values)
            response = connection.getresponse()
            data = response.read()
            return response.status, dict(response.getheaders()), json.loads(data) if data and response.getheader('Content-Type', '').startswith('application/json') else data
        finally:
            connection.close()

    def enroll(self):
        status, headers, _ = self.request('/auth/enroll', {'password': 'EXAMPLE_LOCAL_PASSWORD', 'pairingCode': self.app.bootstrap['token']})
        self.assertEqual(status, 200)
        self.cookie = headers['Set-Cookie'].split(';', 1)[0]
        status, _, data = self.request('/api/bootstrap')
        self.assertEqual(status, 200)
        self.csrf = data['csrfToken']

    def test_public_login_and_auth_gate(self):
        for path in ('/login', '/assets/login.js', '/assets/login.css', '/auth/status'):
            self.assertEqual(self.request(path)[0], 200)
        status, headers, _ = self.request('/')
        self.assertEqual((status, headers['Location']), (302, '/login'))
        for path in ('/health', '/api/state', '/api/setup', '/api/bootstrap', '/assets/app.js'):
            status, _, data = self.request(path)
            self.assertEqual(status, 401)
            self.assertTrue(data['loginRequired'])
        self.assertEqual(self.request('/api/state', headers={'X-CM-Dashboard-Proxy': 'x' * 40})[0], 401)
        self.factory.assert_not_called()

    def test_dns_rebinding_and_cross_site_login_rejected(self):
        self.assertEqual(self.request('/login', headers={'Host': 'evil.example'})[0], 403)
        body = {'password': 'EXAMPLE_LOCAL_PASSWORD', 'pairingCode': self.app.bootstrap['token']}
        for origin in ('', 'https://evil.example'):
            self.assertEqual(self.request('/auth/enroll', body, headers={'Origin': origin})[0], 403)
        self.assertFalse(self.app.auth_path.exists())

    def test_authenticated_setup_requires_csrf_and_not_proxy_header(self):
        self.enroll()
        self.assertEqual(self.request('/setup')[0], 200)
        self.assertEqual(self.request('/api/state')[0], 409)
        for csrf in ('', 'incorrect'):
            self.assertEqual(self.request('/api/setup', {'action': 'registration-cancel'}, headers={'X-CSRF-Token': csrf})[0], 403)
        self.assertEqual(self.request('/api/setup', {'action': 'registration-cancel'})[0], 200)
        self.factory.assert_not_called()

    def test_enroll_setup_finish_and_lazy_manager_end_to_end(self):
        self.enroll()
        settings = {'action': 'save-settings', 'app_id': '123', 'app_secret': 'EXAMPLE_QQ_SECRET',
                    'model': 'example-model', 'reasoning_effort': 'high', 'codex_binary': '/example/codex'}
        status, _, data = self.request('/api/setup', settings)
        self.assertEqual(status, 200)
        self.assertNotIn('EXAMPLE_QQ_SECRET', json.dumps(data))
        self.assertFalse(self.setup.config_path.exists())
        self.assertEqual(self.request('/api/setup', {'action': 'group-manual', 'group_openid': 'test_group', 'admin_openid': 'test_user'})[0], 200)
        with patch('admin_setup.subprocess.run', return_value=subprocess.CompletedProcess([], 0)):
            self.assertEqual(self.request('/api/setup', {'action': 'finish'})[0], 200)
        self.assertFalse(self.request('/api/bootstrap')[2]['setupRequired'])
        self.assertEqual(self.request('/')[0], 200)
        self.assertEqual(self.request('/api/state')[0], 200)
        self.factory.assert_called_once()
        self.assertEqual(self.request('/api/service', {'action': 'start'})[0], 200)
        self.manager.post.assert_called_once_with('/api/service', {'action': 'start'})

    def test_logout_invalidates_browser_session(self):
        self.enroll()
        status, headers, _ = self.request('/auth/logout', {})
        self.assertEqual(status, 200)
        self.assertIn('Max-Age=0', headers['Set-Cookie'])
        self.assertEqual(self.request('/api/bootstrap')[0], 401)

    def test_malformed_body_and_unsupported_content_type(self):
        self.assertEqual(self.request('/auth/login', raw=b'{invalid')[0], 400)
        self.assertEqual(self.request('/auth/login', {}, headers={'Content-Type': 'text/plain'})[0], 415)
        self.assertEqual(self.request('/auth/login', raw=b'x' * 65537)[0], 413)

    def test_proxy_mode_never_accepts_local_cookie(self):
        self.enroll()
        handler_type = admin.handler_for(self.manager, b'p' * 40)
        handler = handler_type.__new__(handler_type)
        handler.headers = Message()
        handler.headers['Cookie'] = self.cookie
        self.assertFalse(handler.authorized())
        handler.headers['X-CM-Dashboard-Proxy'] = 'p' * 40
        self.assertTrue(handler.authorized())

    def test_main_rejects_unsafe_setup_paths_before_creating_pairing(self):
        real = self.home / 'separate'
        real.mkdir(mode=0o700)
        linked = self.home / 'linked'
        linked.symlink_to(real, target_is_directory=True)
        for path in (linked / 'social.toml', real / '../separate/social.toml'):
            with patch('sys.argv', ['clawmingle_admin.py', '--standalone', '--init', '--config', str(path)]), \
                 patch('admin_setup.Path.home', return_value=self.home):
                with self.assertRaises(Exception):
                    admin.main()
            self.assertEqual(list(real.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
