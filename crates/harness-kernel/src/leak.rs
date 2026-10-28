//! 凭据形态检测：命中就替换成 `[REDACTED:<kind>]`。
//!
//! 目的不是"禁止模型做某件事"（靠形状禁止永远追不上编码变体），而是
//! **让值不落地**：工具输出在进入上下文与账本之前过一遍这里。
//!
//! 覆盖的是"看起来像凭据"的常见形态，属于兜底；真正的边界是凭据根本不进
//! 沙盒、不在工具参数里出现（见 `docs/DECISIONS.md` 的 D18）。

fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.'
}

/// 从 `s` 开头起，满足条件的字节数（`s` 必须是 ASCII 起始的片段）。
fn run_len(s: &str, pred: impl Fn(u8) -> bool) -> usize {
    s.as_bytes().iter().take_while(|b| pred(**b)).count()
}

/// 大小写不敏感的前缀匹配，返回匹配到的字节数。
fn prefix_ci(s: &str, pat: &str) -> Option<usize> {
    let head = s.get(..pat.len())?;
    head.eq_ignore_ascii_case(pat).then_some(pat.len())
}

/// 在 `s` 位置 0 处尝试识别一种凭据形态，返回 (kind, 总字节数)。
fn match_at(s: &str) -> Option<(&'static str, usize)> {
    // 私钥块：整段遮掉
    if s.starts_with("-----BEGIN") {
        if let Some(end) = s.find("PRIVATE KEY-----") {
            let tail = &s[end + "PRIVATE KEY-----".len()..];
            let span = match tail.find("-----END") {
                Some(e) => end + "PRIVATE KEY-----".len() + e + tail[e..].find("-----").map(|x| x + 5).unwrap_or(0),
                None => end + "PRIVATE KEY-----".len(),
            };
            return Some(("private-key", span));
        }
    }

    // 明确的厂商前缀：前缀 + 最小长度 + 字符集
    const PREFIXED: &[(&str, &str, usize, fn(u8) -> bool)] = &[
        ("icsbx_", "placeholder", 16, is_alnum),
        ("sk-", "api-key", 16, is_alnum),
        ("ghp_", "github-token", 20, is_alnum),
        ("gho_", "github-token", 20, is_alnum),
        ("ghu_", "github-token", 20, is_alnum),
        ("ghs_", "github-token", 20, is_alnum),
        ("ghr_", "github-token", 20, is_alnum),
        ("xoxb-", "slack-token", 10, is_token_char),
        ("xoxp-", "slack-token", 10, is_token_char),
        ("xoxa-", "slack-token", 10, is_token_char),
        ("xoxr-", "slack-token", 10, is_token_char),
        ("AKIA", "aws-key", 16, is_alnum),
    ];
    for (pfx, kind, min, charset) in PREFIXED {
        if let Some(rest) = s.strip_prefix(pfx) {
            let n = run_len(rest, charset);
            if n >= *min {
                return Some((kind, pfx.len() + n));
            }
        }
    }

    // JWT：三段 base64url，用 . 分隔
    if s.starts_with("eyJ") {
        let n = run_len(s, |b| is_token_char(b));
        let head = &s[..n];
        if n >= 24 && head.matches('.').count() == 2 {
            return Some(("jwt", n));
        }
    }

    // Authorization: xxx —— 整行遮掉（最常见的"手滑打日志"）
    if let Some(n) = prefix_ci(s, "authorization:") {
        let rest = &s[n..];
        let skip = run_len(rest, |b| b == b' ');
        let val = &rest[skip..];
        let vlen = val.find(['\n', '\r']).unwrap_or(val.len());
        if vlen >= 4 {
            return Some(("auth-header", n + skip + vlen));
        }
    }

    // Bearer <token>
    if let Some(n) = prefix_ci(s, "bearer ") {
        let n = run_len(s, |b| b == b' ').max(n);
        let rest = &s[n..];
        let vlen = run_len(rest, is_token_char);
        if vlen >= 16 {
            return Some(("bearer", n + vlen));
        }
    }

    None
}

/// 把 `text` 里像凭据的片段替换掉，返回 (处理后文本, 命中的种类)。
pub fn redact(text: &str) -> (String, Vec<&'static str>) {
    let mut out = String::with_capacity(text.len());
    let mut hits: Vec<&'static str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        // 只有当当前字节是 ASCII 起点时才可能是前缀
        match match_at(rest) {
            Some((kind, len)) if len > 0 && len <= rest.len() => {
                out.push_str(&format!("[REDACTED:{kind}]"));
                hits.push(kind);
                rest = &rest[len..];
            }
            _ => {
                let ch = rest.chars().next().expect("非空");
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    (out, hits)
}

/// 已知的具体值（例如本次请求注入的 token）：出现即替换。
/// 少于 6 个字符的值不处理，避免把正常文字打得到处是洞。
pub fn scrub_values(text: &str, values: &[&str]) -> String {
    let mut out = text.to_owned();
    for v in values {
        let v = v.trim();
        if v.chars().count() < 6 {
            continue;
        }
        if out.contains(v) {
            out = out.replace(v, "***");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        redact(s).0
    }

    #[test]
    fn catches_common_token_shapes() {
        assert!(clean("key sk-abcdefghijklmnop1234 here").contains("[REDACTED:api-key]"));
        assert!(clean("ghp_abcdefghijklmnopqrstuvwxyz0123456789").contains("[REDACTED:github-token]"));
        assert!(clean("xoxb-1234567890-abcdefghij").contains("[REDACTED:slack-token]"));
        assert!(clean("AKIAIOSFODNN7EXAMPLE").contains("[REDACTED:aws-key]"));
        assert!(clean("icsbx_0123456789abcdef0123456789abcdef").contains("[REDACTED:placeholder]"));
    }

    #[test]
    fn catches_auth_headers_and_bearer() {
        let out = clean("Authorization: Bearer abc.def.ghi\nnext line");
        assert!(out.contains("[REDACTED:auth-header]"), "{out}");
        assert!(out.contains("next line"), "只遮这一行: {out}");
        let out = clean("use Bearer abcdefghijklmnopqrstuvwxyz to call");
        assert!(out.contains("[REDACTED:bearer]"), "{out}");
        assert!(out.contains("to call"));
    }

    #[test]
    fn catches_private_key_block() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nAAAA\n-----END RSA PRIVATE KEY-----";
        let out = clean(pem);
        assert!(out.contains("[REDACTED:private-key]"), "{out}");
        assert!(!out.contains("MIIEow"), "私钥内容不能留下: {out}");
    }

    #[test]
    fn leaves_normal_text_alone() {
        let text = "主机 k1 跑了 3 天，磁盘用了 18G。见 /ws/report.md";
        assert_eq!(clean(text), text);
        // 短值、相似但不够长的前缀都不该被误伤
        assert_eq!(clean("sk-short"), "sk-short");
        assert_eq!(clean("Authorization:"), "Authorization:");
    }

    #[test]
    fn scrub_known_values() {
        let out = scrub_values("token=abc123456789&x=1", &["abc123456789"]);
        assert_eq!(out, "token=***&x=1");
        // 太短的值不动，避免误伤
        let out = scrub_values("id=42", &["42"]);
        assert_eq!(out, "id=42");
    }
}
