# Native browser environment

ClawMingle reuses the Playwright MCP bundled in the configured Playwright Core
runtime. It does not implement navigation, element actions, screenshots, video
playback, or website-specific browser tools. Native Codex discovers the server's
complete schemas and receives its original MCP content, including PNG image
blocks. No personal browser profile, account connector, extension, cookie file,
client certificate, or storage-state file is imported.

The tested runtime is Playwright Core 1.63.0. Its original entry point remains
`node <node_modules>/playwright-core/cli.js mcp --config <config.json>`. A small
Node bootstrap starts the environment's local network bridge, supplies the proxy
setting, and starts that CLI with unchanged stdio. It does not replace MCP or
load another browser automation package. The configuration enables the standard
tools plus `vision` and `pdf`, allows image responses, and creates an isolated
Chromium context. Pages remain available across successive calls during the
native execution environment's lifetime.

`NativeBrowser::prepare(&BrowserRuntime)` prepares the private socket and base
configuration. The environment owner calls `append_mounts` before writable
workspace mounts, uses `mcp_config(environment_id)`, and retains the object until
the executor has been killed and reaped, then awaits `shutdown()`. The explicit
environment ID makes Codex launch MCP stdio inside that executor. Cancellation
closes active and pending outbound connections, including idle WebSockets.

## Native network protocols

Chromium uses its ordinary SOCKS5 proxy support. The bootstrap listens on an
operating-system-selected loopback port inside the executor's isolated network
namespace and streams each connection over a private Unix socket. The parent
implements only the SOCKS5 CONNECT handshake and public TCP destination policy.
It does not parse, decompress, buffer, or reconstruct HTTP response bodies.

HTTP, HTTPS, all HTTP methods and request bodies, redirects, cookies, compression,
HTTP caching, WS/WSS, service workers and accepted downloads are handled by the
browser itself. TLS runs end to end between Chromium and the public server;
public system CA certificates are mounted read-only. Redirects to a new target
need another checked proxy connection. All public TCP ports are eligible, subject
to Chromium's own unsafe-port rules. There is no website allowlist or separate
per-page, response-body, request-count, or total-traffic quota.

Chrome resolves SOCKS5 destinations through the proxy and supports only TCP
through this protocol. UDP, QUIC, direct WebRTC networking and WebTransport over
HTTP/3 are consequently unavailable; ordinary websites can use their TCP
fallback where supported. This is a transport limitation, not a site plugin.
See [Chromium's proxy documentation](https://chromium.googlesource.com/chromium/src/+/HEAD/net/docs/proxy.md)
and [Playwright's proxy setting](https://playwright.dev/docs/api/class-browsertype#browser-type-launch-option-proxy).

The previous CDP Fetch interception, JSON/base64 resource bridge, blanket
WebSocket block and GET/HEAD/OPTIONS-only policy are removed. There is no extra
CDP listening port. The adapter also stops imposing an independent 64 MiB MCP
artifact eviction threshold, so a download is not silently removed by a second,
smaller browser budget.

## Isolation and resource ownership

Every TCP destination is resolved once on the host. The entire DNS answer must
contain only permitted public addresses; mixed public/private results fail. The
parent connects to a checked numeric address and verifies the connected peer,
so later DNS changes cannot redirect that connection. Private, loopback,
link-local and other special-purpose ranges remain unavailable. Globally
routable addresses assigned to the host's own interfaces are also denied; the
interface metadata is read when the environment is prepared. This lookup does
not change interfaces, routes, DNS, firewall rules or system services.

Chromium's implicit localhost/link-local proxy bypass is explicitly disabled.
The JavaScript bootstrap and proxy preferences are still not the security
boundary: removing them leaves the executor with no direct external network.
The socket is private to this execution environment; another conversation's
socket, workspace, browser state and host credentials are never mounted.
The native unsafe Node tool can use this scope's public-network grant, but
cannot use it to select a private or host-local destination.

The host bounds resources to 128 established or connecting tunnels and 128
pending connections. Queue admission and connection establishment each have a
15-second timeout; established streams have no browsing-duration or idle
WebSocket deadline. Native Node and Tokio streams apply backpressure using
bounded buffers. Cancellation aborts the connection tasks and closes both ends.
No external proxy environment variables or personal authentication are inherited.

Temporary browser profiles and cache live in the executor's private `/tmp`.
Downloads and saved MCP artifacts live under `/workspace/.browser`; accepted
downloads use its `downloads` directory before any native MCP save operation.
They remain subject to the executor's existing single-file and total-workspace
resource limits. Service memory/process limits, executor CPU limits and workspace
monitoring continue to apply. The workspace monitor is a periodic budget check,
not a filesystem quota; this change does not relax those resource boundaries.

## Offline verification

The opt-in smoke uses the installed runtime, an unmodified MCP, a netless
Bubblewrap root and synthetic documentation-domain servers. An explicit test
SOCKS endpoint maps those domains to loopback HTTP/TLS fixtures; this mapping is
not available in production configuration. The test generates an ephemeral TLS
key and trusts only that key's SPKI in the test browser. Production does not
ignore certificate errors or import that key.

```sh
CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON=/absolute/path/browser-runtime.json \
  python3 -B scripts/test_native_browser.py -v
```

The smoke checks native schemas and PNG image blocks, retained click state,
HTTP/HTTPS POST bodies, WS/WSS messages, service-worker control and CacheStorage,
a 9 MiB native download, more than 512 fetches, supported video playback with
Range, compressed responses, public nonstandard ports, cross-site redirects and
iframes, `window.open`, and `target=_blank` without an opener. It verifies private
HTTP/WebSocket/redirect rejection, cancellation of a live WSS connection, and an
unsafe-Node attempt to reach a host-only listener and file after removing browser
routing. It contacts no model, real website, QQ account or personal application,
and downloads no runtime package.

Rust unit tests separately exercise the production SOCKS handshake, public and
host-interface address policy, fragmented requests, unsupported UDP commands,
streaming and cancellation of both ends of an established tunnel. These checks
do not establish compatibility with any particular live website, login flow,
proprietary codec or DRM implementation.

`tests/social_native_tools_smoke.rs` additionally drives ClawMingle's public
`AppServerHandle::start_isolated` and `execute_social_with_input` APIs against a
loopback Responses fixture. It checks the schemas sent by the real Codex process,
remote file writes/reads, native `view_image`, original MCP image content reaching
the provider, and thread resume with a fresh executor. Its browser uses
`about:blank` and synthetic DOM and its app-server profile is anonymous:

```sh
CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON=/absolute/path/browser-runtime.json \
  cargo test --test social_native_tools_smoke -- --ignored --nocapture
```
