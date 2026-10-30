# 语音消息（入站）

> 状态：**方案稿，未实现**。本文只覆盖**语音条入站**；出站语音与音频文件的特殊解析都不做。
> 与 `WEIXIN.md`（微信 iLink 实测事实）、`DECISIONS.md` D17/D18/D25 配套阅读。
> 最后更新：2026-10-30。

## 0. 范围

| 做 | 不做 |
|---|---|
| 用户按住说话发的**语音条**能进到模型面前 | 出站语音（TTS / SILK 编码 / ffmpeg 转码） |
| 模型知道"这是一条语音条，需要先转写" | 音频文件（mp3 等）的特殊解析——**当普通附件，和 PDF/图片一样由模型自己决定** |
| 微信侧优先用平台自带转写 | 视频、贴纸、位置等其它消息类型 |

判据来自需求：**只有"用户对着我说话"需要被听懂**；音频文件是资料，不是对话。

---

## 1. 平台事实：语音条与音频文件是两个独立字段

### 1.1 Telegram〔官方文档〕

消息对象里三者**互斥**（同一条消息只会有其中一个）：

| 字段 | 是什么 | 关键属性 |
|---|---|---|
| `message.voice` | **语音条**（按住录音） | `file_id`、`duration`、`mime_type`（`audio/ogg`）、`file_size`、`waveform` |
| `message.audio` | **音频文件**（按音乐发） | `file_id`、`duration`、`performer`、`title`、`file_name`、`mime_type` |
| `message.document` | 任意文件（含直接拖进来的 mp3） | `file_name`、`mime_type` |

- 下载：`getFile` → `https://api.telegram.org/file/bot<token>/<file_path>`，**bot 上限 20MB**，链接至少 1h 有效〔官方〕。语音条通常远小于此。
- **Bot API 没有转写**：语音转文字是客户端给 Premium 用户的功能，不进 API〔推断，基于 API 无相关字段〕。
- 语音条**没有 caption**；音频文件可能有。

### 1.2 微信 iLink〔官方协议文档 + 官方插件源码〕

消息内容是一串 `item_list`，靠 `type` 区分：

| `type` | 内容 |
|---:|---|
| 2 | `image_item` |
| **3** | **`voice_item`** |
| **4** | **`file_item`** |
| 5 | `video_item` |

`voice_item` 字段：`text`（**转写文本**）、`encode_type`、`sample_rate`、`playtime`、`media`（CDN 引用 + `aes_key`）。

官方插件的行为（`src/messaging/inbound.ts::bodyFromItemList`）：

```
text_item.text 有值 → 用它当消息正文
voice_item.text 有值 → **直接用它当消息正文**（这就是"微信自带转写"）
其它媒体 → 只给一个标签（[语音] / [图片] / …），文件落盘后当附件
```

- 音频格式是 **SILK**；官方用 `silk-wasm` 解成 WAV（`SILK_SAMPLE_RATE = 24000`），解码失败就退回原始 `.silk`〔官方插件 `src/media/silk-transcode.ts`〕。
- 媒体下载：优先 `full_url`，否则按 `encrypt_query_param` 拼 CDN 地址；**AES-128-ECB + PKCS#7** 解密，密钥来自 `media.aes_key`。官方实现会**跳过缺少 `media.aes_key` 的语音**〔官方协议文档〕。

### 1.3 结论：能区分

| | 语音条 | 音频文件 |
|---|---|---|
| Telegram | `message.voice` | `message.audio` 或 `message.document` |
| 微信 | `item.type == 3` | `item.type == 4`（在微信里发 mp3 走 file_item —— ⚠️ 未实测） |

两条通道都在协议层分得干净，**不需要靠 mime 猜**。

---

## 2. 现状缺口（代码事实，2026-10-30）

| # | 事实 | 位置 |
|---|---|---|
| G1 | `Message` 只有 `text / photo / document / caption`，**没有 `voice` / `audio` / `video`** | `crates/harness-channels/src/telegram/api.rs:19` |
| G2 | `media_picks()` 只认 document 和 photo → 语音条**不产生附件** | 同文件 `Message::media_picks` |
| G3 | 无内容守卫：`text` 为空且无附件时，`handle_run` **照样跑一整轮 LLM** | `crates/harness-kernel/src/actor.rs`（`run()` 只按 `kind` 分流，无空内容判断） |

后果：**现在给 bot 发一条语音，它会空跑一轮**（上下文里只有一个时间戳），既花钱又不回应。发一个 mp3、发一段视频同理（G1 对三者一样成立）。

> 这三条是当前最该先修的：与"支不支持语音"无关，先把"空跑"堵住。

---

