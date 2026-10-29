# ClawMingle：QQ 消息格式、交互与能力适配

核实日期：2026-10-23。依据本轮实时读取的腾讯 QQ 机器人官方文档及
`tencent-connect/qqbot-nodejs` 官方 SDK；没有登录平台、读取凭据、发送测试消息或调用写接口。
文档中的平台能力不等于 ClawMingle 已实现或当前机器人的所有权限均已验证。

## 1. 首要结论

1. 群聊与 C2C 可以直接发送自定义 Markdown：使用 `msg_type=2`、`markdown.content`。
   官方 2026-04-23 更新明确已向所有机器人开放，不必申请 Markdown 模板。
   群/C2C 发送接口将旧模板字段标为废弃；模板模式仍有旧示例，不能把模板 ID 视为必需。
   频道的开通规则不同，不能套用到 QQ 群。
   [Markdown 官方说明](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/type/markdown.html)
2. `keyboard.content.rows` 可以动态定义消息按钮，不必提供键盘模板 ID；`keyboard.id`
   是已有模板的另一种用法，两者互斥。最小可用交互是 Markdown 加指令按钮，点击填入命令，
   发送后经过现有后端权限检查。群聊不应依赖 `enter=true` 自动发送。
   [群发送结构](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)
3. “收到分享卡片”和“发送 ARK 卡片”是不同能力。当前类型表将群/C2C 结构化卡片列为
   可接收、不可发送；当前群/C2C 请求体也没有承诺 `ark` 输出。首版应以 Markdown、图片及按钮
   组合呈现信息卡，不承诺任意原生分享卡片、Embed 或 HTML 气泡。
   [消息类型](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/type/overview.html)
4. 原生流式仅 C2C，使用单独的 `stream_messages` 接口；群聊仍生成后分泡发送。
   [C2C 流式接口](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_stream_messages.post.html)

## 2. 群聊与 C2C 能力矩阵

表中“支持”指官方协议；具体客户端效果仍需用当前机器人验证。

| 类型/功能 | QQ 群输入 | C2C 输入 | QQ 群输出 | C2C 输出 |
|---|---|---|---|---|
| 普通文本、Unicode emoji | 支持 | 支持 | `msg_type=0` | `msg_type=0` |
| 原生 Markdown | 概览列为可收；事件没有独立 Markdown 源字段保证 | 概览未列原生 Markdown 接收；普通文本中的语法仍是文本 | 支持，无需模板 | 支持，无需模板 |
| 模板 Markdown | 不作为独立输入处理 | 同左 | 旧兼容能力，不作为新实现依赖 | 同左 |
| 消息键盘按钮 | 指令结果作为消息；回调另外接互动事件 | 同左 | 支持动态键盘/已有模板 | 同左；还可用自动发送指令 |
| 图片 | `attachments` | `attachments` | 上传后 `msg_type=7`，或 Markdown 图片 | 同左 |
| 文件 | 附件元数据/下载地址 | 同左 | `file_type=4` 上传并发送 | 同左 |
| 语音 | 附件，可附 WAV 地址、ASR 参考文字 | 同左 | 语音富媒体；先验证编码与客户端展示 | 同左 |
| 视频 | 附件 | 附件 | MP4 富媒体 | 同左 |
| 分享/小程序/位置等卡片 | `message_type=3`、`ark_data` | 同左 | 当前不承诺 ARK 卡片发送 | 同左 |
| 引用回复 | `msg_elements`、引用索引、可能包含嵌套内容 | 同左 | 支持 `message_reference` | 支持 `message_reference` |
| 合并/聊天记录 | 文档枚举含 101/102，递归元素结构 | 同左 | 未见创建任意聊天记录的承诺 | 同左 |
| Embed | 不作为当前输入承诺 | 同左 | 不支持；频道能力不可套用 | 不支持 |
| 表情表态/贴消息回应 | 不支持对应机器人 API | 不支持 | 不支持；仅频道 | 不支持 |
| 原生流式 | 不是独立入站格式 | 同左 | 不支持 | `POST /v2/users/{user_openid}/stream_messages` |
| 输入中状态 | 非普通入站消息 | 同左 | 当前未列支持 | `msg_type=6`、`input_notify`，最长 60 秒 |
| `/` 原生命令面板 | 客户端输入辅助，最终仍需后端执行 | 同左 | 可以 API 配置群面板 | 可以 API 配置 C2C 面板 |
| 单聊底部菜单 | 不适用 | 可发送文字/快捷菜单互动 | 不适用 | `/v2/menu` 全局菜单 |

