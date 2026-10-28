//! 渲染策略：节流、合并、分片、阶段状态、工具轨迹。
//!
//! 只产出 `View`（完整快照），不做任何 IO；通道负责把它变成一条消息。
//! 目标：让用户清楚知道"Agent 现在在干嘛"，而不是一句固定的"处理中"。

use crate::channel::{Channel, ChannelError};
use crate::ids::{CallId, MsgRef, Timestamp, now_ms};
use crate::view::{
    Block, Button, Caps, EditOutcome, RenderAction, RenderEvent, Renderer, RunStatus, ToolState,
    View,
};

const MAX_TOOL_LINES: usize = 8;
const REASONING_TAIL: usize = 120;
/// 被限流时最多挡多久：窗口再长也不该让渲染永远停摆，终稿另有兜底重发
const MAX_SEND_DEFER_MS: i64 = 60_000;

#[derive(Clone, Debug)]
struct ToolEntry {
    call: CallId,
    name: String,
    state: ToolState,
    args_preview: String,
    result_preview: String,
    started_at: Timestamp,
    duration_ms: Option<u64>,
}

pub struct TurnRenderer {
    caps: Caps,
    status: Option<String>,
    tools: Vec<ToolEntry>,
    text: String,
    reasoning: String,
    turn: u32,
    last_flush: Timestamp,
    msg: Option<MsgRef>,
    need_send: bool,
    /// 上次真正发出去的内容签名；内容没变就不发 edit（否则长任务每秒一次编辑会被限流）
    last_sent: Option<String>,
    /// 通道刚拒绝过（429）：这个时刻之前不再发起任何发送/编辑
    send_after: Timestamp,
    /// 定稿最后一条消息上的按钮（如「沉淀为 Skill」）
    trailing: Vec<Vec<Button>>,
}

impl TurnRenderer {
    pub fn new(caps: Caps) -> Self {
        Self {
            caps,
            status: None,
            tools: Vec::new(),
            text: String::new(),
            reasoning: String::new(),
            turn: 0,
            last_flush: 0,
            msg: None,
            need_send: true,
            last_sent: None,
            send_after: 0,
            trailing: Vec::new(),
        }
    }

    fn tool_line(&self, t: &ToolEntry) -> Block {
        let mut detail = String::new();
        if !t.args_preview.is_empty() {
            detail.push_str(&t.args_preview);
        }
        if let Some(ms) = t.duration_ms {
            if !detail.is_empty() {
                detail.push_str(" · ");
            }
            detail.push_str(&format_duration(ms));
        }
        if t.state == ToolState::Failed && !t.result_preview.is_empty() {
            detail.push_str(&format!(" · {}", t.result_preview));
        }
        Block::ToolCall {
            call: t.call.clone(),
            name: t.name.clone(),
            state: t.state.clone(),
            preview: detail,
        }
    }

    fn compose(&self, truncate: bool) -> Vec<Block> {
        let mut blocks = Vec::new();

        if let Some(s) = &self.status {
            blocks.push(Block::Paragraph(s.clone()));
        }

        // 还没开始出字时，展示推理尾部（让人看到"在想什么"）
        if self.text.is_empty() && !self.reasoning.is_empty() {
            let tail = tail_chars(&self.reasoning, REASONING_TAIL);
            blocks.push(Block::Paragraph(format!("💭 {tail}")));
        }

        let overflow = self.tools.len().saturating_sub(MAX_TOOL_LINES);
        if overflow > 0 {
            blocks.push(Block::Paragraph(format!("…（另有 {overflow} 条更早的工具调用）")));
        }
        for t in self.tools.iter().skip(overflow) {
            blocks.push(self.tool_line(t));
        }

        if !self.text.is_empty() {
            let body = if truncate {
                let overhead = self.status.as_ref().map(|s| s.chars().count() + 2).unwrap_or(0)
                    + MAX_TOOL_LINES * 60
                    + 64;
                let cap = self.caps.max_text.saturating_sub(overhead).max(200);
                tail_chars(&self.text, cap)
            } else {
                self.text.clone()
            };
            blocks.push(Block::Paragraph(body));
        }
        blocks
    }

