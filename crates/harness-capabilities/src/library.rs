//! 文件库（KB 最小版）：把工作区里的产物长期存下来，可检索、可取回。
//!
//! 三个数据平面里的"资料"层：`/ws` 是草稿（7 天清理），文件库是永久的。
//! 索引走 doc（`kind=file`），内容按摘要去重落盘。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, DocId, DocOp, DocWrite, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};

use crate::tools::fs::resolve;

const MAX_BYTES: u64 = 50 * 1024 * 1024;

fn digest(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn mime_of(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("csv") => "text/csv",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("txt") | Some("py") | Some("sh") | Some("rs") | Some("toml") => "text/plain",
        _ => "application/octet-stream",
    }
}

fn safe_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, '\0' | '/' | '\\'))
        .collect();
    if cleaned.trim().is_empty() {
        "file".into()
    } else {
        cleaned
    }
}

/// `keep(path, name?, tags?)`：把文件存进文件库。
pub struct KeepTool {
    dir: PathBuf,
}

impl KeepTool {
    pub fn new(dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { dir: dir.into() })
    }
}

#[async_trait]
impl Tool for KeepTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "keep".into(),
            description: "把一个工作区文件长期保存到文件库（`/ws` 会被定期清理，这里不会）。\
内容相同的文件只存一份。适合保存生成好的文档、表格、脚本；只想发出去看看用 `send_file`。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "工作区内的路径，如 /ws/out/清单.xlsx" },
                    "name": { "type": "string", "description": "保存后的名字（默认用原文件名）" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "便于以后检索" }
                },
                "required": ["path"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 同一内容得到同一个 doc id 与同一个目录，重复保存等价
        EffectClass::Idempotent { key: "keep".into() }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 path", false))?;
        let src = resolve(&ctx.workspace.root, raw)?;
        let meta = std::fs::metadata(&src)
            .map_err(|e| ToolError::new(ErrorCode::NotFound, e.to_string(), false))?;
        if !meta.is_file() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "不是普通文件", false));
        }
        if meta.len() > MAX_BYTES {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("文件 {} MB，超过 50MB 上限", meta.len() / 1024 / 1024),
                false,
            ));
        }
        let bytes = std::fs::read(&src)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
        let d = digest(&bytes);
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .map(safe_name)
            .unwrap_or_else(|| {
                src.file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into())
            });
        let tags: Vec<String> = args
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str()).map(str::to_owned).collect())
            .unwrap_or_default();

        let rel = format!("{d}/{name}");
        let dest = self.dir.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
        }
        if !dest.exists() {
            std::fs::write(&dest, &bytes)
                .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
        }

        let id = DocId(format!("file-{d}"));
        let doc = DocWrite {
            id: id.clone(),
            kind: "file".into(),
            scope: "family".into(),
            payload: serde_json::json!({
                "name": name,
                "rel": rel,
                "size": bytes.len(),
                "mime": mime_of(&name),
                "tags": tags,
                "saved_at": harness_kernel::now_ms(),
                "source": ctx.call.0,
            }),
            version: 1,
            target: None,
            op: DocOp::Upsert,
        };
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已存入文件库：{name}（{} KB）\nid：{id}\n以后用 `files` 找它，`send_file` 发出去。",
                bytes.len() / 1024
            ))],
            doc_writes: vec![doc],
            artifacts: Vec::new(),
        })
    }
}

/// `files(action, ...)`：列出 / 取回 / 删除。
pub struct FilesTool {
    dir: PathBuf,
}

impl FilesTool {
    pub fn new(dir: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { dir: dir.into() })
    }
}

