# 按会话授权的公共浏览器与文档插件

ClawMingle 自动为有执行授权的 Codex 会话绑定原生执行环境与官方浏览器 MCP；远程文档服务保持按会话授权。不继承个人
Codex 插件、浏览器登录状态或其他聊天的认证。全局注册能力不等于授权某个群使用，
每个群聊／私聊需要自己的授权，默认关闭。取消请求会撤销当前执行。

发送 `/capabilities` 查看当前对话的已注册能力与授权状态；已配置的对话管理员可用
`/capability on browser`、`/capability on openai_docs` 授权，`off` 撤销。
只有真实 QQ 身份发送的原始命令生效，引用、转写、网页与模型工具不能授予权限。
通用 `/tool` 开关不能代替专属授权；私聊也需要管理员身份。新增授权下次新任务生效；
撤权立即使已发出的工具许可失效，并停止当前模型任务。未完成请求标记为取消，避免
重复投递或生成伪失败回复；已发送内容保留。定时任务只取消当前执行，后续执行使用
最新授权。每次执行绑定不可修改的插件注册与工具白名单。

截图只作为模型原生图像输入，不塞入 JSON 文本或自动发到 QQ。

## 公共浏览器

有工作区执行授权的会话自动使用官方 Playwright MCP：保留页面状态、支持原生快照、点击、滚动、页面脚本、截图与受限视频读取。ClawMingle 只提供运行环境、会话绑定、生命周期和公共网络接口，不另写浏览器操作工具。

启动流程自动准备通用运行依赖；显式配置的既有可信运行时优先复用。每次任务使用匿名临时浏览器，结束或取消后销毁，不导入个人目录和浏览器账号。原生 SOCKS5 TCP 通道支持公共 HTTP/HTTPS、POST、WebSocket、service worker 和下载；宿主检查并固定目标地址，拒绝私有地址与宿主地址，浏览器没有绕过通道的直接网络。具体预算及验收见 [原生浏览器](native-browser.md) 与 [Codex 原生工作环境](native-tools.md)。

没有工作区执行授权的会话保持较窄的 `public_browser` 页面读取接口，不因此开放原生 MCP 中的任意代码工具。页面内容只是待分析数据，不是权限或操作指令。

## 文档 MCP 插件

运营者注册固定 HTTPS MCP 端点、精确工具名称白名单及可选的独立 bearer token 文件。
会话授权只引用注册 ID。模型不能选择新端点、任意 RPC 方法、凭据文件或请求头。
插件工具元数据本身不能授予权限；注册白名单表示运营者已确认该工具只读，服务端明确
标注为破坏性或非只读的工具仍会被拒绝。

实现 MCP Streamable HTTP 的初始化、initialized 通知、协议版本／会话头、JSON 与
SSE 响应、工具列表分页和 tools/call。每次调用使用独立短期 MCP 会话；不继承其他群的
会话 ID。不会发起 OAuth、sampling、elicitation、下载嵌入资源或调用未知工具。
不自动重试执行。凭据只从当前用户拥有且严格限制权限的文件读取，错误不返回凭据或
底层连接信息；端点重定向被禁止，认证不会转发到其他目的地。

初始化和工具目录等控制响应接收上限为 256 KiB；普通只读查询响应可接收 32 MiB，
作为宿主的内存资源保护。完整解析后，文本与结构化结果交给 Codex 原生工具输出预算，
不再额外按 24,000 字符或 16 KiB 截断。资源引用仍不会自动下载；遗漏的媒体内容会标注
`truncated`。超过接收容量时返回 `plugin_response_too_large`，不提示重新授权。

