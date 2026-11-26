# 钉钉通道调研（DingTalk）

> **状态：调研结论，暂不采用。** 原计划作为第一个适配器，但目标群属于学校组织，无法在其中创建/发布企业内部应用；
> 协议验证用 **Echo** 适配器；首个真实平台改为**微信**（见 `EXTENSIONS.md §1.8`）。
> 本文保留作为将来有自有组织时的参考。
> 事实来源：钉钉开放平台文档（2026-11 检索）。未实测的项在 §7 单列，**不要当成已成立**。

---

## 0. 结论

1. **可行，且是合适的第一个适配器**：Stream 模式是出站 WebSocket，不需要公网 IP/域名/证书，适合 NAT 后的板子。
2. **能替代 webhook 机器人的“推送”**，并且多出入站、单聊、图片/语音/文件、互动卡片。
3. **但不能完全等价替换**：webhook 是“往固定群发”，企业内部机器人是“以应用身份发到会话”，需要建应用、发布、加群、拿 `openConversationId`；纯推送场景保留 webhook 更省事。
4. **两条路可以并存**：现有 `[[service]]` + 加签继续做兜底通知；适配器上线后，交互走企业内部机器人。

---

## 1. 两种机器人

| | 自定义群机器人（webhook） | 企业内部应用机器人 |
|---|---|---|
| 接入方式 | 群设置里加机器人，拿 webhook URL + 加签 | 开发者后台建应用 → 加“机器人”能力 → 发布 |
| 会话 | **仅群聊** | **单聊 + 群聊** |
| 收消息 | **不能**（没有回调/应答机制） | 单聊直接发；群聊只有 **@机器人** 才收 |
| 出站类型 | text / markdown / actionCard / link / feedCard | text / markdown / image / audio / file / video / link / actionCard / 互动卡片 |
| 频率 | **20 条/分钟/机器人**，超限限流 10 分钟 | 受应用级接口限额；单聊批量单次 ≤20 人 |
| 公开地址 | 不需要 | Stream 模式不需要；HTTP 模式需要 |
| 适用 | 监控告警、固定群通知 | 对话入口、审批卡、文件收发 |

来源：自定义机器人接入、消息发送与接收类型、调用频率限制。

---

## 2. Stream 模式

- 机器人设置里把“消息接收模式”设为 **Stream**，钉钉与适配器之间走 WebSocket 长连接；**无需公网回调地址**。
- 官方 Python SDK：`dingtalk-stream`。机器人消息 topic：`/v1.0/im/bot/messages/get`；互动卡片回调 topic：`/v1.0/card/instances/callback`。
- 凭据：`Client ID`（即 AppKey）与 `Client Secret`（即 AppSecret），由适配器自己的 `LoadCredential` 持有。
- 事件会重投，`msgId` 可作 `dedupe_key`。

---

## 3. 能收到什么

| 场景 | 文本 | 富文本 | 图片 | 语音 | 视频 | 文件 |
|---|---|---|---|---|---|---|
| 单聊（用户直接发） | ✅ | ✅ | ✅ | ✅（带 `recognition` 转写） | ✅ | ✅ |
| 群聊（**只有 @机器人**） | ✅ | ✅ | ✅（待实测） | ❌ | ❌ | ❌ |

关键字段（文本示例）：

- `conversationId`、`conversationType`（`1` 单聊 / `2` 群聊）、`conversationTitle`
- `senderStaffId`（企业内部成员 userId）、`senderUnionId`、`senderNick`
- `msgId`、`createAt`、`msgtype`
- 图片/语音/视频/文件：`content.downloadCode`，需再调“下载机器人接收消息的文件内容”换取临时下载链接
- 语音：`content.recognition` 是钉钉侧转写文本；没有则只能拿音频文件

注意：

- **群聊里机器人拿不到未 @ 它的消息**，也不能主动拉取群历史；它只是一个“被 @ 的成员”。
- `senderStaffId` 等字段在**机器人发布上线后**才稳定返回。
- 外部群/外部成员可能没有 `senderStaffId`，只能拿到 `senderUnionId`/加密 id。

---

## 4. 能发什么

接口方式（推荐）：

| 类型 | msgKey | 备注 |
|---|---|---|
| 文本 | `sampleText` | `{content}` |
| Markdown | `sampleMarkdown` | 子集：标题/引用/粗斜体/链接/图片/列表 |
| 图片 | `sampleImageMsg` | `photoURL` 或上传后的 `mediaId` |
| 语音 | `sampleAudio` | `mediaId` + 时长；ogg/amr |
| 文件 | `sampleFile` | `mediaId` + 文件名/类型；xlsx/pdf/zip/rar/doc/docx |
| 视频 | `sampleVideo` | `videoMediaId` + 封面 |
| 链接 | `sampleLink` | |
| ActionCard | `sampleActionCard`…`6` | **按钮只能跳 URL，不能回调** |

