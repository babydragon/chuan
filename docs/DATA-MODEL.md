# 数据模型与事件分类

> **本文是 Journal、投影表、事件种类的唯一出处。** 契约见 `INTERFACES.md`；时间与重试见 `OPS.md`；术语见 `GLOSSARY.md`。

---

## 1. 层 → 模块 → crate 映射（唯一编号体系）

**L1–L10 是唯一的层编号**；`DESIGN.md` 的 ①–⑧ 是"模块组"的展示顺序，不是层号。

| 层 | 模块组 | 职责 | crate | 目录 |
|---|---|---|---|---|
| L1 接入 | ① | 收发、鉴权、流式回显 | `harness-channels` | `channels/telegram`, `channels/cli` |
| L2 状态 | ⑦ | Journal、投影、迁移、FTS 维护 | `harness-store` | `store/` |
| L3 循环 | ② | Turn 状态机、预算、取消、事件流 | `harness-kernel` + `harness-engine` | `kernel/turn`, `engine/` |
| L4 上下文 | ③ | 组装、压缩、注入 | `harness-kernel` | `kernel/context` |
| L5 工具面 | ④ | 注册表、内置工具、技能、MCP(预留) | `harness-kernel` + `harness-capabilities` | `kernel/tools`, `capabilities/` |
| L6 执行 | ⑤ | 沙盒、workspace、资源限制 | `harness-capabilities` | `capabilities/sandbox` |
| L7 治理 | ⑥ | 策略、审批、副作用、预算 | `harness-kernel` | `kernel/governance` |
| L8 记忆 | ⑦ | doc 容器；语义在应用代码里 | `harness-kernel` + `apps/*` | `kernel/doc`, `apps/home/src/memory` |
| L9 编排 | ② | 子 agent、任务调度 | `harness-kernel` | `kernel/orchestration` |
| L10 可观测 | ⑧ | trace、成本、告警 | `harness-observe` | `observe/` |

`apps/*` = 薄二进制 crate + 数据目录（`config/`、`skills/`、`prompts/`、`evals/`）；"记忆语义在应用层"指代码在 `apps/home/src/memory.rs`，数据仍走内核 `doc` 容器。

---

## 2. Journal：状态唯一真相

```sql
CREATE TABLE journal (
  conv_id    TEXT    NOT NULL,
  seq        INTEGER NOT NULL,
  run_id     TEXT    NOT NULL,
  turn_id    TEXT,
  kind       TEXT    NOT NULL,
  payload    JSON    NOT NULL,
  meta       JSON,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (conv_id, seq)
);
CREATE INDEX journal_kind ON journal(kind, created_at, conv_id, seq);
```

### 2.1 seq 分配

```sql
-- 在同一写事务内执行；单写者保证无并发
INSERT INTO journal (conv_id, seq, ...)
VALUES (?1, (SELECT COALESCE(MAX(seq),0)+1 FROM journal WHERE conv_id=?1), ...);
```

- 会话内单调唯一；**空洞只来自整事务回滚**，不来自并发。
- 非全局主键；跨会话顺序不承诺，排序一律 `ORDER BY created_at, conv_id, seq`。

### 2.2 Run 与 Turn

- **Run**：一条入站输入触发的完整生命周期；同一 `run_id` 贯穿其所有事件。
- **Turn**：一次模型请求 + 该轮工具调用，同属一个 run。
- **终止由 `run.finished` 唯一表示**；`turn.finished` 只表示"这一轮结束，可能还有下一轮"。

---

## 3. 事件分类

| kind | 何时写 | payload 关键字段 | 写者 |
|---|---|---|---|
| `run.started` | 入站消息归一化后 | `text`, `media[]`, `actor`, `channel`, `dedupe_key` | kernel |
| `turn.started` | 每轮开始 | `turn_no` | kernel |
| `turn.finished` | 每轮结束（run 可能继续） | `turn_no`, `stop_reason` | kernel |
| `msg.assistant` | 模型响应完整落库 | `content[]`, `tool_calls[]`, `finish_reason` | kernel |
| `msg.tool_result` | 工具结束后 | `call_id`, `ok`, `output_preview`, `artifact_refs[]` | kernel |
| `msg.synthetic` | 模型不可见但影响上下文的插入 | `reason`(`repair`/`no_result`/`compaction_head`), `content` | kernel |
| `tool.call_started` | 决定调用时（**尚未执行**） | `call_id`, `name`, `args`, `effect`, `idem_key` | kernel |
| `tool.call_dispatched` | **真正执行前的最后一刻** | `call_id` | kernel |
| `tool.call_finished` | 执行结束 | `call_id`, `status`, `duration_ms`, `truncated` | kernel |
| `tool.call_abandoned` | 恢复/取消判定不可续 | `call_id`, `reason`(`crash`/`approval_expired`/`cancelled`) | kernel |
| `approval.requested` | 发起审批（卡片发出后另记 `card_sent`） | `approval_id`, `call_id`, `action`, `scope`, `ttl_s` | kernel |
| `approval.card_sent` | 卡片投递成功 | `approval_id`, `msg_ref` | kernel |
| `approval.resolved` | 点击/超时/取消 | `approval_id`, `decision`, `by?`, `consumed_at` | kernel |
| `task.enqueued` | 建任务 | `task_id`, `kind`, `due_at`, `payload` | kernel |
| `task.leased` | 抢到租约 | `task_id`, `lease_until`, `attempt_no` | kernel |
| `task.finished` | 完成/失败/取消 | `task_id`, `status`, `result_ref` | kernel |
| `run.finished` | run 终态（唯一终止点） | `status`(`done`/`failed`/`cancelled`), `stop_reason` | kernel |
| `usage.recorded` | 每次模型调用 | `model`, `prompt_tokens`, `completion_tokens`, `cost_est` | kernel（engine 只返回 usage） |
| `artifact.created` | 产物落盘 | `artifact_id`, `path`, `kind`, `bytes`, `sha256` | kernel |
| `ctx.compacted` | 压缩发生 | `up_to_seq`, `summary_ref`, `tokens_before/after` | kernel |
| `doc.upserted` | 文档写入 | `doc_id`, `doc_kind`, `scope`, `op`, **`payload`, `version`, `target?`** | kernel（代表应用执行） |

