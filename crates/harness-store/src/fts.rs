//! FTS5 检索实现（trigram 分词，支持中文子串）。

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

        if text.chars().count() >= 3 {
            sql.push_str("SELECT doc_id, bm25(docs_fts) FROM docs_fts WHERE docs_fts MATCH ?");
            // trigram 分词下，把整句作为一个短语查询
            args.push(format!("\"{}\"", text.replace('"', " ")));
        } else if !text.is_empty() {
            sql.push_str("SELECT doc_id, 0.0 FROM docs_fts WHERE text LIKE ?");
            args.push(format!("%{text}%"));
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
