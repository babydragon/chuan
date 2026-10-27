//! 协议无关的聚合：消费 `WireDelta`，产出一次模型回合的完整结果。

use std::collections::BTreeMap;

use harness_kernel::{CallId, EventSink, StreamEvent, ToolCallRequest};

use crate::wire::WireDelta;

#[derive(Default)]
struct CallAcc {
    id: Option<String>,
    name: String,
    args: String,
}

#[derive(Default)]
pub struct Aggregator {
    pub text: String,
    pub reasoning: String,
    calls: BTreeMap<u32, CallAcc>,
    pub finish_reason: Option<String>,
    pub usage: Option<(u32, u32)>,
}

pub struct Aggregate {
    pub text: String,
    pub reasoning: String,
    pub calls: Vec<ToolCallRequest>,
    pub finish_reason: Option<String>,
    pub usage: Option<(u32, u32)>,
}

impl Aggregator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consume(&mut self, d: WireDelta, sink: &EventSink) {
        match d {
            WireDelta::Text(t) => {
                self.text.push_str(&t);
                sink.emit(StreamEvent::TextDelta(t));
            }
            WireDelta::Reasoning(r) => {
                self.reasoning.push_str(&r);
                sink.emit(StreamEvent::ReasoningDelta(r));
            }
            WireDelta::ToolCall { index, id, name, args } => {
                let acc = self.calls.entry(index).or_default();
                if let Some(id) = id {
                    acc.id = Some(id);
                }
                if let Some(n) = name {
                    acc.name.push_str(&n);
                }
                if let Some(a) = args {
                    acc.args.push_str(&a);
                }
            }
            WireDelta::Finish(f) => self.finish_reason = Some(f),
            WireDelta::Usage { prompt_tokens, completion_tokens } => {
                self.usage = Some((prompt_tokens, completion_tokens));
            }
        }
    }

    /// 收尾：参数已拼接完整；冲突（同名 index 分片乱序）由拼接顺序保证。
    pub fn finish(self, sink: &EventSink) -> Aggregate {
        let calls = self
            .calls
            .into_iter()
            .map(|(idx, acc)| {
                let id = acc.id.unwrap_or_else(|| format!("call_{idx}"));
                let args = if acc.args.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(&acc.args)
                        .unwrap_or_else(|_| serde_json::json!({ "__raw": acc.args }))
                };
                let preview = summarize_args(&args);
                sink.emit(StreamEvent::ToolStarted {
                    call: CallId(id.clone()),
                    name: acc.name.clone(),
                    preview,
                });
                ToolCallRequest { call: CallId(id), name: acc.name, args }
            })
            .collect();

        Aggregate {
            text: self.text,
            reasoning: self.reasoning,
            calls,
            finish_reason: self.finish_reason,
            usage: self.usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_fragmented_arguments() {
        let pair = EventSink::channel(64);
        let mut agg = Aggregator::new();
        agg.consume(WireDelta::Text("你".into()), &pair.sink);
        agg.consume(WireDelta::ToolCall { index: 0, id: Some("c".into()), name: Some("exec".into()), args: Some("{\"argv\":[\"ec".into()) }, &pair.sink);
        agg.consume(WireDelta::ToolCall { index: 0, id: None, name: None, args: Some("ho\"]}".into()) }, &pair.sink);
        let done = agg.finish(&pair.sink);
        assert_eq!(done.text, "你");
        assert_eq!(done.calls[0].args["argv"][0], "echo");
    }

    #[test]
    fn accepts_whole_object_arguments() {
        // Gemini 风格：一次给完整 JSON
        let pair = EventSink::channel(8);
        let mut agg = Aggregator::new();
        agg.consume(WireDelta::ToolCall { index: 0, id: None, name: Some("exec".into()), args: Some("{\"a\":1}".into()) }, &pair.sink);
        let done = agg.finish(&pair.sink);
        assert_eq!(done.calls[0].args["a"], 1);
        assert_eq!(done.calls[0].call.0, "call_0");
    }
}

/// 给用户看的单行参数摘要（截断 60 字）。
fn summarize_args(args: &serde_json::Value) -> String {
    let raw = if let Some(a) = args.get("argv").and_then(|v| v.as_array()) {
        a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" ")
    } else if let Some(s) = args.get("script").and_then(|v| v.as_str()) {
        s.to_owned()
    } else {
        args.to_string()
    };
    raw.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(60).collect()
}
