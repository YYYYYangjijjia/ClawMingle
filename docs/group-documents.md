# 群在线文档与文件交付

ClawMingle 的 `group_documents` 工具通过腾讯文档官方 MCP 维护群计划、清单和资料。
群聊 Codex 可在管理员绑定的专用目录内增删改查，不需要每次申请写权限。
目录绑定与账号授权只能从管理员工作台配置；私聊不获得该工具。

## 接入

1. 工作台打开“群在线文档”，点击“授权腾讯文档”，在官方页面登录并授权，返回点击“已完成授权，检查”。链接有效五分钟。也可在密码框填写官方提供的独立 Token。
2. 读取自己的目录，选择一个群专用目录，或新建目录。选定已登记群聊后保存绑定。
3. 在群里要求机器人创建计划、编辑表格、制作幻灯片、维护收集表草稿，或读取、改名和删除指定文档。创建成功后会返回文档链接；不同类型的范围见下表。

腾讯文档授权独立于 QQBot AppID/Secret 和 Codex 登录。Token 不进入模型上下文或 Git；
本机私有目录为 `0700`、凭据文件为 `0600`，工作台不回显 Token。
暂停工具或解除绑定不删除云端文档。

**访问范围**是绑定目录及其直接下属任务目录中的文档。更深层目录、其他群目录、个人首页和知识库空间
不向群模型开放。每个目录只能绑定一个群；每次操作都会重新核验任务目录仍属于本群、文档仍在该目录中。
后台管理员可查看账号目录用于绑定，这不赋予聊天模型账号级目录权限。

## 任务目录与交付规则

目录采用“本群专用目录 → 任务目录 → 文档”。同一任务可包含主计划、路线图、预算表、幻灯片等多份文档。
先调用 `tasks` 查看已有任务和未归类文档，结合对话判断是续接还是独立任务；续接时复用原目录与文档，
追加或局部修改，不为每次补充要求重新创建一份。只有独立任务才使用 `task_create`。
程序会复用同名任务目录；语义相近但名称不同的任务仍需 Codex 结合目录和聊天判断，不能声称程序能完全自动识别。

新建、原生创建工具及导入都要求 `task_id` 与稳定的 `document_key`（例如 `main-plan`、`route-map`）。
同一任务内再次创建同一用途或同名文档，会返回已有文档，不应用新内容；随后必须读取、追加或编辑原文档。
同一用途尚有未确认的创建/导入时，会返回原操作号，阻止换号重复创建。旧根目录文档继续可读写，
`organize` 可以在核对最新版本后归入任务目录，保留原文档 ID 与链接。

支持的结构化成果默认通过在线文档交付，回复可访问链接。只有用户明确要求文件，或在额度受限后同意降级，
才发送 QQ 文件附件；用户要求的普通图片、路线图图片不受这项文档交付规则影响。

这里的文件夹是**腾讯文档账号目录**。QQ 原生“群在线文档”列表与它独立，目前已验证逐文档登记，
尚未验证 QQ 列表展示这些任务文件夹。不能把腾讯账号目录 ID 当成 QQ 原生群目录 ID。

## 额度、内容保留与修订来源

官方 `400007` 表示调用频率/额度限制，HTTP 429 也单独识别；不能把所有权限、网络错误都叫额度不足。
遇限频停止反复调用，告知用户并询问是否改发文件。每次写操作的输入在调用前存入私有操作草稿；
导入文件还保留不可变字节副本，重试不会读到用户后来修改的文件。
`draft(operation_id)` 可读回输入，`draft_export(operation_id)` 恢复到当前工作区，**不会自动发送附件**。
原生结构化操作恢复为 JSON 草稿；追加内容仅是该次追加，不是整篇云端文档的备份。
创建成功但移动失败时保留原 ID 和部分完成状态；限频不能把已部分完成的操作变成“可以重新创建”。

