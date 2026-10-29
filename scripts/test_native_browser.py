"""Opt-in offline smoke for unmodified Playwright MCP inside a networkless root.

CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON selects the existing BrowserRuntime paths.
The Unix SOCKS socket maps synthetic documentation domains to HTTP/TLS fixtures. No model, real
website, account, browser download, or existing profile is used.
"""
import base64
import gzip
import hashlib
import http.server
import select
import ssl
import struct
import json
import os
from pathlib import Path
import re
import selectors
import socket
import subprocess
import tempfile
import threading
import time
import unittest


HOOK = Path(__file__).with_name('clawmingle_browser_network.cjs').resolve()


class McpClient:
    def __init__(self, process):
        self.process = process
        self.selector = selectors.DefaultSelector()
        self.selector.register(process.stdout, selectors.EVENT_READ)
        self.sequence = 0
        self.buffer = b''

    def send(self, message):
        self.process.stdin.write(json.dumps(message).encode() + b'\n')
        self.process.stdin.flush()

    def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        self.send({'jsonrpc': '2.0', 'id': request_id, 'method': method, 'params': params})
        deadline = time.monotonic() + 35
        while time.monotonic() < deadline:
            while b'\n' in self.buffer:
                line, self.buffer = self.buffer.split(b'\n', 1)
                value = json.loads(line)
                if value.get('id') == request_id:
                    if 'error' in value:
                        raise AssertionError(value['error'])
                    return value['result']
                if value.get('method') == 'roots/list' and 'id' in value:
                    self.send({'jsonrpc': '2.0', 'id': value['id'], 'result': {
                        'roots': [{'uri': 'file:///workspace', 'name': 'fixture'}],
                    }})
                elif 'method' in value and 'id' in value:
                    self.send({'jsonrpc': '2.0', 'id': value['id'], 'error': {
                        'code': -32601, 'message': 'unsupported fixture request',
                    }})
            if not self.selector.select(1):
                continue
            chunk = os.read(self.process.stdout.fileno(), 65536)
            if not chunk:
                raise AssertionError('MCP stdout closed')
            self.buffer += chunk
            if len(self.buffer) > 16 * 1024 * 1024:
                raise AssertionError('MCP response exceeded smoke budget')
        raise AssertionError(f'MCP timeout: {method}')

    def call(self, name, arguments):
        result = self.request('tools/call', {'name': name, 'arguments': arguments})
        if result.get('isError'):
            raise AssertionError(result)
        return result

    def close(self):
        self.selector.close()


def text(result):
    return '\n'.join(item['text'] for item in result.get('content', []) if item['type'] == 'text')


def receive_exact(connection, size):
    data = bytearray()
    while len(data) < size:
        chunk = connection.recv(size - len(data))
        if not chunk:
            raise EOFError('fixture connection closed')
        data.extend(chunk)
    return bytes(data)


