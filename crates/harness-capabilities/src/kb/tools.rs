//! KB 的四个工具：入库、检索、取文、出原页。
//!
//! 数据形态（见 D33）：
//! - 原件：`files/<sha256前16>/<name>`，与 `keep` 同一布局，重启不丢；
//! - 每页正文：一条 `kb.page` doc，`payload.text` 被 `docs_fts` 索引；
//! - 来源：一条 `kb.source` doc（标题、页数、正文来源）。
//!
//! **抽取不在这里**：格式解析（PDF/Word/扫描件）由 agent 自己用装好的库或系统工具做完，
//! 把"按页切好的 UTF-8 文本"交给 `kb_ingest`。KB 只负责保存与检索——
//! 否则每支持一种格式就要改一次 Rust（不变量 15）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, Doc, DocId, DocOp, DocWrite, EffectClass, ErrorCode, Query, Retriever, Tool, ToolCtx,
    ToolError, ToolOutput, ToolSpec,
};
use sha2::{Digest, Sha256};


const MAX_SOURCE_BYTES: u64 = 200 * 1024 * 1024;
const MAX_PAGES: usize = 2000;
const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
const KB_SCOPE: &str = "kb";
const PAGE_KIND: &str = "kb.page";
const SOURCE_KIND: &str = "kb.source";

/// 内容摘要的前 16 位十六进制：够区分，又不至于让 id 写不进上下文。
#[cfg(test)]
fn short_sha(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn source_id(sha: &str) -> String {
    format!("kb:{sha}")
}

fn page_id(sha: &str, page: u32) -> String {
    format!("kb:{sha}:p{page}")
}

/// 页之间用换页符 `\x0C` 分隔；末尾的空段是分隔符的尾巴，丢掉。
fn split_pages(text: &str) -> Vec<String> {
    let mut pages: Vec<String> = text.split('\u{0c}').map(|p| p.trim_end().to_owned()).collect();
    while pages.last().is_some_and(|p| p.trim().is_empty()) {
        pages.pop();
    }
    pages
}

/// 只接受 `/ws` 与 `/media`：前者是可写草稿区，后者是通道落盘的附件。
fn resolve_input(raw: &str, ws_root: &str, media_dir: &Path) -> Result<PathBuf, ToolError> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("/media") {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() || rest.contains("..") {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "路径不合法", false));
        }
        return Ok(media_dir.join(rest));
    }
    let rel = if raw == "/ws" {
        ""
    } else {
        raw.strip_prefix("/ws/").unwrap_or(raw)
    };
    if rel.starts_with('/') || rel.contains("..") || rel.is_empty() {
        return Err(ToolError::new(
            ErrorCode::InvalidArgs,
            "只接受 /ws 或 /media 下的文件路径",
            false,
        ));
    }
    Ok(Path::new(ws_root).join(rel))
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | '（' | '）') { c } else { '_' })
        .collect();
    if cleaned.trim_matches('.').is_empty() { "file.pdf".into() } else { cleaned }
}

/// 流式复制并同时算摘要：教材可能上百 MB，不整块读进内存。
fn copy_and_hash(src: &Path, tmp: &Path) -> Result<(String, u64), String> {
    use std::io::{Read, Write};
    let mut input = std::fs::File::open(src).map_err(|e| format!("读不到源文件：{e}"))?;
    let mut out = std::fs::File::create(tmp).map_err(|e| format!("写临时文件失败：{e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = input.read(&mut buf).map_err(|e| format!("读取失败：{e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_SOURCE_BYTES {
            return Err(format!("文件超过 {} MB 上限", MAX_SOURCE_BYTES / 1024 / 1024));
        }
        hasher.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| format!("写入失败：{e}"))?;
    }
    out.sync_all().map_err(|e| format!("落盘失败：{e}"))?;
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Ok((hex, total))
}

// ---------------------------------------------------------------- ingest

pub struct KbIngestTool {
    library: PathBuf,
    media: PathBuf,
}

impl KbIngestTool {
    pub fn new(library: impl Into<PathBuf>, media: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { library: library.into(), media: media.into() })
    }
}