#[async_trait]
impl Tool for FilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "files".into(),
            description: "文件库操作。action=list 按名字/标签检索；action=export 把文件取回 /ws（之后可 send_file 发出）；\
action=del 删除。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "export", "del"] },
                    "query": { "type": "string", "description": "list 用：名字或标签里的关键词" },
                    "id": { "type": "string", "description": "export/del 用：file-xxxx" },
                    "dest": { "type": "string", "description": "export 用：工作区内的目标路径，默认 /ws/<原名>" }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::External
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 action", false))?;
        match action {
            "list" => {
                let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
                let docs = ctx
                    .docs
                    .list("file", 500)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                let mut rows: Vec<(i64, String)> = Vec::new();
                for d in docs {
                    let name = d.payload["name"].as_str().unwrap_or("?");
                    let tags = d.payload["tags"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|t| t.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .unwrap_or_default();
                    if !q.is_empty()
                        && !name.to_lowercase().contains(&q)
                        && !tags.to_lowercase().contains(&q)
                    {
                        continue;
                    }
                    let at = d.payload["saved_at"].as_i64().unwrap_or(0);
                    rows.push((
                        at,
                        format!(
                            "- `{}` · {} · {} KB · {}{}\n",
                            d.id.0,
                            name,
                            d.payload["size"].as_u64().unwrap_or(0) / 1024,
                            harness_kernel::schedule::format_local(at, 480),
                            if tags.is_empty() { String::new() } else { format!(" · [{tags}]") }
                        ),
                    ));
                }
                if rows.is_empty() {
                    return Ok(ToolOutput {
                        content: vec![Block::Paragraph("文件库里没有匹配的文件。".into())],
                        doc_writes: Vec::new(),
                        artifacts: Vec::new(),
                    });
                }
                rows.sort_by(|a, b| b.0.cmp(&a.0));
                let mut text = format!("文件库（{} 项）：\n", rows.len());
                for (_, line) in rows.iter().take(50) {
                    text.push_str(line);
                }
                text.push_str("\n取回：`files(action=\"export\", id=...)`；删除：`files(action=\"del\", id=...)`");
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(text)],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            "export" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 id", false))?;
                let doc = ctx
                    .docs
                    .get(id)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
                    .ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("没有 {id}"), false))?;
                if doc.kind != "file" {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "不是文件库里的条目", false));
                }
                let rel = doc.payload["rel"].as_str().unwrap_or_default();
                let name = doc.payload["name"].as_str().unwrap_or("file");
                let src = self.dir.join(rel);
                if !src.exists() {
                    return Err(ToolError::new(
                        ErrorCode::NotFound,
                        "文件库里的文件不见了（可能被手工删过）",
                        false,
                    ));
                }
                let dest_raw = args
                    .get("dest")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("/ws/{name}"));
                let dest = resolve(&ctx.workspace.root, &dest_raw)?;
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
                }
                std::fs::copy(&src, &dest)
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(format!(
                        "已取回到 {}，可以 send_file 发出去。",
                        dest_raw
                    ))],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            "del" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 id", false))?;
                let doc = ctx
                    .docs
                    .get(id)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
                    .ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("没有 {id}"), false))?;
                if doc.kind != "file" {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "不是文件库里的条目", false));
                }
                let rel = doc.payload["rel"].as_str().unwrap_or_default().to_owned();
                let removed = ctx
                    .docs
                    .delete(id)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
                if removed {
                    if let Some(dir) = self.dir.join(&rel).parent() {
                        let _ = std::fs::remove_dir_all(dir);
                    }
                }
                Ok(ToolOutput {
                    content: vec![Block::Paragraph(if removed {
                        format!("已删除 {id}")
                    } else {
                        format!("{id} 不存在")
                    })],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            other => Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("未知 action：{other}"),
                false,
            )),
        }
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitized() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("报告 v2.pdf"), "报告 v2.pdf");
        assert_eq!(safe_name("   "), "file");
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(digest(b"abc"), digest(b"abc"));
        assert_ne!(digest(b"abc"), digest(b"abd"));
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use harness_kernel::{ConversationId, DocReader, RunId, Workspace};
    use crate::NullEnv;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemDocs(Mutex<Vec<harness_kernel::Doc>>);

    #[async_trait]
    impl DocReader for MemDocs {
        async fn list(&self, kind: &str, _limit: usize) -> Result<Vec<harness_kernel::Doc>, String> {
            Ok(self.0.lock().unwrap().iter().filter(|d| d.kind == kind).cloned().collect())
        }
        async fn get(&self, id: &str) -> Result<Option<harness_kernel::Doc>, String> {
            Ok(self.0.lock().unwrap().iter().find(|d| d.id.0 == id).cloned())
        }
        async fn delete(&self, id: &str) -> Result<bool, String> {
            let mut v = self.0.lock().unwrap();
            let before = v.len();
            v.retain(|d| d.id.0 != id);
            Ok(v.len() != before)
        }
    }

    #[tokio::test]
    async fn keep_then_list_and_export() {
        let ws = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ws.path().join("out")).unwrap();
        std::fs::write(ws.path().join("out/清单.md"), "# 采购\n牛奶").unwrap();

        let docs = MemDocs::default();
        let wsx = Workspace { run: RunId("r".into()), root: ws.path().to_string_lossy().into() };
        let env = NullEnv;
        let ctx = ToolCtx {
            conv: ConversationId("c".into()),
            actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: RunId("r".into()),
            call: harness_kernel::CallId("call-1".into()),
            workspace: &wsx,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };

        let keep = KeepTool::new(lib.path());
        let out = keep
            .execute(
                serde_json::json!({"path":"/ws/out/清单.md","tags":["采购","家庭"]}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.doc_writes.len(), 1);
        let doc = out.doc_writes[0].clone();
        assert_eq!(doc.kind, "file");
        assert!(lib.path().join(doc.payload["rel"].as_str().unwrap()).exists());

        // 把 doc 塞进"库"，模拟 tx 提交后
        docs.0.lock().unwrap().push(harness_kernel::Doc {
            id: doc.id.clone(),
            kind: "file".into(),
            scope: "family".into(),
            payload: doc.payload.clone(),
            version: 1,
            valid_from: 1,
            valid_to: None,
        });

        let files = FilesTool::new(lib.path());
        let out = files
            .execute(serde_json::json!({"action":"list","query":"采购"}), &ctx)
            .await
            .unwrap();
        let text = format!("{:?}", out.content);
        assert!(text.contains("清单.md"), "{text}");

        // 取回到 /ws 后能再发出去
        let out = files
            .execute(serde_json::json!({"action":"export","id":doc.id.0}), &ctx)
            .await
            .unwrap();
        assert!(format!("{:?}", out.content).contains("/ws/清单.md"));
        assert!(ws.path().join("清单.md").exists());

        // 删除
        files
            .execute(serde_json::json!({"action":"del","id":doc.id.0}), &ctx)
            .await
            .unwrap();
        assert!(docs.0.lock().unwrap().is_empty());
        assert!(!lib.path().join(doc.payload["rel"].as_str().unwrap()).exists());
    }
}