来源：[类型矩阵](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/type/overview.html)、
[群事件](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/group_message_create.html)、
[C2C 事件](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/c2c_message_create.html)、
[群发送](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)、
[C2C 发送](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html)、
[表情表态](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/trans/emoji.html)。

入站 Markdown 的原始样式往返、所有转发层级及客户端特殊卡片不应仅凭概览表承诺。
保留已有文本作为降级视图，同时保留平台实际提供的结构，而不是假定可以还原原始排版。

## 3. Markdown 与信息卡

官方已展示的语法子集包括一级/二级标题、粗体/斜体/删除线、链接、图片、有序/无序列表、
有限嵌套列表、引用、分隔线及段落。未在该页面承诺的表格、代码高亮、LaTeX、任意 HTML、
CSS 和自定义气泡颜色，应按不保证呈现处理，转成列表、短段或附件。

公网图片会由平台下载转存；也可使用 2026-10-21 新增的素材管理，将本地图片上传后获得
`qqbot-media://` 引用。后者支持 PNG/JPEG/GIF/WebP，单图 10 MB、每机器人 100 张、
最后引用后 90 天未用过期；与普通富媒体 `file_info` 的上传和有效期不同。
原始预签名地址只是短期上传/预览凭据，不写日志或长期记忆。
[Markdown](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/type/markdown.html)、
[素材管理](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/material-management.html)

推荐信息卡实现为应用自己生成的 Markdown：简短标题、数个字段、摘要、来源链接，按需配图片和按钮。
收到的 `ark_data` 可保存其类型和经过长度限制的标题/描述/来源/跳转 URL；不把原始卡片 JSON
直接交给发送 API，也不自动访问其中全部链接。

## 4. 按钮类型、权限与回调

| `action.type` | 含义 | 建议 |
|---|---|---|
| 0 | 跳转 URL/小程序 | 仅由应用生成已允许的链接；不把任意模型 URL 当管理入口 |
| 1 | 回调按钮 | 需要互动事件、去重、身份/范围/版本检查及及时 ACK |
| 2 | 指令按钮 | 首版首选；将白名单命令填入输入框，由现有命令处理器执行 |

`render_data` 提供按钮文字、点击后文字和有限样式；按钮文字最多 10 字符。
`permission.type` 可限制指定用户、管理员或所有人。该限制只是客户端/平台入口限制，
后端仍必须按 ClawMingle 自己的 `command_admins` 和当前群/私聊权限判定。
群管理员身份与当前项目允许修改设置的人员名单不是同一个概念。

`keyboard.id` 与 `keyboard.content` 二选一。官方错误码包含行/列超限，但本轮所读页面未给出
可直接作为跨客户端保证的统一数字；首版使用少量按钮（例如一至两行，每行两三个）并限制总数。
`enter` 自动发送、`anchor=1` 选图器均有 C2C/手机版本限制；群聊只依赖填框行为。
`modal` 可提供二次确认提示，不能代替后端授权。
[群键盘字段](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_messages.post.html)、
[C2C 键盘字段](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_messages.post.html)

可直接实现的只读按钮示例（全部 ID 均为占位值）：

```json
{
  "msg_type": 2,
  "markdown": {"content": "## ClawMingle\n选择一项查看当前状态。"},
  "keyboard": {"content": {"rows": [{"buttons": [{
    "id": "show_status",
    "render_data": {"label": "查看状态", "style": 1},
    "action": {"type": 2, "permission": {"type": 2}, "data": "/status", "enter": false}
  }]}]}},
  "msg_id": "INBOUND_MESSAGE_ID",
  "msg_seq": 1
}
```

回调路径：订阅 `INTERACTION (1<<26)` → 接收 `INTERACTION_CREATE` → 读取 `d.id`、
场景及真实操作人 → 校验服务器登记的 action ID → `PUT /interactions/{interaction_id}` 回应。
群身份使用 `group_openid`、`group_member_openid`，C2C 使用 `user_openid`；不同命名空间不能混用。
`button_data` 是用户可触发的数据，不是已经获得授权的系统指令。