class FixtureHttp(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *_):
        pass

    def respond(self, body=b'', status=200, headers=None):
        self.send_response(status)
        for name, value in (headers or {'Content-Type': 'text/html'}).items():
            self.send_header(name, value)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        size = int(self.headers.get('Content-Length', 0))
        body = self.rfile.read(size)
        self.server.requests.append(('POST', self.path, body))
        self.respond(b'POST_OK:' + body, headers={'Content-Type': 'text/plain'})

    def do_GET(self):
        self.server.requests.append(('GET', self.path, self.headers.get('Cookie', '')))
        if self.path == '/':
            self.respond(b'''<!doctype html><title>Native SOCKS browser</title>
            <h1>Native browser</h1><div id="state">before click</div>
            <button onclick="document.getElementById('state').textContent='CLICK_PERSISTED'">Change text</button>
            <video id="movie" muted controls width="160" height="90"></video>
            <iframe src="https://example.org/frame-redirect"></iframe>''')
        elif self.path == '/frame-redirect':
            self.respond(status=302, headers={'Location': 'https://example.org/frame'})
        elif self.path == '/frame':
            self.respond(b'<!doctype html><h2>CROSS_SITE_FRAME_OK</h2>')
        elif self.path == '/popup-start':
            self.respond(status=302, headers={'Location': 'https://example.org/popup-middle',
                                             'Set-Cookie': 'popup=fixture; Secure; SameSite=Lax'})
        elif self.path == '/popup-middle':
            self.respond(status=302, headers={'Location': 'https://example.com/popup-final'})
        elif self.path == '/popup-final':
            self.respond(b'''<!doctype html><h1>POPUP_CHAIN_OK</h1>
            <iframe src="https://example.org/frame-redirect"></iframe><script src="/popup.js"></script>''')
        elif self.path == '/popup.js':
            self.respond(b'document.body.append("POPUP_SCRIPT_OK")', headers={'Content-Type': 'application/javascript'})
        elif self.path == '/compressed':
            self.respond(gzip.compress(b'<!doctype html><h1>COMPRESSED_RESOURCE_OK</h1>'),
                         headers={'Content-Type': 'text/html', 'Content-Encoding': 'gzip'})
        elif self.path == '/sw.js':
            self.respond(b'''self.addEventListener('install',event=>self.skipWaiting());
                self.addEventListener('activate',event=>event.waitUntil(self.clients.claim()));
                self.addEventListener('fetch',event=>{
                    if(new URL(event.request.url).pathname==='/sw-value')
                        event.respondWith(caches.open('fixture').then(async cache=>{
                            let found=await cache.match('/sw-value');
                            if(!found){found=new Response('SERVICE_WORKER_CACHE_OK');await cache.put('/sw-value',found.clone())}
                            return found;
                        }));
                });''', headers={'Content-Type': 'application/javascript', 'Cache-Control': 'no-cache'})
        elif self.path == '/clip.webm':
            body = self.server.webm
            headers = {'Content-Type': 'video/webm', 'Accept-Ranges': 'bytes'}
            status = 200
            requested = self.headers.get('Range')
            if requested:
                self.server.ranges.append(requested)
                start, end = requested.removeprefix('bytes=').split('-', 1)
                start = int(start)
                end = min(int(end) if end else len(body) - 1, len(body) - 1)
                headers['Content-Range'] = f'bytes {start}-{end}/{len(body)}'
                body = body[start:end + 1]
                status = 206
            self.respond(body, status, headers)
        elif self.path == '/download':
            size = 9 * 1024 * 1024
            self.send_response(200)
            self.send_header('Content-Type', 'application/octet-stream')
            self.send_header('Content-Disposition', 'attachment; filename="native-download.bin"')
            self.send_header('Content-Length', str(size))
            self.end_headers()
            for _ in range(size // 65536):
                self.wfile.write(b'D' * 65536)
        elif self.path == '/ws':
            key = self.headers.get('Sec-WebSocket-Key', '')
            magic = '258EAFA5-E914-' + '47DA-95CA-' + 'C5AB0DC85B11'
            accept = base64.b64encode(hashlib.sha1((key + magic).encode()).digest()).decode()
            self.send_response(101)
            self.send_header('Upgrade', 'websocket')
            self.send_header('Connection', 'Upgrade')
            self.send_header('Sec-WebSocket-Accept', accept)
            self.end_headers()
            self.wfile.flush()
            try:
                while True:
                    first, second = self.rfile.read(2)
                    length = second & 127
                    if length == 126:
                        length = struct.unpack('!H', self.rfile.read(2))[0]
                    elif length == 127:
                        length = struct.unpack('!Q', self.rfile.read(8))[0]
                    if length > 4096:
                        raise ValueError('fixture WS payload limit')
                    mask = self.rfile.read(4) if second & 128 else None
                    data = self.rfile.read(length)
                    if mask:
                        data = bytes(value ^ mask[index % 4] for index, value in enumerate(data))
                    if first & 15 == 8:
                        break
                    if first & 15 == 1:
                        response = b'ECHO:' + data
                        self.wfile.write(bytes([0x81, len(response)]) + response)
                        self.wfile.flush()
            except (OSError, ValueError):
                pass
            self.close_connection = True
        elif self.path == '/private-redirect':
            self.respond(status=302, headers={'Location': 'http://127.0.0.1:9999/private'})
        else:
            self.respond(b'FIXTURE_OK', headers={'Content-Type': 'text/plain'})


class SyntheticNetwork:
    """SOCKS fixture maps only documentation domains to loopback test servers.

    Production uses the Rust public-address policy; this explicit fixture mapping
    is never part of application code or available through runtime configuration.
    """
    def __init__(self, path, directory):
        cert, key = directory / 'cert.pem', directory / 'key.pem'
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                        '-keyout', str(key), '-out', str(cert), '-days', '1', '-subj', '/CN=example.com',
                        '-addext', 'subjectAltName=DNS:example.com,DNS:example.org'],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        public = subprocess.run(['openssl', 'x509', '-pubkey', '-noout', '-in', str(cert)],
                                check=True, stdout=subprocess.PIPE).stdout
        der = subprocess.run(['openssl', 'pkey', '-pubin', '-outform', 'DER'], input=public,
                             check=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL).stdout
        self.spki = base64.b64encode(hashlib.sha256(der).digest()).decode()
        self.requests = []
        self.ranges = []
        self.http = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FixtureHttp)
        self.https = http.server.ThreadingHTTPServer(('127.0.0.1', 0), FixtureHttp)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(cert, key)
        self.https.socket = tls.wrap_socket(self.https.socket, server_side=True)
        self.server_threads = []
        for server in (self.http, self.https):
            server.requests = self.requests
            server.ranges = self.ranges
            server.webm = b''
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            self.server_threads.append(thread)
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path))
        self.listener.listen(128)
        self.listener.settimeout(.2)
        self.stop = threading.Event()
        self.targets = []
        self.sockets = set()
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        while not self.stop.is_set():
            try:
                connection, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError:
                break
            threading.Thread(target=self.handle, args=(connection,), daemon=True).start()

    def handle(self, client):
        upstream = None
        with self.lock:
            self.sockets.add(client)
        try:
            client.settimeout(5)
            version, size = receive_exact(client, 2)
            methods = receive_exact(client, size)
            if version != 5 or 0 not in methods:
                return
            client.sendall(b'\x05\x00')
            version, command, reserved, kind = receive_exact(client, 4)
            if (version, command, reserved) != (5, 1, 0):
                return
            if kind == 3:
                host = receive_exact(client, receive_exact(client, 1)[0]).decode()
            elif kind == 1:
                host = socket.inet_ntop(socket.AF_INET, receive_exact(client, 4))
            elif kind == 4:
                host = socket.inet_ntop(socket.AF_INET6, receive_exact(client, 16))
            else:
                return
            port = struct.unpack('!H', receive_exact(client, 2))[0]
            self.targets.append((host, port))
            if host not in ('example.com', 'example.org') or port not in (80, 443, 8443):
                client.sendall(b'\x05\x02\x00\x01' + bytes(6))
                return
            selected = self.http if port == 80 else self.https
            upstream = socket.create_connection(selected.server_address, timeout=5)
            with self.lock:
                self.sockets.add(upstream)
            client.sendall(b'\x05\x00\x00\x01' + bytes(6))
            client.settimeout(None)
            upstream.settimeout(None)
            while not self.stop.is_set():
                readable, _, _ = select.select([client, upstream], [], [], .2)
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        return
                    (upstream if source is client else client).sendall(data)
        except (OSError, EOFError, ValueError):
            pass
        finally:
            for item in (client, upstream):
                if item:
                    item.close()
                    with self.lock:
                        self.sockets.discard(item)

    def cancel(self):
        self.stop.set()
        self.listener.close()
        with self.lock:
            for item in list(self.sockets):
                try:
                    item.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                item.close()
        self.thread.join(3)

    def close(self):
        self.cancel()
        for server in (self.http, self.https):
            server.shutdown()
            server.server_close()
        for thread in self.server_threads:
            thread.join(3)