    fn view(&self, shard: u16, is_last: bool) -> View {
        View { shard, is_last, blocks: self.compose(true), status: None, buttons: Vec::new() }
    }
}

fn view_signature(v: &View) -> String {
    // 快照本身不大（正文已截断到一屏），存 debug 串比另做一套哈希结构更不易错
    format!("{v:?}")
}

fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn tail_chars(s: &str, cap: usize) -> String {
    let count = s.chars().count();
    if count <= cap {
        return s.to_owned();
    }
    let start = count - cap;
    format!("…{}", s.chars().skip(start).collect::<String>())
}

impl Renderer for TurnRenderer {
    fn push(&mut self, ev: RenderEvent) {
        match ev {
            RenderEvent::Stream(e) => match e {
                crate::driver::StreamEvent::TextDelta(t) => {
                    if self.text.is_empty() {
                        // 开始出字后不再显示推理摘要
                        self.status = Some(format!("第 {} 轮 · 生成中", self.turn));
                    }
                    self.text.push_str(&t);
                }
                crate::driver::StreamEvent::ReasoningDelta(r) => {
                    self.reasoning.push_str(&r);
                    if self.reasoning.chars().count() > 2000 {
                        self.reasoning = tail_chars(&self.reasoning, 2000);
                    }
                }
                crate::driver::StreamEvent::Restarted => {
                    self.text.clear();
                    self.reasoning.clear();
                    self.status = Some(format!("第 {} 轮 · 重试中", self.turn));
                }
                crate::driver::StreamEvent::TurnStarted { turn_no } => {
                    self.turn = turn_no;
                    self.status = Some(format!("第 {turn_no} 轮 · 思考中"));
                }
                crate::driver::StreamEvent::ToolStarted { call, name, preview } => {
                    self.status = Some(format!("第 {} 轮 · 执行 {name}", self.turn));
                    // 同一个 call 第二次出现 = 审批通过、真正开始执行。
                    // 从这一刻重新计时：否则"耗时"会把主人点确认的时间也算进去
                    // （板上实测：耗时 2s 的命令显示 61.9s，就是等人点按钮的时间）。
                    if let Some(t) = self.tools.iter_mut().find(|t| t.call == call) {
                        t.state = ToolState::Running;
                        t.started_at = now_ms();
                        t.duration_ms = None;
                        if !preview.is_empty() {
                            t.args_preview = preview;
                        }
                    } else {
                        self.tools.push(ToolEntry {
                            call,
                            name,
                            state: ToolState::Running,
                            args_preview: preview,
                            result_preview: String::new(),
                            started_at: now_ms(),
                            duration_ms: None,
                        });
                    }
                }
                crate::driver::StreamEvent::ToolFinished { call, ok, preview } => {
                    if let Some(t) = self.tools.iter_mut().find(|t| t.call == call) {
                        t.state = if ok { ToolState::Ok } else { ToolState::Failed };
                        t.duration_ms = Some((now_ms() - t.started_at).max(0) as u64);
                        if !preview.is_empty() {
                            t.result_preview = preview;
                        }
                    }
                    self.status = Some(format!("第 {} 轮 · 继续思考", self.turn));
                }
            },
            // 空串 = 清空状态（子 agent 跑完时用）
            RenderEvent::Status(s) => {
                self.status = if s.trim().is_empty() { None } else { Some(s) }
            }
            RenderEvent::ToolRetry { call } => {
                if let Some(t) = self.tools.iter_mut().find(|t| t.call == call) {
                    t.state = ToolState::Running;
                    t.duration_ms = None;
                    t.started_at = now_ms();
                }
            }
        }
    }

