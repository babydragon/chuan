//! 超大响应落盘：上下文只留结构 + 预览，完整内容写进工作区。
//!
//! 为什么放 `/ws`：模型能用 `exec` 直接处理（python/grep/jq），
//! 又不用把这些字节塞进历史——每轮都背着 40KB 的 JSON 会让它反复"想看完"。
//! 清理走 workspace GC（默认 7 天），不额外引入生命周期。

use std::path::Path;

/// 落盘结果：沙盒内可见的路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spilled {
    /// 沙盒内路径，例如 `/ws/.tool-results/call_xxx.json`
    pub sandbox_path: String,
    pub bytes: usize,
}

/// 写到 `<workspace>/.tool-results/<call_id>.<ext>`（同一次调用覆盖写）。
pub fn save(root: &str, call_id: &str, body: &[u8], ext: &str) -> Option<Spilled> {
    let dir = Path::new(root).join(".tool-results");
    std::fs::create_dir_all(&dir).ok()?;
    let safe: String = call_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect();
    let name = format!("{safe}.{ext}");
    std::fs::write(dir.join(&name), body).ok()?;
    Some(Spilled {
        sandbox_path: format!("/ws/.tool-results/{name}"),
        bytes: body.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_into_tool_results_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = save(
            &dir.path().to_string_lossy(),
            "call_123",
            br#"{"a":1}"#,
            "json",
        )
        .unwrap();
        assert_eq!(s.sandbox_path, "/ws/.tool-results/call_123.json");
        let on_disk = std::fs::read(dir.path().join(".tool-results/call_123.json")).unwrap();
        assert_eq!(on_disk, br#"{"a":1}"#);
    }

    #[test]
    fn sanitizes_call_id() {
        let dir = tempfile::tempdir().unwrap();
        let s = save(&dir.path().to_string_lossy(), "call/../x", b"x", "txt").unwrap();
        assert_eq!(s.sandbox_path, "/ws/.tool-results/call____x.txt");
    }
}
