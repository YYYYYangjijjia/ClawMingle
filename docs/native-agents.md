# 原生多代理边界

以下契约核对 Codex `rust-v0.161.0`，不把其他版本行为当成保证。
ClawMingle 使用原生 V2 多代理工具，子任务调度、消息、等待、结果回传和模型调用由 Codex 实现。
应用层只管理当前社交任务的权限、执行环境和进程生命周期。

## 按任务传递上下文

启用原生执行的社交任务会配置 V2 `usage_hint_text` 和
`subagent_developer_instructions`。前者追加到原生 spawn 工具说明，后者提供
子任务工作协议；保留 Codex 自带角色提示、调度和结果交付机制。

- 普通闲聊和短问答由主代理直接完成。大量调查、文件分析或执行细节适合拆成
  有明确交付标准的子任务，避免父子重复查同一批资料。
- 每次 spawn 明确选择 `fork_turns`，优先 `none`。任务说明只携带目标、相关
  事实/约束/纠正、已授权动作、可访问材料和完成标准。不会把群历史、完整记忆、
  人设或图片 base64 复制成任务说明。依赖原生动态工具或紧邻历史时才选择必要的
  最近 N 轮；全量 fork 需要任务上的具体理由。N 不是 token 或消息数量，单轮也
  可能很大。现有全量 fork 本身会过滤部分工具过程，也不等于完整原始日志复制。
- 子代理保留调查过程，必要时写入各自的工作空间产物；交接返回结论、关键证据或
  文件位置、实际完成的动作/验证与未决问题。主代理按需读取详细章节，负责统一
  进展反馈和 QQ 最终交付。同一执行中的续作使用原生 followup，只追加新要求。

这是模型调用偏好和交接协议，**不是硬改原生默认值、强制截断或自动总结器**。
0.161 没有全局默认 `fork_turns=none` 的配置；成功结果也没有应用层的自动摘要上限。
模型仍可在必要时继承历史。不能把主代理上下文减少等同于所有代理合计 token
减少，也不能据此声称消除了 429。真实评估应分开检查主上下文峰值、全部代理
输入/输出累计消耗、耗时和任务质量；离线测试的字节对比不代表真实 token 用量。

空历史分支的能力差异由以下方式处理，不扩大权限：

1. 生成一个小型材料清单，仅列出当前已选 Skill 的执行环境路径，子代理按需读取
   `SKILL.md`；不复制 Skill 正文，不声称已恢复原生自动发现。
2. 仅把本次初始输入已经选中的图片复制到临时私有目录，以只读方式挂载为
   `/task-input/<本次唯一目录>/image-N.ext`，顺序与初始附件一致。主代理可把相关
   路径写入任务说明，子代理用原生 `view_image` 读取；不复制整个附件缓存，不再
   下载远程 URL，不把图片全部自动塞进子模型输入。沿用输入封装的三张、每张
   2 MiB 上限。目录随当前执行清理，新执行使用不同路径，旧路径不会误指向新图。
   后续追加或历史图片未出现在清单时，主代理须提供可访问材料或自行处理视觉任务。
3. QQ、记忆、群文档等动态接口优先由主代理获取必要事实并在核验结果后执行更新；
   必须直接使用这些接口的子任务选择有上下文继承的分支，并携带相关工具规则。
   读取 Skill 不会额外授予其中提及的工具；缺少资料/能力时明确报告缺口。

离线真实 Codex 测试比较全量/空历史分支的实际模型输入，检查空历史分支未收到
无关历史但保留任务说明、工作协议和材料路径；验证子代理能读取只读图片/Skill，
主代理只收到简短交接而未收到子代理工具长日志。取消和恢复测试同时验证旧执行
不再工作、临时图片路径不会在下次执行复用。测试不调用真实模型或 QQ。

## 环境与能力继承

原生 spawn 将父任务的 `EnvironmentSelection` 交给子线程；子线程复用同一个
executor handle，并继承父配置与 MCP 配置。因此父子命令均进入本次授权的
无网络工作空间，不能回落到持有凭据的 authority shell。

- V2 默认 `fork_turns="all"`。完整或最近 N 轮 fork 会保留动态工具与选中的
  Skill 根目录，子线程仍可调用经本任务授权的协调接口。
- `fork_turns="none"` 创建空历史线程。0.161 没有把父线程的 `dynamicTools`
  和 `selectedCapabilityRoots` 一起传入这一分支，因此子线程仍有原生执行工具
  和 MCP，但没有父线程动态工具及选中 Skill 的自动发现。已挂载 Skill 文件的
  文件系统可见性不等于已进入原生 Skill 目录。