#[async_trait]
impl Tool for KbIngestTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_ingest".into(),
            description: "把一份资料**已经抽好的正文**存进资料库，建立可按页检索的索引；原件另外永久保存。\
两步：① 先自己把源文件转成 UTF-8 文本（用你装好的库或系统工具，格式由你判断），\
不同页之间用一个换页符 `\\u000c` 分隔，写到 `/ws` 下的一个文件；② 再用这个工具把「原件 + 正文」一起入库。\
同一份文件重复入库只会覆盖，不会产生副本。入库后：`kb_search` 查内容、`kb_read` 读整页、`kb_file` 取原件自己处理。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "原件路径（/media 或 /ws），用于永久保存与出原页" },
                    "pages_file": { "type": "string", "description": "你抽好的正文文件（/ws 下），UTF-8，页之间用 \\u000c 分隔" },
                    "title": { "type": "string", "description": "显示用的标题（默认用文件名）" },
                    "domain": { "type": "string", "description": "归类标签，如 语文/英语/资料；只用于检索过滤展示" },
                    "text_from": { "type": "string", "description": "正文怎么来的（如 pdftotext、python-docx、ocr），记下来供以后核对" }
                },
                "required": ["path", "pages_file"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Idempotent { key: "kb_ingest".into() }
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
        let src = resolve_input(raw, &ctx.workspace.root, &self.media)?;
        let meta = std::fs::metadata(&src)
            .map_err(|e| ToolError::new(ErrorCode::NotFound, format!("找不到文件：{e}"), false))?;
        if !meta.is_file() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "不是普通文件", false));
        }
        if meta.len() > MAX_SOURCE_BYTES {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("文件 {} MB，超过上限", meta.len() / 1024 / 1024),
                false,
            ));
        }

        let pages_raw = args.get("pages_file").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolError::new(
                ErrorCode::InvalidArgs,
                "需要 pages_file：先把正文抽成 UTF-8 文本（页之间用 \\u000c 分隔）再入库",
                false,
            )
        })?;
        let pages_path = resolve_input(pages_raw, &ctx.workspace.root, &self.media)?;
        let body = std::fs::read(&pages_path).map_err(|e| {
            ToolError::new(ErrorCode::NotFound, format!("读不到正文文件：{e}"), false)
        })?;
        if body.len() > MAX_TEXT_BYTES {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "正文超过 32MB，暂不支持这么大的单本入库",
                false,
            ));
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        let pages = split_pages(&text);
        let filled = pages.iter().filter(|p| !p.trim().is_empty()).count();
        if pages.is_empty() || filled == 0 {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "正文文件是空的（或只有空白）。确认抽取这一步成功了，再入库。",
                false,
            ));
        }
        if pages.len() > MAX_PAGES {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("页数 {} 超过 {} 页上限", pages.len(), MAX_PAGES),
                false,
            ));
        }

        let name = safe_name(
            &args
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    src.file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "book.pdf".into())
                }),
        );
        let domain = args
            .get("domain")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let text_from = args
            .get("text_from")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_owned();

        // 原件按内容寻址存进文件库（重复入库等价）
        std::fs::create_dir_all(&self.library)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("文件库不可用：{e}"), false))?;
        let tmp = self.library.join(format!(".ingest-{}.tmp", ctx.call.0));
        let (sha, bytes) = copy_and_hash(&src, &tmp).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            ToolError::new(ErrorCode::Unavailable, e, false)
        })?;
        let dest_dir = self.library.join(&sha);
        std::fs::create_dir_all(&dest_dir)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("文件库不可用：{e}"), false))?;
        let dest = dest_dir.join(&name);
        if dest.exists() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            std::fs::rename(&tmp, &dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                ToolError::new(ErrorCode::Unavailable, format!("落盘失败：{e}"), false)
            })?;
        }

        // 写索引：一个来源 + 每页一条（`payload.text` 会进 FTS）
        let sid = source_id(&sha);
        let now = harness_kernel::now_ms();
        let chars: usize = pages.iter().map(|p| p.chars().count()).sum();
        let mut writes = Vec::with_capacity(pages.len() + 1);
        writes.push(DocWrite {
            id: DocId(sid.clone()),
            kind: SOURCE_KIND.into(),
            scope: KB_SCOPE.into(),
            payload: serde_json::json!({
                "title": name,
                "sha256": sha,
                "rel": format!("{sha}/{name}"),
                "size": bytes,
                "pages": pages.len(),
                "chars": chars,
                "domain": domain,
                "text_from": text_from,
                "saved_at": now,
                "source": ctx.call.0,
            }),
            version: 1,
            target: None,
            op: DocOp::Upsert,
        });
        for (i, page) in pages.iter().enumerate() {
            let no = i as u32 + 1;
            writes.push(DocWrite {
                id: DocId(page_id(&sha, no)),
                kind: PAGE_KIND.into(),
                scope: KB_SCOPE.into(),
                payload: serde_json::json!({
                    "source": sid,
                    "page": no,
                    "text": page,
                }),
                version: 1,
                target: None,
                op: DocOp::Upsert,
            });
        }

        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已入库：{name}\n来源 id：{sid}\n{} 页（其中 {filled} 页有内容），约 {chars} 字；正文来源：{text_from}。\n\
                 下一步：`kb_search` 查内容、`kb_read` 读整页、`kb_file` 取原件。",
                pages.len()
            ))],
            doc_writes: writes,
            artifacts: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------- search

