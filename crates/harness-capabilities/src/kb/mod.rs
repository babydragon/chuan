//! 知识库（KB）：原件 → 页 → 检索 → 取件。
//!
//! 分层与取舍见 `DECISIONS.md` D33：
//! - **原件**按 sha256 存进文件库（与 `keep` 同一布局），是唯一不可重建的东西；
//! - **页**是检索单元，一条 `kb.page` doc，正文在 `payload.text`，由 `docs_fts` 索引；
//! - **来源**是一条 `kb.source` doc（标题 / 页数 / 正文来源）。
//!
//! 格式解析**不在这里**：PDF / Word / 扫描件由 agent 用它装好的库或系统工具抽成文本，
//! KB 只负责保存、索引、取件（不变量 15）。内核对 KB 一无所知——这里只用
//! `doc_writes` / `DocReader` / `Retriever` / `ExecutionEnv` 四个既有端口。

mod outline;
mod tools;

pub use outline::{KbOutlineSetTool, KbOutlineTool, KbSelectTool};
pub use tools::{
    KbFileTool, KbForgetTool, KbIngestTool, KbListTool, KbReadTool, KbSearchTool,
};
