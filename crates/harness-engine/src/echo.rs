//! 回显驱动：不调用任何 provider，把用户最后一句按片吐回去。
//! 用途：在没有 LLM 密钥的情况下验证「通道 → 会话 → 渲染 → 编辑」整条链路。

use std::time::Duration;

use async_trait::async_trait;
use harness_kernel::{
    CallId, EngineError, EventSink, FinishReason, Message, Role, Step, StreamEvent, ToolSpec, Turn,
    TurnDriver, TurnId, TurnSpec, UsageSnapshot,
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct EchoDriver;

#[async_trait]
impl TurnDriver for EchoDriver {
    async fn begin(
        &self,
        spec: TurnSpec,
        history: &[Message],
        _tools: &[ToolSpec],
        sink: EventSink,
    ) -> Result<Box<dyn Turn>, EngineError> {
        let last = history
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| m.payload.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        Ok(Box::new(EchoTurn {
            text: format!("收到：{last}\n\n（EchoDriver：未接入模型）"),
            emitted: false,
            turn_id: TurnId(format!("{}-t0", spec.run.0)),
            cancel: CancellationToken::new(),
            sink,
        }))
    }
}

struct EchoTurn {
    text: String,
    emitted: bool,
    turn_id: TurnId,
    cancel: CancellationToken,
    sink: EventSink,
}

#[async_trait]
impl Turn for EchoTurn {
    async fn step(&mut self) -> Result<Step, EngineError> {
        if !self.emitted {
            self.emitted = true;
            let chars: Vec<char> = self.text.chars().collect();
            for chunk in chars.chunks(6) {
                if self.cancel.is_cancelled() {
                    return Err(EngineError::Cancelled);
                }
                let s: String = chunk.iter().collect();
                self.sink.emit(StreamEvent::TextDelta(s));
                tokio::time::sleep(Duration::from_millis(60)).await;
            }
        }
        Ok(Step::Done { text: self.text.clone(), finish: FinishReason::Stop })
    }

    async fn on_tool_result(
        &mut self,
        _call: CallId,
        _ok: bool,
        _preview: String,
    ) -> Result<(), EngineError> {
        Ok(())
    }

    fn canceller(&self) -> CancellationToken {
        self.cancel.clone()
    }

    fn usage(&self) -> UsageSnapshot {
        UsageSnapshot::default()
    }

    fn turn_id(&self) -> TurnId {
        self.turn_id.clone()
    }
}