pub struct KbSearchTool {
    retriever: Arc<dyn Retriever>,
}

impl KbSearchTool {
    pub fn new(retriever: Arc<dyn Retriever>) -> Arc<Self> {
        Arc::new(Self { retriever })
    }
}

#[async_trait]
impl Tool for KbSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_search".into(),
            description: "在资料库里找**字面出现**的片段（不是语义相似）。\
返回页码与摘要，用 `kb_read` 读整页、`kb_file` 取原件。\
查具体词（人名、诗句、生词、术语）用它；问“第几单元有哪些生字”这类结构问题，先看有没有结构索引。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": { "type": "string", "description": "要找的词句（≥2 字；多词用空格分隔，全部命中才算）" },
                    "source": { "type": "string", "description": "只在这份资料里找（如 kb:ab12cd34），可省略" },
                    "domain": { "type": "string", "description": "只在这个归类里找（入库时给的 domain，如 语文），可省略" },
                    "k": { "type": "integer", "description": "返回条数，默认 8" }
                },
                "required": ["text"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 text", false))?;
        let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(8).clamp(1, 50) as usize;
        let source = args
            .get("source")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| if s.starts_with("kb:") { s.to_owned() } else { format!("kb:{s}") });
        let domain = args
            .get("domain")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        // 页 doc 只存 source；归类要从来源 doc 取，按来源缓存一次
        let mut source_domain: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();

        // 指定来源时先多取一些再过滤（Retriever 只认 scope/kind，不认文档前缀）
        let fetch = if source.is_some() { (k * 6).min(300) } else { k };
        let hits = self
            .retriever
            .search(Query {
                text: text.to_owned(),
                scopes: vec![KB_SCOPE.into()],
                kinds: vec![PAGE_KIND.into()],
                k: fetch,
            })
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        let mut lines = Vec::new();
        for hit in hits {
            if source
                .as_ref()
                .is_some_and(|src| !hit.doc_id.0.starts_with(&format!("{src}:p")))
            {
                continue;
            }
            let Some(doc) = ctx
                .docs
                .get(&hit.doc_id.0)
                .await
                .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
            else {
                continue;
            };
            if let Some(want) = &domain {
                let sid = doc
                    .payload
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                let got = match source_domain.get(&sid) {
                    Some(v) => v.clone(),
                    None => {
                        let d = ctx
                            .docs
                            .get(&sid)
                            .await
                            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
                            .and_then(|d| {
                                d.payload.get("domain").and_then(|v| v.as_str()).map(str::to_owned)
                            });
                        source_domain.insert(sid.clone(), d.clone());
                        d
                    }
                };
                if got.as_deref() != Some(want.as_str()) {
                    continue;
                }
            }
            let page = doc.payload.get("page").and_then(|v| v.as_u64()).unwrap_or(0);
            let body = doc.payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let snippet: String = body
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(80)
                .collect();
            lines.push(format!("{} · 第 {page} 页 · {snippet}", hit.doc_id.0));
            if lines.len() >= k {
                break;
            }
        }

        if lines.is_empty() {
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(format!(
                    "没有命中「{text}」。检索是字面匹配：换个更具体的词（≥2 字），\
                     或确认这份资料是否已入库（`kb_search` 只查资料库）。"
                ))],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "命中 {} 条（字面检索）：\n{}",
                lines.len(),
                lines.join("\n")
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------- read

