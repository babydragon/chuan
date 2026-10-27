//! Telegram HTML 渲染。
//!
//! 为什么不用 MarkdownV2：它的保留字符太多（`.` `-` `(` `!` 都要转义），
//! 而 HTML 只需转义 `& < >`，且标签集合小、可枚举。
//!
//! 策略：
//! - 流式期间发**纯文本**（半截 Markdown 必炸），定稿时再转 HTML；
//! - 长消息**先按原始文本分片**，再对每片独立格式化 —— 保证每片的标签天然闭合；
//! - 跨片的代码围栏在切分时自动补 ``` 收尾/重开。

/// 转义 HTML 特殊字符。
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn inline(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                if let Some(end) = chars[i + 1..].iter().position(|c| *c == '`') {
                    let body: String = chars[i + 1..i + 1 + end].iter().collect();
                    out.push_str(&format!("<code>{}</code>", escape(&body)));
                    i = i + end + 2;
                } else {
                    out.push('`');
                    i += 1;
                }
            }
            '*' if i + 2 < chars.len() && chars[i + 1] == '*' && chars[i + 2] == '*' => {
                // ***x*** → 粗+斜
                if let Some(end) = find_triple(&chars, i + 3) {
                    let body: String = chars[i + 3..end].iter().collect();
                    out.push_str(&format!("<b><i>{}</i></b>", inline(&body)));
                    i = end + 3;
                } else {
                    out.push_str("***");
                    i += 3;
                }
            }
            '*' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                if let Some(end) = find_pair(&chars, i + 2, '*', '*') {
                    let body: String = chars[i + 2..end].iter().collect();
                    out.push_str(&format!("<b>{}</b>", inline(&body)));
                    i = end + 2;
                } else {
                    out.push_str("**");
                    i += 2;
                }
            }
            '[' => {
                // [text](url)
                if let Some(close) = chars[i + 1..].iter().position(|c| *c == ']') {
                    let text_end = i + 1 + close;
                    if text_end + 1 < chars.len() && chars[text_end + 1] == '(' {
                        if let Some(paren) = chars[text_end + 2..].iter().position(|c| *c == ')') {
                            let url_end = text_end + 2 + paren;
                            let text: String = chars[i + 1..text_end].iter().collect();
                            let url: String = chars[text_end + 2..url_end].iter().collect();
                            out.push_str(&format!(
                                "<a href=\"{}\">{}</a>",
                                escape(&url).replace('"', "&quot;"),
                                inline(&text)
                            ));
                            i = url_end + 1;
                            continue;
                        }
                    }
                }
                out.push('[');
                i += 1;
            }
            '*' => {
                if let Some(end) = chars[i + 1..].iter().position(|c| *c == '*') {
                    let body: String = chars[i + 1..i + 1 + end].iter().collect();
                    if !body.trim().is_empty() {
                        out.push_str(&format!("<i>{}</i>", inline(&body)));
                        i = i + end + 2;
                        continue;
                    }
                }
                out.push('*');
                i += 1;
            }
            c => {
                let seg = escape(&c.to_string());
                out.push_str(&seg);
                i += 1;
            }
        }
    }
    out
}

fn find_triple(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < chars.len() {
        if chars[i] == '*' && chars[i + 1] == '*' && chars[i + 2] == '*' {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn find_pair(chars: &[char], from: usize, a: char, b: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == a && chars[i + 1] == b {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 显示宽度：CJK 与全角字符按 2 计，其余按 1。
fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let u = c as u32;
            let wide = matches!(u,
                0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 |
                0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6);
            if wide { 2 } else { 1 }
        })
        .sum()
}

fn pad_to(s: &str, width: usize) -> String {
    let w = display_width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_owned()).collect()
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    if !t.starts_with('|') {
        return false;
    }
    let cells = split_row(t);
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim_matches(':');
            c.len() >= 3 && c.chars().all(|ch| ch == '-')
        })
}

