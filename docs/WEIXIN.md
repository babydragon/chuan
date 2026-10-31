# 微信通道（ClawBot / iLink）实测事实

> 状态：**调研中**。本文只记录事实，不含取舍与接入方案——结论形成后再开 ADR。
> 实测时间：2026-10-29。实测机：开发机（x86_64 Linux），**不是目标板**；手机微信 iOS。
> 连接方式：直连（不走代理）。
> 来源标注：`〔官方〕` = `github.com/Tencent/openclaw-weixin` 的 `docs/protocol_zh_CN.md`；
> `〔实测〕` = 本轮亲自跑出来的；`〔未验证〕` = 只有二手资料或只有官方描述、我方未证实。

## 0. 范围

- **覆盖**：微信 ClawBot（协议名 iLink），个人号 Bot 通道。
- **不含**：公众号、微信客服、企业微信、第三方 hook（wechaty / padlocal / wcferry 等）。这些只有二手资料，未实测，不写入事实文档。
- 本文与 `PLATFORM.md` 的区别：`PLATFORM.md` 是**目标板**实测；本文是**外部服务行为**实测，与板子无关。

---

## 1. 产品形态

| 事实 | 来源 |
|---|---|
| ClawBot 是**微信客户端内置插件**（我 → 设置 → 插件），2026-03 上线 | 〔实测〕可见入口 + 二手报道 |
| 扫码后服务端同时返回 `ilink_bot_id`（bot 身份，形如 `xxx@im.bot`）与 `ilink_user_id`（扫码人） | 〔官方〕协议文档 `get_bot_qrcode` 返回体 |
| 对话发生在微信内的「微信ClawBot」会话页，不是接管个人微信号，也不是小号 | 〔实测〕截图 |
| 官方 npm 包 `@tencent-weixin/openclaw-weixin` 2.4.10（2026-10-11），MIT，maintainers 均为 `@tencent.com` | 〔实测〕npm registry |
| 官方协议文档 `docs/protocol_zh_CN.md` 明确面向「实现或对接兼容微信后端的开发者」 | 〔官方〕 |
| 入站发送者 id 形如 `o9cq80_...@im.wechat` | 〔实测〕`listen` 输出 |
| **ClawBot 是"每人一个"**：没绑定过的微信账号打开插件只会被要求扫码绑定；没有分享/推荐入口，看不到别人的 bot 会话 | 〔实测观察（UI）2026-10-30〕 |
| 推论：**一个 bot 只服务扫码的那个微信号**——所以"加人"不是往一个 bot 里加，而是**每人各绑一个 bot**（每个 bot 一个 token） | 〔推论〕 |
| 一个实例可同时持有**多个 bot 的 token**：一个凭据文件、`accounts` 数组，每个 bot 一条 | 〔已实现〕`weixin-login` 按 `user_id` 合并追加；每条独立长轮询、独立游标 |

---

## 2. 传输与鉴权

全部为 HTTPS + JSON。默认 API 地址 `https://ilinkai.weixin.qq.com`〔官方〕。

### 2.1 端点

| 方法 | 路径 | 用途 | 本轮是否实测 |
|---|---|---|---|
| POST | `/ilink/bot/get_bot_qrcode?bot_type=3` | 取登录二维码 | ✅ 200 |
| GET | `/ilink/bot/get_qrcode_status?qrcode=<值>[&verify_code=<码>]` | 轮询扫码状态 | ✅ 200 |
| POST | `/ilink/bot/getupdates` | 长轮询收消息 | ✅ 200 |
| POST | `/ilink/bot/sendmessage` | 发消息 | ✅ 200 |
| POST | `/ilink/bot/getconfig` | 取账号配置 / `typing_ticket` | ✅ 200 |
| POST | `/ilink/bot/sendtyping` | 输入状态 | ✅ 200 |
| POST | `/ilink/bot/getuploadurl` | 媒体上传参数 | ❌ 未测 |
| POST | `/ilink/bot/msg/notifystart` | 客户端启动通知 | ⚠️ 已调用，未看效果 |
| POST | `/ilink/bot/msg/notifystop` | 客户端停止通知 | ⚠️ 同上 |

### 2.2 请求头〔官方〕