pub struct KbReadTool;

fn normalize_source(raw: &str) -> String {
    let raw = raw.trim();
    if raw.starts_with("kb:") { raw.to_owned() } else { format!("kb:{raw}") }
}

async fn get_doc(ctx: &ToolCtx<'_>, id: &str) -> Result<Option<Doc>, ToolError> {
    ctx.docs.get(id).await.map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))
}

#[async_trait]
impl Tool for KbReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_read".into(),
            description: "按页读资料库里的原文。一次最多 50 页；只读已入库的资料，\
不在库里的内容读不到。要原件（出图、看版式）用 `kb_file`。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "来源 id，如 kb:ab12cd34" },
                    "from": { "type": "integer", "description": "起始页（从 1 开始）" },
                    "to": { "type": "integer", "description": "结束页（含），省略则同 from" }
                },
                "required": ["source", "from"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let sid = normalize_source(
            args.get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 source", false))?,
        );
        let from = args
            .get("from")
            .and_then(|v| v.as_u64())
            .filter(|v| *v >= 1)
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 from（≥1）", false))?
            as u32;
        let to = args
            .get("to")
            .and_then(|v| v.as_u64())
            .unwrap_or(from as u64)
            .max(from as u64) as u32;
        if to - from + 1 > 50 {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "一次最多读 50 页，缩小范围再读",
                false,
            ));
        }

        let meta = get_doc(ctx, &sid).await?;
        if meta.is_none() {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("资料库里没有 {sid}（可能是还没入库，或 id 写错）"),
                false,
            ));
        }
        let title = meta
            .as_ref()
            .and_then(|d| d.payload.get("title").and_then(|v| v.as_str()))
            .unwrap_or("");

        let mut out = String::new();
        if !title.is_empty() {
            out.push_str(&format!("{title}\n\n"));
        }
        let mut found = 0usize;
        for page in from..=to {
            let id = format!("{sid}:p{page}");
            if let Some(doc) = get_doc(ctx, &id).await? {
                let text = doc.payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
                out.push_str(&format!("—— 第 {page} 页 ——\n{text}\n\n"));
                found += 1;
            }
        }
        if found == 0 {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("{sid} 没有第 {from}–{to} 页的内容"),
                false,
            ));
        }
        Ok(ToolOutput {
            content: vec![Block::Paragraph(out)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------- file

/// `kb_file(source)`：把原件放到工作区，剩下的事由 agent 自己办。
///
/// 框架不内置渲染器：出图/裁剪/节选都是领域能力，换一种文件格式就变
/// （不变量 15）。这里只解决"原件在宿主上、沙盒里看不到"这一件事。
pub struct KbFileTool {
    library: PathBuf,
}

impl KbFileTool {
    pub fn new(library: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { library: library.into() })
    }
}

#[async_trait]
impl Tool for KbFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_file".into(),
            description: "把资料库里的**原件**放进工作区，返回 `/ws` 路径。\
要看版式、出图、节选、或把原件直接发给用户，都先取出来自己处理，\
处理完用 `send_file` 发。系统不替你转换格式。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "来源 id，如 kb:ab12cd34" }
                },
                "required": ["source"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 只往 /ws 拷一份草稿，重复执行等价
        EffectClass::Idempotent { key: "kb_file".into() }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let sid = normalize_source(
            args.get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 source", false))?,
        );
        let Some(meta) = get_doc(ctx, &sid).await? else {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("资料库里没有 {sid}（可能是还没入库，或 id 写错）"),
                false,
            ));
        };
        let rel = meta
            .payload
            .get("rel")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::Unavailable, "来源记录缺少文件位置", false))?;
        let src = self.library.join(rel);
        if !src.is_file() {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("原件不在文件库里：{}", src.display()),
                false,
            ));
        }
        let sha = sid.strip_prefix("kb:").unwrap_or(&sid);
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let dir = Path::new(&ctx.workspace.root).join("kb").join(sha);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("建目录失败：{e}"), false))?;
        let dest = dir.join(&name);
        tokio::fs::copy(&src, &dest)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("复制失败：{e}"), false))?;
        let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "原件已放到 /ws/kb/{sha}/{name}（{} MB）。\n\
                 要处理它就自己来（转图、裁剪、抽页都行），做完用 `send_file` 发出去；\
                 `/ws` 是临时区，别指望它长期留着。",
                size / 1024 / 1024
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