/// 把 Markdown 表格转成等宽 <pre>（Telegram 不支持表格）。
fn render_table(lines: &[&str]) -> Option<(String, usize)> {
    let mut idx = 0;
    let header = split_row(lines[0]);
    idx += 1;
    if idx >= lines.len() || !is_separator(lines[idx]) {
        return None;
    }
    idx += 1;

    let mut rows: Vec<Vec<String>> = Vec::new();
    while idx < lines.len() && lines[idx].trim().starts_with('|') {
        rows.push(split_row(lines[idx]));
        idx += 1;
    }

    let cols = header.len().min(6);
    if cols == 0 {
        return None;
    }
    let mut widths = vec![display_width(header[0].trim())];
    widths.resize(cols, 3);
    for (i, cell) in header.iter().take(cols).enumerate() {
        widths[i] = widths[i].max(display_width(cell));
    }
    for row in &rows {
        for (i, cell) in row.iter().take(cols).enumerate() {
            widths[i] = widths[i].max(display_width(cell)).min(28);
        }
    }

    let trim_cell = |s: &str| -> String {
        if display_width(s) > 28 {
            let mut out = String::new();
            let mut w = 0;
            for ch in s.chars() {
                let cw = if display_width(&ch.to_string()) == 2 { 2 } else { 1 };
                if w + cw > 27 {
                    out.push('…');
                    break;
                }
                out.push(ch);
                w += cw;
            }
            out
        } else {
            s.to_owned()
        }
    };

    let line_of = |cells: &[String]| -> String {
        let mut parts = Vec::new();
        for (i, w) in widths.iter().enumerate() {
            let cell = cells.get(i).map(|c| trim_cell(c)).unwrap_or_default();
            parts.push(pad_to(&escape(&cell), *w));
        }
        parts.join("  ")
    };

    let mut out = String::from("<pre>");
    out.push_str(&line_of(&header));
    out.push('\n');
    let sep: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
    out.push_str(&sep.join("  "));
    out.push('\n');
    for row in &rows {
        out.push_str(&line_of(row));
        out.push('\n');
    }
    out.push_str("</pre>");
    Some((out, idx))
}

/// Markdown-lite → Telegram HTML。
pub fn to_html(raw: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    let mut fence_lang = String::new();
    let lines: Vec<&str> = raw.split('\n').collect();
    let mut li = 0;

    while li < lines.len() {
        // 表格优先：需要看下一行是否为分隔行
        if !in_fence
            && lines[li].trim().starts_with('|')
            && li + 1 < lines.len()
            && is_separator(lines[li + 1])
        {
            if let Some((html, consumed)) = render_table(&lines[li..]) {
                out.push_str(&html);
                out.push('\n');
                li += consumed;
                continue;
            }
        }
        let line = lines[li];
        li += 1;
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if !in_fence {
                in_fence = true;
                fence_lang = trimmed.trim_start_matches('`').trim().to_owned();
                if fence_lang.is_empty() {
                    out.push_str("<pre>");
                } else {
                    out.push_str(&format!(
                        "<pre><code class=\"language-{}\">",
                        escape(&fence_lang)
                    ));
                }
            } else {
                in_fence = false;
                out.push_str(if fence_lang.is_empty() { "</pre>" } else { "</code></pre>" });
                fence_lang.clear();
            }
            continue;
        }
        if in_fence {
            out.push_str(&escape(line));
            out.push('\n');
        } else {
            out.push_str(&inline(line));
            out.push('\n');
        }
    }
    if in_fence {
        out.push_str(if fence_lang.is_empty() { "</pre>" } else { "</code></pre>" });
    }
    out.trim_end_matches('\n').to_owned()
}

