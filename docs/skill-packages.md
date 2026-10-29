# 会话技能包与 QQ 资料查询

## 技能包

管理员可以继续使用 `/skill-install GitHub链接`，或直接明确请求机器人安装一个公开
GitHub Skill。来源须是 GitHub 目录或 `SKILL.md` 链接，不支持私有仓库凭据、其他下载站、
查询参数或跳转。安装只下载选定子目录，先解析并固定 Git 提交，再验证每个 Git blob 的哈希。

包必须有合法 `SKILL.md`，可以包含 `scripts/`、`resources/` 等合法子目录。限制为 64 个文件、
128 个目录条目、每个资源 256 KiB、`SKILL.md` 16 KiB、全包 2 MiB、路径最多 8 层；
本机最多 32 个包，同一会话最多启用 16 项。拒绝符号链接、子模块、路径穿越、特殊文件、
硬链接、宽松权限及不匹配的内容哈希。下载不运行安装脚本或安装依赖。

包保存在机器人的私有 `data_dir/skills/<name>/`，文件权限为 0600（原 Git 可执行文件为
0700），目录为 0700。`.clawmingle-package.json` 记录来源 URL、完整提交、安装时间及逐文件
哈希。相同版本重复安装是幂等的；同名不同版本或已有文字 Skill 会明确拒绝，保留原内容。
首版不提供自动更新或原地覆盖包。

安装后只为发起安装的会话启用。管理员可用 `/skill on NAME`、`/skill off NAME` 控制当前
会话，`/skills` 查看当前选择。权限由本机 `scope_admins` 决定：键是 `group:<ID>` 或
`private:<ID>`；没有该键时回退到 `command_admins`，显式空列表表示无人有管理权限。
官方群主或管理员角色仅作为查询结果，不会因此获得本机权限。

启用包的说明会进入该会话的提示。包目录只在该次 `workspace_exec` 中以
`/skills/<name>` **只读挂载**；脚本使用解释器运行，例如
`python3 /skills/example/scripts/run.py`，输出写入当前会话 `/workspace`。执行器保留既有
超时、资源限制、网络隔离、取消和清理规则。共享 App Server 不挂载包，不开启原生宿主
shell、个人文件、认证或跨会话访问。停用后下一轮不挂载该包；执行结果文件仍属于会话工作区。

管理页面同时列出旧文字 Skill 与完整包。完整包只读显示来源、版本、文件数及启用范围；
后台也拒绝直接 API 覆盖或删除完整包。旧文字 Skill 保留原编辑功能。

## `qq_info` 只读工具

工具由既有会话工具白名单控制，无 QQ 写操作。参数只有固定 `action`，可选分页 `cursor`，
以及查询成员详情时使用的 `participant` 别名；模型不能指定群 ID、用户 ID、地址或 HTTP 方法。

| action | 官方端点 | 范围与边界 |
|---|---|---|
| `self` | `GET /users/@me` | 本应用已于 2026-10-23 验证 HTTP 200 且具有 username/avatar；只透传官方提供的头像 URL，不猜测地址 |
| `group` | `GET /v2/groups/{group_openid}/info` | 仅当前群；本应用已验证 HTTP 200，存在群名、人数及群介绍 |
| `members` | `GET /v2/groups/{group_openid}/members` | 仅当前群，按官方 cursor 分页、每页最多 30；本应用返回 HTTP 400/code 11253，明确未授权 |
| `member` | `GET /v2/groups/{group_openid}/members/{member_openid}` | 仅当前群已知别名；本应用返回 HTTP 400/code 11253，明确未授权 |
| `bot_state` | `GET /v2/groups/{group_openid}/bot_state` | 仅当前群；本应用已验证 HTTP 200，包含主动消息开关、接收设置、群角色、加入时间 |

成员别名来自当前会话记录，或本轮官方成员列表的结果。列表中新发现的成员会保留于本轮
宿主侧映射，便于紧接着查询详情；原始成员、群、union 标识不交给模型。不同群和不同 turn
不会共享这份查询映射。返回的数据带有不可信数据标记；昵称、介绍或角色不能授予权限。

私聊只允许 `self`，不假设频道成员、QQ群成员或私聊用户资料卡互通。现有官方接口没有在
本实现中证实可读取对方的性别、职业、个人简介或完整资料卡，也没有证实可修改机器人昵称、
头像、简介的公开接口。`bot_state` 是读取机器人在当前群的状态，不能修改资料。

平台权限不足、限频、网络或响应错误均明确返回，不使用虚构资料替代、不自动重试。
群资料/成员等接口的文档权限要求和本应用个体权限分开记录；本机启用工具不能获得平台权限。
本轮应用探测由主代理进行，只读、无发信；原始响应及标识没有写入文档。

官方依据：

- [读取机器人自身信息](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/users_me.get.html)
- [读取群信息](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_info.get.html)
- [读取群成员列表](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_members.get.html)
- [读取群成员详情](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_members_member_openid.get.html)
- [读取机器人群内状态](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_bot_state.get.html)
- [官方 FAQ：权限与内邀接口边界](https://bot.q.qq.com/wiki/bot_FAQ/)
