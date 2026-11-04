//! FTS5 检索实现（trigram 分词，支持中文子串）。
//!
//! 索引侧：`docs_fts` 自 V8 起显式 `tokenize='trigram'`，任意 ≥3 字子串可命中。
//! 查询侧：按空白切成多个词组，用 `AND` 连接（整句当一个 phrase 会因为空格而不命中）；
//! 任一词组 <3 字时 trigram 无法索引，退回 `LIKE`（trigram 下仍走索引优化）。

use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{DocId, Hit, Query, Retriever};
use rusqlite::{Connection, OpenFlags, params_from_iter};

use crate::sqlite::SqliteStore;

pub struct Fts5Retriever {
    store: Arc<SqliteStore>,
}

impl Fts5Retriever {
    pub fn new(store: Arc<SqliteStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Retriever for Fts5Retriever {
    async fn search(&self, q: Query) -> Result<Vec<Hit>, String> {
        let path = self.store.path().to_path_buf();
        let k = q.k.max(1);
        let overfetch = k * 4;
        let text = q.text.trim().to_owned();

        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;

        let mut sql = String::new();
        let mut args: Vec<String> = Vec::new();

        let tokens: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
        let long_enough =
            !tokens.is_empty() && tokens.iter().all(|t| t.chars().count() >= 3);
        let mut ranked = false;

        if long_enough {
            // trigram 分词下按词组做 phrase 查询：`"打印机" AND "型号"`。
            // 不能把整句塞进一个 phrase——中间的空格会成为字面量，必然不命中。
            let phrase = tokens
                .iter()
                .map(|t| format!("\"{}\"", t.replace('"', " ")))
                .collect::<Vec<_>>()
                .join(" AND ");
            sql.push_str("SELECT doc_id, bm25(docs_fts) FROM docs_fts WHERE docs_fts MATCH ?");
            args.push(phrase);
            ranked = true;
        } else if !tokens.is_empty() {
            sql.push_str("SELECT doc_id, 0.0 FROM docs_fts WHERE ");
            for (i, t) in tokens.iter().enumerate() {
                if i > 0 {
                    sql.push_str(" AND ");
                }
                sql.push_str("text LIKE ?");
                args.push(format!("%{t}%"));
            }
        } else {
            sql.push_str("SELECT doc_id, 0.0 FROM docs_fts WHERE 1=1");
        }

        if !q.scopes.is_empty() {
            let placeholders = vec!["?"; q.scopes.len()].join(",");
            sql.push_str(&format!(" AND scope IN ({placeholders})"));
            args.extend(q.scopes.iter().cloned());
        }
        if !q.kinds.is_empty() {
            let placeholders = vec!["?"; q.kinds.len()].join(",");
            sql.push_str(&format!(" AND kind IN ({placeholders})"));
            args.extend(q.kinds.iter().cloned());
        }
        if ranked {
            // bm25 越小越相关；以前只取了分数没排序，等于按扫描顺序返回。
            sql.push_str(" ORDER BY bm25(docs_fts)");
        }
        sql.push_str(&format!(" LIMIT {overfetch}"));

        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), |r| {
                Ok(Hit { doc_id: DocId(r.get(0)?), score: r.get::<_, f64>(1).unwrap_or(0.0) as f32 })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(rows.into_iter().take(k).collect())
    }
}
