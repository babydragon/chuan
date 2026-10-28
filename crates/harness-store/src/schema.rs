//! Schema 与迁移。只前向，靠 `PRAGMA user_version` 记录版本。

pub const SCHEMA_VERSION: i64 = 5;

pub const V1: &str = r#"
CREATE TABLE journal (
  conv_id    TEXT    NOT NULL,
  seq        INTEGER NOT NULL,
  run_id     TEXT    NOT NULL,
  turn_id    TEXT,
  kind       TEXT    NOT NULL,
  payload    TEXT    NOT NULL,
  meta       TEXT,
  dedupe_key TEXT,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (conv_id, seq)
);
CREATE UNIQUE INDEX journal_dedupe ON journal(conv_id, dedupe_key)
  WHERE dedupe_key IS NOT NULL;
CREATE INDEX journal_kind ON journal(kind, created_at);

-- 投影：可从 journal 重建
CREATE TABLE tool_calls (
  call_id      TEXT PRIMARY KEY,
  conv_id      TEXT NOT NULL,
  run_id       TEXT NOT NULL,
  name         TEXT NOT NULL,
  args         TEXT NOT NULL,
  effect       TEXT NOT NULL,
  idem_key     TEXT NOT NULL,
  status       TEXT NOT NULL,
  dispatched_at INTEGER,
  finished_at  INTEGER,
  preview      TEXT
);
CREATE INDEX tool_calls_conv ON tool_calls(conv_id, status);

CREATE TABLE docs (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  scope      TEXT NOT NULL,
  payload    TEXT NOT NULL,
  version    INTEGER NOT NULL,
  valid_from INTEGER NOT NULL,
  valid_to   INTEGER
);
CREATE INDEX docs_scope ON docs(scope, kind);

CREATE TABLE usage_rollup (
  row_id            INTEGER PRIMARY KEY AUTOINCREMENT,
  at                INTEGER NOT NULL,
  model             TEXT NOT NULL,
  prompt_tokens     INTEGER NOT NULL,
  completion_tokens INTEGER NOT NULL,
  cost_est          REAL NOT NULL
);
CREATE INDEX usage_at ON usage_rollup(at);

CREATE TABLE tasks (
  id      TEXT PRIMARY KEY,
  kind    TEXT NOT NULL,
  due_at  INTEGER NOT NULL,
  payload TEXT NOT NULL,
  status  TEXT NOT NULL
);
CREATE INDEX tasks_due ON tasks(status, due_at);
"#;

pub const V2: &str = r#"
CREATE TABLE kv (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
"#;

pub const V3: &str = r#"
CREATE TABLE approvals (
  id          TEXT PRIMARY KEY,
  conv_id     TEXT NOT NULL,
  call_id     TEXT NOT NULL,
  action      TEXT NOT NULL,
  scope       TEXT NOT NULL,
  status      TEXT NOT NULL,          -- pending | approved | denied | expired
  card_chat   TEXT,
  card_msg    TEXT,
  expires_at  INTEGER NOT NULL,
  created_at  INTEGER NOT NULL,
  resolved_at INTEGER
);
CREATE INDEX approvals_pending ON approvals(status, expires_at);
"#;

pub const V4: &str = r#"
CREATE VIRTUAL TABLE docs_fts USING fts5(
  doc_id UNINDEXED,
  scope UNINDEXED,
  kind UNINDEXED,
  text
);
"#;

pub const V5: &str = r#"
-- 用量归属（D20）：能按会话 / run / 主体（主 or 子 agent）拆开。
-- 历史行没有 conv 信息，agent 一律记为 main。
ALTER TABLE usage_rollup ADD COLUMN conv_id TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_rollup ADD COLUMN run_id  TEXT NOT NULL DEFAULT '';
ALTER TABLE usage_rollup ADD COLUMN agent   TEXT NOT NULL DEFAULT 'main';
CREATE INDEX usage_agent ON usage_rollup(agent, at);
"#;

pub const MIGRATIONS: &[(i64, &str)] = &[(1, V1), (2, V2), (3, V3), (4, V4), (5, V5)];
