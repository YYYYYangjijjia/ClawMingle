# ClawMingle

[中文](README.md) · [Deployment](docs/deploy.md) · [Agent deployment runbook](docs/agent-deployment.md) · [Admin](docs/admin.md)

ClawMingle is an AI participant for QQ group conversations, built on the official QQ bot platform. **Jev decides whether to participate, whom a message addresses, and whether it changes an existing task. Codex writes replies and carries out tasks.** The application handles bursts of short messages, context selection, cancellation, delivery, and rate-limit recovery.

Built on the Rust foundation of [CodexClaw](https://github.com/Rhapsody0x1/CodexClaw), it focuses on multi-party conversation and task continuity. It uses a separate bot Codex login and runtime. Each group can have its own interaction style, memory, and tool permissions.

## Deploy with a coding agent

Give the repository to a coding agent that can work on the target Linux machine. The [deployment runbook](docs/agent-deployment.md) covers prerequisites, build, installation, validation, and updates. You create the QQ bot and enter credentials and register your group in the local or SSH-tunneled admin page. **Keep credentials out of agent messages.**

Suggested request:

> Deploy ClawMingle on this Linux machine from https://github.com/YYYYYangjijjia/ClawMingle . Read the repository guidance and docs/agent-deployment.md first. Check dependencies and existing installations, then build, install, and validate. Keep administration accessible only locally or through an SSH tunnel. I will enter QQ credentials and complete the bot login in the browser; do not reuse my personal Codex authentication or history. Prepare administration first, and start the bot only after setup and login are complete. Preserve existing data and report validation and rollback instructions.

See [the operator introduction](docs/deploy.md) for what to prepare and how to use the installed system. The supported path uses Linux, an official QQ bot, native Codex, Bubblewrap, Python 3.11+, and user-level systemd for persistent services.

**The admin UI includes its own login and first-time setup. It does not require personal-dashboard.** An existing authenticated proxy remains an optional integration. Configuration creation does not start the bot.

## Features

- **Participation modes:** respond to mentions, assess help requests, join casual conversation, or reply to every message; configurable per group.
- **Conversation and task continuity:** recognize additions, revisions, and cancellations; batch suitable short replies and report progress on longer work.
- **Personality and memory:** Identity, Soul, group profiles, and categorized memory with review and revision tools.
- **Media and tools:** images, QQ voice transcripts, stickers, group documents, isolated workspaces, and separately authorized browser and integration tools.
- **Built-in admin:** configuration, separate bot login, group registration, service status, and data management.

First-time setup defaults to mention-only replies with private conversations and the browser disabled. Enable the browser during setup or later in the admin UI when its prerequisites are ready. Adjust modes and features in the admin UI. Participation decisions and message display depend on model behavior and QQ platform capabilities, so evaluate them in your own groups.

Received messages, including those left unanswered, are archived privately; reducing model context does not delete that archive. Members share their group's workspace. Enabling the browser prepares a separate runtime, with download, system-library, and disk requirements covered in the deployment runbook.

## Documentation

| Task | Guide |
| --- | --- |
| Prepare accounts and understand deployment | [Operator introduction](docs/deploy.md) |
| Have an agent install, verify, and update | [Agent deployment runbook](docs/agent-deployment.md) |
| Manage login, personality, groups, and services | [Admin](docs/admin.md) |
| Understand participation modes and group behavior | [Group runtime](docs/social-pilot.md) |
| Understand context, history, batching, and recovery | [Message context](docs/message-context.md) |
| Configure memory, tasks, stickers, and documents | [Memory](docs/memory.md) · [Long tasks](docs/long-tasks.md) · [Stickers](docs/stickers.md) · [Group documents](docs/group-documents.md) |
| Set up isolated tools and integrations | [Native runtime](docs/native-runtime.md) · [Browser](docs/native-browser.md) · [Scoped integrations](docs/scoped-integrations.md) |
| Develop and validate changes | [Development](docs/development.md) |

Most operator guides are currently in Chinese. [Design records](docs/continuation-design.md) explain historical decisions; use the deployment and admin guides for current operation.

## Development and upstream

Build the application and its two runtime helpers:

```sh
cargo build --release --locked --bin clawmingle --bin clawmingle-hook-proxy --bin clawmingle-hook-input
```

Use the `clawmingle` executable. The default `cargo run` target remains upstream `codex-claw`.

The upstream MIT license and attribution are preserved in [LICENSE](LICENSE). Complete upstream README archives are available in [Chinese](docs/upstream/README.md) and [English](docs/upstream/README_en.md). Their personal-session import and `codex-claw` deployment instructions describe the upstream product; use this project's deployment guide for ClawMingle.
