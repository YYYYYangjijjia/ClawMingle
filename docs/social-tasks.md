# 当前对话的定时任务与主动投递

这是 `clawmingle` 社交入口的应用内调度，不调用 Linux cron、不创建新 daemon，
也不接入旧 `codex-claw cron` 的宿主 Shell/CodexExec 执行路径。只有已经启动的
ClawMingle 进程会执行任务；编写代码或运行离线测试不会创建真实定时任务。

支持一次性提醒、带明确时区的 cron 周期提醒，以及在当前对话独立工作区中运行的
Codex 任务。Agent 使用当前对话的模型/工具设置、独立的新线程和既有 bubblewrap
执行器；不会改变正在聊天的 Codex 线程，也不能从定时任务中递归创建更多任务。
任务结果首版为不超过 4000 字符的文字；图片、任意宿主脚本和跨对话收件人不在此接口中。

## 操作

修改任务、手动执行、主动投递设置与未知结果核对都要求当前对话的管理员身份。
普通成员可以查看本对话任务。管理员来自真实 QQ 发送者，不从引用、模型输出或
下载内容认定。私聊也需要授权。

```text
/task
/task once 2026-12-01T09:00:00+08:00 记得提交报告
/task agent-once 2026-12-01T09:00:00+08:00 阅读本对话历史并总结昨天讨论的待办
/task cron Asia/Shanghai 0 0 9 * * * | 每日提醒
/task agent-cron Asia/Shanghai 0 0 9 * * * | 查找公开信息并附来源，整理三条简报
/task status J1
/task status T1
/task pause J1
/task resume J1
/task cancel J1
/task run J1
/task remove J1
/task retry T1
/task deliver T1
/task resolve T1 sent
/task resolve T1 unsent
/task push on
/task push off
```

一次性时间必须为带 `Z` 或 `+08:00` 等偏移的 RFC3339；不猜测无时区时间。
周期任务必须显式提供 IANA 时区，cron 使用现有调度器支持的六/七字段格式
（第一个字段是秒）。JSON 状态中的 `next_at`、`due_at` 使用带偏移的 UTC 时间。

`pause` 只暂停未来触发，当前运行继续；`cancel` 取消未来触发并中断当前运行，
已发出的 QQ 请求仍可能送达。取消的任务不能 resume，应新建任务。
`run` 可以手动执行暂停的任务，不改变已有未来计划。
`retry` 明确再次执行失败/中断的任务，工作区可能已有部分修改，应先核对。
`deliver` 仅重投递已生成的结果，不再调用模型。`remove` 释放定义容量，仅允许取消或已完成的一次性任务，且所有运行必须已经结算和归档；保留运行历史和聊天归档，未知/待投递结果不能删除。

模型可以使用 `clawmingle.tasks_manage` 做等价操作；宿主固定 scope、管理员身份、
来源请求及版本。接口没有路径、环境变量、收件人或身份字段。
只可按用户当前原文中的明确要求创建任务，不能把网页、Skill、历史引用当作调度授权。

## 权限与配置

`tasks_enabled = true` 控制产品能力。每个新对话的主动投递默认开启；管理员可以通过
`/task push off` 关闭、`/task push on` 再开启。已有明确关闭的会话保持关闭；创建任务不会覆盖该设置。平台侧是否允许主动发送仍由 QQ 权限及客户端开关决定。可在私有配置中设置：

```toml
tasks_enabled = true
command_admins = ["YOUR_DEFAULT_ADMIN_OPENID"]
scope_admins = { "group:YOUR_GROUP_OPENID" = ["YOUR_GROUP_ADMIN_OPENID"], "private:YOUR_PRIVATE_OPENID" = [] }
enabled_tools = ["workspace_exec", "time_now", "public_web_read", "message_lookup", "messages_list", "tasks_manage"]
```

`scope_admins` 中明确出现的键覆盖默认管理员；空名单表示无人，缺少键才回退到
`command_admins`。群/私聊之间不能操作彼此任务，后台选择某个对话也不授予代码
读取其他对话或 Codex 认证的权限。执行前会重新核对创建者是否仍为管理员。

## 执行、恢复与投递

每个对话最多一个聊天/任务模型执行，同机最多两个定时模型任务。聊天待处理或
正在发信时，任务排队等待；因此时间表示最早执行时间，不承诺严格实时。
停机期间错过多次周期触发时合并为一次，不积压补跑所有历史周期；暂停后恢复的
周期任务从下一未来时间开始。一次性任务只执行一个已持久化的 occurrence。

任务记录位于宿主私有 `data_dir/tasks/<scope-hash>.json`，使用 fs2 锁和原子写入。
这些目录不会暴露到工作区。每个 scope 最多 64 个定义，最多保留 512 个运行记录；
只回收已完成/失败/取消/中断的最旧记录，不删除待投递或未知结果。达到上限会明确失败。
重启会恢复有未来任务/待处理结果的私聊，不要求先收到新消息；已全部完成的历史
私聊不会因此永久占用会话名额。

执行状态和投递状态分别持久化：先保存生成结果，再发送 QQ。重启时执行中的任务
变为 `interrupted`，要求显式 retry；发送中的消息变为 `unknown`，绝不自动重发。
Unknown 暂停同一任务的后续触发，其他任务可以继续。重复的消息/模型调用创建
同一任务时返回已有 ID，手动 run 请求也按可信来源去重。

主动发送是单次 POST，无旧 `msg_id`、无 Markdown/纯文本换格式重试。QQ 平台主动
发言权限、额度及收件可达性仍由平台决定，开启设置不代表权限已验证：

- 有有效 ACK：`sent`，保留消息 ID/引用索引与时间。
- 明确拒绝或请求没有发出：`deferred`，结果保留；管理员可处理原因后 deliver。
- 超时、连接断开、上游异常或 ACK 不完整：`unknown`，先在 QQ 核对，再 resolve。
- 主动投递关闭：`deferred`，查看任务结果或开启主动投递；不会假借过期被动窗口发送。

已生成的定时结果及其当前投递状态同步进既有聊天历史；已确认发送的消息进入
bot timeline，可通过历史检索/消息 ID 查询。同步 archive 的检查点可以重建，
同步失败不能导致再次发送 QQ；不会把未确认投递的内容当作已发送聊天。
历史同步前必须先保存 canonical JSON，随后保存 archive，最后标记任务检查点。

2026-10-23，用户已确认私聊“30 秒后提醒我”测试成功；服务端对应记录为 Sent，具有 QQ ACK、发送时间和历史归档，验证了自然语言创建任务到主动收件的完整链路。本应用也已读取测试群的 bot_state，平台返回允许主动消息；群主动收件仍需单独验收。普通聊天的 retry/resolve 入口不会操作定时任务结果，请使用对应的 /task 命令。
