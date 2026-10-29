# Automatic native runtime preparation

ClawMingle prepares its independent runtime after acquiring the application lock
and before starting Codex. This is part of normal startup; there is no separate
native-tools switch. Conversation permissions still decide which tools are
available.

The machine needs Linux, a native Codex installation and Bubblewrap. Automatic
browser preparation also requires Python 3.10 or newer.
ClawMingle uses `codex.binary` when configured. If the historical default
`/opt/codex/current/bin/codex` is absent, it discovers `codex` on `PATH`, including
the native vendor executable behind the official `@openai/codex` npm wrapper.
It checks ownership, executable format and writable ancestors before use.
An invalid custom path fails instead of selecting another executable.

Missing `runtime/codex`, `runtime/sqlite` and `runtime/workspace` directories are
created privately under `data_dir`. The first startup writes a minimal independent
Codex `config.toml` with the configured model/reasoning effort, live web search and
file-based credentials storage. Existing configuration is never overwritten.
Configure the bot's own provider credentials or independent Codex login; the
bootstrap never reads or copies a personal profile, authentication, cookies or
browser state. Custom provider definitions remain explicit configuration.

When the global browser is enabled and no runtime is configured, startup installs
the following private, versioned runtime under `data_dir/runtime/tools`:

- Node.js 24.20.0, verified against the pinned official release SHA-256 checksum.
- `playwright` and `playwright-core` 1.63.0 through an exact npm lock and package
  integrity checks. The browser MCP comes from Playwright Core's bundled CLI.
- Playwright's pinned Chromium headless shell through the official installer.
  Its executable has a same-directory hardlink named `chrome` for the native
  browser adapter. Playwright checks download length over HTTPS; this does not
  provide an independently pinned Chromium archive digest.

Linux x86_64 and aarch64 are supported where the pinned artifacts and required
system libraries are available. The installer reports missing prerequisites;
it never runs apt, sudo, or a system package installer. Resolve any named missing
libraries through the deployment's normal system provisioning.

An existing configured browser runtime is reused without installing packages.
An explicitly disabled browser stays disabled and triggers no download.
Installing the runtime does not grant a conversation browser access, nor enable
personal account integrations.

Preparation uses a private lock, staging directory and atomic release manifest.
It reserves the larger of 15 GiB and 20% of filesystem capacity plus a 1 GiB
installation allowance before downloading. It monitors the entire tools directory
during installation and refuses to publish a runtime at or above 1 GiB. This is
a checked installation budget, not a filesystem quota; subprocess writes can
briefly cross the limit between checks. Downloads and installer subprocesses have
size, time and output limits. Startup cancellation stops the whole installer
process group. A failed preparation is reported before the bot starts accepting
messages.

The installer and browser worker are embedded in the Rust binary, so deployment
does not require a source checkout or manually supplying individual tool paths.
Automatic registration is derived at startup; it does not rewrite user TOML or
change per-conversation grants.
The private `runtime/native-tools.json` readiness record lets the HTML admin show
the prepared browser without persisting host-specific tool paths in user TOML.

Registry and browser CDN access are deployment prerequisites. A refused download
fails preparation explicitly and preserves existing installations. Fresh browser
installation requires access to the pinned official Chromium archive; existing
explicit runtime paths remain available for deployments with restricted CDN access.

Offline tests use fixtures and fake subprocesses:

```sh
python3 -B scripts/test_prepare_native_runtime.py
```

These tests do not install browsers, contact a model, or exercise a live bot.
