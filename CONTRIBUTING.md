# 参与贡献 ClawMingle

<b>简体中文</b> · [English](CONTRIBUTING_en.md)

感谢你愿意参与 ClawMingle！这里欢迎 Bug 报告、使用反馈、文档改进和代码贡献。ClawMingle 基于 [CodexClaw](https://github.com/Rhapsody0x1/CodexClaw) 开发，上游原有的贡献指南存档在 [docs/upstream/CONTRIBUTING.md](docs/upstream/CONTRIBUTING.md)。

## 开始之前

- **报告问题**：请使用 Issue 模板。截图和日志务必脱敏，不要包含 AppSecret、API Key、Cookie、群或成员 OpenID、聊天原文。
- **较大的改动**：比如新功能、新的聊天平台或 Agent 后端适配、改变持久化格式，请先开 Issue 讨论方向，再动手写代码。
- **项目约定**：仓库根目录的 [`AGENTS.md`](AGENTS.md) 描述模块结构、分层规则和编码约定，[`docs/development.md`](docs/development.md) 描述开发边界与提交前检查。人类贡献者和 AI 编程助手都应遵守。

## 开发环境

开发环境由 [`environment.dev.yml`](environment.dev.yml) 声明，包括 Rust、C 编译工具链、Python 3.12 和 Node.js 24：

```sh
mamba env create -f environment.dev.yml
git config --local core.hooksPath .githooks   # 启用提交与推送前的敏感数据检查
```

如果你已经有可用的 Rust/Python/Node 环境，也可以不用 conda，但版本需要满足上述要求。

## 构建与检查

```sh
# 完整的离线检查：Rust 格式、Clippy、测试，以及 Python 与 Node 测试
mamba run -n clawmingle-dev bash scripts/dev-checks.sh
```

只改 Rust 代码时，至少运行：

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings   # 必须 0 警告
cargo test --all-targets
```

这些检查**不会**启动机器人，也不会连接真实的 QQ 或模型服务。需要真实 Codex 的集成测试默认被忽略，只在明确需要时手动运行。

## 代码约定

- **分层**：按 `AGENTS.md` 中的依赖分层组织代码。底层模块（如 `codex/`）不能反向依赖上层模块（如 `social/`），需要时通过注册接口注入。
- **持久化兼容**：持久化类型新增的字段要加 `#[serde(default)]`。`src/model/wire_compat.rs` 中的快照测试负责保护磁盘格式。
- **测试位置**：测试写在源文件旁的 `#[cfg(test)] mod tests` 中。异步测试用 `#[tokio::test]`，网络调用用 `wiremock` 模拟，临时文件用 `tempfile`。
- **路径**：写盘路径必须是绝对路径。测试中使用临时目录，不要依赖默认配置里的 `~`。
- **失败要明确**：不要用静默回退掩盖错误；也不要为了让检查通过而放宽隔离、权限或认证。
- **国际化**：用户可见的文本使用 `t!` 宏，并同步更新 `locales/en.yml` 和 `locales/zh.yml`。

## 提交与 Pull Request

- 提交信息遵循 [Conventional Commits](https://www.conventionalcommits.org/zh-hans/)，例如 `feat(social): ...`、`fix(admin): ...`、`docs: ...`。
- 每个提交只做一件事。纯重构和行为修改分开提交。
- PR 描述需要说明行为变化、实际运行过的检查命令，以及关联的 Issue。只有改动界面或消息格式时才需要截图。
- 行为变化要同步更新 `docs/` 中的相关文档。

## 安全与隐私

- 不要提交任何真实凭据、服务器地址、身份标识、聊天记录、运行数据（`data/`、备份、日志）或个人 Codex 认证。
- 示例一律使用明显的占位符，例如 `YOUR_HOST`。
- `.gitignore` 不能保护已跟踪的文件和历史记录。提交前请运行 `python3 -B scripts/check_sensitive_data.py --staged`，钩子会自动执行这一检查。
- 发现安全问题时，请不要在公开 Issue 中贴出利用细节，先通过 Issue 联系维护者。

## 许可

ClawMingle 以 [MIT License](LICENSE) 发布。提交贡献即表示你同意以相同许可授权你的贡献。
