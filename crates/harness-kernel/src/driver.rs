use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::EngineError;
use crate::ids::{CallId, ConversationId, RunId, TurnId};
use crate::record::Message;
use crate::record::UsageRecord;
use crate::tool::{ToolCallRequest, ToolSpec};

/// 流式事件：只用于唤醒渲染，View 是全量快照。
#[derive(Clone, Debug)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// 新一轮开始（第 N 轮模型请求）
    TurnStarted { turn_no: u32 },
    /// 本轮请求失败并重试：清空已累积的文本，避免重复显示
    Restarted,
    /// 参数已聚合完整；`preview` 是给用户看的参数摘要
    ToolStarted { call: CallId, name: String, preview: String },
    ToolFinished { call: CallId, ok: bool, preview: String },
}

/// 有界、非阻塞的事件出口：满则丢弃并计数（丢事件只影响一帧新鲜度）。
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::Sender<StreamEvent>,
    dropped: Arc<AtomicU64>,
}

pub struct SinkPair {
    pub sink: EventSink,
    pub rx: mpsc::Receiver<StreamEvent>,
}

impl EventSink {
    pub fn channel(capacity: usize) -> SinkPair {
        let (tx, rx) = mpsc::channel(capacity);
        SinkPair {
            sink: EventSink { tx, dropped: Arc::new(AtomicU64::new(0)) },
            rx,
        }
    }

    pub fn emit(&self, ev: StreamEvent) {
        if self.tx.try_send(ev).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// 随本轮用户消息一起发的一张图片（只在当轮注入，不进历史）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImagePart {
    pub path: String,
    pub mime: String,
}

pub struct TurnSpec {
    pub conv: ConversationId,
    pub run: RunId,
    pub model: String,
    pub max_turns: u32,
    pub wall_clock_ms: u64,
    /// 每轮都放在最前面的 system 指令（稳定前缀，利于 prompt cache）。
    pub system_prompt: Option<String>,
    /// 当轮用户消息附带的图片（多模态）
    pub images: Vec<ImagePart>,
}

#[derive(Clone, Debug)]
pub enum Step {
    Model { turn_no: u32 },
    Tools { text: String, calls: Vec<ToolCallRequest> },
    Done { text: String, finish: FinishReason },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    Error,
    Cancelled,
}

#[derive(Clone, Debug, Default)]
pub struct UsageSnapshot {
    pub records: Vec<UsageRecord>,
}

/// 一次「运行」的驱动接口。实现放在 engine crate。
#[async_trait]
pub trait TurnDriver: Send + Sync + 'static {
    async fn begin(
        &self,
        spec: TurnSpec,
        history: &[Message],
        tools: &[ToolSpec],
        sink: EventSink,
    ) -> Result<Box<dyn Turn>, EngineError>;
}

#[async_trait]
pub trait Turn: Send {
    async fn step(&mut self) -> Result<Step, EngineError>;
    /// 只记录工具结果；推进由下一次 `step()` 负责。
    async fn on_tool_result(
        &mut self,
        call: CallId,
        ok: bool,
        preview: String,
    ) -> Result<(), EngineError>;
    fn canceller(&self) -> tokio_util::sync::CancellationToken;
    fn usage(&self) -> UsageSnapshot;
    /// 本 Turn 到目前为止发出的模型请求次数（含重试）。默认 0：桩不需要实现。
    /// 记进 `turn.finished`，形成可审计的 attempts 账本（D20）。
    fn attempts(&self) -> u32 {
        0
    }
    fn turn_id(&self) -> TurnId;
}