| Header | 值 |
|---|---|
| `Content-Type` | `application/json` |
| `AuthorizationType` | `ilink_bot_token` |
| `Authorization` | `Bearer <bot_token>`（登录类请求不带） |
| `X-WECHAT-UIN` | 随机 uint32 的**十进制字符串再 base64** |
| `iLink-App-Id` | `bot` |
| `iLink-App-ClientVersion` | 版本按 `0x00MMNNPP` 编码后的十进制字符串；2.4.8 → `132104` |
| `SKRouteTag` | 可选路由标签 |

鉴权类 POST 另带 `base_info`：

```json
{ "base_info": { "channel_version": "2.4.8", "bot_agent": "OpenClaw" } }
```

### 2.3 登录

```http
POST /ilink/bot/get_bot_qrcode?bot_type=3
{ "local_token_list": [] }
```

响应（〔实测〕原样）：

```json
{
  "qrcode": "1f22a7c9a34411678e020015934375c0",
  "qrcode_img_content": "https://liteapp.weixin.qq.com/q/7GiQu1?qrcode=1f22a7c9...&bot_type=3",
  "ret": 0
}
```

- 二维码里编码的是 `qrcode_img_content`；`qrcode` 只是状态轮询用的键〔实测〕。
- 状态值：`wait` / `scaned` / `confirmed` / `expired` / `need_verifycode` / `verify_code_blocked` / `scaned_but_redirect` / `binded_redirect`〔官方〕。
- `confirmed` 返回 `bot_token` / `ilink_bot_id` / `baseurl` / `ilink_user_id`〔官方〕。

---

## 3. 实测事实

| # | 事实 | 证据 |
|---|---|---|
| F1 | 上述端点全部可达，无代理、无特殊网络要求 | 逐条 200 |
| F2 | **`get_qrcode_status` 本身是长轮询，单次约 30s 才返回** `{"ret":0,"status":"wait"}` | 日志时间戳间隔 30.9s / 31.0s；官方文档未写这一点 |
| F3 | **发送成功响应没有 `ret` 字段**，只有 `{"message_id": <i64>}` | 全部成功记录 |
| F4 | 失败是 **HTTP 200 + 负 `ret`**：`{"ret":-2,"errmsg":"prepare failed"}` | 第 11 条 / 20000 字 |
| F5 | 部分请求返回 **HTTP 200 + 空 body**（item type 11/12） | 连续两次 |
| F6 | **同一 `client_id` 的后续消息不会更新已发出的气泡**：四条消息各自返回不同的 `message_id`，客户端只显示第一条，内容停在首帧 | 截图 + 4 条不同 `message_id` |
| F7 | 加上 `update_time_ms`（递增）仍不更新；把服务端 `message_id` 当出站 `msg_id` 传会得到 `{"ret":-1,"errmsg":"invalid request"}` | 两轮对照 |
| F8 | **item type 11/12（`tool_call_start_item` / `tool_call_result_item`）不被渲染**，微信端无任何变化 | 截图；响应为空 body（F5） |
| F9 | 单条文本：**5000 字完整到达**；4000 字分片完整到达；**20000 字被拒**（`ret=-2`），手机端无任何消息 | 用户实看 `[099]` / `[079]` 标记 + 响应 |
| F10 | **出站额度 = 每条用户消息 10 条**（含回复本身）。R1：10 成 / 第 11 条 `ret=-2`；用户再说一句后 R2+R3 = 1+9 = 10 / 第 11 条 `ret=-2` | 逐条 `message_id` 计数 |
| F11 | `sendtyping` `status=1` 生效，几秒后自动消失；`status=2` 的效果未单独验证 | 用户实看 |
| F11b | `typing_ticket` 由 `getconfig(ilink_user_id)` 返回，可缓存复用；通道已接入（rc.29） | 探针 + 实现 |
| F18 | `context_token` 可跨进程复用：通道按会话落盘到 kv，重启后仍能主动发（rc.31） | 实现 + 单测 |
| F19 | 语音入站：`voice_item.text` **确实有值**——上板发语音，bot 正确复述了转写内容；无转写时回提示 | 实现 + 上板实测 |
| F12 | `ret=-2 "prepare failed"` 是**通用拒绝码**：文本过大与额度耗尽都用它，不能据码区分 | 20000 字与配额两处都出现 |
| F13 | `message_state=1` 的文本消息照常以普通气泡渲染，没有「生成中」的特殊外观 | 截图 |
| F14 | **协议没有 reaction / 编辑接口**：唯一能"表达状态"的手段是再发一条消息，而每条出站消息占 10 条额度中的 1 条 | 端点表 + F10 |
| F15 | **`context_token` 不随 24h 过期**：拿到 24h06m 之后的旧令牌仍能成功发送 | 新旧令牌对照（旧令牌 ok / 新令牌 ok） |
| F16 | **额度不随时间恢复**：24h 后、期间没有新入站消息时发送仍 `ret=-2`；用户发一句话后立刻满额 | 跨日对照（10-29 打满 → 10-30 仍失败 → 新消息后成功） |
| F17 | **`bot_token` 无 24h TTL**：连续长轮询 24h20m 无 `-14`、无鉴权错误 | `heartbeat.log` 2622 拍 |