- 单聊：`POST /v1.0/robot/oToMessages/batchSend`，单次最多 20 人。
- 群聊：`POST /v1.0/robot/groupMessages/send`，需要 `openConversationId` + `robotCode`。
- 媒体：先调“上传媒体文件”拿 `mediaId`，再发送；媒体存钉钉侧空间，不占钉盘。
- **审批/交互按钮必须用“互动卡片”**（可回调、可更新）；ActionCard 的按钮是 URL 跳转，不能当审批按钮。

### 不能做的事

- 普通机器人消息**不能编辑**（没有 Telegram 那种 `editMessage`）。流式输出要么退化成“只发最终结果”，要么用互动卡片实例更新（工作量大）。
- 主动单聊可能要求用户先给机器人发过消息（社区反馈，**未在官方文档确认**，见 §7）。
- 群聊不能收语音/视频/文件；想收这些只能引导用户私聊。

---

## 5. 能否替换现有 webhook 机器人

| 现有能力 | 适配器后 | 说明 |
|---|---|---|
| 往固定群发文本/Markdown 通知 | ✅ | 需要机器人已在群里，拿 `openConversationId` |
| 加签 webhook | ✅（改用应用凭据） | 不再用 webhook URL；也可两者并存 |
| 发图片/文件 | ✅（webhook 本来不支持） | 先上传媒体 |
| 收用户消息 | ✅（webhook 不支持） | 单聊 / 群 @ |
| 审批/确认按钮 | ✅（互动卡片） | ActionCard 不行 |
| 多群广播 | 需逐个会话发送 | 没有“所有群”语义 |

迁移步骤（一次性）：

1. 开发者后台建 **企业内部应用**，加“机器人”能力，消息接收模式选 **Stream**。
2. 申请权限：机器人消息收发、通讯录（读用户/部门，用于身份映射）、互动卡片。
3. 发布应用（否则 `senderStaffId` 等字段不稳定）。
4. 把机器人加进目标群；记录群的 `openConversationId`（从一条 @ 消息的回调里取）。
5. 在 `[identity]` 里把人的 `channels` 写成 `"dingtalk:<userId>"`（身份按发送者绑定；
   群里的回复仍然回到该群的 `conversationId`）。
6. 适配器配置 AppKey/AppSecret/robotCode，启动 unit，`systemctl restart chuan`。
7. 保留 webhook 作为兜底，验证一周后再决定是否下线。

---

## 6. 映射到 Channel 协议（D44）

| 协议字段 | 钉钉来源 |
|---|---|
| `conversation` | `conversationId`（单聊/群聊都有） |
| `actor` | `senderStaffId`，缺失时 `senderUnionId` |
| `kind` | `text` → `Text`；`richText`/`picture`/`audio`/`video`/`file` → `Media`（有文本时合并为 `Text`+附件）；卡片回调 → `Callback` |
| `dedupe_key` | `msgId` |
| `msg.id` | `msgId`（用于被动回复/引用） |
| `attachments` | `downloadCode` 换临时链接后下载，落适配器 media 目录 |
| `caps` | `edit: false`（普通消息不可编辑）、`buttons: true`（互动卡片）、`markdown: true`、`min_edit_interval_ms: 0`（不流式） |

实现建议：

- v1 只做：**单聊**的文本/图片/语音/文件 + **群 @** 的文本（群聊图片待实测）；
  出站 text/markdown/image/file；`edit=false`，只发终稿。
- v2 再做互动卡片（审批卡、流式进度、按钮回调）。
- 适配器用 Python + `dingtalk-stream`；媒体上传/下载用 OpenAPI；token 缓存与刷新由 SDK/适配器处理。

---

## 7. 未验证 / 待实测

1. **主动单聊前置条件**：社区反馈“用户必须先给机器人发过消息”，官方文档未明确；上线前先用两个账号验证。
2. **消息量配额**：错误码 `20001` 表示“消息服务已暂停”，官方说明与专业版/增购包相关；免费组织是否够用要实测。
3. **群聊能否接收图片**：官方只明确“不能 @ 语音/视频/文件”，图片未单列；需实测。
4. **互动卡片更新频率/限量**：用于流式进度前先压测。
5. **单聊 `conversationId` 的稳定性**：是否需要每次从消息里取，待验证。
6. **发送接口的应用级频率**：官方只给了工作通知与自定义机器人的数字，机器人接口的每分钟上限未找到明确值。

---

## 8. 参考

- 机器人接收消息：<https://open-dingtalk.github.io/developerpedia/docs/learn/bot/appbot/receive>
- 消息发送与接收类型：<https://open.dingtalk.com/document/development/robot-message-type>
- 机器人发送群聊消息：<https://open.dingtalk.com/document/development/the-robot-sends-a-group-message>
- 批量发送单聊消息：<https://open.dingtalk.com/document/development/chatbots-send-one-on-one-chat-messages-in-batches>
- 自定义机器人（20 条/分钟）：<https://open.dingtalk.com/document/isvapp/custom-bot-access-send-message>
- 调用频率限制：<https://open.dingtalk.com/document/isvapp/invocation-frequency-limit-1>
- Stream 协议：<https://open.dingtalk.com/document/direction/stream-mode-protocol-access-description>