每次工具操作由宿主记录固定来源“来自 ClawMingle”、提交时间、原操作号及实际确认状态，
`revisions(file_id)` 可查看本地最近 100 条关联记录。它是本地操作审计，**不是腾讯原生历史版本备注**。
官方 MCP 没有修订备注参数；独立网页登录现可为普通文档保存命名版本“来自 ClawMingle”，见下方来源节点说明。同账号的自动修订作者仍显示该账号。
不擅自给正文添加页脚或批注冒充版本标记。腾讯部分类型的 `list_recent_ai_edits` 是短期 MCP 编辑记录，
不代替本地长期记录，也不能定制修订来源文字。

依据：[官方错误码](https://docs.qq.com/open/document/mcp/error-code/)、[官方调用频率约定](https://docs.qq.com/open/document/mcp/)。

## 文档类型

| 类型 | 已接入的主要操作 | 边界 |
| --- | --- | --- |
| 智能文档 `smartcanvas` | Markdown 创建、分页读取、块搜索、追加和编辑 | 保留既有块级编辑校验 |
| 普通文档 `doc` | 创建、读取和编辑正文、格式、图片、表格 | 先读取当前结构，再按官方位置参数编辑 |
| 表格 `sheet` | 创建、单元格与公式读写、样式、图表、工作表管理 | 不开放任意脚本执行接口 |
| 幻灯片 `slide` | 创建、页面、文字、形状、图片、表格、图表编辑 | 不开放任意 JSX 执行接口 |
| 智能表格 `smartsheet` | 创建、记录、字段、视图、仪表盘管理 | 不开放自动化任务、原始 changeset 或账号管理 |
| 收集表 `form` | 创建草稿、编辑问题和设置、读取提交结果与统计 | 官方接口不能发布，需在腾讯文档中人工发布；结果读取受账号权限限制 |
| 流程图 `flowchart` | 创建、读取、增删改和移动图形节点 | 使用流程图专用接口 |
| 思维导图 `mind` | 从 Markdown 创建、文件信息、改名、回收 | 未发现官方专用节点读取和编辑接口 |
| 智能白板 `board` | 创建空白白板、文件信息、改名、回收 | 未发现官方画布内容读取和编辑接口 |
| PDF | 导入、查询导入结果、导出 | 不提供 PDF 在线正文编辑；导出格式由官方接口决定 |

后台“群在线文档”展示同一范围的能力说明。可创建一种文件，不代表它具有全部正文操作。
正文工具目录来自腾讯文档官方 MCP 的公开参数，按文档类型提供发现和调用入口；没有向群模型开放任意远程 MCP 调用。

## 工具行为

### 容量与生命周期

QQ 群文件、腾讯文档个人存储、QQ 群在线文档的入口列表分别管理。群入口登记成功不代表文件占用群文件容量；
腾讯文档授权账号可能同时存放其他群和个人资料，绑定一个群目录不等于独占整个账号的剩余容量。
[官方个人存储说明](https://docs.qq.com/doc/p/567106ac886d5be6d50384e91869075eac91988e)
说明在线文档占所有者的空间，正文、插入附件/图片和版本历史均计入；因此容量不能直接换算为固定文档份数。
当前不读取或猜测账号余量，不把 MCP 调用次数/频率受限解释为空间不足。

已预留按文档 ID 持久保存的 `retention` 策略，可通过工具查询；明确指定后传 `retention_class` 和
`operation_id` 修改：`standard`（普通）、`keep_online`（长期保留在线）、`memory`（记忆用途）。
后两者明确排除未来自动归档。策略跟随文档 ID，不因改名、任务目录调整而丢失。
当前所有类型都不自动迁移、回收或永久删除；`memory` 标签也不启用在线记忆同步，不改变本地正式记忆的真值地位。

未来启用冷文档归档前需另行落实以下条件：

- 只考虑由当前群工作区管理、未豁免、无活跃任务引用的文档；最后编辑时间同时涵盖用户和机器人。
  “未修改”不等于“未使用”，没有可靠阅读数据时只能提出候选，不能据此自动删除。
- 先确认文档类型可完整导出；保留可编辑格式，必要时另存阅读快照。评论、版本历史、协作权限、交互组件
  不保证能随导出保留；不能导出或不能验证完整性的类型跳过。
- 保存源 ID/链接、版本、时间、摘要、校验和与目标群文件位置的索引；确认群文件上传成功且可重新读取后，
  再重新核对原文档版本，避免将导出期间的新编辑一并删除。失败和中断必须可恢复且不重复上传。
- QQ 群入口解除与腾讯文档回收分别确认；现已验证本库管理且已回收文档的入口清理；完整迁移链仍未实现。
  回收站是否释放容量仍需官方依据，不通过自动永久删除腾空间。归档后的群文件保留，删除由用户明确要求。

不在本轮新增定时任务或默认归档天数。未来规则还需在后台展示豁免、候选理由、导出完整性、上传校验和失败状态。

| action | 用途 |
| --- | --- |
| `status` | 查看配置与逐类型能力，获取当前请求的操作 ID 前缀 |
| `tasks` / `task_create` | 查看任务与旧根目录文档；创建或复用同名任务目录 |
| `list` | 列出目录内容；传 `task_id` 查看任务，用 `next_start` 翻页 |
| `organize` | 核验版本后将旧文档移入任务目录，保留 ID、链接及群登记 |
| `draft` / `draft_export` | 读取保留的操作输入，或恢复到工作区；不自动发附件 |
| `revisions` | 查看固定标注“来自 ClawMingle”的本地操作记录 |
| `retention` | 查询/标注普通、长期保留在线或记忆用途；不会执行归档或删除 |
| `resume` | 核验原创建结果；仅在移动被明确拒绝时恢复移动，绝不重新创建 |
| `qq_list` | 分页读取 QQ 原生群文档入口，保留原始状态码而不猜测含义 |
| `registration` | 对可信群登记核验文档状态；明确回收后标注入口待清理 |
| `qq_cleanup` | 实时核验本库管理文档已回收后清理群入口；未知结果只核验，不重复删除 |
| `inspect` | 元数据、链接、最近修改时间 |
| `create` | 先按任务和用途查重，再按 `document_type` 创建并移入任务目录；默认智能文档 |
| `read` | 智能文档：不传 `page_id` 时列出页面；传入后读取页面，用 `next_token` 继续 |
| `find` | 智能文档：搜索文档块，取得块 ID 和当前原文 |
| `append` | 智能文档：在文档末尾追加内容 |
| `edit` | 智能文档：重新查找并比对原文后修改指定块；搜索尚未收录时，可用精确页面片段核对后修改普通顶层文字块 |
| `tools` / `tool` | 按类型分页发现专用工具；读取选中工具的参数要求 |
| `invoke` | 按已查到的参数调用专用正文工具；程序校验文档所属目录及参数 |
| `import` / `import_progress` | 从当前会话工作区导入支持的文件，查询异步任务并确认移入绑定目录 |
| `export` / `export_progress` | 为目录内文档发起导出，再查询结果；任务与源文档绑定 |
| `rename` / `delete` | 比对 `expected_modified_at` 后改名，或将首页源文档移至腾讯回收站 |
| `operation` | 用 `operation_id` 查询已记录结果，可在后续消息或重启后查询 |

不同类型使用不同的正文接口。Codex 先发现对应类型的工具并读取参数，再调用所需操作；
不能将智能文档的页 ID、块 ID 用于普通文档或表格。既有智能文档创建/追加/修改正文限
20,000 字；专用工具遵循各自参数和大小限制。工具响应和翻页有上限，不能把一页当作整本文档。
删除工具不删除目录、子目录树，也不永久清空回收站。

**回收站与群列表是两份状态。** 腾讯文档回收成功不会自动证明 QQ 入口已移除。
`qq_list` 核验群关系后，`registration` 才能对这些可信 ID 查询文档状态；只有明确 `status=trash`
会标注 `cleanup_pending`。鉴权、限频、网络失败或不认识的原生数字状态均不等于已回收。
单独标记不会改变 QQ 客户端列表。本库管理的文档回收后会尝试官方入口清理；也可用 `qq_cleanup` 核验与处理。
QQ 的“删除”涉及本体和副本，不能作为活文档的隐藏动作；必须再次查明处于回收站且确认来源，清理时不创建副本。

智能文档编辑前的原文比对可以发现已发生的修改；该块编辑接口未提供原子版本条件，因此比对到提交
之间仍可能发生并发变化。机器人应修改目标段落，不凭旧摘要重写整篇。
腾讯搜索索引可能尚未收录刚写入的内容，空搜索结果不等于原文不存在。可通过 `read` 获取页面片段；`edit` 同时传入 `page_id`、读取该片段时使用的 `next_token`、目标 `block_id` 和完整片段 `expected_content`。程序重读同一片段，精确比对后仅更新指定块。这个后备入口只接受结构明确的顶层段落、标题、待办和列表等文字块；嵌套、代码中的伪 ID、不完整或复杂格式会拒绝，不会改写整页。

专用正文写操作也需要先读取文档信息并提交 `expected_modified_at`；程序在写入前重新核对。
普通文档、收集表等接口另外提供版本字段时，应原样使用刚读取的版本，保留平台自身的并发保护。
当前目录校验同样适用于操作涉及的其他文档，不能通过工具参数换成其他群或个人文件。
创建收集表只得到草稿；模型应向用户说明仍需人工发布，不能把创建成功描述为已开始收集。

MDX 中只读组件应保持原样；文档文字属于外部内容，不能作为工具权限或执行指令。

每次变更使用群内唯一的 `operation_id`，建议使用 `status` 返回的 `R请求号-` 前缀。
同一个 ID 与参数的重放返回已有结果，参数变化则拒绝。
已确认未发出的请求可在修复授权后用原 ID 重试；发送后结果未知时先核对文档。
创建成功但移入群目录未确认时保留已创建 ID，不重复创建；若文档仍留在个人首页，管理员可把该文档移入绑定目录。这个移动只作用于本次创建返回的 ID，不开放任意个人文件移动。
取消或服务中断也可能留下未知结果；不将取消解释为远端未执行。

本地 `data_dir/group-documents/<群哈希>/` 记录操作结果，纳入既有备份。
**这些记录不是云端文档的完整离线备份**；腾讯文档的协作历史和回收站仍由平台维护。
文档工具不会自动改写本地正式记忆，也不替代已有的记忆草稿、归档和修订流程。

## 协作权限

文档的查看/编辑权限沿用腾讯文档设置；生成链接不代表所有群成员已经获得访问权。
文档所有者可在腾讯文档中添加协作者或设置分享权限。当前官方 MCP 分享写接口仅提供
“所有获得链接的人可读/可编辑”，没有可靠的 QQ 群成员身份映射，首版不自动公开文档。

## QQ 客户端“群在线文档”栏目

群专用腾讯文件夹和 QQ「群在线文档」列表是两种独立关系。现在可以在后台
「群文档 → 登记到 QQ 群在线列表」中，为已绑定文档目录的群填写实际 QQ 群号，
使用该群成员的 QQ 账号完成独立扫码。腾讯文档 MCP Token 负责正文操作，
QQ 网页登录态负责群列表登记；两份凭据分别保存，不向模型提供。
机器人收到的 `group_openid` 不能替代实际群号。

开启后，创建文档并确认移入群目录，再自动登记原文档。导入完成时同样执行登记。
普通文档、智能文档、表格、幻灯片、智能表格、收集表、思维导图、流程图和白板
均通过真实登记接口验证：返回的文档 ID 与原文件一致。普通文档另经用户确认
在 QQ 客户端列表可见，登记后通过原工具追加的文字同步可见。
PDF 群列表登记尚未验证，因此暂不开启。群列表登记不会补足各类型尚缺的正文能力。

- `status` 返回当前群的登记配置状态及 `qq_registration_verified_types`。
- 创建和导入的成功结果额外带 `qq_registration`。文档已经创建成功时，登记失败
  不会把它误报为创建失败；模型应保留并返回原文档链接，不能反复创建。
- `registration` 按 `file_id` 读取本地最后一次登记确认，不代表实时读取整个 QQ 群列表。
- `register` 为当前群目录内的既有文档补登记，需新的 `operation_id`。每次均检查
  目录归属、独立网页登录及当前群权限。已确认的同一群/文档不会重复登记。
- `unknown` 表示登记可能已经到达腾讯。先在 QQ 客户端核对；重启、更换操作 ID
  或模型重复调用都不会盲目重发。不支持通过模型参数更换目标群、凭据或调用地址。
- 登录过期时在后台重新扫码。替换账号前会验证当前全部绑定群的访问权限；暂停或
  解绑仅停止后续登记，保留已有文档。解除腾讯文档目录绑定时同步解除该群登记映射。

网页登录材料与二维码只保存在应用的私有目录（目录 `0700`、文件 `0600`），
扫码最长等待 15 分钟；取消或到期结束临时浏览器。二维码经既有管理接口返回，
继续要求 Dashboard 管理员登录，不新建公开认证页面。登录配置显示“已保存”，
不以 Cookie 文件存在推断远端登录始终有效。

访问权限仍由腾讯账号与文档原有权限控制。本功能不开放所有链接访问、不自动修改
分享权限；应使用能操作这些文档且属于对应群的账号。群内入口与普通 QQ 文件附件不同。

### 接口依据及维护边界

2026-10-24 检查腾讯官方网页，并以隔离登录进行真实验证：

- 官方[模板库](https://docs.qq.com/mall/index/doc)的
  [公共脚本](https://docs.gtimg.com/tmall/chunk-common-af6c47286a.js)在创建后调用
  `POST https://docs.qq.com/cgi-bin/group_online_docs/doc_add`，携带群号、原文档 ID、
  `copy=1`、`domain_id`、`folder_id` 和 `xsrf`。上述九类实测均保留原文档身份；
  不因参数名称推断会产生副本。脚本中的收集表排除分支并不代表服务端登记不支持它。
- 官方[移动模板库脚本](https://docs.gtimg.com/tmall/mhome/mhome-0d0b7e8ff8.js)
  用 `group_online_docs/group_info_get` 检查群权限。网页登录以 `docs.qq.com`
  适用的 Cookie 为范围；没有 `skey` 时沿用官方 `bkn` 计算的空值结果。
- [已有文档选择页](https://docs.qq.com/desktop/m/send.html)另有 QQ 客户端桥接分享，
  本实现不依赖该桥接，也不把消息卡片发送成功当作群登记成功。

这是腾讯现行客户端使用的接口，尚无公开 OpenAPI 稳定性承诺。固定官方 HTTPS 主机、
禁用重定向/代理、核对连接地址、限制响应大小，错误及结果仅返回脱敏状态。
协议或登录失效会停止登记，保留已创建的文件和操作日志；正文 MCP 编辑链路独立。

### Native cleanup and source checkpoints

Recycling a host-managed document now also attempts to remove its native QQ group-list entry. `qq_cleanup(file_id, operation_id)` can explicitly reconcile/clean such entries after a fresh Tencent `status=trash` check. The official creator delete action is not a safe hide/unlink for a live document; this tool refuses live or merely observed, unmanaged documents. It always uses `save_back=false`, because that checkbox creates another copy. An uncertain delete is only reconciled by reading the list and document status; it is never blindly repeated. Permanent deletion and automatic retention-based deletion remain disabled.

For ordinary `doc` content writes, the host separately saves a native version named **来自 ClawMingle**, visible in **修订记录 → 已保存版本**. The content result is journaled first, so a failed or uncertain source checkpoint cannot cause content to be replayed. `checkpoint_status(file_id, source_operation_id)` only reconciles an existing checkpoint journal; it cannot create a missing old marker or replay content. `native_source_checkpoint.status=confirmed` identifies a read-back verified saved version; it does not change the automatic revision author and does not prove that simultaneous human edits are absent. Existing historical versions are not retroactively labelled.

Smartcanvas's native history panel does not display saved-version aliases, even though its version API accepts them. Source checkpoints therefore remain restricted to verified ordinary documents. Other types report this limitation. Distinguishing human and bot authors consistently across all types requires a dedicated Tencent Docs account; the operator chose to switch accounts later. This release preserves the existing login and permissions.
