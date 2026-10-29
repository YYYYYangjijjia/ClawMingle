# ClawMingle 开发约定

当前阶段已进入真实群聊/私聊与常驻服务开发，管理入口见[工作台](admin.md)。下文保留初始仓库准备阶段的边界，后续明确授权与已实施状态以继续开发文档第18节为准。

ClawMingle 在 CodexClaw 的 Rust 底座上渐进开发，保留上游许可、源码结构和既有指导。
本文件记录项目约定；机器专属路径、操作记录和排障材料放在被忽略的
`.local/DEVELOPMENT.md`，凭据继续使用受保护的应用配置位置。

后续产品设计与开发接续入口见[继续开发文档：社交决策、请求关联与首测准备](continuation-design.md)。
该文档区分已确认决策、建议方案、待讨论事项和待验证能力；本文继续负责开发与 Git 保护约定。

用户已开启功能开发；已实现的文字群聊首版及运行方法见[群聊开发版](social-pilot.md)。
原有仓库准备边界不代表本次开发授权启动常驻服务或生产部署。

## 已确认的产品起点

- 已创建官方 QQ 机器人，尚未接入真实模型；首个试点是所有者的两人测试群。
- Participation Controller 首先使用 Jev，优先贯通流程，准确率评估后置。
- 长期记忆先采用确认后保存。
- 本次仓库准备不包含产品功能、真实模型接入、服务启动或生产部署。
- QQ 官方私聊提供专用流式接口，群聊发送接口明确不支持流式参数；群聊先生成后发送。
  当前代码的私聊分段消息不是该专用流式接口。接口依据见
  [私聊流式消息](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_stream_messages.post.html)
  和[发送群消息](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)。

## 本机开发边界

开发环境由根目录 `environment.dev.yml` 声明，包含 Rust/C 编译工具、Python 3.12 和 Node.js 24。
`scripts/dev-checks.sh` 同时运行 Rust、Python 与 Node 的离线测试；Node 是必需的开发依赖，
不能用浏览器私有运行目录中的 Node 自动替代，也不应跳过 QQ 文档登录测试。
新机器可使用 `mamba env create -f environment.dev.yml` 重建环境，再运行
`mamba run -n clawmingle-dev bash scripts/dev-checks.sh`。此步骤不启动 Bot 或真实模型。

- 使用既有 Cargo 工具链，按改动范围运行检查；文档与 Git 规则变更不启动 Rust 服务。
- 运行测试前确认使用临时目录和虚构凭据；真实 QQ、模型和 Codex 集成检查单独安排。
- 不继承个人 Codex 认证、Skills 或历史会话来运行群聊测试；当前上游启动默认行为尚未改造。
- 应用开发不得影响机器上的 VPN、SSH、共享入口、Dashboard 和其他项目。
  不默认执行部署、自更新、重启、端口开放或后台常驻任务。
- 后续开发监听仅绑定 loopback；任务执行使用独立身份、目录和沙箱。
- 保持变更可审查，不自动提交或推送；上线和服务管理需要单独任务授权。

## 独立 QQ 收发测试

`scripts/qq_smoke.py` 是一次性群聊连接诊断，不启动 Rust 应用、Codex、Jev、
记忆或调度器。它不修改现有产品行为，也不开放监听端口。诊断使用隔离 Python
环境；主应用仍使用 Rust/Cargo。本机配置默认为 `~/.config/clawmingle/qq.env`：

```dotenv
QQ_APP_ID='YOUR_APP_ID'
QQ_APP_SECRET='YOUR_APP_SECRET'
```

