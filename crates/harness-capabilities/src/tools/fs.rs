//! 工作区内的读写工具。路径必须在 workspace 内（canonicalize + 前缀检查）。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

const MAX_READ: u64 = 256 * 1024;

pub(crate) fn resolve(root: &str, rel: &str) -> Result<PathBuf, ToolError> {
    let root = std::fs::canonicalize(root).map_err(|e| {
        ToolError::new(ErrorCode::Unavailable, format!("workspace: {e}"), false)
    })?;
    // 模型在沙盒里看到的是 `/ws`，工具路径就是 workspace 内路径：把两者对齐，
    // 否则模型照抄 `ls` 的结果写 `/ws/x.py` 会被判越界（用户实际踩过）。
    let rel = if rel == "/ws" {
        ""
    } else {
        rel.strip_prefix("/ws/").unwrap_or(rel)
    };
    if rel.starts_with('/') || rel.contains("..") {
        return Err(ToolError::new(ErrorCode::NotPermitted, "路径越界", false));
    }
    let target = root.join(rel);
    // 目标可能尚不存在：向上找第一个存在的祖先，校验后再拼接剩余部分。
    let mut cur = target.clone();
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    while !cur.exists() {
        let Some(name) = cur.file_name().map(|s| s.to_owned()) else {
            return Err(ToolError::new(ErrorCode::NotPermitted, "路径越界", false));
        };
        suffix.push(name);
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return Err(ToolError::new(ErrorCode::NotPermitted, "路径越界", false)),
        }
    }
    let base = std::fs::canonicalize(&cur).map_err(|e| {
        ToolError::new(ErrorCode::NotPermitted, format!("路径不可用: {e}"), false)
    })?;
    if !base.starts_with(&root) {
        return Err(ToolError::new(ErrorCode::NotPermitted, "路径越界", false));
    }
    let mut full = base;
    for part in suffix.into_iter().rev() {
        full.push(part);
    }
    if !full.starts_with(&root) {
        return Err(ToolError::new(ErrorCode::NotPermitted, "路径越界", false));
    }
    Ok(target)
}

pub struct ReadTool;

#[async_trait]
impl Tool for ReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "读取工作区内的文本文件（上限 256KB）。".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
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
        let rel = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidArgs, "需要 path", false)
        })?;
        let path = resolve(&ctx.workspace.root, rel)?;
        let meta = std::fs::metadata(&path)
            .map_err(|e| ToolError::new(ErrorCode::NotFound, e.to_string(), false))?;
        if meta.len() > MAX_READ {
            return Err(ToolError::new(ErrorCode::NotPermitted, "文件过大", false));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
        Ok(ToolOutput {
            content: vec![Block::Code { lang: None, text }],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

pub struct WriteTool;

#[async_trait]
impl Tool for WriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write".into(),
            description: "写入工作区内的文本文件（覆盖）。".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        }
    }
    fn effect(&self) -> EffectClass {
        // 同路径同内容重写等价，可按幂等处理
        EffectClass::Idempotent { key: "write".into() }
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let rel = args.get("path").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidArgs, "需要 path", false)
        })?;
        let content = args.get("content").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidArgs, "需要 content", false)
        })?;
        let path = resolve(&ctx.workspace.root, rel)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
        }
        std::fs::write(&path, content)
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!("已写入 {} 字节", content.len()))],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[allow(dead_code)]
fn _unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_prefix_maps_into_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        assert_eq!(resolve(&root, "a.txt").unwrap(), dir.path().join("a.txt"));
        // 沙盒里看到的是 /ws，模型照抄过来必须能落盘
        assert_eq!(resolve(&root, "/ws/a.txt").unwrap(), dir.path().join("a.txt"));
        assert_eq!(
            resolve(&root, "/ws/publish/x/run.py").unwrap(),
            dir.path().join("publish/x/run.py")
        );
    }

    #[test]
    fn still_jails_other_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_string_lossy().to_string();
        for bad in ["/etc/passwd", "/wsfoo/x", "../x", "a/../../x"] {
            assert!(resolve(&root, bad).is_err(), "应拒绝 {bad}");
        }
    }
}