    fn flush(&mut self, now: Timestamp) -> RenderAction {
        // 通道刚给过 429：窗口内一次都别发，否则等于自己把洪水控制续上
        if now < self.send_after {
            return RenderAction::None;
        }
        if !self.need_send && now - self.last_flush < self.caps.min_edit_interval_ms as i64 {
            return RenderAction::None;
        }
        if self.text.is_empty() && self.status.is_none() && self.tools.is_empty() && self.reasoning.is_empty() {
            return RenderAction::None;
        }
        let view = self.view(0, true);
        self.last_flush = now;
        let sig = view_signature(&view);
        if self.need_send {
            self.need_send = false;
            self.last_sent = Some(sig);
            return RenderAction::Send(view);
        }
        // 内容没变就不要 edit：等模型思考、等工具、等审批时快照是静止的，
        // 每秒一次 editMessageText 会把 Telegram 的洪水控制点着（429 retry_after 动辄 30s+）。
        if self.last_sent.as_deref() == Some(sig.as_str()) {
            return RenderAction::None;
        }
        self.last_sent = Some(sig);
        RenderAction::Edit(view)
    }

    fn on_edit_result(&mut self, outcome: EditOutcome) {
        match outcome {
            EditOutcome::TooOld => {
                self.need_send = true;
                self.msg = None;
                self.last_sent = None;
            }
            EditOutcome::Rejected { retry_after_ms } => {
                // 没发出去：推迟下一拍，并把签名清掉，恢复后重发同一份快照
                self.last_flush += retry_after_ms as i64;
                self.last_sent = None;
            }
            EditOutcome::Edited | EditOutcome::NotModified => {}
        }
    }

    fn on_sent(&mut self, msg: MsgRef) {
        self.msg = Some(msg);
    }

    fn on_send_failed(&mut self, retry_after_ms: u32) {
        // 没发出去：下一拍继续尝试，但在洪水窗口内不要再碰通道
        self.need_send = true;
        self.last_sent = None;
        self.send_after = now_ms() + (retry_after_ms as i64).min(MAX_SEND_DEFER_MS);
    }

    fn replace_text(&mut self, text: String) {
        self.text = text;
    }

    fn finish(&mut self, status: RunStatus) -> Vec<RenderAction> {
        let text = std::mem::take(&mut self.text);
        // 阶段状态只用于过程展示，终稿一律丢弃，避免停留在"第 N 轮 · 生成中"
        self.status = None;
        let tools = std::mem::take(&mut self.tools);
        let buttons = if self.caps.buttons {
            std::mem::take(&mut self.trailing)
        } else {
            Vec::new()
        };

        let mut head: Vec<Block> = Vec::new();
        match status {
            RunStatus::Done => {}
            RunStatus::Failed => head.push(Block::Paragraph("❌ 本次运行失败".into())),
            RunStatus::Cancelled => head.push(Block::Paragraph("⏹ 已取消".into())),
        }
        for t in &tools {
            head.push(self.tool_line(t));
        }

        let cap = self.caps.max_text.saturating_sub(512).max(512);
        let shards = split_text(&text, cap);
        let total = shards.len().max(1);
        let mut actions = Vec::new();

        if shards.is_empty() {
            // 没有正文：只更新已有消息
            let view = View {
                shard: 0,
                is_last: true,
                blocks: head,
                status: None,
                buttons,
            };
            return vec![if self.msg.is_some() && !self.need_send {
                RenderAction::Edit(view)
            } else {
                RenderAction::Send(view)
            }];
        }

        for (i, shard) in shards.into_iter().enumerate() {
            let mut blocks = if i == 0 { head.clone() } else { Vec::new() };
            blocks.push(Block::Paragraph(shard));
            let last = i + 1 == total;
            let view = View {
                shard: i as u16,
                is_last: last,
                blocks,
                status: None,
                buttons: if last { buttons.clone() } else { Vec::new() },
            };
            if i == 0 && self.msg.is_some() && !self.need_send {
                actions.push(RenderAction::Edit(view));
            } else {
                actions.push(RenderAction::Send(view));
            }
        }
        actions
    }

    fn msg_ref(&self) -> Option<&MsgRef> {
        self.msg.as_ref()
    }

