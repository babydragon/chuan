//! OpenAI Chat Completions 的线上类型。仅供 `openai_chat` 使用。

use serde::{Deserialize, Deserializer, Serialize};

/// `null` 也要当成"没有"。
///
/// `#[serde(default)]` 只兜**缺字段**，不兜显式 `null`：很多 OpenAI 兼容实现会在
/// 首个 chunk 里把没用到的字段写成 `"tool_calls": null`，按 `Vec<T>` 解析直接
/// `invalid type: null, expected a sequence`，整轮 run 就废了
/// （2026-11-19 板上真实事故：opencode-go / deepseek-v4.1-flash 首个 chunk）。
fn null_to_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

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
    #[serde(default, deserialize_with = "null_to_default")]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<UsageJson>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct Choice {
    #[serde(default, deserialize_with = "null_to_default")]
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
    #[serde(default, deserialize_with = "null_to_default")]
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
    /// DeepSeek：命中/未命中的输入 token；`prompt_tokens = hit + miss`。
    #[serde(default)]
    pub prompt_cache_hit_tokens: Option<u32>,
    #[serde(default)]
    pub prompt_cache_miss_tokens: Option<u32>,
    /// OpenAI / 智谱 GLM：命中部分是 `prompt_tokens` 的子集。
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// Anthropic 口径（当前没有对应 wire，先兼容字段，避免以后重复改结构）。
    /// 注意：它的 `input_tokens` 不含缓存读写，归一化时要加回去。
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u32>,
}

impl UsageJson {
    /// 归一化成统一口径：`prompt` 始终包含缓存命中部分。
    /// 返回 `(prompt, completion, hit, write)`。
    pub fn normalized(&self) -> (u32, u32, u32, u32) {
        let write = self.cache_creation_input_tokens.unwrap_or(0);
        if let Some(hit) = self.prompt_cache_hit_tokens {
            // DeepSeek：prompt 已含命中
            return (self.prompt_tokens, self.completion_tokens, hit, write);
        }
        if let Some(hit) = self.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens) {
            // OpenAI / GLM：cached 是 prompt 的子集
            return (self.prompt_tokens, self.completion_tokens, hit, write);
        }
        if let Some(hit) = self.cache_read_input_tokens {
            // Anthropic：input 不含缓存读/写，加回去才是完整输入
            let prompt = self.prompt_tokens.saturating_add(hit).saturating_add(write);
            return (prompt, self.completion_tokens, hit, write);
        }
        (self.prompt_tokens, self.completion_tokens, 0, write)
    }
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

#[cfg(test)]
mod chunk_tests {
    use super::Chunk;

    /// 板上原文（2026-11-19）：首个 chunk 里 `tool_calls` / `reasoning_content` / `usage`
    /// 都是显式 null。以前 `tool_calls` 是 `Vec`，直接解析失败，整轮 run 报
    /// `bad chunk: invalid type: null, expected a sequence`。
    #[test]
    fn explicit_nulls_in_first_chunk_are_tolerated() {
        let raw = r#"{"id":"chatcmpl-RVKVHQvlOTtlFTZheAF22wdX","object":"chat.completion.chunk","created":1795069560,"model":"deepseek-v4.1-flash","choices":[{"index":0,"finish_reason":null,"logprobs":null,"delta":{"role":"assistant","content":"","reasoning_content":null,"tool_calls":null}}],"usage":null}"#;
        let c: Chunk = serde_json::from_str(raw).expect("null 字段不该让整条流解析失败");
        assert_eq!(c.choices.len(), 1);
        assert!(c.choices[0].delta.tool_calls.is_empty());
        assert_eq!(c.choices[0].delta.content.as_deref(), Some(""));
        assert!(c.usage.is_none());
    }

    /// 同理：`choices: null` / `delta: null`（个别中转站会这么发）。
    #[test]
    fn null_choices_and_delta_are_empty_not_fatal() {
        let c: Chunk = serde_json::from_str(r#"{"choices":null,"usage":null}"#).unwrap();
        assert!(c.choices.is_empty());
        let c: Chunk = serde_json::from_str(r#"{"choices":[{"delta":null}]}"#).unwrap();
        assert!(c.choices[0].delta.tool_calls.is_empty());
    }

    /// 正常分片仍然要能解析出内容与工具调用（别把修复做成"什么都当空"）。
    #[test]
    fn normal_chunks_still_parse() {
        let raw = r#"{"choices":[{"delta":{"content":"你好","tool_calls":[{"index":0,"id":"c1","function":{"name":"exec","arguments":"{}"}}]}}]}"#;
        let c: Chunk = serde_json::from_str(raw).unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("你好"));
        assert_eq!(c.choices[0].delta.tool_calls.len(), 1);
        assert_eq!(c.choices[0].delta.tool_calls[0].index, 0);
    }
}

#[cfg(test)]
mod usage_tests {
    use super::UsageJson;

    fn parse(raw: &str) -> UsageJson {
        serde_json::from_str(raw).expect("usage 应可解析")
    }

    #[test]
    fn deepseek_hit_and_miss_are_already_in_prompt() {
        let u = parse(
            r#"{"prompt_tokens":100,"completion_tokens":7,
                "prompt_cache_hit_tokens":80,"prompt_cache_miss_tokens":20}"#,
        );
        assert_eq!(u.normalized(), (100, 7, 80, 0));
    }

    #[test]
    fn openai_and_glm_cached_tokens_are_a_subset() {
        let u = parse(
            r#"{"prompt_tokens":100,"completion_tokens":7,
                "prompt_tokens_details":{"cached_tokens":60}}"#,
        );
        assert_eq!(u.normalized(), (100, 7, 60, 0));
    }

    #[test]
    fn anthropic_cache_tokens_are_added_to_input() {
        let u = parse(
            r#"{"prompt_tokens":10,"completion_tokens":7,
                "cache_read_input_tokens":90,"cache_creation_input_tokens":5}"#,
        );
        // input 不含缓存读写 → 完整输入 = 10 + 90 + 5
        assert_eq!(u.normalized(), (105, 7, 90, 5));
    }

    #[test]
    fn providers_without_cache_fields_report_zero() {
        let u = parse(r#"{"prompt_tokens":12,"completion_tokens":3}"#);
        assert_eq!(u.normalized(), (12, 3, 0, 0));
    }
}