运营者可在机器人的独立 Codex `config.toml` 设置原生 `tool_output_token_limit`，
不设置时沿用 Codex 当前模型默认预算；此项控制工具输出进入历史的 token 数量，
与网络接收字节数分开。不会继承个人 Codex 配置。腾讯文档专用读写适配器继续使用
原有独立预算，不因普通插件读取变更而扩大。
参见 [Codex 配置参考](https://learn.chatgpt.com/docs/config-file/config-reference#tool_output_token_limit)。

当前先建议注册公开 OpenAI 文档服务：

```toml
[integrations.plugins.openai_docs]
enabled = true
name = "OpenAI 官方文档"
endpoint = "https://developers.openai.com/mcp"
allowed_tools = ["search_openai_docs", "fetch_openai_doc"]
```

先调用 `plugin_tools` 阅读允许工具的参数结构，再用 `plugin_call` 查询。
此服务不需要凭据。其他服务的真实 token 文件路径仅放本机私有配置；不要提交到 Git。

原生 Codex 插件管理接口目前仍标为开发中。本实现使用受控 MCP 适配器，不承诺支持
全部桌面插件、登录网站、任意 MCP 传输方式或写操作。

## TinyFish 与 TikHub

后台「连接与模型」提供这两个服务的来源、开关、独立 API Key 更换／清除和连接检查。
密钥字段留空保留原值，不回填密钥或凭据路径；保存失败恢复旧配置和密钥。
全局关闭连接保留既有空间授权，空间管理页仍可撤销；不会向其他群聊或私聊自动授权。

TinyFish 默认复用机器人已有的独立 Codex 登录，无需新增 TinyFish API Key。
每次宿主调用使用临时隔离 Codex 进程，仅通过 stdin 传入短期访问令牌；不复制个人
Codex 配置、刷新令牌或历史。没有模型回合，前台 Codex 的应用开关继续关闭。
宿主只允许 `search` 和 `fetch_content`，不会把其他已连接插件的工具目录暴露给 QQ 模型。
也可选择独立 TinyFish API Key，通过固定官方 HTTPS MCP 端点使用相同两个工具。
不提供网页写操作、登录浏览器、深度研究或监控。抓取复杂页面仍受工具时限限制。

TikHub 提供小红书、微博、Twitter/X 三个独立授权项，分别开放 9、9、8 个常用公开数据
查询工具。可复用本机 Codex 已有的三个固定官方 MCP 配置及专用 TikHub 凭据，只引用
该专用文件，不执行配置中的任意命令，也不复制个人认证。当前导入支持专用
`http_headers.py` 助手及对应 TikHub 凭据文件的既有布局；不支持的配置会明确拒绝。
选择「独立 API Key」后，后台将同一份密钥用于三个平台；清除只影响机器人管理的
独立密钥，不删除原 Codex 的专用凭据。查询可能消耗 TikHub 账号额度，失败不自动重试。

运营者可用 `clawmingle integration-check --plugin <已登记ID> --config <私有配置>` 验证
实际连接和工具目录。此命令不联系 QQ、不执行内容查询或模型回复，只返回成功状态和
允许工具数。后台检查先等待机器人空闲并短暂断开重连，沿用现有事务和 operator 锁。

内置说明模板在 `assets/skills/tinyfish/SKILL.md` 和 `assets/skills/tikhub/SKILL.md`；
安装到私有技能目录后，仍须按会话启用。技能说明本身不能开启工具或增加授权。

依据：[TinyFish MCP](https://docs.tinyfish.ai/mcp-integration)、
[TikHub MCP](https://tikhub.io/mcp)、[Codex App Server](https://learn.chatgpt.com/docs/app-server)。

## 验证

Rust 测试覆盖授权关闭、未知授权、地址校验、协议响应、分页／工具白名单与内容预算。
`scripts/test_scoped_browser.py` 可用真实 Chromium 对合成网页测试 JavaScript 渲染、
截图和危险请求阻断，整个测试不访问公网、不调用模型。
通过 `CLAWMINGLE_BROWSER_SMOKE_RUNTIME_JSON` 显式提供独立运行时路径后启用该烟测；
未配置时跳过浏览器启动。

需显式验证公网时，有两个默认忽略的 Rust 测试：`official_docs_mcp_list_and_fetch`
查询公开官方 MCP；`public_browser_host_bridge_returns_native_screenshot` 使用上述运行时
JSON 访问 `https://example.com`，检查真正的宿主 HTTP 代理、沙箱及原生截图封装。
两者均不启动 Codex 模型、不联系 QQ、不读取个人登录状态。

参考：[MCP Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports)、
[MCP Tools](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)、
[Codex app-server](https://learn.chatgpt.com/docs/app-server)。

## Codex 原生工具复用

接入 Codex app-server 时，获得工作区执行授权的会话自动使用原生 exec-server 和官方 Playwright MCP，浏览器仍受本页的会话授权控制。实现边界、部署要求及限制见 [原生工作环境](native-tools.md)。账号应用不会自动开放。