目录与文件须分别为 `0700`、`0600`，属于当前用户。不要复制个人 Codex 配置。
AppID 和 AppSecret 来自 [QQ 开放平台](https://bot.q.qq.com/wiki/develop/api-v2/)。
首次测试前在平台将机器人加入自己的测试群，并确认测试成员和群的使用权限。

```bash
python3 -m venv .local/qq-smoke-venv
.local/qq-smoke-venv/bin/python -m pip install -r scripts/requirements-qq-smoke.txt
python3 -B scripts/test_qq_smoke.py
.local/qq-smoke-venv/bin/python -B scripts/qq_smoke.py --check-config
.local/qq-smoke-venv/bin/python -B scripts/qq_smoke.py --seconds 300
```

本地配置检查不访问网络；连接命令会请求 QQ 凭据和网关，并短暂连接 WebSocket。
登录后仅在所有者的测试群中真正 @ 机器人，发送终端展示的本次 `/clawmingle-ping`
口令。程序只接受匹配口令且具有真实 @ 证据的群事件，回复一条固定文本后
关闭连接。普通群消息和私聊不会触发回复。默认最多等待五分钟，上限十分钟；
退出后口令失效，没有后台驻留、自动重连或自动重发。若平台要求来源地址白名单，
由所有者在平台配置，不将服务器地址写入仓库或聊天。

首次登记测试群可使用 `--register-group /absolute/private/path/social.toml`，替代固定回复测试。
它用独立的一次性口令，仅把匹配群的 OpenID 写入既有私有配置的空白名单并退出；
不发送消息、不调用模型、不覆盖其他群。具体步骤见[群聊开发版](social-pilot.md)。

程序不记录正文、群/成员标识、凭据或原始错误响应，不下载附件。
真实 @ 证据包括专用事件或全量事件中的服务器自指标记/已知机器人结构化身份匹配；
不将正文中的名字当作 @。登记诊断只展示固定状态和不含身份值的布尔比较。
发送超时或缺少确认视为结果未知，需先查看 QQ，避免重复发送。
此测试只验证连接、群 @ 事件与被动文本发送，不能据此宣称模型或完整群聊流程已接通。
协议依据：[事件订阅与连接](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/interface-framework/event-emit.html)、
[群 @ 事件](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_at_message_create.html)、
[群消息发送](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)。

## 文件与敏感信息

可跟踪：源码、Cargo 锁文件、工具、通用说明、只含占位值的配置模板。
不可跟踪：真实配置、API/QQ 凭据、密码、私钥、认证文件、服务器 IP/私有域名、
可用连接 URI、个人 Codex 状态、聊天/记忆/附件、日志、数据库、导出和备份。

- 本地 Bot 配置使用被忽略的 `codexclaw.toml`，或仓库外配置配合 `CODEX_CLAW_CONFIG`。
  配置中的运行数据路径也必须指向被忽略的位置或仓库外。
- 环境文件和本机说明分别使用 `.env`、`.local/`；新配置应在创建前验证忽略规则。
- 模板只使用 `YOUR_APP_SECRET` 等显式占位值。示例地址使用 `localhost`、loopback，
  或 RFC 文档网段；不要使用真实公网或私网服务器地址。
- 密钥文件限制为仅所有者可读，目录仅所有者可访问；不要把密钥放进命令参数、
  聊天提示、提交说明、普通日志或报告。
- 不在截图中展示后台密钥或服务器连接信息。文本检查无法审查图片中的内容。
- `.gitignore` 不会让已提交的秘密消失。发现泄露时先停止推送，以脱敏位置报告；
  再安排凭据撤销/轮换。历史清理须另行确认，不能自行强推。

## 提交与推送前检查

仓库提供无第三方依赖的基础检查，输出文件位置、规则和行号，不输出匹配值：

```bash
python3 -B scripts/check_sensitive_data.py --tracked
python3 -B scripts/check_sensitive_data.py --staged
python3 -B scripts/check_sensitive_data.py --history HEAD
python3 -B scripts/test_sensitive_data.py
```

`--tracked` 检查工作树中已跟踪文件；`--staged` 读取真正将提交的暂存区内容；
`--history` 检查指定分支可达的全部提交、文件路径与提交说明，能发现已在后续删除的敏感内容。
未跟踪的新工具需在审查后加入暂存区，或由其测试单独验证。

本地启用仓库 hooks：

```bash
git config --local core.hooksPath .githooks
```

配置前检查是否存在已有 hook 路径或活动脚本；有则先合并流程，不直接覆盖。
这项配置只对当前 clone 生效，新 clone 需要重新启用。
`pre-commit` 检查暂存区；`pre-push` 根据 Git 传入的引用检查待推送历史。
两者失败时停止操作并处理原因，不使用 `--no-verify` 绕过。

检查器拦截常见凭据、私钥标记、完整 UUID、非示例 IP、含密码连接 URL、
敏感文件名和运行数据路径。它是基础防误提交措施，不保证识别任意格式的秘密。
私有主机名、截图、无引号的 YAML 凭据值、任意位置的无标签密码等仍需人工审查。
可在被忽略的 `.local/sensitive-values.txt` 每行配置一个必须禁止出现的精确值，
用于补充私有主机名等规则；该文件本身属于敏感文件，权限设为 `0600`，不要提交或展示。
禁止值文件若无法读取，检查会失败，不会静默跳过。

手动确认候选文件：

```bash
git check-ignore -v codexclaw.toml .env .local/DEVELOPMENT.md data/session/state.json
git diff --cached --stat
```

没有通过审查的文件不应加入暂存区。`git add -f` 同样不能代替审查。
