//! OpenAI Chat Completions（含 DeepSeek/OpenRouter/Ollama/Kimi/智谱 等兼容面）。

use harness_kernel::{CallId, EngineError, Message, Role, ToolCallRequest, ToolSpec};

use super::{Wire, WireConversation, WireDelta};
use super::openai_types::{ChatRequest, Chunk, FnCall, Msg, StreamOptions, ToolCallJson, ToolFn, ToolJson};

pub struct OpenAiChat;

impl Wire for OpenAiChat {
    fn decode(&self, payload: &str) -> Result<Vec<WireDelta>, EngineError> {
        let chunk: Chunk = serde_json::from_str(payload)
            .map_err(|e| EngineError::Provider(format!("bad chunk: {e}: {payload}")))?;
        let mut out = Vec::new();
        if let Some(u) = chunk.usage {
            out.push(WireDelta::Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
            });
        }
        for choice in chunk.choices {
            if let Some(c) = choice.delta.content {
                if !c.is_empty() {
                    out.push(WireDelta::Text(c));
                }
            }
            if let Some(r) = choice.delta.reasoning_content {
                if !r.is_empty() {
                    out.push(WireDelta::Reasoning(r));
                }
            }
            for tc in choice.delta.tool_calls {
                out.push(WireDelta::ToolCall {
                    index: tc.index,
                    id: tc.id,
                    name: tc.function.as_ref().and_then(|f| f.name.clone()),
                    args: tc.function.and_then(|f| f.arguments),
                });
            }
            if let Some(fr) = choice.finish_reason {
                out.push(WireDelta::Finish(fr));
            }
        }
        Ok(out)
    }

    fn conversation(
        &self,
        history: &[Message],
        _tools: &[ToolSpec],
        system: Option<&str>,
        images: &[harness_kernel::ImagePart],
    ) -> Box<dyn WireConversation> {
        let mut messages = Vec::new();
        if let Some(s) = system {
            if !s.trim().is_empty() {
                messages.push(Msg::system(s));
            }
        }
        let mut msgs = build_messages(history);
        if !images.is_empty() {
            attach_images(&mut msgs, images);
        }
        messages.extend(msgs);
        Box::new(OpenAiConversation { messages })
    }
}

struct OpenAiConversation {
    messages: Vec<Msg>,
}

impl WireConversation for OpenAiConversation {
    fn encode(&mut self, model: &str, tools: &[ToolSpec]) -> Vec<u8> {
        let req = ChatRequest {
            model: model.to_owned(),
            messages: self.messages.clone(),
            tools: tools_json(tools),
            stream: true,
            stream_options: Some(StreamOptions { include_usage: true }),
        };
        serde_json::to_vec(&req).expect("request 序列化不会失败")
    }

    fn on_assistant(&mut self, text: &str, calls: &[ToolCallRequest]) {
        let tool_calls = if calls.is_empty() {
            None
        } else {
            Some(
                calls
                    .iter()
                    .map(|c| ToolCallJson {
                        id: c.call.0.clone(),
                        kind: "function",
                        function: FnCall { name: c.name.clone(), arguments: c.args.to_string() },
                    })
                    .collect(),
            )
        };
        self.messages.push(Msg::assistant(
            if text.is_empty() { None } else { Some(text.to_owned()) },
            tool_calls,
        ));
    }

    fn on_tool_result(&mut self, call: &CallId, _ok: bool, preview: &str) {
        self.messages.push(Msg::tool(call.0.clone(), preview));
    }
}

/// 折叠后的消息 → OpenAI 消息数组。协议相关，因此留在这里。
fn build_messages(history: &[Message]) -> Vec<Msg> {
    let mut out = Vec::new();
    for ev in history {
        match ev.role {
            Role::User => {
                if let Some(t) = ev.payload.get("text").and_then(|v| v.as_str()) {
                    out.push(Msg::user(t));
                }
            }
            Role::Assistant => {
                let content = ev.payload.get("text").and_then(|v| v.as_str()).map(str::to_owned);
                let calls = ev.payload.get("tool_calls").and_then(|v| v.as_array()).map(|arr| {
                    arr.iter()
                        .map(|c| ToolCallJson {
                            id: c["call_id"].as_str().unwrap_or_default().to_owned(),
                            kind: "function",
                            function: FnCall {
                                name: c["name"].as_str().unwrap_or_default().to_owned(),
                                arguments: c["args"].to_string(),
                            },
                        })
                        .collect::<Vec<_>>()
                });
                out.push(Msg::assistant(content, calls));
            }
            Role::ToolResult => {
                out.push(Msg::tool(
                    ev.payload["call_id"].as_str().unwrap_or_default(),
                    ev.payload["preview"].as_str().unwrap_or_default(),
                ));
            }
            Role::Synthetic => {
                out.push(Msg::system(ev.payload["content"].as_str().unwrap_or_default()));
            }
        }
    }
    out
}

/// 把图片挂到**最后一条 user 消息**上；历史里的旧图不重发。
fn attach_images(messages: &mut [Msg], images: &[harness_kernel::ImagePart]) {
    use base64::Engine as _;
    let Some(idx) = messages.iter().rposition(|m| m.role == "user") else { return };
    let text = match &messages[idx].content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let mut urls = Vec::new();
    for img in images {
        let Ok(bytes) = std::fs::read(&img.path) else {
            tracing::warn!(path = %img.path, "图片读取失败，跳过");
            continue;
        };
        // 单张超过 4MB 就别塞了：base64 之后还会再涨 1/3
        if bytes.len() > 4 * 1024 * 1024 {
            tracing::warn!(path = %img.path, bytes = bytes.len(), "图片过大，跳过");
            continue;
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        urls.push(format!("data:{};base64,{}", img.mime, b64));
    }
    if !urls.is_empty() {
        messages[idx] = Msg::user_with_images(&text, &urls);
    }
}

fn tools_json(tools: &[ToolSpec]) -> Option<Vec<ToolJson>> {
    if tools.is_empty() {
        return None;
    }
    Some(
        tools
            .iter()
            .map(|t| ToolJson {
                kind: "function",
                function: ToolFn {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect(),
    )
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use harness_kernel::{ImagePart, Message, Role, Timestamp};

    fn history(text: &str) -> Vec<Message> {
        vec![Message {
            seq: 1,
            turn: None,
            role: Role::User,
            payload: serde_json::json!({ "text": text }),
        }]
    }

    #[test]
    fn images_become_multimodal_content_on_the_last_user_message() {
        let dir = std::env::temp_dir().join(format!("chuan-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nfake").unwrap();

        let wire = OpenAiChat;
        let images = vec![ImagePart {
            path: png.to_string_lossy().into_owned(),
            mime: "image/png".into(),
        }];
        let mut convo = wire.conversation(&history("这是什么"), &[], None, &images);
        let bytes = convo.encode("m", &[]);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let content = &v["messages"][0]["content"];
        assert!(content.is_array(), "应变成多模态数组: {content}");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "这是什么");
        assert_eq!(content[1]["type"], "image_url");
        let url = content[1]["image_url"]["url"].as_str().unwrap();
        assert!(url.starts_with("data:image/png;base64,"), "{url}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_images_keeps_plain_text_content() {
        let wire = OpenAiChat;
        let mut convo = wire.conversation(&history("hi"), &[], None, &[]);
        let v: serde_json::Value = serde_json::from_slice(&convo.encode("m", &[])).unwrap();
        assert!(v["messages"][0]["content"].is_string());
    }
}
