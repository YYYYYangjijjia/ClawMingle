# 原生 Hooks 与会话执行环境

ClawMingle 复用 Codex 的 Hooks 事件、匹配、信任检查、超时、异步调度和结果解释。
宿主只负责把经过授权的命令送进当前任务的原生执行环境。

## 为什么需要执行适配

核对的版本是 Codex `0.161.0`，对应官方源码提交
`979011409de0a60b52f179721948e65531d26144`。该版本的普通 command Hook
由 app-server 进程直接启动，并继承该进程捕获的环境；它还没有迁移到
exec-server。直接开启这些 Hooks 会使命令处在持有机器人登录信息的进程环境。

上游的 environment-scoped Hooks 目前只允许特定内置插件的 MCP 清理行为，
并不能据此让任意 `hooks.json` 命令在远端执行。
依据：[原生命令执行](https://github.com/openai/codex/blob/979011409de0a60b52f179721948e65531d26144/codex-rs/hooks/src/engine/command_runner.rs)、
[执行环境 Hooks 筛选](https://github.com/openai/codex/blob/979011409de0a60b52f179721948e65531d26144/codex-rs/core-plugins/src/executor_hooks.rs)。

## 接入边界

- 运行时只读取机器人独立 profile 中的 owner-only `hooks.json`。后台可发现本机
  command Hooks，用户显式导入后先保存为停用状态，再单独启用；不会复制个人
  Codex 的认证、完整 profile 或信任状态。见[后台迁移](codex-capabilities-admin.md)。
- 一份 `PreparedHooks` 只属于一个任务 worker。创建时生成随机命令标识，保存改写的
  原生 `hooks.json`，并创建私有 IPC socket。目录权限为 `0700`，文件和 socket 为 `0600`。
- authority 的 `/bin/sh` 是固定转发程序，只接受 `-lc clawmingle-hook:<标识>`。
  它不解释 shell、不启动命令、不读取 profile，也不把 authority 的环境变量发送出去。
- 原命令只存在于宿主绑定表。宿主把输入绑定到唯一的 `NativeEnvironmentRpc`；
  输入中的 `session_id`、`cwd`、`transcript_path` 不能选择另一个 scope 或执行器。
- exec-server 通过 `process/start` 启动固定输入适配器；后者在隔离环境中启动原命令，
  原样写入 JSON 并关闭脚本 stdin。stdout、stderr 和退出码返回原生 Hooks 引擎解释。
- 原生超时或取消会结束代理连接，宿主随即请求 `process/terminate`。
  任务结束时仍必须销毁整个任务 worker 和 exec-server PID namespace，覆盖子代理和
  脱离父进程的辅助进程。不能仅结束父 turn 后留下同一 worker 供另一 scope 使用。

`process/write` 在此版本没有 stdin-close 接口。固定输入适配器使用定长帧，
因此脚本读取直到 EOF 时也能正常完成；这不是重新实现 Hook 引擎。

## 宿主接线契约

1. 安全读取独立配置后调用 `PreparedHooks::prepare(source)`；无 Hooks 配置时不创建。
2. authority 新建 `/bin` 和 `/run/clawmingle-hooks`。将 `clawmingle-hook-proxy`
   只读挂到 `/bin/sh`；把 `socket_directory()` 只读挂到
   `/run/clawmingle-hooks`，把 `hooks_file()` 只读挂到 `/state/codex/hooks.json`。
   这些位置不得出现在 executor 的挂载中。
3. 在 executor 绑定 workspace 前，把 `clawmingle-hook-input` 只读挂到
   `/runtime/clawmingle-hook-input`。它只出现在 executor 中。
4. 环境连接就绪后调用 `hooks.attach_executor(environment.host_rpc())`，仅允许绑定一次。
   在开始 native thread/turn 前完成绑定；不能在收到 Hook 请求时再启动环境。
5. 使用原生 `hooks/list` 读取改写文件的元数据，再调用
   `trust_state(catalog, "/state/codex/hooks.json")`。返回值是精确的 `hooks.state`
   对象，应作为此任务 worker 的 session config 覆盖。返回的 hash 来自原生引擎，
   宿主不自行计算；元数据缺失、来源不同或存在警告时，不开放 Hooks。
6. 仅对此任务 worker 开启 `features.hooks`。普通共享 authority 继续关闭 Hooks。
   任务收尾先执行正常的 native thread 生命周期，再关闭 Hooks broker、worker 和执行环境。

IPC 请求由固定 magic、随机 binding 标识、输入长度和原始 JSON 组成；响应只包含
退出码与两个独立输出流。socket 路径、执行路径、环境和命令均不能由模型或 IPC 请求指定。
原生进程桥为宿主 RPC 分配独立 ID，拦截对应回复和输出，避免混入模型 shell 会话。

## 构建与部署文件

新增两个 Cargo 自动发现的二进制目标，不增加第三方依赖：

```bash
cargo build --release --bin clawmingle-hook-proxy --bin clawmingle-hook-input
```

部署必须同时携带这两个产物，放在主程序同目录，且不可被组或其他用户写入。
当前自动挂载覆盖 Debian 系 x86_64/aarch64 的 loader、`libc.so.6`、`libgcc_s.so.1`
路径；不会为了启动代理挂入完整系统命令目录。其他布局需补充经验证的库路径，
或使用兼容的静态产物；缺少依赖时明确失败。executor 已有只读系统 runtime。

## 当前限制与验证

当前传输接收原生 command Hooks；`mcp_tool`、prompt/agent、Windows 命令覆写或额外
环境注入字段会明确拒绝。现有匹配规则和 native 输出协议保持原生处理。
约束为每份配置 32 个 handler、16 个并发调用、输入 1 MiB、输出合计 2 MiB、
显式 timeout 不超过 600 秒。最终超时仍由 native engine 控制。

原生输入里的 `transcript_path` 保留原值，但执行器不会因此获得 authority 的会话文件
或凭据目录。依赖读取该路径的 Hooks 不能宣称可直接迁移；应先提供受授权的 scope 数据。
Hooks 也不会自动取得网络、个人账号应用或其他会话的访问权。

叶模块测试覆盖命令改写、来源/信任检查、输入边界、RPC 分流、输出连续读取和取消终止。
独立输入适配器测试确认：即使传输 stdin 仍保持打开，脚本也能收到 EOF；原始输入字节、
stderr 和退出码分别保留。`social_native_hooks_smoke` 使用离线 provider 和真实 Codex
引擎，已验证 SessionStart、PostToolUse、Stop、SessionEnd、Interrupt、上下文返回、
两个 scope 的隔离，以及取消后辅助进程停止；不使用个人认证或真实模型请求。
