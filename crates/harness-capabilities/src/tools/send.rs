//! `send_file`：把工作区里的文件/图片发到当前会话。
//!
//! 只允许发 `/ws`、`/media` 下的文件——避免模型把宿主上的任意文件（密钥、日志）贴出去。
//! 通道侧按 mime 决定用 `sendPhoto` 还是 `sendDocument`；没有文件的通道降级成文本。

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use super::fs::resolve;

const MAX_BYTES: u64 = 50 * 1024 * 1024;

fn guess_mime(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("zip") => "application/zip",
        Some("csv") => "text/csv",
        Some("txt") | Some("md") => "text/plain",
        Some("json") => "application/json",
        Some("py") | Some("sh") | Some("rs") => "text/plain",
        _ => "application/octet-stream",
    }
}

pub struct SendFileTool;

#[async_trait]
impl Tool for SendFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "send_file".into(),
            description: "把 `/ws` 或 `/media` 里的文件发给用户（图片按图片发，其余按文档发）。\
用于把生成的 PDF/表格/脚本交给用户，或打印前给用户预览。单文件上限 50MB。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "工作区内的路径，如 /ws/out/report.pdf" },
                    "caption": { "type": "string", "description": "随文件发的一句话说明" }
                },
                "required": ["path"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 发消息是外部副作用，失败不自动重发
        EffectClass::External
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 path", false))?;
        let caption = args.get("caption").and_then(|v| v.as_str()).unwrap_or("").to_owned();

        // 只允许工作区内的文件（`/media` 里的先 `cp` 到 `/ws` 再发，避免把附件目录当出口）
        let path = resolve(&ctx.workspace.root, raw)?;

        let meta = std::fs::metadata(&path)
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
        let name = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let mime = guess_mime(&name);

        ctx.outbox
            .send_file(&path.to_string_lossy(), &name, mime, &caption)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, false))?;

        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!("已发送：{name}（{} KB）", meta.len() / 1024))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}
