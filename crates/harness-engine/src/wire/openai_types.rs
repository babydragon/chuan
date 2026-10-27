//! OpenAI Chat Completions 的线上类型。仅供 `openai_chat` 使用。

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Msg>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolJson>>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Msg {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallJson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolCallJson {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: FnCall,
}

#[derive(Clone, Debug, Serialize)]
pub struct FnCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolJson {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub function: ToolFn,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolFn {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Chunk {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<UsageJson>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Choice {
    #[serde(default)]
    pub delta: Delta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Delta {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolDelta>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolDelta {
    pub index: u32,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FnDelta>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct FnDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct UsageJson {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user",
            content: Some(serde_json::Value::String(text.into())),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    /// 多模态：文本 + 若干 data URL 图片（只在当轮使用）
    pub fn user_with_images(text: &str, urls: &[String]) -> Self {
        let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
        for u in urls {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": u }
            }));
        }
        Self {
            role: "user",
            content: Some(serde_json::Value::Array(parts)),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant(content: Option<String>, tool_calls: Option<Vec<ToolCallJson>>) -> Self {
        Self {
            role: "assistant",
            content: content.map(serde_json::Value::String),
            tool_calls,
            tool_call_id: None,
        }
    }
    pub fn tool(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool",
            content: Some(serde_json::Value::String(content.into())),
            tool_calls: None,
            tool_call_id: Some(call_id.into()),
        }
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system",
            content: Some(serde_json::Value::String(text.into())),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}