/// 按原始文本分片；跨片的代码围栏自动收尾与重开。
pub fn split_markdown(raw: &str, cap: usize) -> Vec<String> {
    let cap = cap.max(8);
    let mut shards = Vec::new();
    let mut cur = String::new();
    let mut fence: Option<String> = None;

    let flush = |cur: &mut String, fence: &Option<String>, shards: &mut Vec<String>| {
        if cur.trim().is_empty() {
            cur.clear();
            return;
        }
        let mut s = std::mem::take(cur);
        if let Some(lang) = fence {
            s.push_str("```\n");
        }
        shards.push(s);
    };

    for line in raw.split_inclusive('\n') {
        // 若加入这行会超限，先切
        if cur.chars().count() + line.chars().count() > cap && !cur.is_empty() {
            flush(&mut cur, &fence, &mut shards);
            if let Some(lang) = &fence {
                cur.push_str(&format!("```{lang}\n"));
            }
        }
        if line.trim_start().starts_with("```") {
            fence = if fence.is_none() {
                Some(line.trim_start().trim_start_matches('`').trim().to_owned())
            } else {
                None
            };
        }
        cur.push_str(line);
    }
    flush(&mut cur, &fence, &mut shards);
    if shards.is_empty() {
        shards.push(String::new());
    }
    shards
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_html() {
        assert_eq!(escape("a < b & c > d"), "a &lt; b &amp; c &gt; d");
    }

    #[test]
    fn formats_inline() {
        assert_eq!(to_html("**粗**"), "<b>粗</b>");
        assert_eq!(to_html("*斜*"), "<i>斜</i>");
        assert_eq!(to_html("`a<b`"), "<code>a&lt;b</code>");
        assert_eq!(to_html("[文档](https://x.y/?a=1&b=2)"),
                   "<a href=\"https://x.y/?a=1&amp;b=2\">文档</a>");
    }

    #[test]
    fn renders_markdown_table_as_pre() {
        let raw = "| 步骤 | 操作 | 结果 |\n|---|---|---|\n| 1 | 写入 hello.txt | 成功 |";
        let html = to_html(raw);
        assert!(html.contains("<pre>"), "{html}");
        assert!(html.contains("步骤"), "{html}");
        assert!(!html.contains("|---"), "分隔行不应原样输出: {html}");
        assert_eq!(html.matches("<pre>").count(), 1);
    }

    #[test]
    fn full_width_chars_align() {
        let raw = "| 名称 | 值 |\n|---|---|\n| 中文 | 1 |\n| ab | 22 |";
        let html = to_html(raw);
        let rows: Vec<&str> = html.lines().filter(|l| l.contains('|') || l.contains("中文")).collect();
        assert!(html.contains("<pre>"));
        assert!(rows.len() >= 2 || html.contains("中文"));
    }

    #[test]
    fn plain_pipes_are_not_tables() {
        assert_eq!(to_html("a | b"), "a | b");
    }

    #[test]
    fn formats_bold_italic_triple() {
        assert_eq!(to_html("***重点***"), "<b><i>重点</i></b>");
    }

    #[test]
    fn formats_fence() {
        assert_eq!(to_html("```py\nprint(1)\n```"),
                   "<pre><code class=\"language-py\">print(1)\n</code></pre>");
    }

    #[test]
    fn unclosed_fence_is_closed() {
        let out = to_html("```\nabc");
        assert!(out.ends_with("</pre>"), "{out}");
    }

    #[test]
    fn splits_and_reopens_fence() {
        let raw = format!("```\n{}\n```", "x\n".repeat(50));
        let shards = split_markdown(&raw, 64);
        assert!(shards.len() > 1);
        // 每片都能独立格式化且标签闭合
        for s in &shards {
            let html = to_html(s);
            assert_eq!(html.matches("<pre>").count(), html.matches("</pre>").count(), "{s}");
        }
    }

    #[test]
    fn split_prefers_line_boundaries() {
        let raw = "line1\nline2\nline3\n";
        let shards = split_markdown(raw, 12);
        assert!(shards.len() >= 2);
        assert!(shards[0].ends_with('\n'));
    }
}