官方交互概述要求指令回调约 3 秒内响应；先快速判定并 ACK，再异步处理耗时业务。
`code` 可表达成功、失败、频繁、重复、无权限、仅管理员。互动 ACK 使用 `d.id`，
不是拼接前缀后的内部事件名；同一 ID 只回应一次。
普通发消息可用外层事件 ID 作 `event_id`，不要与互动 ACK ID 的用法混淆。
[交互流程](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/trans/overview.html)、
[事件结构](https://bot.q.qq.com/wiki/develop/api-v2/autogen/event/interaction_create.html)、
[ACK 接口](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/interactions_interaction_id.put.html)

特殊 Intents 的公开权限说明要求按应用授权范围订阅，未授权可能导致连接关闭。
本轮没有验证当前机器人是否已有互动回调权限，因此先做 type=2 指令按钮，不能据 SDK 默认值
断言本应用一定可收所有互动。
[Intents 与权限](https://bot.q.qq.com/wiki/develop/api-v2/dev-prepare/event-emit/payload.html)

## 5. 富媒体与引用

群和单聊分别上传，获取 `media.file_info` 后以 `msg_type=7` 发送；不要跨场景复用。
选择 `srv_send_msg=false` 可保留现有被动回复和发送 ACK 流程；true 会直接发送并消耗主动额度。
支持 URL 转存、本地小文件上传和分片上传。文件信息有 `ttl`，过期后需重新上传。
首版图片优先 PNG/JPEG、视频 MP4；语音字段明确包含 SILK/MP3/WAV，但概览与格式限制表不完全一致，
应以实测成功的编码作为应用白名单。不能把“作为文件上传成功”当成“按语音条/图片正常展示”。
[富媒体概述](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/rich-media.html)、
[群上传接口](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_groups_group_openid_files.post.html)、
[单聊上传接口](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_files.post.html)

引用原消息依赖 `message_reference.message_id`，来自入站 `message_scene.ext` 的 `msg_idx`，
或已发送消息 ACK 的 `ext_info.ref_idx`。它与用于被动回复额度的入站 `msg_id` 不同。主动推送也传递该引用字段，并保留配置的 Markdown 和按钮；不会为了引用而复用过期的被动 `msg_id`。
输入中的引用、聊天记录、卡片都只是数据，不赋予命令执行或记忆确认权限。

### 群成员原生 @

QQ 官方的[文本交互说明](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/trans/text-chain.html)
明确群聊的文本、图文、Markdown 支持 `<qqbot-at-user id="MEMBER_OPENID" />`；
旧 `<@userid>` 协议将弃用。`@全部成员` 仅适用于文字子频道，不据此开放QQ群群发提醒。

ClawMingle 用结构化 `mentions` 选择当前群的稳定成员别名与目标文字气泡。
程序核对当前请求来源和同群成员 ID，持久保存发送意图，再在发送时生成原生标签；
普通昵称、代词和正文内手写标签不直接触发 @。用户明确要求提醒时才选用，
对象有歧义则询问；昵称变化不改变成员 ID。私聊禁用该能力。
引用与 Markdown 分两条时仅正文携带 @，不会因拆分重复提醒。
本地模拟验证覆盖主动/被动、文本/Markdown、引用兼容和恢复；实际客户端展示及
接收方提醒仍需群内验证，平台接受不等于对方已读或设备一定弹出通知。

## 6. C2C 流式与发送确定性

原生流式使用 `POST /v2/users/{user_openid}/stream_messages`，支持 text/markdown。
首片获得流 ID，后续保持同一流、递增 `index`，完成时 `input_state=10`。
协议支持默认 append 和显式 replace；replace 仍必须保留已经实际发出的前缀，不能任意改写已发内容。
响应可提供剩余长度。接口字段与普通分泡发送不同，应单独维护状态机。
[流式协议](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_users_user_openid_stream_messages.post.html)

群聊保持完整生成后分泡。格式降级只在收到明确“未发送”的格式/权限错误时进行：
例如 Markdown 不可用时降为纯文本，按钮参数错误时移除按钮，并记录能力状态。
请求超时、连接中断或未知发送结果仍按既有 Unknown 逻辑处理，不能盲目再发一遍纯文本。
内容违规、限频等错误也不能通过格式重试绕过。

被动回复概览当前写群聊 5 分钟/5 次、C2C 60 分钟/4 次；C2C 请求字段仍残留 5 分钟描述。
在未实测较长窗口前，沿用保守的短窗口更稳。流式 SDK 的回复窗口也不能据此扩大。
[时效与额度](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/message/overview.html)

## 7. 程序菜单、设置命令与工具能力

官方已提供 `GET/POST /v2/panels`、`GET/PUT/DELETE /v2/panels/{panel_id}` 及关联对象接口。
群可以指定群，C2C 可以指定用户；面板最多 20 个，每面板最多 20 项。
`/v2/menu` 是单聊底部全局菜单，查询 GET、整体修改 PUT。
菜单和面板是发现/输入辅助，不会实现命令权限或参数处理。
[菜单入口](https://bot.q.qq.com/wiki/develop/api-v2/server-inter/menu-panel/)、
[创建面板](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_panels.post.html)、
[修改菜单](https://bot.q.qq.com/wiki/develop/api-v2/autogen/api/v2_menu.put.html)

现有 mode/model/help/status/ping/cancel/reset 等命令可以增加 Markdown 菜单与按钮展现。
修改类行为统一进入已有后端命令权限路径，按钮和手输命令不能拥有两套不同权限。
未来联网、技能列表/启用及安装应由确定的命令白名单与作用域处理：

- 模型可提出“搜索”“使用技能”的需求，真正执行由应用的能力开关、固定工具入口和权限校验决定。
- 当前隔离运行时禁用宿主 shell/apps/MCP/自动技能等入口；消息排版升级不会自动启用联网或技能。
- 安装只作用于机器人独立身份与配置目录，固定可信来源/版本；不能为了装技能导入个人 Codex home、
  个人工作区、全局凭据或放开任意 shell。群普通消息和模型输出不能动态注册新管理命令。
- 本次研究没有安装任何技能或改动工具权限。实际执行能力需另做实现及隔离验证。

## 8. 保留结构化 segments，避免把所有输入压成文本

建议在现有消息模型上增加可默认缺省的结构化字段，保留旧 `text` 为展示/文本模型降级视图。
不直接持久化整段原始事件，继续丢弃 scene `auth_token`、连接秘密及无关未知字段。

```text
MessageEnvelope
  scope: group | c2c
  conversation_ref, sender_ref, message_ref, timestamp
  segments: Segment[]

Segment
  text       { text }
  mention    { member_ref, display?, is_bot }
  image      { asset_ref, mime, filename?, width?, height? }
  audio      { asset_ref, mime, wav_asset_ref?, asr_reference? }
  video      { asset_ref, mime, filename? }
  file       { asset_ref, mime?, filename?, size? }
  card       { card_kind, title?, description?, source?, url?, preview_asset_ref? }
  quote      { message_ref?, sender_ref?, segments }
  forwarded  { messages }                 # 递归深度、条目数、总字节有界
  unknown    { wire_type, summary? }       # 不附整份未经筛选的 raw payload

OutgoingMessage
  format: text | markdown | media
  text_or_markdown?, media_asset_ref?, reply_reference?
  keyboard?: rows<Button>

Button
  id, label, visited_label?, style
  action: command | callback | link
  command_key?/arguments?                 # 应用白名单，非模型任意脚本
  callback_action_ref?                    # 服务端绑定主体/会话/版本/有效期
  allowed_user_refs?, requires_admin?
```

模型只生成内容段或选择已注册 action key；真实 OpenID、文件路径、模板 ID、授权范围和按钮
回调状态由应用填充。资产引用解析、下载和 MIME/大小验证在应用层完成，不能把不可信 URL 或路径
直接变成模型可访问的本机目录。首版可以保留媒体/卡片元数据而暂不处理全部内容，必须向用户说明
尚未解析的附件，不能假装已经看图、听语音或读取文件。

## 9. 建议交付小步

1. 保留文本主链路，新增选择 text/markdown 的发送构造器及明确拒绝时的降级；用当前机器人
   验证两种场景的标题、列表、链接和引用。先为 `/help`、`/status`、`/mode`、`/models` 生成格式化回复。
2. 增加动态 type=2 指令按钮，后台固定命令映射及同一路径权限；群端验证填框，C2C 再验证自动发送。
3. 增补入站 typed segments，优先保留引用、卡片摘要和图片/文件/语音/视频元数据；保持角色和来源。
4. 接通图片上传/发送与安全的附件读取，再逐项验证语音、视频、文件。只启用通过验收的格式。
5. 再实现 type=1 回调、互动 ACK/去重/过期，以及 C2C 原生流式；任何网络未知结果保持显式状态。

记忆不在此阶段实施。最新产品边界是本机 confirmed 为权威，群文档用于交互临时草稿，
每日 review 后合并，首版群文档权限有限；本文件不沿用先前“confirmed 档案直接放群文档目录”的提案。

## 10. 官方资料的冲突与未验证项

- 官方新 Markdown 页明确群/C2C 全机器人开放；官方 Node SDK 使用指南仍有“Markdown 需特殊审核”
  的旧提示。以明确能力更新的文档为主，实际发送失败时显示错误和降级，不把 SDK 开关当平台事实。
- 交互文档写约 3 秒 ACK，SDK 示例写 5 秒；实现按 3 秒以内处理。
- SDK 通用发送方法可透传 `ark`/`embed` 并推导 `msg_type`，只说明封装不阻拦字段，
  不能推翻官方能力表或证明当前群/C2C 可以发送这些类型。
- SDK 默认使用 replace、500 ms 节流并限制最小 300 ms；协议页同时支持 append。
  使用时明确传模式，不把 SDK 选择当成协议只存在一种模式。
- 尚未验证本应用按钮回调权限、所有客户端效果、旧模板兼容、URL 限制及各类媒体编码。
  也未对接收消息的全量推送开关、机器人角色或应用个体权限进行登录核验。

参考：[官方 SDK 使用指南](https://github.com/tencent-connect/qqbot-nodejs/blob/main/USAGE.md)、
[通用消息发送封装](https://github.com/tencent-connect/qqbot-nodejs/blob/main/src/protocol/api/messages.ts)、
[流式封装](https://github.com/tencent-connect/qqbot-nodejs/blob/main/src/streaming.ts)。

部分官方文档路径在浏览工具中返回不可读取，本轮通过官方站点的公开 HTML 直接读取正文复核；
没有用第三方文档代替关键能力结论。


## 当前图像输入接线

新消息中的 PNG、JPEG、GIF、WebP 可作为真实图像输入传给模型，而不只是附件类型描述。
图片候选与文字使用同一 Codex 消息窗口，包含静默消息，优先当前请求及其明确引用，再取近期上下文图。
每次最多三张、每张 2 MiB；QQ 官方图床签名地址仅保留在私有传输记录，提示和日志不包含地址。
相同来源缓存优先，过期或下载失败明确标记；不能用任意近期图替换失败的引用图。
图片暂存于当前会话隔离工作区，绑定原消息来源；详细边界见[消息上下文](message-context.md)。
任意文件解析、音视频直接分析和卡片发送仍须分别接入。

## 下一阶段：图片与语音收发

最初设计见[继续开发文档第21节](continuation-design.md#21-图片语音输入输出下一阶段设计)，
最新范围和实现见[第22节](continuation-design.md#22-语音输入与图片收发首版)。
QQ 官方语音附件的 `content_type` 为 `voice`，并定义可缺失的 `voice_wav_url`、
`asr_refer_text`；首版只使用 QQ 的参考转写，缺失时不能假装听懂原始音频。
群/C2C 图片上传和带回执的发送已接入 PNG/JPEG，每张2 MiB、每轮最多两张；客户端实测另行记录。
当前不做语音输出。共享主进程继续关闭原生画图；独立的 Codex 账号生图/改图及条件引用回复
见[继续开发文档第23节](continuation-design.md#23-codex-账号生图与条件引用回复)。用户已确认原图回传通过，
新生图与原生引用的客户端效果分别验收。

## 引用与 Markdown 的客户端兼容

发送前将普通引用聊天转为原生引用的文本消息。确需 Markdown（含链接、排版或按钮）时，
将原生引用前导与 Markdown 正文分成两个持久化气泡，正文保留按钮且不携带 message_reference。
优先用原回答的首个短纯文本段落作前导，不能拆时使用简短引用说明。二者各自记录发送确认，
前导送达不代表正文已送达；未知结果不自动重复发送，恢复后不重放已确认部分。

依据：[腾讯官方 SDK 的消息封装](https://github.com/tencent-connect/qqbot-nodejs/blob/main/src/protocol/api/messages.ts)
避开 Markdown 与 message_reference 同时发送。QQ API 接收成功不证明所有客户端已正常渲染，
iOS/Android 的显示效果仍需客户端验收。
