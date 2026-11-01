"""Single-operator authentication for the loopback/SSH-tunnel admin entry."""
from collections import deque
import hashlib
import hmac
from http.cookies import CookieError, SimpleCookie
import json
import math
import os
from pathlib import Path
import secrets
import stat
import threading
import time

ITERATIONS = 600_000
SESSION_SECONDS = 8 * 60 * 60
ENROLLMENT_SECONDS = 30 * 60


class AuthError(Exception):
    def __init__(self, message, status=400):
        super().__init__(message)
        self.status = status


def private_directory(path):
    path = Path(path)
    path.mkdir(parents=True, mode=0o700, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise AuthError('管理认证目录必须属于当前用户，权限为 0700。')


def read_private(path):
    path = Path(path)
    info = path.parent.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise AuthError('管理认证目录权限不正确。')
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600 or info.st_size > 8192:
            raise AuthError('管理认证文件权限或大小不正确。')
        return json.loads(stream.read(8193))


def create_private(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, 'w') as stream:
            json.dump(value, stream)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        Path(path).unlink(missing_ok=True)
        raise


class LocalAuth:
    def __init__(self, directory, port, *, initialize=False, clock=time.time):
        self.directory = Path(directory)
        self.auth_path = self.directory / 'admin-auth.json'
        self.bootstrap_path = self.directory / 'admin-bootstrap.json'
        self.cookie_name = f'cm_admin_{port}'
        self.hosts = {f'127.0.0.1:{port}', f'localhost:{port}'}
        self.clock = clock
        self.lock = threading.RLock()
        self.sessions = {}
        self.failures = deque(maxlen=10)
        self.credentials = None
        self.bootstrap = None
        if not self.auth_path.exists():
            if not initialize:
                raise AuthError('尚未设置管理密码，请使用 --standalone --init 启动首次设置。')
            private_directory(self.directory)
            if self.bootstrap_path.exists():
                self.bootstrap = read_private(self.bootstrap_path)
                self._validate_bootstrap()
                if self.bootstrap['expires_at'] <= self.clock():
                    self.bootstrap_path.unlink()
                    self.bootstrap = None
            if self.bootstrap is None:
                self.bootstrap = {'token': secrets.token_urlsafe(32),
                                  'expires_at': self.clock() + ENROLLMENT_SECONDS}
                try:
                    create_private(self.bootstrap_path, self.bootstrap)
                except FileExistsError:
                    self.bootstrap = read_private(self.bootstrap_path)
                    self._validate_bootstrap()
        else:
            self.credentials = read_private(self.auth_path)
            value = self.credentials
            try:
                valid = set(value) == {'version', 'iterations', 'salt', 'hash'} and value['version'] == 1
                valid = valid and value['iterations'] == ITERATIONS and len(bytes.fromhex(value['salt'])) == 16 and len(bytes.fromhex(value['hash'])) == 32
            except (TypeError, ValueError, KeyError):
                valid = False
            if not valid:
                raise AuthError('管理认证文件无效；不会自动覆盖。')

    def _validate_bootstrap(self):
        value = self.bootstrap
        if not isinstance(value, dict) or set(value) != {'token', 'expires_at'} or not isinstance(value['token'], str) or not 32 <= len(value['token']) <= 128 or type(value['expires_at']) not in (int, float) or not math.isfinite(value['expires_at']):
            raise AuthError('首次配对文件无效；不会自动覆盖。')

    def check_request(self, headers, *, write=False):
        host = headers.get('Host', '')
        if host not in self.hosts:
            raise AuthError('请通过本机地址或 SSH 隧道访问管理台。', 403)
        if write and (headers.get('Origin') != 'http://' + host or headers.get('Sec-Fetch-Site', 'same-origin') != 'same-origin'):
            raise AuthError('请求来源不匹配，请从管理页面重试。', 403)

    def status(self):
        return {'enrollmentRequired': self.credentials is None,
                'enrollmentExpired': self.credentials is None and self.bootstrap['expires_at'] <= self.clock()}

    def _rate_limit(self):
        while self.failures and self.clock() - self.failures[0] >= 300:
            self.failures.popleft()
        if len(self.failures) >= 10:
            raise AuthError('尝试过于频繁，请五分钟后重试。', 429)

    def _new_session(self):
        self.sessions = {key: value for key, value in self.sessions.items() if value['expires_at'] > self.clock()}
        if len(self.sessions) >= 16:
            self.sessions.pop(next(iter(self.sessions)))
        token = secrets.token_urlsafe(32)
        value = {'csrfToken': secrets.token_urlsafe(32), 'expires_at': self.clock() + SESSION_SECONDS}
        self.sessions[token] = value
        return token

    def authenticate(self, body, *, enroll=False):
        with self.lock:
            self._rate_limit()
            expected = {'password', 'pairingCode'} if enroll else {'password'}
            if not isinstance(body, dict) or set(body) != expected or not isinstance(body.get('password'), str) or not 12 <= len(body['password']) <= 256:
                self.failures.append(self.clock())
                raise AuthError('请输入 12–256 字符的管理密码。')
            if enroll:
                if self.credentials is not None or self.auth_path.exists():
                    raise AuthError('管理密码已设置，请直接登录。', 409)
                code = body.get('pairingCode')
                if self.bootstrap['expires_at'] <= self.clock():
                    raise AuthError('配对码已过期，请重新启动管理服务后读取新的配对文件。', 403)
                if not isinstance(code, str) or not hmac.compare_digest(code.encode(), self.bootstrap['token'].encode()):
                    self.failures.append(self.clock())
                    raise AuthError('配对码不正确。', 401)
                salt = secrets.token_bytes(16)
                hashed = hashlib.pbkdf2_hmac('sha256', body['password'].encode(), salt, ITERATIONS)
                value = {'version': 1, 'iterations': ITERATIONS, 'salt': salt.hex(), 'hash': hashed.hex()}
                create_private(self.auth_path, value)
                self.credentials = value
                if self.bootstrap_path.exists() and read_private(self.bootstrap_path) == self.bootstrap:
                    self.bootstrap_path.unlink()
                self.bootstrap = None
            else:
                if self.credentials is None:
                    raise AuthError('请先完成首次管理密码设置。', 409)
                value = self.credentials
                hashed = hashlib.pbkdf2_hmac('sha256', body['password'].encode(), bytes.fromhex(value['salt']), ITERATIONS)
                if not hmac.compare_digest(hashed.hex(), value['hash']):
                    self.failures.append(self.clock())
                    raise AuthError('管理密码不正确。', 401)
            self.failures.clear()
            return self._new_session()

    def session(self, headers):
        with self.lock:
            try:
                cookie = SimpleCookie(headers.get('Cookie', ''))
                token = cookie[self.cookie_name].value if self.cookie_name in cookie else ''
            except CookieError:
                return None
            value = self.sessions.get(token)
            if value is None:
                return None
            if value['expires_at'] <= self.clock():
                self.sessions.pop(token, None)
                return None
            return dict(value, token=token)

    def check_csrf(self, headers, session):
        given = headers.get('X-CSRF-Token', '')
        if not session or not hmac.compare_digest(given.encode(), session['csrfToken'].encode()):
            raise AuthError('管理会话已失效，请重新登录。', 403)

    def logout(self, session):
        with self.lock:
            self.sessions.pop(session['token'], None)

    def cookie(self, token, *, clear=False):
        return f'{self.cookie_name}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={0 if clear else SESSION_SECONDS}'
