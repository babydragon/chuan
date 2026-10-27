//! 持久化：唯一允许依赖 rusqlite 的 crate。
//!
//! 设计要点（见 `docs/DATA-MODEL.md` 与 `docs/INTERFACES.md §2`）：
//! - 单写者线程持有唯一写连接；`Tx` 是它的远端句柄，commit 时才 COMMIT。
//! - 投影表与 journal 在同一事务内更新，可从 journal 重建。
//! - `seq` 在写事务内用 `MAX(seq)+1` 分配，会话内单调。

pub mod ephemeral;
pub mod fts;
pub mod schema;
pub mod sqlite;

pub use ephemeral::EphemeralStore;
pub use fts::Fts5Retriever;
pub use sqlite::SqliteStore;

pub const SCHEMA_VERSION: u32 = schema::SCHEMA_VERSION as u32;

#[cfg(test)]
mod conformance;