// ---------------------------------------------------------------- list / forget

/// `kb_list`：库里有哪几份资料。没有它，模型只能靠 `kb_search` 猜。
pub struct KbListTool;

#[async_trait]
impl Tool for KbListTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_list".into(),
            description: "列出资料库里有哪些资料（标题 / 页数 / 归类 / 正文来源 / id）。\
入库后想确认“我手上有哪些书和资料”、或者要挑一个 source 给 `kb_read` / `kb_select` 时用它。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "domain": { "type": "string", "description": "只看这个归类（如 语文），可省略" },
                    "limit": { "type": "integer", "description": "最多列多少份，默认 100" }
                }
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Pure
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(100).clamp(1, 500) as usize;
        let domain = args
            .get("domain")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let docs = ctx
            .docs
            .list("kb.source", limit * 2)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;
        let mut lines: Vec<String> = Vec::new();
        for d in docs.iter().filter(|d| d.scope == KB_SCOPE) {
            let p = &d.payload;
            if domain.is_some_and(|want| p.get("domain").and_then(|v| v.as_str()) != Some(want)) {
                continue;
            }
            let title = p.get("title").and_then(|v| v.as_str()).unwrap_or("(无标题)");
            let pages = p.get("pages").and_then(|v| v.as_u64()).unwrap_or(0);
            let d_ = p.get("domain").and_then(|v| v.as_str()).unwrap_or("-");
            let from = p.get("text_from").and_then(|v| v.as_str()).unwrap_or("?");
            lines.push(format!("{} · {title} · {pages} 页 · {d_} · 正文来源 {from}", d.id.0));
            if lines.len() >= limit {
                break;
            }
        }
        if lines.is_empty() {
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(match domain {
                    Some(d) => format!("资料库里还没有归类为「{d}」的资料。"),
                    None => "资料库是空的。先用 `kb_ingest` 入库（原件 + 抽好的按页文本）。".into(),
                })],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "共 {} 份：\n{}",
                lines.len(),
                lines.join("\n")
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

/// `kb_forget`：把一份资料从索引里移除，原件进回收站。
pub struct KbForgetTool {
    library: PathBuf,
}

impl KbForgetTool {
    pub fn new(library: impl Into<PathBuf>) -> Arc<Self> {
        Arc::new(Self { library: library.into() })
    }
}

