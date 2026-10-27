//! LLM 层：协议中立的 Turn 驱动 + 可插拔的 Wire。
//!
//! 分层：`transport`（字节搬运）→ `wire`（协议编解码）→ `stream`（中立聚合）→ `driver`（Turn）。

pub mod driver;
pub mod echo;
pub mod sse;
pub mod stream;
pub mod transport;
pub mod transport_http;
pub mod wire;

pub use driver::ModelDriver;
pub use echo::EchoDriver;
pub use sse::SseParser;
pub use stream::Aggregator;
pub use transport::{HttpRequest, SseBytes, SseTransport};
pub use transport_http::ReqwestTransport;
pub use wire::{Wire, WireConversation, WireDelta, openai_chat::OpenAiChat};