    fn set_trailing_buttons(&mut self, buttons: Vec<Vec<Button>>) {
        self.trailing = buttons;
    }
}

fn split_text(text: &str, cap: usize) -> Vec<String> {
    let cap = cap.max(1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let end = (i + cap).min(chars.len());
        let mut cut = end;
        if end < chars.len() {
            if let Some(pos) = chars[i..end].iter().rposition(|c| *c == '\n') {
                if pos > cap / 2 {
                    cut = i + pos + 1;
                }
            }
        }
        out.push(chars[i..cut].iter().collect());
        i = cut;
    }
    out
}

/// 一次落地的结果：`Deferred` 表示"这一拍没送到，稍后按 `retry_after_ms` 再来"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionOutcome {
    Done,
    Deferred { retry_after_ms: u32 },
}

/// 把渲染动作落到通道：Send → deliver，Edit → update，并把结果回填 Renderer。
///
/// 限流（429）不是致命错误：发不出去就报告 `Deferred`，绝不会把整轮 run 打死
/// ——那会让界面停在半截且没有任何终态。
pub async fn apply_action<C: Channel + ?Sized>(
    channel: &C,
    conversation: &str,
    renderer: &mut dyn Renderer,
    action: RenderAction,
) -> Result<ActionOutcome, ChannelError> {
    match action {
        RenderAction::None => Ok(ActionOutcome::Done),
        RenderAction::Send(view) => match deliver_with_retry(channel, conversation, view).await {
            Ok(msg) => {
                renderer.on_sent(msg);
                Ok(ActionOutcome::Done)
            }
            Err(ChannelError::RateLimited { retry_after_ms }) => {
                renderer.on_send_failed(retry_after_ms);
                Ok(ActionOutcome::Deferred { retry_after_ms })
            }
            Err(e) => Err(e),
        },
        RenderAction::Edit(view) => {
            let Some(msg) = renderer.msg_ref().cloned() else {
                return match deliver_with_retry(channel, conversation, view).await {
                    Ok(m) => {
                        renderer.on_sent(m);
                        Ok(ActionOutcome::Done)
                    }
                    Err(ChannelError::RateLimited { retry_after_ms }) => {
                        renderer.on_send_failed(retry_after_ms);
                        Ok(ActionOutcome::Deferred { retry_after_ms })
                    }
                    Err(e) => Err(e),
                };
            };
            match channel.update(conversation, &msg, view).await {
                Ok(outcome) => {
                    let deferred = match outcome {
                        EditOutcome::Rejected { retry_after_ms } => {
                            Some(ActionOutcome::Deferred { retry_after_ms })
                        }
                        _ => None,
                    };
                    renderer.on_edit_result(outcome);
                    Ok(deferred.unwrap_or(ActionOutcome::Done))
                }
                Err(ChannelError::RateLimited { retry_after_ms }) => {
                    // 退化成"这一拍没发出去"，交给 Renderer 的退避窗口
                    renderer.on_edit_result(EditOutcome::Rejected { retry_after_ms });
                    Ok(ActionOutcome::Deferred { retry_after_ms })
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// 限流退避上限：Telegram 的 retry_after 可能是 60s，等满会把正在跑的 run 冻住，
/// 所以流式过程中只做短退避，剩下的交给下一拍。
const RATE_LIMIT_BACKOFF_CAP_MS: u64 = 2_000;
const DELIVER_ATTEMPTS: usize = 3;

/// 发消息（不是编辑）时有界重试；仍然被限流就返回 `RateLimited`，
/// 由调用方决定是"下一拍再来"还是"等够窗口再发"。
async fn deliver_with_retry<C: Channel + ?Sized>(
    channel: &C,
    conversation: &str,
    view: View,
) -> Result<MsgRef, ChannelError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match channel.deliver(conversation, view.clone()).await {
            Ok(m) => return Ok(m),
            Err(ChannelError::RateLimited { retry_after_ms }) if attempt < DELIVER_ATTEMPTS => {
                let backoff = (retry_after_ms as u64).min(RATE_LIMIT_BACKOFF_CAP_MS);
                tracing::warn!(retry_after_ms, backoff, attempt, "发消息被限流，退避后重试");
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::StreamEvent;
    use crate::view::Caps;

    fn caps() -> Caps {
        Caps { edit: true, buttons: true, markdown: true, max_text: 4096, min_edit_interval_ms: 1000 }
    }

    #[test]
    fn throttles_by_interval() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TurnStarted { turn_no: 1 }));
        assert!(matches!(r.flush(0), RenderAction::Send(_)));
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("x".into())));
        assert!(matches!(r.flush(500), RenderAction::None));
        assert!(matches!(r.flush(1000), RenderAction::Edit(_)));
    }

    /// 内容静止时不要一直 edit：板上实测每秒一次 editMessageText 会被 Telegram 洪水控制，
    /// 一次 429(retry_after=43s) 就曾把整轮 run 打死。
    #[test]
    fn skips_edit_when_nothing_changed() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TurnStarted { turn_no: 1 }));
        assert!(matches!(r.flush(0), RenderAction::Send(_)));
        assert!(matches!(r.flush(1000), RenderAction::None), "内容没变不该 edit");
        assert!(matches!(r.flush(60_000), RenderAction::None), "一直没变就一直不 edit");
    }

    /// 429 退避之后，同一份快照要能再发出去（不能因为签名已记录就永远丢掉）。
    #[test]
    fn retries_after_rate_limit_backoff() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("x".into())));
        assert!(matches!(r.flush(0), RenderAction::Send(_)));
        r.on_sent(MsgRef("m1".into()));
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("y".into())));
        assert!(matches!(r.flush(1000), RenderAction::Edit(_)));
        r.on_edit_result(EditOutcome::Rejected { retry_after_ms: 5000 });
        assert!(matches!(r.flush(2000), RenderAction::None), "退避窗口内不该再发");
        assert!(matches!(r.flush(7000), RenderAction::Edit(_)), "退避结束后重发同一份快照");
    }

    #[test]
    fn shows_turn_phase_and_tool_details() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TurnStarted { turn_no: 2 }));
        r.push(RenderEvent::Stream(StreamEvent::ToolStarted {
            call: CallId("c1".into()),
            name: "exec".into(),
            preview: "echo hi".into(),
        }));
        match r.flush(0) {
            RenderAction::Send(v) => {
                let text = format!("{v:?}");
                assert!(text.contains("第 2 轮"), "{text}");
                assert!(text.contains("echo hi"), "{text}");
            }
            other => panic!("{other:?}"),
        }
        r.push(RenderEvent::Stream(StreamEvent::ToolFinished {
            call: CallId("c1".into()),
            ok: true,
            preview: "hi".into(),
        }));
        let text = format!("{:?}", r.flush(1000));
        assert!(text.contains("Ok"), "工具状态应变为 Ok: {text}");
        assert!(text.contains("ms") || text.contains('s'), "{text}");
    }

    #[test]
    fn trailing_buttons_only_on_last_shard() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::ToolStarted {
            call: CallId("c1".into()),
            name: "write".into(),
            preview: "a.py".into(),
        }));
        r.set_trailing_buttons(vec![vec![Button {
            label: "沉淀".into(),
            data: "skill:r1".into(),
        }]]);
        r.replace_text("x".repeat(5000));
        let actions = r.finish(RunStatus::Done);
        assert!(actions.len() > 1, "应分片: {}", actions.len());
        for (i, a) in actions.iter().enumerate() {
            let view = match a {
                RenderAction::Send(v) | RenderAction::Edit(v) => v,
                RenderAction::None => panic!("unexpected none"),
            };
            if view.is_last {
                assert_eq!(view.buttons.len(), 1, "最后一片才挂按钮");
            } else {
                assert!(view.buttons.is_empty(), "中间分片不应带按钮");
            }
            let _ = i;
        }
    }

    /// 审批通过后 runner 会再发一次 ToolStarted：不能变成两行，且耗时从这一刻起算。
    #[test]
    fn repeated_tool_started_resets_timer_without_duplicating() {
        let mut r = TurnRenderer::new(caps());
        let call = CallId("c1".into());
        r.push(RenderEvent::Stream(StreamEvent::ToolStarted {
            call: call.clone(),
            name: "lan_exec".into(),
            preview: "getent hosts x.local".into(),
        }));
        // 模拟"等人点确认"很久之后才真正开跑
        std::thread::sleep(std::time::Duration::from_millis(30));
        r.push(RenderEvent::Stream(StreamEvent::ToolStarted {
            call: call.clone(),
            name: "lan_exec".into(),
            preview: String::new(),
        }));
        r.push(RenderEvent::Stream(StreamEvent::ToolFinished {
            call: call.clone(),
            ok: true,
            preview: "10.0.0.1".into(),
        }));
        let text = format!("{:?}", r.flush(0));
        assert_eq!(
            text.matches("lan_exec").count(),
            1,
            "同一个 call 不应出现两行: {text}"
        );
        assert!(
            text.contains("0ms"),
            "耗时应从审批通过那一刻起算，不该包含等人确认的时间: {text}"
        );
        assert!(
            text.contains("getent hosts x.local"),
            "第二次 ToolStarted 的空 preview 不该覆盖已有参数预览: {text}"
        );
    }

    #[test]
    fn caps_tool_lines() {
        let mut r = TurnRenderer::new(caps());
        for i in 0..12 {
            r.push(RenderEvent::Stream(StreamEvent::ToolStarted {
                call: CallId(format!("c{i}")),
                name: "exec".into(),
                preview: "x".into(),
            }));
        }
        let text = format!("{:?}", r.flush(0));
        assert!(text.contains("另有 4 条"), "{text}");
    }

    #[test]
    fn shows_reasoning_tail_before_text() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::ReasoningDelta("先检查文件是否存在".into())));
        let text = format!("{:?}", r.flush(0));
        assert!(text.contains("💭"), "{text}");
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("答案".into())));
        let text = format!("{:?}", r.flush(2000));
        assert!(!text.contains("💭"), "出字后不再显示推理: {text}");
    }

    #[test]
    fn too_old_triggers_new_message() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("x".into())));
        assert!(matches!(r.flush(0), RenderAction::Send(_)));
        r.on_sent(MsgRef("m1".into()));
        r.on_edit_result(EditOutcome::TooOld);
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("y".into())));
        assert!(matches!(r.flush(2000), RenderAction::Send(_)));
    }

    #[test]
    fn finish_edits_the_streaming_message() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TurnStarted { turn_no: 1 }));
        match r.flush(0) {
            RenderAction::Send(_) => {}
            other => panic!("首帧应 Send: {other:?}"),
        }
        r.on_sent(MsgRef("m1".into()));
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("答案".into())));
        r.replace_text("最终答案".into());
        let actions = r.finish(RunStatus::Done);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], RenderAction::Edit(_)), "定稿必须编辑原消息");
    }

    #[test]
    fn finish_drops_transient_status() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TurnStarted { turn_no: 4 }));
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("答案".into())));
        let actions = r.finish(RunStatus::Done);
        let text = format!("{actions:?}");
        assert!(!text.contains("第 4 轮"), "终稿不应保留阶段状态: {text}");
        assert!(!text.contains("生成中"), "{text}");
    }

    #[test]
    fn finish_marks_failure() {
        let mut r = TurnRenderer::new(caps());
        r.push(RenderEvent::Stream(StreamEvent::TurnStarted { turn_no: 2 }));
        let actions = r.finish(RunStatus::Failed);
        assert!(format!("{actions:?}").contains("失败"));
    }

    #[test]
    fn finish_splits_long_text() {
        let mut r = TurnRenderer::new(Caps { max_text: 600, ..caps() });
        r.push(RenderEvent::Stream(StreamEvent::TextDelta("行\n".repeat(400))));
        let actions = r.finish(RunStatus::Done);
        assert!(actions.len() > 1, "长文本必须分片");
    }
}
