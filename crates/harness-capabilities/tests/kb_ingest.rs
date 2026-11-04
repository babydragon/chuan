//! KB 端到端：真用宿主上的 pdftotext 抽出按页文本（这一步是 agent 的活），
//! 再走 `kb_ingest` 入库、`kb_file` 取原件。
//!
//! 没装 poppler 的机器上跳过，不制造假失败。

use std::path::PathBuf;

use harness_capabilities::{KbFileTool, KbIngestTool};
use harness_kernel::{CallId, ConversationId, ErrorCode, RunId, Tool, ToolCtx, Workspace};
use tokio_util::sync::CancellationToken;

fn have(bin: &str) -> bool {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {bin}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 两页的极小 PDF（无 xref，poppler 会自动重建）。
///
/// 只用 ASCII：Helvetica 编不了中文；中文页的抽取由单测里的假文本覆盖。
fn two_page_pdf() -> Vec<u8> {
    let mut s = String::from("%PDF-1.4\n");
    s.push_str("1 0 obj<</Type/Catalog/Pages 2 0 R>>endobj\n");
    s.push_str("2 0 obj<</Type/Pages/Kids[3 0 R 4 0 R]/Count 2>>endobj\n");
    for (obj, content, text) in [(3, 5, "Page one"), (4, 6, "Page two")] {
        s.push_str(&format!(
            "{obj} 0 obj<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]\
             /Resources<</Font<</F1 7 0 R>>>>/Contents {content} 0 R>>endobj\n"
        ));
        let stream = format!("BT /F1 12 Tf 20 100 Td ({text}) Tj ET");
        s.push_str(&format!(
            "{content} 0 obj<</Length {}>>stream\n{stream}\nendstream\nendobj\n",
            stream.len()
        ));
    }
    s.push_str("7 0 obj<</Type/Font/Subtype/Type1/BaseFont/Helvetica>>endobj\n");
    s.push_str("trailer<</Root 1 0 R/Size 8>>\n%%EOF\n");
    s.into_bytes()
}

fn ctx<'a>(ws: &'a Workspace, env: &'a harness_capabilities::NullEnv) -> ToolCtx<'a> {
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

#[tokio::test]
async fn ingest_real_pdf_then_export_original() {
    if !have("pdftotext") {
        eprintln!("跳过：本机没有 pdftotext");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws_root = dir.path().join("ws");
    let library = dir.path().join("files");
    std::fs::create_dir_all(&ws_root).unwrap();
    std::fs::write(ws_root.join("book.pdf"), two_page_pdf()).unwrap();
    // 抽取由 agent 自己做：这里就是"先 pdftotext，再入库"的真实顺序
    let st = std::process::Command::new("pdftotext")
        .args(["-layout", "-enc", "UTF-8"])
        .arg(ws_root.join("book.pdf"))
        .arg(ws_root.join("pages.txt"))
        .status()
        .unwrap();
    assert!(st.success(), "pdftotext 应该成功");

    let ws = Workspace {
        run: RunId("r".into()),
        root: ws_root.to_string_lossy().into_owned(),
    };
    let env = harness_capabilities::NullEnv;
    let c = ctx(&ws, &env);

    let out = KbIngestTool::new(&library, dir.path().join("media"))
        .execute(
            serde_json::json!({
                "path": "/ws/book.pdf",
                "pages_file": "/ws/pages.txt",
                "title": "测试课本",
                "text_from": "pdftotext"
            }),
            &c,
        )
        .await
        .unwrap();
    assert_eq!(out.doc_writes.len(), 3, "一个来源 + 两页");
    let pages: Vec<&str> = out
        .doc_writes
        .iter()
        .filter(|d| d.kind == "kb.page")
        .map(|d| d.payload["text"].as_str().unwrap_or(""))
        .collect();
    assert!(pages[0].contains("Page one"), "{pages:?}");
    assert!(pages[1].contains("Page two"), "{pages:?}");

    let sid = out
        .doc_writes
        .iter()
        .find(|d| d.kind == "kb.source")
        .map(|d| d.id.0.clone())
        .unwrap();
    let sha = sid.trim_start_matches("kb:");
    assert!(library.join(sha).exists(), "原件必须落在文件库");

    // kb_file 把原件放进 /ws（渲染/出图交给 agent 自己）
    let docs = OneSource {
        doc: harness_kernel::Doc {
            id: harness_kernel::DocId(sid.clone()),
            kind: "kb.source".into(),
            scope: "kb".into(),
            payload: out
                .doc_writes
                .iter()
                .find(|d| d.kind == "kb.source")
                .map(|d| d.payload.clone())
                .unwrap(),
            version: 1,
            valid_from: 0,
            valid_to: None,
        },
    };
    let ws_path: PathBuf = ws_root.join("kb").join(sha).join("测试课本");
    assert!(!ws_path.exists(), "取之前工作区里不该有它");
    let c2 = ToolCtx {
        system_prompt: "",
        spawner: None,
        conv: ConversationId("c".into()),
        actor: "owner",
        outbox: &harness_kernel::NullOutbox,
        docs: &docs,
        run: RunId("r".into()),
        call: CallId("call-2".into()),
        workspace: &ws,
        env: &env,
        cancel: CancellationToken::new(),
    };
    KbFileTool::new(&library)
        .execute(serde_json::json!({"source": sid}), &c2)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(&ws_path).unwrap(),
        two_page_pdf(),
        "工作区里的原件必须与入库前逐字节一致"
    );
}

/// 只认一个来源的 DocReader：kb_file 只需要 `rel`。
struct OneSource {
    doc: harness_kernel::Doc,
}

#[async_trait::async_trait]
impl harness_kernel::DocReader for OneSource {
    async fn list(
        &self,
        _kind: &str,
        _limit: usize,
    ) -> Result<Vec<harness_kernel::Doc>, String> {
        Ok(vec![self.doc.clone()])
    }
    async fn get(&self, id: &str) -> Result<Option<harness_kernel::Doc>, String> {
        Ok((id == self.doc.id.0).then(|| self.doc.clone()))
    }
    async fn delete(&self, _id: &str) -> Result<bool, String> {
        Ok(false)
    }
}

#[tokio::test]
async fn ingest_missing_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let ws_root = dir.path().join("ws");
    std::fs::create_dir_all(&ws_root).unwrap();
    let ws = Workspace { run: RunId("r".into()), root: ws_root.to_string_lossy().into_owned() };
    let env = harness_capabilities::NullEnv;
    let c = ctx(&ws, &env);

    let err = KbIngestTool::new(dir.path().join("files"), dir.path().join("media"))
        .execute(serde_json::json!({"path": "/ws/nope.pdf", "pages_file": "/ws/x.txt"}), &c)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound, "{}", err.message);
}
