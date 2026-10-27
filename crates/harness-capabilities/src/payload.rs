//! 回包策略：**进来给什么，取决于模型能不能读**。
//!
//! 判断顺序：
//! 1. 先看 `Content-Type`，没有就嗅探内容；
//! 2. 文本类（json/html/text/csv）才可能进上下文，且有预算；
//! 3. 二进制（图片/音视频/未知）**一律不进上下文**，只给元信息和落盘路径；
//! 4. 超出预算的，一律落 `/ws/.tool-results/`，上下文里只留结构与预览。

use crate::spill::{self, Spilled};

/// JSON 小于这个大小就整段给；超过只给结构
pub const JSON_INLINE: usize = 12_000;
/// 文本类（html/text/csv）内联预算
pub const TEXT_INLINE: usize = 8_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Json,
    Html,
    Text,
    Csv,
    Image,
    Audio,
    Video,
    Binary,
}

pub struct Rendered {
    pub text: String,
    pub spilled: Option<Spilled>,
}

pub fn classify(content_type: Option<&str>, body: &[u8]) -> Kind {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    let ct = ct.split(';').next().unwrap_or("").trim().to_owned();
    if ct.contains("json") {
        return Kind::Json;
    }
    if ct.contains("html") || ct.contains("xml") {
        return Kind::Html;
    }
    if ct.starts_with("image/") {
        return Kind::Image;
    }
    if ct.starts_with("audio/") {
        return Kind::Audio;
    }
    if ct.starts_with("video/") {
        return Kind::Video;
    }
    if ct.contains("csv") {
        return Kind::Csv;
    }
    if ct.starts_with("text/") {
        return Kind::Text;
    }
    // 嗅探
    let head = &body[..body.len().min(512)];
    let h = String::from_utf8_lossy(head).trim_start().to_ascii_lowercase();
    if h.starts_with('{') || h.starts_with('[') {
        return Kind::Json;
    }
    if h.starts_with("<!doctype html") || h.starts_with("<html") {
        return Kind::Html;
    }
    if body.contains(&0) {
        return Kind::Binary;
    }
    match body.get(..4) {
        Some([0x89, b'P', b'N', b'G']) => Kind::Image,
        Some([0xFF, 0xD8, ..]) => Kind::Image,
        Some([b'G', b'I', b'F', b'8']) => Kind::Image,
        _ => Kind::Text,
    }
}

fn ext_of(kind: Kind, content_type: Option<&str>) -> &'static str {
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    match kind {
        Kind::Json => "json",
        Kind::Html => "html",
        Kind::Csv => "csv",
        Kind::Text => "txt",
        Kind::Binary => "bin",
        Kind::Image => {
            if ct.contains("png") {
                "png"
            } else if ct.contains("gif") {
                "gif"
            } else if ct.contains("webp") {
                "webp"
            } else {
                "jpg"
            }
        }
        Kind::Audio => "audio",
        Kind::Video => "mp4",
    }
}