class NativeBrowserSmoke(unittest.TestCase):
    @unittest.skipUnless(os.environ.get('CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON'),
                         'explicit existing browser runtime required')
    def test_official_mcp_uses_general_native_network_and_retains_scope_isolation(self):
        runtime = json.loads(Path(os.environ['CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON']).read_text())
        with tempfile.TemporaryDirectory(prefix='clawmingle-native-browser-smoke-') as temporary:
            directory = Path(temporary)
            workspace = directory / 'workspace'
            workspace.mkdir()
            sentinel = directory / 'host-only-fixture'
            sentinel.write_text('not browser state')
            host_listener = socket.socket()
            host_listener.bind(('127.0.0.1', 0))
            host_listener.listen(1)
            network = SyntheticNetwork(directory / 'network.sock', directory)
            config = directory / 'config.json'
            config.write_text(json.dumps({
                'browser': {
                    'browserName': 'chromium', 'isolated': True,
                    'launchOptions': {'headless': True, 'chromiumSandbox': False,
                        'executablePath': '/runtime/browser/chromium/chrome',
                        'downloadsPath': '/workspace/.browser/downloads',
                        # Trust only this ephemeral fixture key, not arbitrary TLS.
                        'args': ['--disable-dev-shm-usage', '--renderer-process-limit=2',
                                 '--ignore-certificate-errors-spki-list=' + network.spki]},
                    'contextOptions': {'serviceWorkers': 'allow', 'acceptDownloads': True},
                },
                'capabilities': ['vision', 'pdf'], 'imageResponses': 'allow',
                'outputDir': '/workspace/.browser', 'outputMaxSize': 0,
            }))
            argv = [runtime['bubblewrap'], '--unshare-all', '--unshare-user', '--cap-drop', 'ALL',
                    '--new-session', '--die-with-parent', '--ro-bind', '/usr', '/usr',
                    '--symlink', 'usr/bin', '/bin', '--symlink', 'usr/lib', '/lib',
                    '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp']
            for path in ['/etc', '/run', '/home/browser', '/runtime/browser/node_modules/playwright-core',
                         '/runtime/browser/chromium', '/workspace']:
                argv += ['--dir', path]
            for path in ['/etc/fonts', '/etc/ssl/certs']:
                if Path(path).is_dir():
                    argv += ['--ro-bind', path, path]
            for source, target in [
                (Path(runtime['node']).resolve(), '/runtime/browser/node'),
                (Path(runtime['node_modules']).resolve() / 'playwright-core', '/runtime/browser/node_modules/playwright-core'),
                (Path(runtime['chromium']).resolve().parent, '/runtime/browser/chromium'),
                (HOOK, '/runtime/browser/network.cjs'), (config, '/runtime/browser/config.json'),
                (directory / 'network.sock', '/run/clawmingle-browser.sock'),
            ]:
                argv += ['--ro-bind', str(source), target]
            argv += ['--bind', str(workspace), '/workspace', '--chdir', '/workspace', '--',
                     '/runtime/browser/node', '/runtime/browser/network.cjs']
            process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=subprocess.DEVNULL, env={
                                           'HOME': '/home/browser', 'PATH': '/usr/bin', 'TMPDIR': '/tmp', 'LANG': 'C.UTF-8',
                                       })
            client = McpClient(process)
            try:
                client.request('initialize', {'protocolVersion': '2025-06-18',
                    'capabilities': {'roots': {'listChanged': False}},
                    'clientInfo': {'name': 'clawmingle-offline-test', 'version': '2'}})
                client.send({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
                tools = client.request('tools/list', {})['tools']
                names = {tool['name'] for tool in tools}
                self.assertTrue({'browser_navigate', 'browser_click', 'browser_take_screenshot',
                                 'browser_mouse_wheel', 'browser_evaluate'} <= names)
                self.assertTrue(all(isinstance(tool['inputSchema'], dict) for tool in tools))
                client.call('browser_navigate', {'url': 'http://example.com/'})
                plain = client.call('browser_evaluate', {'function': '''async () => {
                    const post=await fetch('/post',{method:'POST',body:'fixture-body'}).then(r=>r.text());
                    const ws=await new Promise((resolve,reject)=>{
                        const socket=new WebSocket('ws://example.com/ws');
                        socket.onopen=()=>socket.send('plain');socket.onmessage=e=>{socket.close();resolve(e.data)};
                        socket.onerror=()=>reject(new Error('WS failed'));
                    });return {post,ws};
                }'''})
                self.assertIn('POST_OK:fixture-body', text(plain))
                self.assertIn('ECHO:plain', text(plain))
                client.call('browser_navigate', {'url': 'https://example.com/'})
                snapshot = client.call('browser_snapshot', {})
                self.assertIn('CROSS_SITE_FRAME_OK', text(snapshot))
                match = re.search(r'button "Change text" \[ref=(\w+)\]', text(snapshot))
                self.assertIsNotNone(match, text(snapshot))
                client.call('browser_click', {'target': match.group(1), 'element': 'Change text'})
                self.assertIn('CLICK_PERSISTED', text(client.call('browser_snapshot', {})))
                native = client.call('browser_evaluate', {'function': '''async () => {
                    const post=await fetch('/post',{method:'POST',body:'tls-body'}).then(r=>r.text());
                    await navigator.serviceWorker.register('/sw.js');await navigator.serviceWorker.ready;
                    if(!navigator.serviceWorker.controller) await new Promise(resolve=>navigator.serviceWorker.addEventListener('controllerchange',resolve,{once:true}));
                    const sw=await fetch('/sw-value').then(r=>r.text());
                    const cached=await (await caches.open('fixture')).match('/sw-value').then(r=>r.text());
                    const ws=await new Promise((resolve,reject)=>{
                        window.fixtureSocket=new WebSocket('wss://example.com/ws');
                        fixtureSocket.onopen=()=>fixtureSocket.send('secure');
                        fixtureSocket.onmessage=e=>resolve(e.data);
                        fixtureSocket.onerror=()=>reject(new Error('WSS failed'));
                        fixtureSocket.onclose=()=>window.fixtureSocketClosed=true;
                    });return {post,sw,cached,ws};
                }'''})
                for expected in ['POST_OK:tls-body', 'SERVICE_WORKER_CACHE_OK', 'ECHO:secure']:
                    self.assertIn(expected, text(native))
                generated = client.call('browser_evaluate', {'function': '''async () => {
                    const canvas=document.createElement('canvas');canvas.width=160;canvas.height=90;
                    const ctx=canvas.getContext('2d');const stream=canvas.captureStream(10);
                    const recorder=new MediaRecorder(stream,{mimeType:'video/webm;codecs=vp8'});
                    const chunks=[];recorder.ondataavailable=e=>chunks.push(e.data);
                    const ended=new Promise(resolve=>recorder.onstop=resolve);recorder.start();
                    const timer=setInterval(()=>{ctx.fillStyle='red';ctx.fillRect(0,0,160,90)},50);
                    await new Promise(resolve=>setTimeout(resolve,500));recorder.stop();await ended;
                    clearInterval(timer);stream.getTracks().forEach(track=>track.stop());
                    const bytes=new Uint8Array(await new Blob(chunks).arrayBuffer());
                    return 'MCP_WEBM:'+btoa(String.fromCharCode(...bytes));
                }'''})
                webm = base64.b64decode(re.search(r'MCP_WEBM:([A-Za-z0-9+/=]+)', text(generated)).group(1))
                network.https.webm = webm
                video = client.call('browser_evaluate', {'function': '''async () => {
                    const video=document.getElementById('movie');video.src='/clip.webm';await video.play();
                    await new Promise(resolve=>setTimeout(resolve,200));video.pause();return video.videoWidth;
                }'''})
                self.assertIn('160', text(video))
                self.assertTrue(network.ranges)
                screenshot = client.call('browser_take_screenshot', {'type': 'png'})
                images = [item for item in screenshot['content'] if item['type'] == 'image']
                self.assertTrue(images, text(screenshot))
                png = base64.b64decode(images[0]['data'])
                self.assertTrue(png.startswith(b'\x89PNG\r\n\x1a\n'))
                download = client.call('browser_run_code_unsafe', {'code': '''async page => {
                    const [download]=await Promise.all([page.waitForEvent('download'),
                        page.evaluate(()=>{const a=document.createElement('a');a.href='/download';a.textContent='Download';document.body.append(a);a.click()})]);
                    await download.saveAs('/workspace/.browser/native-download.bin');return download.suggestedFilename();
                }'''})
                self.assertIn('native-download.bin', text(download))
                self.assertEqual((workspace / '.browser/native-download.bin').stat().st_size, 9 * 1024 * 1024)
                calls = client.call('browser_evaluate', {'function': '''async () => {
                    for(let i=0;i<520;i+=20) await Promise.all(Array.from({length:Math.min(20,520-i)},(_,j)=>fetch('/many?i='+(i+j)).then(r=>{if(!r.ok)throw new Error('request failed')})));
                    return 'MORE_THAN_512_REQUESTS_OK';
                }'''})
                self.assertIn('MORE_THAN_512_REQUESTS_OK', text(calls))
                self.assertEqual(sum(route.startswith('/many?i=') for _, route, _ in network.requests), 520)
                for opener in ["window.open('https://example.com/popup-start')",
                               "(()=>{const a=document.createElement('a');a.href='https://example.com/popup-start';a.target='_blank';document.body.append(a);a.click()})()"]:
                    popup = client.call('browser_run_code_unsafe', {'code': '''async page => {
                        const [popup]=await Promise.all([page.context().waitForEvent('page'),page.evaluate(()=>OPENER)]);
                        await popup.waitForLoadState('domcontentloaded',{timeout:5000});
                        const result={url:popup.url(),body:await popup.locator('body').innerText(),
                            frame:await popup.frameLocator('iframe').locator('body').innerText()};
                        await popup.close();return result;
                    }'''.replace('OPENER', opener)})
                    for expected in ['https://example.com/popup-final', 'POPUP_CHAIN_OK', 'POPUP_SCRIPT_OK', 'CROSS_SITE_FRAME_OK']:
                        self.assertIn(expected, text(popup))
                self.assertTrue(any(method == 'GET' and route == '/popup-final' and 'popup=fixture' in cookie
                                    for method, route, cookie in network.requests))
                client.call('browser_navigate', {'url': 'https://example.com/compressed'})
                self.assertIn('COMPRESSED_RESOURCE_OK', text(client.call('browser_snapshot', {})))
                client.call('browser_navigate', {'url': 'https://example.com:8443/'})
                self.assertIn(('example.com', 8443), network.targets)
                private = client.call('browser_evaluate', {'function': '''async () => {
                    const http=await fetch('http://127.0.0.1:9999/private').then(()=>false).catch(()=>true);
                    const ws=await new Promise(resolve=>{
                        const socket=new WebSocket('wss://127.0.0.1:9999/private');
                        socket.onopen=()=>{socket.close();resolve(false)};
                        socket.onerror=()=>resolve(true);socket.onclose=()=>resolve(true);
                    });return {httpBlocked:http,webSocketBlocked:ws};
                }'''})
                self.assertIn('"httpBlocked":true', text(private).replace(' ', ''))
                self.assertIn('"webSocketBlocked":true', text(private).replace(' ', ''))
                self.assertTrue(any(host == '127.0.0.1' for host, _ in network.targets))
                denied = client.request('tools/call', {'name': 'browser_navigate', 'arguments': {'url': 'http://example.com/private-redirect'}})
                self.assertTrue(denied.get('isError'), text(denied))
                # Neither changing browser routing nor native Node code can
                # reach an unmounted host file or a host-only TCP listener.
                sandbox = client.call('browser_run_code_unsafe', {'code': '''async page => {
                    await page.context().unrouteAll();
                    const process=page.constructor.constructor('return process')();
                    const fs=process.getBuiltinModule('node:fs');const net=process.getBuiltinModule('node:net');
                    const blocked=await new Promise(resolve=>{
                        const socket=net.connect({host:'127.0.0.1',port:HOST_PORT});socket.setTimeout(1000);
                        const done=value=>{socket.destroy();resolve(value)};
                        socket.once('connect',()=>done(false));socket.once('error',()=>done(true));socket.once('timeout',()=>done(true));
                    });return {networkBlocked:blocked,hostFileVisible:fs.existsSync(HOST_FILE)};
                }'''.replace('HOST_PORT', str(host_listener.getsockname()[1])).replace('HOST_FILE', json.dumps(str(sentinel)))})
                self.assertIn('"networkBlocked":true', text(sandbox).replace(' ', ''))
                self.assertIn('"hostFileVisible":false', text(sandbox).replace(' ', ''))
                client.call('browser_navigate', {'url': 'https://example.com/'})
                client.call('browser_evaluate', {'function': '''async () => {
                    window.fixtureSocketClosed=false;
                    await new Promise((resolve,reject)=>{
                        window.fixtureSocket=new WebSocket('wss://example.com/ws');
                        fixtureSocket.onopen=resolve;fixtureSocket.onerror=reject;
                        fixtureSocket.onclose=()=>window.fixtureSocketClosed=true;
                    });return 'CONNECTED';
                }'''})
                network.cancel()
                client.call('browser_evaluate', {'function': '''async () => {
                    for(let i=0;i<50&&!window.fixtureSocketClosed;i++)await new Promise(r=>setTimeout(r,20));
                    return {closed:window.fixtureSocketClosed};
                }'''})
                closed = client.call('browser_evaluate', {'function': '() => window.fixtureSocketClosed'})
                self.assertIn('true', text(closed))
                print(f'Official MCP tools={len(tools)}, PNG={len(png)} bytes, WebM={len(webm)} bytes; '
                      'HTTP/HTTPS POST, WS/WSS, SW/cache, 9MiB download, 520 requests, popup, private denial and cancellation verified')
            finally:
                process.kill()
                process.wait(timeout=5)
                client.close()
                process.stdin.close()
                process.stdout.close()
                host_listener.close()
                network.close()


if __name__ == '__main__':
    unittest.main()
