use crate::ids::{CallId, MsgRef};

#[derive(Clone, Debug)]
pub struct Caps {
    pub edit: bool,
    pub buttons: bool,
    pub markdown: bool,
    pub max_text: usize,
    pub min_edit_interval_ms: u32,
}

impl Default for Caps {
    fn default() -> Self {
        Self { edit: true, buttons: true, markdown: false, max_text: 4096, min_edit_interval_ms: 1000 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug)]
pub enum Block {
    Paragraph(String),
    Code { lang: Option<String>, text: String },
    Image { uri: String, alt: String },
    Divider,
    ToolCall {
        call: CallId,
        name: String,
        state: ToolState,
        preview: String,
    },
}

#[derive(Clone, Debug)]
pub struct Button {
    pub label: String,
    /// <= 64 字节（Telegram 限制）
    pub data: String,
}

/// 交互选项：与通道无关。通道决定渲染成按钮、序号还是弹窗。
#[derive(Clone, Debug)]
pub struct Choice {
    /// 稳定标识（回调时原样带回）
    pub id: String,
    pub label: String,
    /// 选择结果（通道原样回传，如 "y" / "n"）
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub text: String,
    pub choices: Vec<Choice>,
}

#[derive(Clone, Debug, Default)]
pub struct View {
    pub shard: u16,
    pub is_last: bool,
    pub blocks: Vec<Block>,
    pub status: Option<String>,
    pub buttons: Vec<Vec<Button>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunStatus {
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub enum RenderEvent {
    Stream(crate::driver::StreamEvent),
    Status(String),
    ToolRetry { call: CallId },
}

#[derive(Clone, Debug)]
pub enum RenderAction {
    None,
    Edit(View),
    Send(View),
}

/// 内核内的渲染策略（不可替换），负责节流/合并/分片/降级。
pub trait Renderer: Send {
    fn push(&mut self, ev: RenderEvent);
    fn flush(&mut self, now: crate::ids::Timestamp) -> RenderAction;
    fn on_edit_result(&mut self, outcome: EditOutcome);
    /// 通道成功发出新消息后回填引用，后续才能原地编辑。
    fn on_sent(&mut self, msg: MsgRef);
    /// 发消息失败（如 429）：重新置起"待发送"，并在此时间点之前不再碰通道。
    fn on_send_failed(&mut self, retry_after_ms: u32) {
        let _ = retry_after_ms;
    }
    /// 用权威终稿替换累积文本（流式 delta 可能与最终文本有差异）。
    fn replace_text(&mut self, text: String) {
        let _ = text;
    }
    /// 定稿可能分多条，因此返回动作列表。
    fn finish(&mut self, status: RunStatus) -> Vec<RenderAction>;
    /// 当前已定稿消息；用于换新消息时保持引用。
    fn msg_ref(&self) -> Option<&MsgRef>;
    /// 在定稿的最后一条消息上附加按钮；无按钮能力的通道自行忽略。
    fn set_trailing_buttons(&mut self, buttons: Vec<Vec<Button>>) {
        let _ = buttons;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditOutcome {
    Edited,
    NotModified,
    TooOld,
    Rejected { retry_after_ms: u32 },
}
