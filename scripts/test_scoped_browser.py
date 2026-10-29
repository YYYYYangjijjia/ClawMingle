"""Offline browser smoke: real isolated Chromium, synthetic public-page responses.

Opt in with CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON pointing at a private JSON
object with node, node_modules, chromium and bubblewrap paths. No network/model
or real account is used. Default tests only validate the worker source contract.
"""
import base64
import json
import os
from pathlib import Path
import selectors
import subprocess
import time
import unittest

WORKER = Path(__file__).with_name('clawmingle_browser_worker.mjs').resolve()

class WorkerContractTests(unittest.TestCase):
    def test_worker_has_no_direct_network_fallback_or_user_script(self):
        source = WORKER.read_text()
        self.assertIn("serviceWorkers: 'block'", source)
        self.assertIn("method !== 'GET'", source)
        self.assertIn("context.routeWebSocket", source)
        self.assertNotIn('route.continue', source)
        self.assertNotIn('route.fetch', source)
        self.assertNotIn('eval(config.', source)

    @unittest.skipUnless(os.environ.get('CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON'), 'explicit browser runtime required')
    def test_isolated_worker_renders_javascript_and_rejects_non_get(self):
        runtime = json.loads(Path(os.environ['CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON']).read_text())
        modules = Path(runtime['node_modules']).resolve()
        chrome = Path(runtime['chromium']).resolve()
        argv = [runtime['bubblewrap'], '--unshare-all', '--unshare-user', '--cap-drop', 'ALL', '--new-session', '--die-with-parent', '--ro-bind', '/usr', '/usr', '--symlink', 'usr/bin', '/bin', '--symlink', 'usr/lib', '/lib', '--symlink', 'usr/lib64', '/lib64']
        for path in ['/etc','/runtime','/runtime/node_modules','/runtime/node_modules/playwright','/runtime/node_modules/playwright-core','/runtime/chromium','/home','/home/browser']:
            argv += ['--dir', path]
        argv += ['--proc','/proc','--dev','/dev','--tmpfs','/tmp']
        if Path('/etc/fonts').is_dir(): argv += ['--ro-bind','/etc/fonts','/etc/fonts']
        for source, target in [(Path(runtime['node']).resolve(),'/runtime/node'),(WORKER,'/runtime/worker.mjs'),(modules/'playwright','/runtime/node_modules/playwright'),(modules/'playwright-core','/runtime/node_modules/playwright-core'),(chrome.parent,'/runtime/chromium')]:
            argv += ['--ro-bind',str(source),target]
        argv += ['--chdir','/tmp','--','/runtime/node','/runtime/worker.mjs']
        env={'HOME':'/home/browser','PATH':'/usr/bin','TMPDIR':'/tmp','LANG':'C.UTF-8'}
        process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=env, text=True)
        selector = selectors.DefaultSelector(); selector.register(process.stdout, selectors.EVENT_READ)
        requests=[]; result=None
        try:
            process.stdin.write(json.dumps({'url':'https://example.com/','wait_ms':1200,'screenshot':True})+'\n'); process.stdin.flush()
            deadline=time.monotonic()+25
            while time.monotonic()<deadline:
                if not selector.select(1): continue
                line=process.stdout.readline(3*1024*1024)
                if not line: break
                value=json.loads(line)
                if value['type']=='result': result=value; break
                self.assertEqual(value['type'],'fetch'); requests.append((value['url'],value['method']))
                if value['url']=='https://example.com/':
                    body=b'''<!doctype html><html><title>Isolated fixture</title><body>Initial<script>
                    document.body.insertAdjacentHTML('beforeend','<p>JS_RENDERED</p>');
                    fetch('https://example.com/data.json').then(r=>r.json()).then(v=>document.body.insertAdjacentText('beforeend',v.result));
                    fetch('https://example.com/forbidden-post',{method:'POST',body:'no'}).catch(()=>{});
                    fetch('http://127.0.0.1/forbidden').catch(()=>{});
                    fetch('file:///etc/passwd').catch(()=>{});
                    new WebSocket('wss://example.com/forbidden-socket');
                    </script></body></html>'''; mime='text/html'
                elif value['url']=='https://example.com/data.json': body=b'{"result":" JSON_RENDERED"}'; mime='application/json'
                else: body=b''; mime='text/plain'
                process.stdin.write(json.dumps({'type':'fetch_result','id':value['id'],'ok':True,'status':200,'content_type':mime,'body':base64.b64encode(body).decode()})+'\n'); process.stdin.flush()
            self.assertIsNotNone(result,'worker produced no result')
            self.assertTrue(result.get('ok'),result.get('code'))
            self.assertIn('JS_RENDERED',result['text']); self.assertIn('JSON_RENDERED',result['text'])
            self.assertTrue(base64.b64decode(result['image']['data']).startswith(b'\x89PNG\r\n\x1a\n'))
            self.assertEqual(set(requests),{('https://example.com/','GET'),('https://example.com/data.json','GET')})
        finally:
            selector.close(); process.kill(); process.wait(timeout=5)
            process.stdin.close();process.stdout.close()

if __name__ == '__main__': unittest.main()
