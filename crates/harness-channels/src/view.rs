//! View → 文本。格式化（HTML）在 `html` 层，这里只做结构展开。

use harness_kernel::{Block, ToolState, View};

fn tool_icon(s: &ToolState) -> &'static str {
    match s {
        ToolState::Running => "⏳",
        ToolState::Ok => "✅",
        ToolState::Failed => "❌",
    }
}

pub fn render_plain(view: &View) -> String {
    let mut out = String::new();
    for b in &view.blocks {
        match b {
            Block::Paragraph(t) => {
                out.push_str(t);
                out.push('\n');
            }
            Block::Code { lang, text } => {
                out.push_str("```");
                if let Some(l) = lang {
                    out.push_str(l);
                }
                out.push('\n');
                out.push_str(text);
                out.push_str("\n```\n");
            }
            Block::Image { alt, .. } => out.push_str(&format!("[图片 {alt}]\n")),
            Block::Divider => out.push_str("———\n"),
            Block::ToolCall { name, state, preview, .. } => {
                let icon = tool_icon(state);
                if preview.is_empty() {
                    out.push_str(&format!("{icon} {name}\n"));
                } else {
                    out.push_str(&format!("{icon} {name} · {preview}\n"));
                }
            }
        }
    }
    out.trim_end().to_owned()
}