## 3. 最小改动集（方案稿，未定稿）

只做语音条入站，四步：

| # | 改动 | 量级 | 依赖 |
|---|---|---|---|
| 1 | `Message` 加 `voice: Option<Value>`（顺带 `audio`，让音频文件能当普通附件） | ~2 行 | 无 |
| 2 | `media_picks()` 加分支：`voice` → 名字 `voice_<message_id>.ogg`、mime 取 `voice.mime_type`；`audio` → `file_name` / `mime_type` | ~20 行 | 无（复用 `download_file` + `save_media`） |
| 3 | 空内容守卫：`text` 空且无附件 → 回一句"这类消息我暂时处理不了"，**不进 LLM** | ~5 行 | 无 |
| 4 | 让模型知道"这是语音条，需要先转写"：`Attachment` 加 `kind: Voice \| File`（构造点只有 2 处），`attachment_lines()` 输出改成「语音条（12s，需转写）」 | ~10 行 | 无 |

**合计约 37 行，无新增依赖，内核零改动。**

第 4 步为什么不用 `InboundKind::Voice`：`InboundKind` 承载的是**路由**语义（Text / Command / Media / Callback），加一个 `Voice` 会让"算不算前台活动""要不要显示 typing"这类判断多一个分支；而"这是语音条"属于**内容属性**，放在 `Attachment` 上更贴合。（好消息：`InboundKind` 没有 exhaustive match，真要加也不破坏编译。）

### 3.1 微信侧

- 语音条是 `type=3`，**零额外判断**；媒体下载（CDN + AES-128-ECB）是图片/文件/语音**共用**的一条路径，语音不额外加成本。
- **优先用 `voice_item.text`**：有值就直接当正文，连 SILK 解码都省掉。
- 只有 `text` 为空时才需要下载 + 解码 SILK —— 是否要做，取决于 §4 的 U1。

### 3.2 转写（本方案不含）

通道只负责"让语音变成模型看得见的东西"，转写方式另定：

- **技能方案**：`transcribe` 技能走 `http` 服务 + 宿主侧凭据注入（D18），Rust 零改动；
- **通道内 ASR**：约 120–150 行 + 配置 + 每轮 1–3s 延迟。

倾向先技能（D17：领域能力下沉），不够再升级。

---

## 4. 未验证清单 ⚠️

| # | 项 | 为什么重要 | 怎么验 |
|---|---|---|---|
| U1 | **微信 `voice_item.text` 是否每条语音都有、质量如何** | 有 → 连 SILK 解码都不用做；没有 → 必须解 SILK 或走 ASR | 用探针 `listen` 发几条语音看输出（它已经会打印 `voice_item.text`） |
| U2 | 微信语音是否**必须** AES 解密；`media.aes_key` 是否总存在 | 官方实现会跳过缺 key 的语音，说明存在这种消息 | 下载一条语音看 `media.aes_key` 是否为空 |
| U3 | SILK 参数（24kHz / mono）是否对所有语音成立；`encode_type` 取值含义 | 决定解码器参数 | 取几条语音比对 `sample_rate` / `encode_type` |
| U4 | TG `voice.mime_type` 是否稳定为 `audio/ogg` | 决定落盘扩展名与 mime | 发一条语音看日志 |
| U5 | TG 20MB 下载上限对语音无影响 | 只是确认 | 长语音（>5min）实测 |
| U6 | 微信里发 mp3 是否走 `file_item`(4) 而非 `voice_item`(3) | 决定"音频文件"路径是否正确 | 发一个 mp3 看探针输出 |
| U7 | 空内容守卫的边界：只有 caption 的图片、只有 sticker 的消息 | 别把正常消息误判成"处理不了" | 逐个类型发一遍 |

U1 最关键，而且**今天就能验**：探针的 `listen` 已经会打印 `voice=` 或 item 类型，不需要写代码。

---

## 5. 复现 / 验证方法

```bash
cd /tmp/opencode/chuan-wx-probe
cargo run -- listen          # 另一个终端保持运行
# 手机上给 bot 发一条语音
```

- 打印出 `voice=Some("...")` → U1 成立（微信带转写）
- 打印出 `voice=""` 或 `voice=None` → U1 不成立，必须解 SILK
- 打 `msgs=0` 一直没有 → 语音消息可能压根不下发（那更是结论）

> 探针目前**只打印、不下载媒体**，所以 U2/U3 需要先给探针加一条"下载并解密"的分支（约 60 行），或等媒体通道实现时顺手验。

---

## 6. 变更记录

| 日期 | 变更 |
|---|---|
| 2026-10-30 | 首版：范围、两平台事实、三处现状缺口、四步最小改动集、7 条未验证项 |