#[async_trait]
impl Tool for KbForgetTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "kb_forget".into(),
            description: "把一份资料从索引里删掉（页索引、结构索引、来源记录），原件移进回收站。\
用于入错了、看错文件、或资料过期。回收站按保留期自动清理；要恢复原件，回执里会给路径。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "来源 id，如 kb:ab12cd34" }
                },
                "required": ["source"]
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
        let sid = normalize_source(
            args.get("source")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 source", false))?,
        );
        let Some(meta) = ctx
            .docs
            .get(&sid)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?
        else {
            return Err(ToolError::new(
                ErrorCode::NotFound,
                format!("资料库里没有 {sid}"),
                false,
            ));
        };
        let pages = meta.payload.get("pages").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let title = meta.payload.get("title").and_then(|v| v.as_str()).unwrap_or("").to_owned();

        // 先删页，再删结构，最后删来源：中途失败时来源还在，重跑一次即可
        let mut removed = 0usize;
        for page in 1..=pages {
            match ctx.docs.delete(&format!("{sid}:p{page}")).await {
                Ok(true) => removed += 1,
                Ok(false) => {}
                Err(e) => {
                    return Err(ToolError::new(
                        ErrorCode::Unavailable,
                        format!("删到第 {page} 页时失败：{e}（来源记录保留，可重试）"),
                        false,
                    ))
                }
            }
        }
        let _ = ctx.docs.delete(&format!("{sid}:outline")).await;

        let sha = sid.strip_prefix("kb:").unwrap_or(&sid);
        let dir = self.library.join(sha);
        let trash_root = self.library.join(crate::selfmod::TRASH_DIR);
        let moved = crate::selfmod::trash(&dir, &trash_root)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        ctx.docs
            .delete(&sid)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        let where_ = moved
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "（原件已不在文件库）".into());
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已移除 {sid}{}：删掉 {removed}/{} 页索引与结构索引。\n原件在 {where_}，\
                 保留期内可以搬回文件库；恢复索引要重新 `kb_ingest`（正文需要重抽）。",
                if title.is_empty() { String::new() } else { format!("（{title}）") },
                pages
            ))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_kernel::{CallId, ConversationId, RunId, Workspace};
    use tokio_util::sync::CancellationToken;

    fn ctx<'a>(ws: &'a Workspace, env: &'a crate::NullEnv) -> ToolCtx<'a> {
        ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("c".into()),
            actor: "owner",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: CallId("call-kb".into()),
            workspace: ws,
            env,
            cancel: CancellationToken::new(),
        }
    }

    #[test]
    fn splits_on_form_feed_and_drops_trailing_blank() {
        let pages = split_pages("第一页\n内容\u{0c}第二页\u{0c}   \u{0c}");
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0], "第一页\n内容");
        assert_eq!(pages[1], "第二页");
    }

    #[test]
    fn ids_are_content_derived() {
        let a = short_sha(b"same");
        let b = short_sha(b"same");
        let c = short_sha(b"different");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.chars().count(), 16);
        assert_eq!(source_id(&a), format!("kb:{a}"));
        assert_eq!(page_id(&a, 12), format!("kb:{a}:p12"));
    }

    #[test]
    fn resolves_only_ws_and_media() {
        let ws = "/tmp/ws";
        let media = Path::new("/tmp/media");
        assert_eq!(
            resolve_input("/ws/a/b.pdf", ws, media).unwrap(),
            Path::new("/tmp/ws/a/b.pdf")
        );
        assert_eq!(
            resolve_input("/media/x.pdf", ws, media).unwrap(),
            Path::new("/tmp/media/x.pdf")
        );
        assert!(resolve_input("/etc/passwd", ws, media).is_err());
        assert!(resolve_input("/ws/../x", ws, media).is_err());
    }

    #[tokio::test]
    async fn ingest_writes_source_and_pages() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        let library = dir.path().join("files");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(ws_root.join("book.pdf"), b"%PDF-1.4 fake").unwrap();
        std::fs::write(ws_root.join("pages.txt"), "第一单元 生字\u{0c}第二页 内容\u{0c}").unwrap();

        let tool = KbIngestTool::new(&library, dir.path().join("media"));
        let ws = Workspace {
            run: RunId("r".into()),
            root: ws_root.to_string_lossy().into_owned(),
        };
        let env = crate::NullEnv;
        let c = ctx(&ws, &env);

        let out = tool
            .execute(
                serde_json::json!({
                    "path": "/ws/book.pdf",
                    "pages_file": "/ws/pages.txt",
                    "domain": "语文",
                    "text_from": "pdftotext"
                }),
                &c,
            )
            .await
            .unwrap();

        assert_eq!(out.doc_writes.len(), 3, "一个来源 + 两页");
        let source = out.doc_writes.iter().find(|d| d.kind == SOURCE_KIND).unwrap();
        assert_eq!(source.payload["text_from"], "pdftotext");
        assert_eq!(source.payload["pages"], 2);
        let pages: Vec<&str> = out
            .doc_writes
            .iter()
            .filter(|d| d.kind == PAGE_KIND)
            .map(|d| d.payload["text"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(pages, vec!["第一单元 生字", "第二页 内容"]);
        let dirs: Vec<_> = std::fs::read_dir(&library).unwrap().flatten().collect();
        assert_eq!(dirs.len(), 1, "内容寻址目录只有一个");
    }

    #[tokio::test]
    async fn ingest_requires_pages_file() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(ws_root.join("book.pdf"), b"%PDF-1.4 fake").unwrap();

        let tool = KbIngestTool::new(dir.path().join("files"), dir.path().join("media"));
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into_owned() };
        let env = crate::NullEnv;
        let c = ctx(&ws, &env);

        let err = tool
            .execute(serde_json::json!({"path": "/ws/book.pdf"}), &c)
            .await
            .unwrap_err();
        assert!(err.message.contains("pages_file"), "{}", err.message);
    }

    #[tokio::test]
    async fn ingest_rejects_blank_text() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        std::fs::create_dir_all(&ws_root).unwrap();
        std::fs::write(ws_root.join("book.pdf"), b"%PDF-1.4 fake").unwrap();
        std::fs::write(ws_root.join("pages.txt"), "\u{0c}  \u{0c}").unwrap();

        let tool = KbIngestTool::new(dir.path().join("files"), dir.path().join("media"));
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into_owned() };
        let env = crate::NullEnv;
        let c = ctx(&ws, &env);

        let err = tool
            .execute(serde_json::json!({"path": "/ws/book.pdf", "pages_file": "/ws/pages.txt"}), &c)
            .await
            .unwrap_err();
        assert!(err.message.contains("正文文件是空的"), "{}", err.message);
    }
}

