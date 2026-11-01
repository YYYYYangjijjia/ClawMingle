# ClawMingle

[English](README_en.md) · [部署入口](docs/deploy.md) · [Agent 部署手册](docs/agent-deployment.md) · [管理台](docs/admin.md)

ClawMingle 是接入 QQ 官方机器人平台的 AI 群聊参与者。**Jev 判断何时接话、消息说给谁，以及是否在补充已有任务；Codex 负责回复和执行任务。** 应用处理连续短消息的合并、上下文选择、取消、投递和限流恢复。

它基于 [CodexClaw](https://github.com/Rhapsody0x1/CodexClaw) 的 Rust 底座开发，重点是多人群聊中的参与时机和任务连续性。机器人使用独立的 Codex 认证与工作目录；每个群可以设置自己的互动风格、记忆和工具权限。

## 部署：让 Agent 完成环境准备

推荐把仓库交给能操作目标 Linux 机器的编程 Agent，由它按[部署手册](docs/agent-deployment.md)完成检查、构建和安装。你负责准备 QQ 机器人，并在本机或 SSH 隧道中的管理网页完成密码、凭据和群登记；**不需要把密钥发给 Agent**。

可以这样交代：

> 请在这台 Linux 机器部署 ClawMingle，仓库是 https://github.com/YYYYYangjijjia/ClawMingle 。先读仓库指导和 docs/agent-deployment.md，检查依赖与已有安装，按手册完成构建、安装和验证。管理台只允许本机或 SSH 隧道访问。QQ 密钥和机器人账号由我在网页配置；不要复用个人 Codex 的认证或历史。首次配置完成前只准备管理台，配置和登录完成后再启动机器人。保留已有数据，并说明验证结果与回滚方式。

你需要准备什么、部署后如何操作，见[用户部署说明](docs/deploy.md)。当前支持路径为 Linux、官方 QQ 机器人、原生 Codex、Bubblewrap 和 Python 3.11+；常驻运行使用 systemd 用户服务。

**管理台自带登录和首次初始化，不依赖 personal-dashboard。** 已有鉴权代理可以继续作为可选接入方式。安装与初始化是独立步骤；创建配置不会自动启动机器人。

## 能做什么

- **按场合参与**：只在被 @ 时回应、判断帮助需求、参与日常聊天，或逐条回应；群聊模式可单独设置。
- **连续聊天与任务**：识别追加、修订和取消，合并适合共享输入的短回复，长任务按进展反馈。
- **个性与记忆**：Identity、Soul、每群档案和可查阅、修订的分类记忆。
- **多种消息与工具**：图片、平台语音转写、表情包、群文档、独立工作区及按会话授权的浏览器和扩展。
- **自带管理台**：配置、机器人独立登录、群登记、服务状态和数据管理。

首次配置默认仅 @ 回应，关闭私聊和浏览器。浏览器可在首次设置时勾选，或准备好依赖后在管理台开启；模式和其他功能也可在管理台调整。参与判断和消息展示仍受模型与 QQ 平台能力影响，真实群聊效果需要按自己的场景验证。

机器人接收的消息（包括未回复的消息）会保存到本地私有存档；裁剪模型上下文不会删除存档。群成员共享该群工作区，不能把它当作成员间私有空间。启用浏览器会准备独立运行依赖，其下载、系统库与磁盘要求见部署手册。

## 使用文档

| 想做什么 | 文档 |
| --- | --- |
| 了解部署流程和需要准备的账号 | [用户部署说明](docs/deploy.md) |
| 让 Agent 检查、安装、验证、更新 | [Agent 部署手册](docs/agent-deployment.md) |
| 管理配置、登录、人格、群档案和服务 | [管理台](docs/admin.md) |
| 了解参与模式、命令和群聊行为 | [群聊运行说明](docs/social-pilot.md) |
| 了解上下文、历史存档、合批与恢复 | [消息与上下文](docs/message-context.md) |
| 管理记忆、长任务、表情和文档 | [记忆](docs/memory.md) · [长任务](docs/long-tasks.md) · [表情](docs/stickers.md) · [群文档](docs/group-documents.md) |
| 配置隔离执行与扩展能力 | [原生环境](docs/native-runtime.md) · [浏览器](docs/native-browser.md) · [扩展授权](docs/scoped-integrations.md) |
| 修改和验证代码 | [开发约定](docs/development.md) |

[设计记录](docs/continuation-design.md)供开发者理解历史决策；安装和日常使用从上述操作指南进入。

## 开发与上游

构建本项目入口及两个运行辅助程序：

```sh
cargo build --release --locked --bin clawmingle --bin clawmingle-hook-proxy --bin clawmingle-hook-input
```

`clawmingle` 是本项目的群聊入口；默认 `cargo run` 仍会启动上游 `codex-claw`。

保留 CodexClaw 的 MIT 许可和署名，见 [LICENSE](LICENSE)。上游 README 完整存档于[中文](docs/upstream/README.md)和[English](docs/upstream/README_en.md)，其中个人会话导入和 `codex-claw` 部署步骤属于上游产品。ClawMingle 的独立配置和运行方式以本项目部署指南为准。