### 3.1 额度模型（可直接用于设计）

把 F10 / F16 合起来看，规则只有一条：**每条入站消息给 10 条出站，不随时间恢复**。
由此推出两种完全不同的用法：

| 用法 | 上限 | 是否会枯竭 |
|---|---|---|
| **对话**（用户说 → bot 答） | 每条用户消息重置为 10 条 | ❌ 不会。只要一轮回复不超过 10 条，对话是无限的 |
| **推送**（用户不说 → bot 主动发） | 只剩"最后一次入站之后"的余量 | ✅ 会。最多 10 条，之后必须等用户开口 |

**额度的作用域**：上面说的是"一条入站消息给 10 条出站"，但**没有验证是 per-conversation 还是 per-bot**（U10）——
我们只有一个会话，分不出来。回复必须带**该会话**的 `context_token`，所以按会话计更可能；
若实际是 per-bot，那"回陌生人不占主人额度"这个推断就不成立。见 §5 U10。

一句话概括：**它是聊天通道，不是推送通道。**
日更一条提醒听起来没问题，但用户 10 天不说话就彻底发不出去了；纯单向告警（用户永远不开口）从第 11 条起就死。

> 定位与接入约束见 `DECISIONS.md` D25。

### 3.2 出站消息体（实测可用）

```json
{
  "msg": {
    "from_user_id": "",
    "to_user_id": "o9cq80_...@im.wechat",
    "client_id": "probe-...",
    "message_type": 2,
    "message_state": 2,
    "context_token": "<入站消息里的 context_token>",
    "item_list": [{ "type": 1, "text_item": { "text": "你好" } }]
  },
  "base_info": { "channel_version": "2.4.8", "bot_agent": "..." }
}
```

- `context_token` 只来自入站消息；没有它发不出去〔官方〕+〔实测〕。
- 媒体发送时文本与媒体拆成两个请求，各带独立 `client_id`〔官方〕。

---

### 3.3 出站媒体（官方源码确认的实现细节）

发一个文件/图片 = **两条消息**（说明文字 + 媒体），各占 1 条额度。

```
读文件 → md5(hex) → filesize = AES-128-ECB+PKCS#7 后的密文长度
filekey = 16 字节随机(hex)；aeskey = 16 字节随机
  → getuploadurl{ filekey, media_type(1图/2视频/3文件/4语音), to_user_id,
                  rawsize, rawfilemd5, filesize, no_need_thumb: true, aeskey: hex }
  → POST 密文到 upload_full_url（优先；否则用 upload_param + filekey 拼）
     Content-Type: application/octet-stream
  → 成功判据：HTTP 200 且响应头 x-encrypted-param 非空；4xx 立即放弃，其它最多重试 3 次
     （实现只支持 `upload_full_url`；`upload_param` 拼接未做，见 §5 M2）
  → sendmessage: item.media = { encrypt_query_param: 该响应头,
                                aes_key: base64(hex 字符串),   ← 注意是"hex 的 base64"
                                encrypt_type: 1 }
     image_item.mid_size = 密文长度；file_item.file_name / len = 明文长度
```

## 4. 官方文档另有、我方未验证的部分

以下全部来自〔官方〕`docs/protocol_zh_CN.md`，本轮**未实测**：

