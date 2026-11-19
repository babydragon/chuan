//! 上下文裁剪：按"轮"为单位保留最近内容，绝不切开半轮。
//!
//! 为什么不能按消息条数截断：assistant(tool_calls) 和对应 tool_result 必须成对出现，
//! 缺一条 provider 会直接 400。
//!
//! 三级降级（`DESIGN.md` L4「超限降级」的前两级，第三级是拒绝）：
//! 1. **最近 N 轮原文不动**（`KEEP_RECENT_TURNS`）——正在进行的工作不能被动过；
//! 2. **更早轮次的工具输出折叠成一行引用**——原文是"过程"，结论才是"结果"；
//! 3. 仍然超预算才**整轮丢弃**，但被丢轮次的提问与结论会压成摘要留在前面。
//!
//! 第 3 级的摘要**不是模型生成的**：它是原文截断，所以不会编造，也不会多花一次请求。
//! 真正的语义压缩（LLM 摘要 + `ctx.compacted`）是另一件事，见 ROADMAP。

use serde_json::{Value, json};

use crate::ids::Seq;
use crate::record::{Message, Role};

/// 最近这么多轮（turn 组）永远原文保留。
///
/// 8 与默认 `max_turns=8` 对齐：一次长的前台 run 的全部回合都落在保护区内，
/// 不会出现"跑到一半自己的工具输出被叠成引用"。
const KEEP_RECENT_TURNS: usize = 8;

/// 折叠摘要每行的上限（字）。结论通常在前几百字，再长就不是摘要了。
const CARRY_LINE_CAP: usize = 400;

/// 折叠摘要最多占总预算的 1/N：摘要自己不能把预算吃光。
const CARRY_SHARE: usize = 8;

/// 旧 assistant 的 `tool_calls` 参数裁到多少字：保留"调过什么"，不保留整段脚本。
const ARGS_CAP: usize = 120;

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

fn cost(m: &Message) -> usize {
    estimate_tokens(&m.payload.to_string())
}

/// 从尾部保留，直到估算 token 数超过预算；返回 (保留的消息, 丢弃条数)。
///
/// "丢弃"只统计**整轮被丢**的消息；折叠成引用的工具输出仍在返回结果里（条数不变）。
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

    let recent_start = groups.len().saturating_sub(KEEP_RECENT_TURNS);

    // 第 2 级：更早轮次的工具输出折叠成引用（参数也一起收），保留工具名与落盘路径。
    let mut elided_tools = 0usize;
    for g in &mut groups[..recent_start] {
        for m in g.iter_mut() {
            if m.role == Role::ToolResult {
                if m.payload.get("elided").is_some_and(|v| v == &json!(true)) {
                    continue;
                }
                m.payload = elided_tool_payload(&m.payload);
                elided_tools += 1;
            } else if m.role == Role::Assistant && elide_tool_args(&mut m.payload) {
                elided_tools += 1;
            }
        }
    }

    let mut total: usize = groups.iter().flatten().map(cost).sum();
    if total <= budget_tokens {
        return (groups.into_iter().flatten().collect(), 0);
    }

    // 第 3 级：整轮丢弃，但把被丢轮次的提问与结论压成摘要放在最前面。
    let carry_cap = (budget_tokens / CARRY_SHARE).max(64);
    let mut start = 0usize;
    let mut dropped = 0usize;
    let mut carry: Vec<String> = Vec::new();
    let mut carry_cost = 0usize;
    // 至少留最后一组：那一组是"用户刚问的这句"，丢了等于没输入。
    while start + 1 < groups.len() && total > budget_tokens {
        let g = &groups[start];
        total = total.saturating_sub(g.iter().map(cost).sum::<usize>());
        dropped += g.len();
        // 摘要是新加的内容，也要占预算；只累加这一轮新增的部分。
        let before = carry_cost;
        for line in carry_lines(g) {
            let c = estimate_tokens(&line);
            if carry_cost + c > carry_cap {
                break;
            }
            carry_cost += c;
            carry.push(line);
        }
        total += carry_cost - before;
        start += 1;
    }
    if dropped > 0 || elided_tools > 0 {
        tracing::debug!(
            dropped,
            elided_tools,
            kept_tokens = total,
            "history trimmed"
        );
    }

    let mut out = Vec::new();
    if start > 0 {
        let mut body = format!(
            "（较早的 {dropped} 条消息已折叠：下面是当时的提问与结论摘录，工具原始输出未保留）"
        );
        for line in &carry {
            body.push('\n');
            body.push_str(line);
        }
        out.push(Message {
            seq: 0 as Seq,
            turn: None,
            role: Role::Synthetic,
            payload: json!({ "content": body }),
        });
    }
    for g in &groups[start..] {
        out.extend(g.clone());
    }
    (out, dropped)
}