- 这属于上游能力边界；不能把手工提示词注入称为原生继承，也不能通过对未知
  子线程放宽权限来补偿。

依据：[V2 spawn](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/tools/handlers/multi_agents_v2/spawn.rs)、
[原生 spawn/fork](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/agent/control/spawn.rs)、
[线程创建](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/thread_manager.rs)。

## 线程归属与权限

子线程动态工具请求中的 `threadId`、`turnId` 属于子线程。授权必须从可信原生
通知识别父子关系，再共享当前根任务的权限、工具预算和取消状态。

V1 发布 `item/completed` / `collabAgentToolCall` / `spawnAgent`，接收者在
`receiverThreadIds`。V2 发布 `item/completed` / `subAgentActivity` /
`kind="started"`，子 ID 在 `agentThreadId`；V2 的旧式 collaborator 数据仅用于
内部 analytics。`interacted` 不是新 spawn，不能给旧子线程重新授权。

`thread/read` 的 `parentThreadId` 与
`source.subAgent.thread_spawn.parent_thread_id` 可交叉验证父关系，
`sessionId` 标识整棵持久化线程树。它们都不标识当前根 turn，不能单凭这些字段
把历史子线程绑定到新任务。还需验证准确执行环境，以及连接代际和当前授权 lease。
子线程不保证收到 `thread/started`，不能仅依赖该通知建表。
当前只给本轮可信 spawn 建立动态权限；原生对历史子任务的 followup/resume
不自动恢复其 QQ 协调权限。这一限制不能描述为完整的跨轮子代理权限继承。

依据：[V2 activity 通知](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/tools/handlers/multi_agents_v2.rs)、
[子线程监听注册](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/app-server/src/lib.rs)、
[协议定义](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/app-server-protocol/src/protocol/v2.rs)。

## 为什么每个任务使用独立 worker

0.161 的公开 app-server RPC 没有原子关闭整棵代理树的操作。
`thread/unsubscribe` 只卸载一个线程；父线程结束不代表子线程结束。
正在 spawn 的清理还可能在后台运行并先从 loaded 列表移除，所以反复扫描
`thread/loaded/list` 至空也不是可靠的树终止屏障。

Codex 内部确有 `request_agent_tree_shutdown().wait()`，能阻止新 starts 并等待
已接纳成员收尾，但该 Rust API 未暴露为本版本的公开 RPC。
因此当前社交任务使用独立 app-server worker：撤销本任务权限，保持 executor
存活以便尽量完成原生关闭，然后明确终止并回收 worker，最后终止并回收 executor。
这同时停止子线程模型活动和工作空间进程，不需要重启其他会话共用的 authority。
正常结束、取消、超时和 dropped future 都必须完成这一所有权链的清理。

同一 bot profile 可供多个进程打开；上游 SQLite 使用 WAL 与 5 秒 busy timeout，
每个线程还有跨进程 writer 文件锁，重复获得同一线程 writer 会返回 `WouldBlock`。
这不允许两个 worker 同时加载同一根线程：ClawMingle 的工作空间/任务 lease 仍需
串行化恢复；authority 的只读查询不能暗中恢复正在执行的线程。只有确认旧 worker
已退出，才能依赖操作系统释放的 writer 锁继续恢复。

单 worker 配合强制原生 V2 才是这里的边界。V2 恢复的 agent registry 限于当前
root 的持久化后代，发送消息还要求目标属于 registry；V1 的 UUID resume 路径
不能作为共享 profile 下的跨会话隔离保证。

依据：[内部树 shutdown](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/thread_manager/shutdown.rs)、
[异步 pending spawn 清理](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/agent/control/spawn_guard.rs)、
[单线程卸载](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/app-server/src/thread_lifecycle.rs)、
[V2 registry 校验](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/core/src/agent/control/api.rs)、
[SQLite 配置](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/state/src/sqlite.rs)、
[跨进程 writer 锁](https://github.com/openai/codex/blob/rust-v0.161.0/codex-rs/rollout/src/writer_lock.rs)。

已用离线 provider 和真实 Codex 验证完整 fork 的原生工具/Skills/动态工具继承、
空 fork 的能力差异、父子同时取消和取消后同一根线程恢复。独立并发烟测验证：
丢弃一个任务的执行 future 后，同一根线程和工作区可恢复，另一个任务持续运行并正常完成。
单元测试另覆盖共享预算、旧连接/旧任务不能重新授权、乱序通知与清理中途被取消；
这些测试不调用真实模型、不使用个人认证。
