//! 上下文裁剪：按"轮"为单位保留最近内容，绝不切开半轮。
//!
//! 为什么不能按消息条数截断：assistant(tool_calls) 和对应 tool_result 必须成对出现，
//! 缺一条 provider 会直接 400。

use serde_json::json;

use crate::ids::Seq;
use crate::record::{Message, Role};

/// 粗略 token 估算：CJK 按 1 字 ≈ 1 token，其它按 ~4 字符 ≈ 1 token。
///
/// 不引真分词器：这里只用来决定"历史裁多少"，宁可保守。
/// 以前按 `字符数 × 3 = token` 换算（英文经验），中文会高估容量约 2–3 倍，
/// 结果是历史塞得过满、请求被供应商以 400 拒绝。
pub fn estimate_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in s.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3000..=0x303F      // CJK 标点
        | 0x3040..=0x30FF    // 假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本区
        | 0xAC00..=0xD7AF    // 谚文
        | 0xF900..=0xFAFF    // 兼容汉字
        | 0xFF00..=0xFFEF    // 全角
        | 0x20000..=0x2FA1F  // 扩展 B+
    )
}

/// 从尾部保留，直到估算 token 数超过预算；返回 (保留的消息, 丢弃条数)。
pub fn select(history: &[Message], budget_tokens: usize) -> (Vec<Message>, usize) {
    if history.is_empty() {
        return (Vec::new(), 0);
    }

    // 按 turn 分组：同一 turn 的消息不可拆分；turn=None（用户消息）自成一组
    let mut groups: Vec<Vec<Message>> = Vec::new();
    for m in history {
        let same = match (m.turn.as_ref(), groups.last().and_then(|g| g.last())) {
            (Some(a), Some(prev)) => prev.turn.as_ref() == Some(a),
            _ => false,
        };
        if same {
            groups.last_mut().unwrap().push(m.clone());
        } else {
            groups.push(vec![m.clone()]);
        }
    }

    let mut total = 0usize;
    let mut start = groups.len();
    for (i, g) in groups.iter().enumerate().rev() {
        let cost: usize = g.iter().map(|m| estimate_tokens(&m.payload.to_string())).sum();
        if total + cost > budget_tokens && total > 0 {
            break;
        }
        total += cost;
        start = i;
    }

    let dropped: usize = groups[..start].iter().map(|g| g.len()).sum();
    let mut out = Vec::new();
    if dropped > 0 {
        out.push(Message {
            seq: 0 as Seq,
            turn: None,
            role: Role::Synthetic,
            payload: json!({ "content": format!("（较早的 {dropped} 条消息已省略）") }),
        });
    }
    for g in &groups[start..] {
        out.extend(g.clone());
    }
    (out, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TurnId;

    fn msg(turn: Option<&str>, role: Role, text: &str) -> Message {
        Message {
            seq: 1,
            turn: turn.map(|t| TurnId(t.into())),
            role,
            payload: json!({ "text": text }),
        }
    }

    #[test]
    fn keeps_whole_turns_only() {
        let mut h = vec![msg(None, Role::User, "开始")];
        for t in 0..10 {
            let id = format!("t{t}");
            h.push(msg(Some(&id), Role::Assistant, &"x".repeat(100)));
            h.push(msg(Some(&id), Role::ToolResult, &"y".repeat(100)));
        }
        let (kept, dropped) = select(&h, 60);
        assert!(dropped > 0, "应丢弃旧内容");
        assert!(matches!(kept[0].role, Role::Synthetic), "应有省略提示");
        // 每条 tool_result 都能找到同 turn 的 assistant
        let body = &kept[1..];
        for (i, m) in body.iter().enumerate() {
            if m.role == Role::ToolResult {
                assert!(i > 0 && body[i - 1].turn == m.turn);
            }
        }
    }

    #[test]
    fn cjk_costs_about_one_token_per_char() {
        assert!(estimate_tokens("第一单元") >= 4);
        // 同样字符数，中文比英文贵得多
        assert!(estimate_tokens("田字格生字表") > estimate_tokens("abcdefg"));
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn small_history_untouched() {
        let h = vec![msg(None, Role::User, "hi")];
        let (kept, dropped) = select(&h, 10_000);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 1);
    }
}
