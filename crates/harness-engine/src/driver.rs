//! 协议无关的 Turn 驱动：encode → 传输 → decode → 聚合。
//! 加新协议只需实现 `Wire`，本文件不动。

use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    CallId, EngineError, EventSink, FinishReason, Message, Step, StreamEvent, ToolSpec, Turn,
    TurnDriver, TurnId, TurnSpec, UsageRecord, UsageSnapshot,
};
use tokio_util::sync::CancellationToken;

use crate::sse::SseParser;
use crate::stream::Aggregator;
use crate::transport::{HttpRequest, SseTransport};
use crate::wire::{Wire, WireConversation};

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_backoff_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 3, base_backoff_ms: 1000 }
    }
}

pub struct ModelDriver<T: SseTransport> {
    transport: Arc<T>,
    wire: Arc<dyn Wire>,
    model: String,
    headers: Vec<(String, String)>,
    /// 会话亲和头（如 opencode-go 的 x-opencode-session），值取会话 id。
    session_header: Option<String>,
    retry: RetryPolicy,
}

impl<T: SseTransport> ModelDriver<T> {
    pub fn new(transport: Arc<T>, wire: Arc<dyn Wire>, model: impl Into<String>) -> Self {
        Self {
            transport,
            wire,
            model: model.into(),
            headers: Vec::new(),
            session_header: None,
            retry: RetryPolicy::default(),
        }
    }

    pub fn with_header(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }

    /// 每个会话一个稳定值：跨 turn 复用同一路由/缓存，`/new` 后自然换新。
    pub fn with_session_header(mut self, name: impl Into<String>) -> Self {
        self.session_header = Some(name.into());
        self
    }

    pub fn with_retry(mut self, policy: RetryPolicy) -> Self {
        self.retry = policy;
        self
    }
}

#[async_trait]
impl<T: SseTransport + 'static> TurnDriver for ModelDriver<T> {
    async fn begin(
        &self,
        spec: TurnSpec,
        history: &[Message],
        tools: &[ToolSpec],
        sink: EventSink,
    ) -> Result<Box<dyn Turn>, EngineError> {
        let mut headers = self.headers.clone();
        if let Some(name) = &self.session_header {
            headers.push((name.clone(), spec.conv.0.clone()));
        }
        Ok(Box::new(WireTurn {
            transport: Arc::clone(&self.transport),
            wire: Arc::clone(&self.wire),
            model: self.model.clone(),
            headers,
            convo: self.wire.conversation(
                history,
                tools,
                spec.system_prompt.as_deref(),
                &spec.images,
            ),
            tools: tools.to_vec(),
            sink,
            turn_no: 0,
            turn_id: TurnId(format!("{}-t0", spec.run.0)),
            cancel: CancellationToken::new(),
            usage: Vec::new(),
            retry: self.retry.clone(),
        }))
    }
}

struct WireTurn<T: SseTransport> {
    transport: Arc<T>,
    wire: Arc<dyn Wire>,
    model: String,
    headers: Vec<(String, String)>,
    convo: Box<dyn WireConversation>,
    tools: Vec<ToolSpec>,
    sink: EventSink,
    turn_no: u32,
    turn_id: TurnId,
    cancel: CancellationToken,
    usage: Vec<UsageRecord>,
    retry: RetryPolicy,
}

