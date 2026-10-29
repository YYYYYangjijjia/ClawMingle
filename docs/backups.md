# 私有聊天历史备份

备份覆盖这台安装实际观察并归档的群聊、私聊原文与处理状态，包括 Silent、未判断和未回复的消息。首次启用完整归档之前已被清理的内容不能恢复。热状态的 24 小时窗口不会删除归档中的历史。

备份目录默认是 `data_dir/backups`，目录 `0700`、文件 `0600`。它包含私人聊天和已确认资料，必须留在仓库之外或被忽略的运行数据目录中。校验和用于发现损坏，不是加密或数字签名；不要公开分享备份或离线检索输出。

保存内容：

- `history/archive.sqlite` 和存在时的 `private/history/archive.sqlite`：通过 SQLite Online Backup API 生成的自包含数据库快照，包含完整原文、请求、消息状态和发送确认。
- `groups/*.json`、`private/groups/*.json`：最近一次持久化的热状态。应用内保存屏障覆盖 JSON 写入和归档事务；备份会逐条验证当前消息和请求与数据库一致，发现写入中断造成的不一致便明确失败。
- `memory/groups/G…/store.json`：群记忆唯一正式记录，包括确认、待确认、撤回与版本。只复制正式记录；镜像可按既有维护流程重建。
- `tasks/{g|p}_….json`：每会话的定时任务、执行记录和发送队列。记忆与任务分别持自己的共享文件锁，是备份开始和完成之间的独立原子快照，与聊天状态没有跨模块事务。
- `manifest.json`：文件清单、字节数、SHA-1 校验、快照时间、归档覆盖范围和序号水位。
- `persona/IDENTITY.md`、`persona/SOUL.md`：存在时保存共享身份和人格，其他同目录文件不收录。管理员修改时机器人已停止并持有操作锁；直接编辑文件的操作员须自行避免并发。

不复制 QQ/Jev/模型凭据、除上述人格文件外的配置、Codex 登录态、Codex profiles/rollouts、网关恢复状态、媒体像素、模型工作区或派生记忆镜像。聊天原文本身仍可能包含成员发送的私人信息。备份不会把模型正在生成或 QQ 尚未确认的发送标成成功。

## 手动备份和检查

所有入口都要求显式指定私有配置文件；不调用模型或 QQ。手动 `backup` 使用机器人现有操作锁，需要机器人已停止；它不会自行停止服务。`backup-list`、校验和离线检索可与机器人并行，备份目录锁会防止与自动轮换竞争。

```bash
clawmingle backup --config /absolute/private/social.toml
clawmingle backup-list --config /absolute/private/social.toml
clawmingle backup-verify BACKUP_NAME --config /absolute/private/social.toml
clawmingle backup-query BACKUP_NAME group 0 0 --config /absolute/private/social.toml
clawmingle backup-query BACKUP_NAME private 0 0 --config /absolute/private/social.toml
```

`backup-query` 的会话索引是该备份内、该类别下按归档 scope 排序的零基索引；第四个参数是事件游标，首次填 `0`。每页最多 50 条，按 `next_cursor` 继续，输出保留消息和当时状态。它访问指定备份，不读取当前会话的新消息；只有成功 Sent 的机器人输出进入聊天时间线，所有请求和未确认输出仍完整保存在数据库的 `requests`、`bubbles` 表中，可供操作员离线分析。

## 导出恢复副本

```bash
clawmingle backup-export BACKUP_NAME /absolute/private/new-restored-directory --config /absolute/private/social.toml
```

目标父目录须已存在并是当前用户的 `0700` 私有目录，目标目录必须不存在，且不能位于活数据或备份树内。导出前后均验证快照；任何已有目录都不覆盖。失败时可能留下部分新副本，需操作员检查后处理，命令不会替换活数据。

导出不是启用恢复：重新选择数据目录、单独准备认证和检查状态后才能启动。尤其要先检查恢复的任务发送队列、`Sending`/`Unknown` 状态、已完成提醒和外部动作，避免重复投递。不要把旧网关游标或个人 Codex 登录态复制过来。旧备份的相对媒体路径可能指向已过期附件，原文和状态仍然保留。

## 可配置的每日应用内维护

```toml
[backup]
auto_enabled = false
daily_hour_utc = 20
retention_days = 7
max_total_bytes = 1073741824
reserve_free_bytes = 16106127360
```

默认关闭；确认要启用后才把 `auto_enabled` 设为 `true`。使用机器人进程中的分钟级检查，不安装 cron、timer 或额外常驻服务。`20` 表示 UTC 20:00，即北京时间次日 04:00；达到时间后运行，当天已有成功备份则跳过。机器人未运行期间不会补建每天的历史快照，启动时只补当天一次；失败当天不循环重试，手动操作可再次检查。

轮换只删除经过清单、权限和校验验证的旧快照，并始终保留最新的一份有效备份。保留期限和总量额度都可能促使更旧的备份提前轮换；若保留最新备份后仍放不下新快照，明确返回 `backup_quota_reached`。新备份前还保留至少 15 GiB、文件系统容量的 20% 与配置要求三者中的最大空闲空间，空间不足明确失败，不为了备份耗尽磁盘。

`backup-list` 展示最近尝试的固定状态/错误码，自动维护同时写固定类别的服务日志，不输出私人原文、配置或底层数据库响应。异常退出遗留的 `.incomplete-*` 或无法识别的目录会阻止后续备份并报 `backup_unfinished_or_unrecognized_entry`；操作员核对后清理，程序不会自动删除来源不明的数据。