/// 工具输出 → 一行引用。保留 `call_id`（配对必需）与 spill 路径（能重新读原文）。
fn elided_tool_payload(payload: &Value) -> Value {
    let call_id = payload.get("call_id").cloned().unwrap_or_default();
    let path = payload.get("spill").and_then(|v| v.as_str());
    let preview = match path {
        Some(p) => format!("（旧工具输出已省略；原文在 {p}，需要时用 read / exec 取）"),
        None => "（旧工具输出已省略，原文未保留；需要时重新调用该工具）".to_owned(),
    };
    json!({ "call_id": call_id, "ok": payload.get("ok").cloned().unwrap_or(json!(true)),
            "preview": preview, "elided": true })
}

/// 把旧 assistant 消息里的工具参数裁短：保留"调了哪个工具"，丢掉整段脚本。
///
/// 返回是否真的裁过，用于日志。
fn elide_tool_args(payload: &mut Value) -> bool {
    let Some(calls) = payload.get_mut("tool_calls").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut hit = false;
    for c in calls.iter_mut() {
        let Some(args) = c.get("args") else { continue };
        if estimate_tokens(&args.to_string()) <= ARGS_CAP {
            continue;
        }
        c["args"] = json!({ "_elided": true });
        hit = true;
    }
    hit
}

/// 被整轮丢掉的内容里，值得留成摘要的那些行。
///
/// 只留"人说的话"与"结论"：工具结果已经是过程，不再占摘要预算。
fn carry_lines(group: &[Message]) -> Vec<String> {
    let mut out = Vec::new();
    for m in group {
        let text = m.payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let line = match m.role {
            Role::User => format!("用户：{}", clip(text, CARRY_LINE_CAP)),
            Role::Assistant => {
                if text.trim().is_empty() {
                    let names = tool_names(&m.payload);
                    if names.is_empty() {
                        continue;
                    }
                    format!("你：调用了 {names}")
                } else {
                    format!("你：{}", clip(text, CARRY_LINE_CAP))
                }
            }
            _ => continue,
        };
        out.push(line);
    }
    out
}

fn tool_names(payload: &Value) -> String {
    payload
        .get("tool_calls")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default()
}

