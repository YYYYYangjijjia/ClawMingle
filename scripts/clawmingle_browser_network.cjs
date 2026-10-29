// Native browser environment bootstrap. All task tools and browser operations
// remain the unmodified Playwright MCP. This process only bridges a netns-local
// SOCKS5 listener to the host's private, destination-checked Unix socket.
'use strict';

const net = require('node:net');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawn } = require('node:child_process');
const SOCKET = '/run/clawmingle-browser.sock';
const RUNTIME = '/runtime/browser';
const sockets = new Set();

const server = net.createServer({ allowHalfOpen: true }, socket => {
  const upstream = net.createConnection({ path: SOCKET, allowHalfOpen: true });
  sockets.add(socket);
  sockets.add(upstream);
  const close = () => {
    socket.destroy();
    upstream.destroy();
    sockets.delete(socket);
    sockets.delete(upstream);
  };
  socket.on('error', close);
  upstream.on('error', close);
  socket.on('close', close);
  upstream.on('close', close);
  // Node streams apply backpressure; response bodies are neither buffered nor
  // rewritten. Half-closes are preserved until the opposite direction ends.
  socket.pipe(upstream);
  upstream.pipe(socket);
});
server.maxConnections = 256;

let child;
let temporary;
let closing = false;
function close() {
  if (closing) return;
  closing = true;
  for (const socket of sockets) socket.destroy();
  sockets.clear();
  server.close();
  if (child && child.exitCode === null && child.signalCode === null) child.kill('SIGTERM');
  if (temporary) fs.rmSync(temporary, { recursive: true, force: true });
}

server.on('error', () => { close(); process.exitCode = 1; });
server.listen(0, '127.0.0.1', () => {
  try {
    temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'clawmingle-browser-config-'));
    fs.chmodSync(temporary, 0o700);
    const config = JSON.parse(fs.readFileSync(`${RUNTIME}/config.json`, 'utf8'));
    config.browser.launchOptions.proxy = {
      server: `socks5://127.0.0.1:${server.address().port}`,
      // Do not let Chromium bypass the checked proxy for localhost/link-local.
      bypass: '<-loopback>',
    };
    fs.mkdirSync(config.browser.launchOptions.downloadsPath, { recursive: true, mode: 0o700 });
    const selected = path.join(temporary, 'config.json');
    fs.writeFileSync(selected, JSON.stringify(config), { mode: 0o600 });
    child = spawn(process.execPath, [`${RUNTIME}/node_modules/playwright-core/cli.js`, 'mcp', '--config', selected], {
      stdio: 'inherit',
    });
    child.once('error', () => { close(); process.exitCode = 1; });
    child.once('exit', code => { close(); process.exitCode = code ?? 1; });
  } catch {
    close();
    process.exitCode = 1;
  }
});
process.once('SIGTERM', close);
process.once('SIGINT', close);
process.once('exit', close);