| 项 | 官方描述 |
|---|---|
| 媒体上传 | `getuploadurl`（`media_type` 1图/2视频/3文件/4语音；`rawsize`/`rawfilemd5`/`filesize`/`aeskey`）→ **AES-128-ECB + PKCS#7** 加密 → POST 到 CDN，成功判据是 HTTP 200 且响应头 `x-encrypted-param` 非空，失败最多重试 3 次 |
| 媒体下载 | 优先 `full_url`，否则 `<cdn>/download?encrypted_query_param=<urlencoded>`；`image_item.aeskey`（hex）优先于 `media.aes_key`（base64） |
| 语音 | `voice_item.text` 携带转写文本 |
| 消息状态 | `message_state`：0 新消息 / 1 生成中 / 2 完成；`run_id` 用于关联一次生成 |
| 会话失效 | `getupdates` 返回 `ret` 或 `errcode` 为 `-14` 时，官方客户端暂停该账号轮询 1 小时 |
| 群聊 | 类型里有 `group_id`，官方明示「不保证插件支持对应功能」 |
| 长轮询 | 服务端返回 `longpolling_timeout_ms` 建议下次超时（实测约 35s） |

---

## 5. 未验证清单（⚠️）

| # | 项 | 为什么重要 | 怎么验 |
|---|---|---|---|
| ~~U1~~ | ~~`bot_token` 寿命~~ | ✅ 已验证：无 24h TTL（F17）；更长周期仍挂着观察 |
| ~~U2~~ | ~~额度窗口长度（24h？）~~ | ✅ 已验证：**没有时间窗口**，只有"每条入站给 10 条"，不随自然时间回血（F16） |
| U3 | 5000～20000 之间的确切上限 | 决定单条分片大小 | 二分（消耗额度） |
| U4 | 群聊是否可用 | 影响使用场景 | 拉群后看 `group_id` 是否下发、能否回复 |
| U5 | 媒体收发全链路 | 图片/文件是刚需 | 实发一张图 + 收一张图 |
| U6 | 空 body 200 的语义（F5） | 是"拒绝"还是"成功但无回执" | 改用能渲染的 item type 对照 |
| U7 | 额度是否计入媒体消息 / typing / `state=1` | 决定预算算法 | 逐类计数 |
| U8 | 安卓 / 低版本微信的渲染一致性 | 只在一台 iOS 上验过 | 换设备 |
| U9 | 多 bot / 多账号是否互不影响 | 多通道时可能碰到 | 再扫一个号 |
| ~~U10~~ | ~~额度是 per-conversation 还是 per-bot~~ | ✅ 不再重要：一个 bot 只有一个对话方（见 §1），两种口径等价 | — |

---

## 6. 复现方法

探针是一个独立小程序，**未入库**，位置 `/tmp/opencode/chuan-wx-probe`（Rust，reqwest + rustls，不依赖 Node）：

```bash
cargo run -- login      # 扫码，凭证写 ~/.local/share/chuan-wx-probe/state.json（0600）
cargo run -- listen     # 长轮询收消息，记录 context_token 与心跳
cargo run -- refresh    # 同 client_id 改写（F6）
cargo run -- refresh2   # 带 update_time_ms / msg_id 的改写尝试（F7）
cargo run -- long       # 5000 vs 4000×2（F9）
cargo run -- big 20000  # 超大文本（F9）
cargo run -- tools      # item 11/12（F8）
cargo run -- quota      # 额度与锚点（F10）
cargo run -- typing on  # 输入状态（F11）
```

产物：

- `~/.local/share/chuan-wx-probe/state.json` —— 凭证、游标、最近会话令牌（0600）
- `~/.local/share/chuan-wx-probe/probe-log.jsonl` —— 每次请求的响应原文（不含 token）
- `~/.local/share/chuan-wx-probe/heartbeat.log` —— 每次轮询一行，用于判断跨日存活

> 探针不在仓库里：本轮只做事实采集，等结论定了再决定是否落成代码。

---

## 7. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-10-29 | 首版：登录/收发/改写/长度/额度/typing 六项实测；列 U1–U10 未验证项 |
| 2026-10-30 | 跨日复测：F15 旧令牌 24h06m 仍可用、F16 额度不随时间回血、F17 bot_token 无 24h TTL；U1/U2 结案 |
