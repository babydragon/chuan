//! 上下文裁剪：按"轮"为单位保留最近内容，绝不切开半轮。
//!
//! 为什么不能按消息条数截断：assistant(tool_calls) 和对应 tool_result 必须成对出现，
//! 缺一条 provider 会直接 400。

use serde_json::json;

use crate::ids::Seq;
use crate::record::{Message, Role};

/// 从尾部保留，直到估算字符数超过预算；返回 (保留的消息, 丢弃条数)。
pub fn select(history: &[Message], budget_chars: usize) -> (Vec<Message>, usize) {
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
        let cost: usize = g.iter().map(|m| m.payload.to_string().len()).sum();
        if total + cost > budget_chars && total > 0 {
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
        let (kept, dropped) = select(&h, 500);
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
    fn small_history_untouched() {
        let h = vec![msg(None, Role::User, "hi")];
        let (kept, dropped) = select(&h, 10_000);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 1);
    }
}