#[cfg(test)]
mod list_forget_tests {
    use super::*;
    use harness_kernel::{CallId, ConversationId, Doc, RunId, Workspace};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[derive(Default)]
    struct MapDocs {
        docs: Mutex<HashMap<String, Doc>>,
    }

    #[async_trait]
    impl harness_kernel::DocReader for MapDocs {
        async fn list(&self, kind: &str, limit: usize) -> Result<Vec<Doc>, String> {
            let mut out: Vec<Doc> = self
                .docs
                .lock()
                .unwrap()
                .values()
                .filter(|d| d.kind == kind)
                .cloned()
                .collect();
            out.truncate(limit);
            Ok(out)
        }
        async fn get(&self, id: &str) -> Result<Option<Doc>, String> {
            Ok(self.docs.lock().unwrap().get(id).cloned())
        }
        async fn delete(&self, id: &str) -> Result<bool, String> {
            Ok(self.docs.lock().unwrap().remove(id).is_some())
        }
    }

    fn doc(id: &str, kind: &str, payload: serde_json::Value) -> Doc {
        Doc {
            id: DocId(id.into()),
            kind: kind.into(),
            scope: KB_SCOPE.into(),
            payload,
            version: 1,
            valid_from: 0,
            valid_to: None,
        }
    }

    #[tokio::test]
    async fn list_shows_sources_and_filters_by_domain() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace { run: RunId("r".into()), root: dir.path().to_string_lossy().into() };
        let docs = MapDocs::default();
        let env = crate::NullEnv;
        let c = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("c".into()),
            actor: "owner",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: RunId("r".into()),
            call: CallId("call".into()),
            workspace: &ws,
            env: &env,
            cancel: CancellationToken::new(),
        };
        for (id, title, domain) in [("kb:aa", "语文课本", "语文"), ("kb:bb", "英语课本", "英语")] {
            let d = doc(
                id,
                SOURCE_KIND,
                serde_json::json!({"title": title, "pages": 10, "domain": domain, "text_from": "pdftotext"}),
            );
            docs.docs.lock().unwrap().insert(d.id.0.clone(), d);
        }

        let out = KbListTool.execute(serde_json::json!({}), &c).await.unwrap();
        let text = match &out.content[0] {
            Block::Paragraph(t) => t.clone(),
            other => format!("{other:?}"),
        };
        assert!(text.contains("语文课本") && text.contains("英语课本"), "{text}");

        let out = KbListTool
            .execute(serde_json::json!({"domain": "语文"}), &c)
            .await
            .unwrap();
        let text = match &out.content[0] {
            Block::Paragraph(t) => t.clone(),
            other => format!("{other:?}"),
        };
        assert!(text.contains("语文课本"), "{text}");
        assert!(!text.contains("英语课本"), "按归类过滤后不该出现英语：{text}");
    }

    #[tokio::test]
    async fn forget_removes_index_and_moves_original_to_trash() {
        let dir = tempfile::tempdir().unwrap();
        let ws_root = dir.path().join("ws");
        let library = dir.path().join("files");
        std::fs::create_dir_all(&ws_root).unwrap();
        let sha = "0123456789abcdef";
        let book_dir = library.join(sha);
        std::fs::create_dir_all(&book_dir).unwrap();
        std::fs::write(book_dir.join("book.pdf"), b"%PDF-1.4").unwrap();

        let docs = MapDocs::default();
        {
            let mut m = docs.docs.lock().unwrap();
            m.insert(
                format!("kb:{sha}"),
                doc(
                    &format!("kb:{sha}"),
                    SOURCE_KIND,
                    serde_json::json!({"title": "测试课本", "pages": 2, "rel": format!("{sha}/book.pdf")}),
                ),
            );
            m.insert(
                format!("kb:{sha}:p1"),
                doc(&format!("kb:{sha}:p1"), PAGE_KIND, serde_json::json!({"page": 1, "text": "甲"})),
            );
            m.insert(
                format!("kb:{sha}:p2"),
                doc(&format!("kb:{sha}:p2"), PAGE_KIND, serde_json::json!({"page": 2, "text": "乙"})),
            );
            m.insert(
                format!("kb:{sha}:outline"),
                doc(&format!("kb:{sha}:outline"), "kb.outline", serde_json::json!({"nodes": []})),
            );
        }
        let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into() };
        let env = crate::NullEnv;
        let c = ToolCtx {
            system_prompt: "",
            spawner: None,
            conv: ConversationId("c".into()),
            actor: "owner",
            outbox: &harness_kernel::NullOutbox,
            docs: &docs,
            run: RunId("r".into()),
            call: CallId("call".into()),
            workspace: &ws,
            env: &env,
            cancel: CancellationToken::new(),
        };

        let tool = KbForgetTool::new(&library);
        let out = tool
            .execute(serde_json::json!({"source": format!("kb:{sha}")}), &c)
            .await
            .unwrap();
        let text = match &out.content[0] {
            Block::Paragraph(t) => t.clone(),
            other => format!("{other:?}"),
        };
        assert!(text.contains("删掉 2/2 页"), "{text}");
        assert!(docs.docs.lock().unwrap().is_empty(), "索引必须清干净");
        assert!(!book_dir.exists(), "原目录应被移走");
        let trash = library.join(".trash");
        assert_eq!(std::fs::read_dir(&trash).unwrap().count(), 1, "原件必须进回收站");

        let err = tool
            .execute(serde_json::json!({"source": format!("kb:{sha}")}), &c)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }
}