#[async_trait]
impl<T: SseTransport + 'static> Turn for WireTurn<T> {
    async fn step(&mut self) -> Result<Step, EngineError> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.attempt().await {
                Ok(step) => return Ok(step),
                Err(e) if retryable(&e) && attempt < self.retry.max_attempts => {
                    let delay = backoff_ms(attempt, self.retry.base_backoff_ms);
                    tracing::warn!(
                        error = %e, attempt, delay_ms = delay,
                        "model request failed, retrying"
                    );
                    self.sink.emit(StreamEvent::Restarted);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn on_tool_result(
        &mut self,
        call: CallId,
        ok: bool,
        preview: String,
    ) -> Result<(), EngineError> {
        self.convo.on_tool_result(&call, ok, &preview);
        let brief = preview.chars().take(40).collect::<String>();
        self.sink.emit(StreamEvent::ToolFinished { call, ok, preview: brief });
        Ok(())
    }

    fn canceller(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn usage(&self) -> UsageSnapshot {
        UsageSnapshot { records: self.usage.clone() }
    }

    fn turn_id(&self) -> TurnId {
        self.turn_id.clone()
    }
}

impl<T: SseTransport + 'static> WireTurn<T> {
    async fn attempt(&mut self) -> Result<Step, EngineError> {
        if self.cancel.is_cancelled() {
            return Err(EngineError::Cancelled);
        }
        self.turn_no += 1;

        let body = self.convo.encode(&self.model, &self.tools);
        let mut bytes = self
            .transport
            .open(HttpRequest { body, headers: self.headers.clone() })
            .await?;

        let mut parser = SseParser::new();
        let mut agg = Aggregator::new();

        while let Some(chunk) = bytes.next_chunk().await {
            if self.cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let chunk = chunk?;
            for payload in parser.feed(&chunk) {
                for d in self.wire.decode(&payload)? {
                    agg.consume(d, &self.sink);
                }
            }
            if parser.done {
                break;
            }
        }
        if let Some(payload) = parser.finish() {
            for d in self.wire.decode(&payload)? {
                agg.consume(d, &self.sink);
            }
        }

        let done = agg.finish(&self.sink);

        // 流中断：没有 finish_reason 也没有工具调用
        if done.calls.is_empty() && done.finish_reason.is_none() {
            return Err(EngineError::Interrupted);
        }

        if let Some((p, c)) = done.usage {
            self.usage.push(UsageRecord {
                model: self.model.clone(),
                prompt_tokens: p,
                completion_tokens: c,
                cost_est: 0.0,
            });
        }

        self.convo.on_assistant(&done.text, &done.calls);

        if !done.calls.is_empty() {
            return Ok(Step::Tools { text: done.text, calls: done.calls });
        }

        let finish = match done.finish_reason.as_deref() {
            Some("length") => FinishReason::Length,
            Some("tool_calls") => FinishReason::ToolCalls,
            _ => FinishReason::Stop,
        };
        Ok(Step::Done { text: done.text, finish })
    }
}

fn retryable(e: &EngineError) -> bool {
    matches!(e, EngineError::Provider(_) | EngineError::Interrupted)
}

/// 指数退避 + ±20% 抖动
fn backoff_ms(attempt: u32, base: u64) -> u64 {
    let exp = base.saturating_mul(1 << attempt.min(5) / 2); // 1s,1s,2s,4s…
    let jitter = (now_nanos() % (exp / 5 + 1)) as i64 - (exp / 10) as i64;
    (exp as i64 + jitter).max(100) as u64
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::ScriptedTransport;
    use crate::wire::{WireDelta, openai_chat::OpenAiChat};

    fn user_msg(text: &str) -> Message {
        Message {
            seq: 1,
            turn: None,
            role: harness_kernel::Role::User,
            payload: serde_json::json!({ "text": text }),
        }
    }

    fn tool_then_text() -> Vec<Vec<Vec<u8>>> {
        let first = [
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","function":{"name":"exec","arguments":"{\"argv\":"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"[\"echo\",\"hi\"]}"}}]}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":7,"completion_tokens":3}}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n")
            + "\n\n";
        let second = [
            r#"data: {"choices":[{"delta":{"content":"好"}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"的"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2}}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n")
            + "\n\n";
        vec![vec![first.into_bytes()], vec![second.into_bytes()]]
    }

    async fn drive<W: Wire + 'static>(wire: Arc<W>, script: Vec<Vec<Vec<u8>>>) -> (Step, Step, Arc<ScriptedTransport>) {
        let transport = Arc::new(ScriptedTransport::new(script));
        let driver = ModelDriver::new(Arc::clone(&transport), wire, "test-model");
        let pair = EventSink::channel(128);
        let history = vec![user_msg("打印")];
        let tools = vec![ToolSpec {
            name: "exec".into(),
            description: "run".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let mut turn = driver
            .begin(
                TurnSpec {
                    conv: harness_kernel::ConversationId::from("c1"),
                    run: harness_kernel::RunId::from("r1"),
                    model: "test-model".into(),
                    max_turns: 8,
                    wall_clock_ms: 60_000,
                    system_prompt: None,
                    images: Vec::new(),
                },
                &history,
                &tools,
                pair.sink,
            )
            .await
            .unwrap();
        let first = turn.step().await.unwrap();
        let call = match &first {
            Step::Tools { calls, .. } => calls[0].call.clone(),
            other => panic!("expected tools, got {other:?}"),
        };
        turn.on_tool_result(call, true, "printed".into()).await.unwrap();
        let second = turn.step().await.unwrap();
        (first, second, transport)
    }

    #[tokio::test]
    async fn openai_wire_runs_tool_then_text() {
        let (first, second, transport) = drive(Arc::new(OpenAiChat), tool_then_text()).await;
        match first {
            Step::Tools { calls, .. } => {
                assert_eq!(calls[0].call.0, "call_a");
                assert_eq!(calls[0].args["argv"][1], "hi");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(second, Step::Done { ref text, .. } if text == "好的"));

        let reqs = transport.requests.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert!(msgs.iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_a"));
        assert!(reqs[0].body.windows(6).any(|w| w == b"\"exec\""));
    }

    /// 中立性证明：换一个完全不同的 wire（工具参数一次给完整对象，事件名也不一样），
    /// driver / 聚合器 / 传输一行没改。
    struct FakeWire;

    impl Wire for FakeWire {
        fn decode(&self, payload: &str) -> Result<Vec<WireDelta>, EngineError> {
            Ok(vec![
                WireDelta::Text(payload.to_owned()),
                WireDelta::ToolCall {
                    index: 0,
                    id: Some("g1".into()),
                    name: Some("exec".into()),
                    args: Some("{\"argv\":[\"ls\"]}".into()),
                },
                WireDelta::Finish("tool_calls".into()),
            ])
        }
        fn conversation(
            &self,
            _history: &[Message],
            _tools: &[ToolSpec],
            _system: Option<&str>,
            _images: &[harness_kernel::ImagePart],
        ) -> Box<dyn crate::wire::WireConversation> {
            Box::new(FakeConvo)
        }
    }

    struct FakeConvo;
    impl crate::wire::WireConversation for FakeConvo {
        fn encode(&mut self, _model: &str, _tools: &[ToolSpec]) -> Vec<u8> {
            b"{}".to_vec()
        }
        fn on_assistant(&mut self, _text: &str, _calls: &[harness_kernel::ToolCallRequest]) {}
        fn on_tool_result(&mut self, _call: &CallId, _ok: bool, _preview: &str) {}
    }

    #[tokio::test]
    async fn second_wire_needs_no_driver_change() {
        let script = vec![vec![b"data: payload\n\n".to_vec()]];
        let transport = Arc::new(ScriptedTransport::new(script));
        let driver = ModelDriver::new(Arc::clone(&transport), Arc::new(FakeWire), "m");
        let pair = EventSink::channel(32);
        let mut turn = driver
            .begin(
                TurnSpec {
                    conv: harness_kernel::ConversationId::from("c1"),
                    run: harness_kernel::RunId::from("r1"),
                    model: "m".into(),
                    max_turns: 4,
                    wall_clock_ms: 1000,
                    system_prompt: None,
                    images: Vec::new(),
                },
                &[],
                &[],
                pair.sink,
            )
            .await
            .unwrap();
        match turn.step().await.unwrap() {
            Step::Tools { calls, .. } => assert_eq!(calls[0].args["argv"][0], "ls"),
            other => panic!("{other:?}"),
        }
    }

    struct Flaky {
        inner: Arc<ScriptedTransport>,
        fails_left: std::sync::atomic::AtomicUsize,
        attempts: std::sync::atomic::AtomicUsize,
        quota: bool,
    }

    #[async_trait]
    impl crate::transport::SseTransport for Flaky {
        async fn open(
            &self,
            req: crate::transport::HttpRequest,
        ) -> Result<Box<dyn crate::transport::SseBytes>, EngineError> {
            use std::sync::atomic::Ordering::Relaxed;
            self.attempts.fetch_add(1, Relaxed);
            if self.quota {
                return Err(EngineError::Quota {
                    message: "insufficient_quota".into(),
                    retry_after_s: None,
                });
            }
            if self.fails_left.load(Relaxed) > 0 {
                self.fails_left.fetch_sub(1, Relaxed);
                return Err(EngineError::Provider("boom".into()));
            }
            self.inner.open(req).await
        }
    }

    fn simple_text_frame() -> String {
        [
            r#"data: {"choices":[{"delta":{"content":"ok"}}]}"#,
            r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n")
            + "\n\n"
    }

    fn retry_policy() -> RetryPolicy {
        RetryPolicy { max_attempts: 3, base_backoff_ms: 10 }
    }

    // 用显式函数替代上面的辅助（避免复杂 trait）
    async fn run_step<T: SseTransport + 'static>(driver: &ModelDriver<T>) -> Result<Step, EngineError> {
        let history = vec![user_msg("hi")];
        let pair = EventSink::channel(16);
        let mut turn = driver
            .begin(
                TurnSpec {
                    conv: harness_kernel::ConversationId::from("c1"),
                    run: harness_kernel::RunId::from("r1"),
                    model: "m".into(),
                    max_turns: 4,
                    wall_clock_ms: 1000,
                    system_prompt: None,
                    images: Vec::new(),
                },
                &history,
                &[],
                pair.sink,
            )
            .await?;
        turn.step().await
    }

    #[tokio::test]
    async fn retries_provider_error_then_succeeds() {
        let inner = Arc::new(ScriptedTransport::new(vec![vec![simple_text_frame().into_bytes()]]));
        let flaky = Arc::new(Flaky {
            inner,
            fails_left: std::sync::atomic::AtomicUsize::new(1),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            quota: false,
        });
        let driver = ModelDriver::new(Arc::clone(&flaky), Arc::new(OpenAiChat), "m").with_retry(retry_policy());
        let step = run_step(&driver).await.unwrap();
        assert!(matches!(step, Step::Done { .. }));
        assert_eq!(flaky.attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn quota_is_never_retried() {
        let inner = Arc::new(ScriptedTransport::new(vec![]));
        let flaky = Arc::new(Flaky {
            inner,
            fails_left: std::sync::atomic::AtomicUsize::new(0),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            quota: true,
        });
        let driver = ModelDriver::new(Arc::clone(&flaky), Arc::new(OpenAiChat), "m").with_retry(retry_policy());
        let err = run_step(&driver).await.unwrap_err();
        assert!(matches!(err, EngineError::Quota { .. }));
        assert_eq!(flaky.attempts.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancel_before_step() {
        let transport = Arc::new(ScriptedTransport::new(vec![]));
        let driver = ModelDriver::new(Arc::clone(&transport), Arc::new(OpenAiChat), "m");
        let pair = EventSink::channel(8);
        let mut turn = driver
            .begin(
                TurnSpec {
                    conv: harness_kernel::ConversationId::from("c1"),
                    run: harness_kernel::RunId::from("r1"),
                    model: "m".into(),
                    max_turns: 4,
                    wall_clock_ms: 1000,
                    system_prompt: None,
                    images: Vec::new(),
                },
                &[],
                &[],
                pair.sink,
            )
            .await
            .unwrap();
        turn.canceller().cancel();
        assert!(matches!(turn.step().await, Err(EngineError::Cancelled)));
    }
}