/// 图片尺寸（PNG / GIF / JPEG），读不出来就算了。
fn image_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() > 24 && &b[..4] == b"\x89PNG" {
        let w = u32::from_be_bytes([b[16], b[17], b[18], b[19]]);
        let h = u32::from_be_bytes([b[20], b[21], b[22], b[23]]);
        return Some((w, h));
    }
    if b.len() > 10 && &b[..3] == b"GIF" {
        let w = u16::from_le_bytes([b[6], b[7]]) as u32;
        let h = u16::from_le_bytes([b[8], b[9]]) as u32;
        return Some((w, h));
    }
    if b.len() > 4 && b[0] == 0xFF && b[1] == 0xD8 {
        let mut i = 2;
        while i + 9 < b.len() {
            if b[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = b[i + 1];
            if (0xC0..=0xC3).contains(&marker) {
                let h = u16::from_be_bytes([b[i + 5], b[i + 6]]) as u32;
                let w = u16::from_be_bytes([b[i + 7], b[i + 8]]) as u32;
                return Some((w, h));
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            i += 2 + len;
        }
    }
    None
}

fn json_shape(v: &serde_json::Value) -> String {
    fn t(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Object(o) => format!("object({} 键)", o.len()),
            serde_json::Value::Array(a) => format!("array({})", a.len()),
            serde_json::Value::String(s) => format!("string({} 字)", s.chars().count()),
            serde_json::Value::Number(_) => "number".into(),
            serde_json::Value::Bool(_) => "bool".into(),
            serde_json::Value::Null => "null".into(),
        }
    }
    let mut out = String::new();
    match v {
        serde_json::Value::Object(o) => {
            for (i, (k, val)) in o.iter().enumerate() {
                if i >= 40 {
                    out.push_str(&format!("…（另有 {} 个键）\n", o.len() - 40));
                    break;
                }
                let extra = match val {
                    serde_json::Value::Array(a) => a
                        .first()
                        .and_then(|f| f.as_object())
                        .map(|f| {
                            let keys: Vec<&str> = f.keys().take(10).map(|s| s.as_str()).collect();
                            format!(" 首项键: {}", keys.join(","))
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                out.push_str(&format!("- {k}: {}{extra}\n", t(val)));
            }
        }
        serde_json::Value::Array(a) => {
            out.push_str(&format!("array({})", a.len()));
            if let Some(f) = a.first().and_then(|f| f.as_object()) {
                let keys: Vec<&str> = f.keys().take(12).map(|s| s.as_str()).collect();
                out.push_str(&format!("\n首项键: {}", keys.join(", ")));
            }
            out.push('\n');
        }
        other => out.push_str(&format!("{}\n", t(other))),
    }
    out
}

/// HTML → 纯文本：去 script/style/标签，压空白，留 title。
pub fn html_to_text(html: &str) -> (Option<String>, String) {
    let lower = html.to_ascii_lowercase();
    let title = lower
        .find("<title")
        .and_then(|i| lower[i..].find('>').map(|j| i + j + 1))
        .and_then(|start| lower[start..].find("</title>").map(|end| (start, start + end)))
        .map(|(s, e)| html[s..e].trim().to_owned());

    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    loop {
        let lower = rest.to_ascii_lowercase();
        let cut = ["<script", "<style", "<!--"]
            .iter()
            .filter_map(|tag| lower.find(tag))
            .min();
        let Some(i) = cut else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let lower_after = &lower[i..];
        let end = if lower_after.starts_with("<!--") {
            lower_after.find("-->").map(|p| p + 3)
        } else if lower_after.starts_with("<script") {
            lower_after.find("</script>").map(|p| p + 9)
        } else {
            lower_after.find("</style>").map(|p| p + 8)
        };
        match end {
            Some(e) => rest = &after[e..],
            None => break,
        }
    }
    // 标签 → 空格
    let mut text = String::with_capacity(out.len());
    let mut in_tag = false;
    for c in out.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut squeezed = String::with_capacity(text.len());
    let mut last_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !last_space {
                squeezed.push(' ');
            }
            last_space = true;
        } else {
            squeezed.push(c);
            last_space = false;
        }
    }
    (title, squeezed.trim().to_owned())
}

/// 统一的"给模型看什么"。
pub fn render(
    body: &[u8],
    content_type: Option<&str>,
    root: &str,
    call_id: &str,
    text_budget: Option<usize>,
) -> Rendered {
    let kind = classify(content_type, body);
    let budget = text_budget.unwrap_or(match kind {
        Kind::Json => JSON_INLINE,
        _ => TEXT_INLINE,
    });
    let ext = ext_of(kind, content_type);
    let text = String::from_utf8_lossy(body).to_string();

    match kind {
        Kind::Image | Kind::Audio | Kind::Video | Kind::Binary => {
            // 二进制永远不进上下文，只给元信息 + 路径
            let spilled = spill::save(root, call_id, body, ext);
            let mut out = match kind {
                Kind::Image => {
                    let dim = image_size(body)
                        .map(|(w, h)| format!("{w}×{h}"))
                        .unwrap_or_else(|| "尺寸未知".into());
                    format!("图片 · {dim} · {} KB\n", body.len() / 1024)
                }
                Kind::Audio => format!("音频 · {} KB\n", body.len() / 1024),
                Kind::Video => format!("视频 · {} KB\n", body.len() / 1024),
                _ => format!("二进制 · {} KB\n", body.len() / 1024),
            };
            match (&spilled, kind) {
                (Some(s), Kind::Image) => {
                    out.push_str(&format!(
                        "已存到 `{}`。要发给用户用 `send_file`；要分析就用 exec（python3 + PIL 如果装了）。",
                        s.sandbox_path
                    ));
                }
                (Some(s), _) => {
                    out.push_str(&format!("已存到 `{}`，需要处理用 exec。", s.sandbox_path));
                }
                (None, _) => out.push_str("（落盘失败）"),
            }
            Rendered { text: out, spilled }
        }
        Kind::Json => {
            if body.len() <= budget {
                return Rendered { text: format!("```json\n{text}\n```"), spilled: None };
            }
            let spilled = spill::save(root, call_id, body, "json");
            let mut out = String::new();
            match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(v) => {
                    out.push_str("响应较大：只给结构（完整内容在磁盘上，需要细节用 exec 读）：\n");
                    out.push_str(&json_shape(&v));
                }
                Err(_) => {
                    out.push_str("响应较大且不是合法 JSON，只给开头：\n");
                }
            }
            let preview: String = text.chars().take(1_200).collect();
            out.push_str("\n样子（前 1200 字）：\n```\n");
            out.push_str(&preview);
            out.push_str("\n```\n");
            if let Some(s) = &spilled {
                out.push_str(&format!(
                    "完整响应：`{}`（{} KB）\n例：`python3 -c \"import json;d=json.load(open('{}'));print(list(d)[:10])\"`\n",
                    s.sandbox_path,
                    s.bytes / 1024,
                    s.sandbox_path
                ));
            }
            out.push_str("不要重复拉同一个大接口；要更小的结果就加 count / 分页参数，或换更窄的接口。");
            Rendered { text: out, spilled }
        }
        Kind::Html => {
            let (title, plain) = html_to_text(&text);
            let head: String = plain.chars().take(budget).collect();
            let mut out = String::new();
            if let Some(t) = &title {
                out.push_str(&format!("标题：{t}\n"));
            }
            out.push_str("正文（已去标签）：\n");
            out.push_str(&head);
            if plain.chars().count() > budget {
                out.push_str(&format!(
                    "\n…（共 {} 字，只给了前 {budget}）",
                    plain.chars().count()
                ));
            }
            let spilled = if body.len() > budget {
                spill::save(root, call_id, body, "html")
            } else {
                None
            };
            if let Some(s) = &spilled {
                out.push_str(&format!(
                    "\n原始 HTML：`{}`（{} KB）",
                    s.sandbox_path,
                    s.bytes / 1024
                ));
            }
            Rendered { text: out, spilled }
        }
        Kind::Csv => {
            let mut lines = text.lines();
            let mut out = String::new();
            if let Some(h) = lines.next() {
                out.push_str(&format!("表头：{h}\n"));
            }
            let mut rows = 0;
            for l in lines.take(20) {
                out.push_str(l);
                out.push('\n');
                rows += 1;
            }
            out.push_str(&format!("（共 {} 行，只给前 {rows} 行）", text.lines().count().saturating_sub(1)));
            let spilled = if body.len() > budget {
                spill::save(root, call_id, body, "csv")
            } else {
                None
            };
            if let Some(s) = &spilled {
                out.push_str(&format!("\n完整 CSV：`{}`", s.sandbox_path));
            }
            Rendered { text: out, spilled }
        }
        Kind::Text => {
            if body.len() <= budget {
                return Rendered { text, spilled: None };
            }
            let spilled = spill::save(root, call_id, body, "txt");
            let head: String = text.chars().take(budget).collect();
            let mut out = format!("{head}\n…（共 {} 字，只给了前 {budget}）", text.chars().count());
            if let Some(s) = &spilled {
                out.push_str(&format!("\n全文：`{}`（{} KB）", s.sandbox_path, s.bytes / 1024));
            }
            Rendered { text: out, spilled }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_header_then_sniff() {
        assert_eq!(classify(Some("application/json; charset=utf-8"), b"{}"), Kind::Json);
        assert_eq!(classify(Some("text/html"), b""), Kind::Html);
        assert_eq!(classify(Some("image/png"), b""), Kind::Image);
        assert_eq!(classify(None, br#"{"a":1}"#), Kind::Json);
        assert_eq!(classify(None, b"<!DOCTYPE html><html>"), Kind::Html);
        assert_eq!(classify(None, b"\x89PNG\r\n\x1a\n"), Kind::Image);
    }

    #[test]
    fn html_is_stripped_of_scripts_and_tags() {
        let (title, text) = html_to_text(
            "<html><head><title>标题</title><style>p{color:red}</style></head>\
             <body><h1>你好</h1><script>alert(1)</script><p>正文 &amp; 内容</p></body></html>",
        );
        assert_eq!(title.as_deref(), Some("标题"));
        assert!(text.contains("你好") && text.contains("正文 & 内容"));
        assert!(!text.contains("alert") && !text.contains("color:red"));
        assert!(!text.contains('<'));
    }

    #[test]
    fn image_never_goes_inline() {
        let dir = tempfile::tempdir().unwrap();
        // 1x1 PNG
        let png: Vec<u8> = vec![
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13, b'I', b'H', b'D', b'R',
            0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4, 0x89,
        ];
        let r = render(&png, Some("image/png"), &dir.path().to_string_lossy(), "call_1", None);
        assert!(r.text.contains("1×1"), "{}", r.text);
        assert!(!r.text.contains('\u{89}'), "不应把二进制塞进上下文");
        assert!(r.spilled.is_some());
    }

    #[test]
    fn small_json_is_inline_big_json_is_shape() {
        let dir = tempfile::tempdir().unwrap();
        let small = br#"{"a":1}"#;
        let r = render(small, Some("application/json"), &dir.path().to_string_lossy(), "c", None);
        assert!(r.text.contains("\"a\":1"));
        assert!(r.spilled.is_none());

        let big = format!(r#"{{"items":[{}]}}"#, (0..1500).map(|i| format!(r#"{{"id":{i}}}"#)).collect::<Vec<_>>().join(","));
        let r = render(big.as_bytes(), Some("application/json"), &dir.path().to_string_lossy(), "c", None);
        assert!(r.text.contains("items: array(1500)"), "{}", r.text);
        assert!(r.text.chars().count() < 2500);
        assert!(r.spilled.unwrap().sandbox_path.starts_with("/ws/.tool-results/"));
    }
}
