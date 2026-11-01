# Contributing to ClawMingle

[简体中文](CONTRIBUTING.md) · <b>English</b>

Thank you for your interest in ClawMingle! Bug reports, feedback, documentation improvements and code contributions are all welcome. ClawMingle is built on [CodexClaw](https://github.com/Rhapsody0x1/CodexClaw); the upstream contributing guide is archived at [docs/upstream/CONTRIBUTING_en.md](docs/upstream/CONTRIBUTING_en.md).

## Before you start

- **Reporting issues**: use the issue templates. Redact screenshots and logs: never include an AppSecret, API key, cookie, group or member OpenID, or original chat content.
- **Larger changes**, such as new features, a new chat-platform or agent-backend adapter, or changes to persisted formats: please open an issue to discuss the direction before writing code.
- **Project conventions**: [`AGENTS.md`](AGENTS.md) describes the module map, layering rules and coding conventions, and [`docs/development.md`](docs/development.md) (Chinese) covers development boundaries and pre-commit checks. Both apply to human contributors and AI coding assistants.

## Development environment

The environment is declared in [`environment.dev.yml`](environment.dev.yml) and includes Rust, a C toolchain, Python 3.12 and Node.js 24:

```sh
mamba env create -f environment.dev.yml
git config --local core.hooksPath .githooks   # enables the sensitive-data checks before commit and push
```

You can use an existing Rust, Python and Node setup instead of conda, provided the versions meet these requirements.

## Building and checks

```sh
# Full offline checks: Rust formatting, Clippy and tests, plus the Python and Node tests
mamba run -n clawmingle-dev bash scripts/dev-checks.sh
```

For Rust-only changes, at least run:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings   # must have zero warnings
cargo test --all-targets
```

These checks do **not** start the bot or connect to the real QQ or model services. Integration tests that need a real Codex binary are ignored by default and run manually only when needed.

## Code conventions

- **Layering**: follow the dependency layers in `AGENTS.md`. Lower layers (such as `codex/`) must not depend on higher layers (such as `social/`); inject what they need through registration interfaces instead.
- **Persisted compatibility**: add `#[serde(default)]` to new fields on persisted types. The snapshot tests in `src/model/wire_compat.rs` guard the on-disk format.
- **Test placement**: tests live next to the code in `#[cfg(test)] mod tests`. Use `#[tokio::test]` for async tests, `wiremock` to mock network calls and `tempfile` for temporary files.
- **Paths**: paths written to disk must be absolute. Use temporary directories in tests, and do not rely on the `~` in default configuration.
- **Fail clearly**: do not hide errors behind silent fallbacks, and never loosen isolation, permissions or authentication just to make a check pass.
- **i18n**: user-visible text goes through the `t!` macro; update both `locales/en.yml` and `locales/zh.yml`.

## Commits and pull requests

- Follow [Conventional Commits](https://www.conventionalcommits.org/), for example `feat(social): ...`, `fix(admin): ...`, `docs: ...`.
- Keep each commit to one logical change, and commit pure refactors separately from behavior changes.
- In the PR description, describe the behavior change, list the checks you actually ran and link related issues. Screenshots are only needed for UI or message-format changes.
- When behavior changes, update the relevant documents in `docs/`.

## Security and privacy

- Never commit real credentials, server addresses, identifiers, chat logs, runtime data (`data/`, backups, logs) or personal Codex authentication.
- Use obvious placeholders such as `YOUR_HOST` in examples.
- `.gitignore` does not protect files that are already tracked or anything in history. Run `python3 -B scripts/check_sensitive_data.py --staged` before committing; the hooks run this check for you.
- If you find a security issue, do not post exploit details in a public issue; contact the maintainer through an issue first.

## License

ClawMingle is released under the [MIT License](LICENSE). By contributing, you agree to license your contributions under the same terms.