**原子性约束**

1. `msg.assistant` + 同轮 `tool.call_started` 同事务。
2. `tool.call_finished` + `doc_writes` 产生的 `doc.upserted` + `artifact.created` + `msg.tool_result` 同事务（可幂等重试）。
3. `expire_approvals` 必须同事务追加 `approval.resolved(Expire)` + `tool.call_abandoned(ApprovalExpired)`。

**`doc.upserted` 必须带 `payload/version`**，否则投影删掉后无法重建——这是 `rebuild` 验收的前提。

---

## 4. 投影表（同事务维护，可重建）

| 表 | 用途 | 来源事件 |
|---|---|---|
| `tool_calls` | 调用状态、幂等键查找（CAS） | `tool.call_*` |
| `tasks` | 调度与租约（CAS） | `task.*` |
| `approvals` | 审批状态、单次消费、**卡片 msg_ref** | `approval.*` |
| `docs` | 记忆/配置/画像（含 payload 与 version） | `doc.upserted` |
| `usage_rollup` | 日/月成本 | `usage.recorded` |
| `artifacts` | 产物索引 | `artifact.created` |
| `docs_fts`（未来） | FTS5 检索 | `doc.upserted` |
| `chunks` / `chunks_fts` / `doc_vectors`（未来） | 知识库 | ingest 管道 |

**判断标准**：删掉全部投影表后 `rebuild`，状态完全恢复。做不到就是 journal 缺字段。

---

## 5. 消息分页

```rust
pub struct MessagePage {
    pub msgs: Vec<Message>,
    pub next: Option<Seq>,     // 下一页起点；None 表示到底
    pub truncated: bool,
}

async fn messages(&mut self, conv: ConversationId, from: Seq, max_turns: u16)
    -> Result<MessagePage, StoreError>;
```

语义：

- 边界 = `run.started` **或** `turn.started`。`from` 对齐到首个 `seq >= from` 的边界；`run.started` 映射为 `Role::User`，保证历史里不会丢用户那句话。
- `max_turns` 数轮，不数事件；缺省 20，上限 200。
- `next` = 本页最后一轮的 `turn.started.seq + 1`。

这样调用方永远拿到完整的轮，不会切开工具上下文。

---

## 6. 恢复判定（唯一规则表，按顺序匹配）

| 条件 | 判定 |
|---|---|
| `run.finished` 存在 | run 已终结，无需恢复 |
| `call_finished` 存在 | 正常 |
| `call_started` 且 `call_dispatched` **不存在** | **从未执行** → `abandoned(approval_expired/cancelled)` 或直接重跑（与 effect 无关，因为没跑过） |
| `call_started` + `approval.requested`，无 `resolved`，审批未过期 | 继续等待（不判崩溃） |
| `call_started` + `approval.requested`，已过期 | `abandoned(approval_expired)` + 告知主人 |
| `call_dispatched` 无 `finished`，`effect=pure` | 重跑 |
| `call_dispatched` 无 `finished`，`effect=idempotent` | 先核对（如 `lpstat`），再决定重跑/放弃 |
| `call_dispatched` 无 `finished`，`effect=external` | `abandoned(crash)`，**永不自动重发** |
| `msg.assistant` 已提交、无 `call_started` | 该轮未开始执行，重新进入循环 |
| `turn.started` 无 `msg.assistant` | 流式中：丢弃半截，重发该轮 |

`call_dispatched` 的存在与否，是"从未执行"与"执行后丢失结果"的唯一区分依据。

---

## 7. 时间

| 用途 | 存储 | 说明 |
|---|---|---|
| 事件时间 | `created_at` UTC ms | 展示与跨会话排序 |
| 任务到期 | `due_at` UTC ms | `now >= due_at` 补跑 |
| 租约 | `lease_until` UTC ms + `owner` | 可持久化；跳变处理见 `OPS.md §6` |

---

## 8. 媒体、产物与回放

- 媒体与产物存 `artifacts/`（sha256 寻址），journal 只存引用。
- tx3 失败会留下**无 `artifact.created` 的孤儿文件**：启动时按 journal 反查，无主的删除。
- TTL 清理后回放降级为 **tombstone**（`[媒体已过期: <sha256 前 8 位>]`），不报错、不删事件。

---

## 9. 迁移与重建

- `PRAGMA user_version` 单调递增，只前向；迁移前自动 `VACUUM INTO`。
- `rebuild` 是验收工具，不是日常路径：删投影表 → 按 `(created_at, conv_id, seq)` 折叠 → 比对结果。
- 不支持二进制回滚（旧代码读不了新 schema）。