fn clip(text: &str, cap: usize) -> String {
    let t = text.trim();
    if t.chars().count() <= cap {
        return t.to_owned();
    }
    let head: String = t.chars().take(cap).collect();
    format!("{head}…")
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

    fn tool_result(turn: &str, call: &str, body: &str) -> Message {
        Message {
            seq: 1,
            turn: Some(TurnId(turn.into())),
            role: Role::ToolResult,
            payload: json!({ "call_id": call, "ok": true, "preview": body }),
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

    /// 核心诉求：工具输出比结论便宜。旧轮次的工具输出先折叠，结论原文留着。
    #[test]
    fn old_tool_output_elided_before_conclusion() {
        let mut h = vec![msg(None, Role::User, "把第15课的词整理出来")];
        // 旧轮：一次 6000 字的工具输出 + 一条 200 字的结论
        h.push(msg(Some("t1"), Role::Assistant, ""));
        h.push(tool_result("t1", "call_1", &"数".repeat(6_000)));
        h.push(msg(Some("t1"), Role::Assistant, &"结".repeat(200)));
        // 最近 8 轮里塞点噪音把预算占住
        for t in 2..12 {
            let id = format!("t{t}");
            h.push(msg(Some(&id), Role::Assistant, &"z".repeat(50)));
            h.push(tool_result(&id, "call_x", &"z".repeat(50)));
        }

        let (kept, _dropped) = select(&h, 3_000);
        let texts: Vec<String> = kept.iter().map(|m| m.payload.to_string()).collect();
        let joined = texts.join("\n");
        assert!(
            joined.contains("旧工具输出已省略"),
            "旧工具输出应被折叠成引用"
        );
        assert!(
            !joined.contains(&"数".repeat(100)),
            "6000 字的输出不该还在上下文里"
        );
        assert!(contains_text(&kept, &"结".repeat(200)), "结论应原文保留");
    }

    fn contains_text(msgs: &[Message], needle: &str) -> bool {
        msgs.iter().any(|m| m.payload.to_string().contains(needle))
    }

    /// 最近几轮一个字都不动：正在做的活不能被动过。
    #[test]
    fn recent_turns_never_elided() {
        let mut h = Vec::new();
        for t in 0..3 {
            let id = format!("t{t}");
            h.push(msg(Some(&id), Role::Assistant, &"a".repeat(10)));
            h.push(tool_result(&id, "call_x", &"b".repeat(1_000)));
        }
        let (kept, _) = select(&h, 500);
        assert!(
            kept.iter()
                .any(|m| m.payload.to_string().contains(&"b".repeat(1_000)))
        );
    }

    /// 硬丢整轮时，被丢轮次的结论要进摘要，而不是凭空消失。
    #[test]
    fn dropped_turns_leave_conclusions_in_summary() {
        let mut h = vec![msg(None, Role::User, "第15课田字格有几个字")];
        h.push(msg(
            Some("t1"),
            Role::Assistant,
            "13 个字：蒲英盛耍使劲脸欠朝钓察拢喜",
        ));
        for t in 2..14 {
            let id = format!("t{t}");
            h.push(msg(Some(&id), Role::Assistant, &"z".repeat(400)));
            h.push(tool_result(&id, "call_x", &"z".repeat(400)));
        }
        let (kept, dropped) = select(&h, 1_500);
        assert!(dropped > 0);
        assert_eq!(kept[0].role, Role::Synthetic);
        let head = kept[0].payload["content"].as_str().unwrap();
        assert!(
            head.contains("13 个字"),
            "被丢弃的结论应从摘要里看得到：{head}"
        );
    }

    /// 折叠后的工具消息仍带 call_id：assistant(tool_calls) 与 tool_result 必须配得上。
    #[test]
    fn elided_tool_keeps_call_id() {
        let mut h = Vec::new();
        for t in 0..10 {
            let id = format!("t{t}");
            h.push(msg(Some(&id), Role::Assistant, &"a".repeat(900)));
            h.push(tool_result(&id, &format!("call_{t}"), &"b".repeat(900)));
        }
        let (kept, _) = select(&h, 2_000);
        for m in &kept {
            if m.role == Role::ToolResult {
                assert!(m.payload["call_id"].as_str().is_some_and(|s| !s.is_empty()));
            }
        }
    }

    /// 旧工具的整段脚本参数要收掉，只留"调了哪个工具"。
    #[test]
    fn old_tool_args_shrunk() {
        let mut h = vec![msg(None, Role::User, "跑一下")];
        h.push(Message {
            seq: 1,
            turn: Some(TurnId("t1".into())),
            role: Role::Assistant,
            payload: json!({ "text": "", "tool_calls": [
                { "call_id": "c1", "name": "write", "args": { "body": "x".repeat(2_000) } }
            ]}),
        });
        h.push(tool_result("t1", "c1", "ok"));
        for t in 2..12 {
            let id = format!("t{t}");
            h.push(msg(Some(&id), Role::Assistant, &"z".repeat(200)));
            h.push(tool_result(&id, "call_x", &"z".repeat(200)));
        }
        let (kept, _) = select(&h, 2_000);
        let call = kept
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .find_map(|m| {
                m.payload
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .cloned()
            })
            .and_then(|arr| arr.first().cloned())
            .expect("旧 assistant 的 tool_calls 应还在");
        assert_eq!(call["name"], "write");
        assert_eq!(call["args"], json!({ "_elided": true }));
    }
}
