//! 协议边界：上层只认 `WireDelta` 与 `WireConversation`，不认任何一家的消息模型。
//!
//! 加一个新协议 = 新写一个 `Wire` 实现；`Aggregator`、传输、`Turn`、内核都不动。

pub mod openai_chat;
pub mod openai_types;

use harness_kernel::{CallId, EngineError, Message, ToolCallRequest, ToolSpec};

/// 一次模型请求的用量增量。缓存字段已按统一口径归一化：
/// `prompt_tokens` 包含 `cache_hit_tokens`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UsageDelta {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cache_hit_tokens: u32,
    pub cache_write_tokens: u32,
}

/// 协议无关的增量。
#[derive(Clone, Debug)]
pub enum WireDelta {
    Text(String),
    Reasoning(String),
    /// `args` 可以是分片（OpenAI/Anthropic），也可以是完整 JSON（Gemini）。
    ToolCall { index: u32, id: Option<String>, name: Option<String>, args: Option<String> },
    Finish(String),
    Usage(UsageDelta),
}

/// 一次对话的协议侧状态（消息数组、system 位置、tool_result 形状等）。
pub trait WireConversation: Send {
    /// 编码成请求体字节（含协议自己的字段布局）。
    fn encode(&mut self, model: &str, tools: &[ToolSpec]) -> Vec<u8>;
    fn on_assistant(&mut self, text: &str, calls: &[ToolCallRequest]);
    fn on_tool_result(&mut self, call: &CallId, ok: bool, preview: &str);

    /// 运行中插一条提醒给模型（不是给用户看的）。默认忽略，只有真实协议实现。
    fn push_notice(&mut self, _text: &str) {}
}

/// 一家协议 = 一个 `decode` + 一个 `conversation`。
pub trait Wire: Send + Sync {
    fn decode(&self, payload: &str) -> Result<Vec<WireDelta>, EngineError>;
    fn conversation(
        &self,
        history: &[Message],
        tools: &[ToolSpec],
        system: Option<&str>,
        images: &[harness_kernel::ImagePart],
    ) -> Box<dyn WireConversation>;
}
