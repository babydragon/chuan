//! 通道端口：只做收发与能力声明，不拼内容、不做业务判断。

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ids::{ChannelId, MsgRef, Timestamp};
use crate::view::{Caps, EditOutcome, View};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboundKind {
    Text,
    Command,
    Media,
    Callback,
}

/// 通道收到的附件。通道只负责**落盘**，怎么理解它由模型自己决定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// 宿主绝对路径（沙盒里以 `/media/<name>` 只读可见）
    pub path: String,
    pub name: String,
    pub mime: String,
    /// 内容摘要（FNV-1a，仅用于去重与命名，不是密码学哈希）
    pub digest: String,
    pub bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Inbound {
    pub channel: ChannelId,
    /// 通道内的会话标识（如 Telegram chat id 的字符串形式）
    pub conversation: String,
    /// 通道内的发送者标识
    pub actor: String,
    pub kind: InboundKind,
    pub text: Option<String>,
    /// 去重键（如 update_id）
    pub dedupe_key: String,
    /// 回调 ID（仅 Callback 类需要，用于 answerCallbackQuery）
    pub callback_id: Option<String>,
    /// 平台侧的消息句柄（用于给这条消息挂状态反应）。
    /// `None` = 这条入站不是平台上真实存在的一条（定时任务合成、内部派发），此时 `react` 必然 no-op。
    pub msg: Option<MsgRef>,
    pub received_at: Timestamp,
    #[allow(missing_docs)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("rate limited, retry after {retry_after_ms}ms")]
    RateLimited { retry_after_ms: u32 },
    #[error("transport: {0}")]
    Transport(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("closed")]
    Closed,
}

#[async_trait]
pub trait Channel: Send + Sync + 'static {
    fn id(&self) -> ChannelId;
    fn caps(&self) -> Caps;

    /// 持续收消息，直到取消或致命错误。
    async fn run(
        &self,
        tx: mpsc::Sender<Inbound>,
        cancel: CancellationToken,
    ) -> Result<(), ChannelError>;

    async fn deliver(&self, conversation: &str, view: View) -> Result<MsgRef, ChannelError>;

    async fn update(
        &self,
        conversation: &str,
        msg: &MsgRef,
        view: View,
    ) -> Result<EditOutcome, ChannelError>;

    async fn ack(&self, callback: &str, text: &str) -> Result<(), ChannelError>;

    /// 撤掉消息上的按钮（按钮已完成使命时调用）。
    /// 默认什么都不做：没有按钮能力的通道本来就没有可撤的。
    async fn clear_buttons(
        &self,
        _conversation: &str,
        _msg: &MsgRef,
    ) -> Result<(), ChannelError> {
        Ok(())
    }

    /// 发一个文件/图片。默认降级成一条文本提示（保证新通道可用）。
    async fn send_file(
        &self,
        conversation: &str,
        path: &str,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<MsgRef, ChannelError> {
        let _ = (path, mime);
        let mut text = format!("文件：{name}");
        if !caption.is_empty() {
            text.push_str(&format!("\n{caption}"));
        }
        self.deliver(
            conversation,
            crate::view::View {
                shard: 0,
                is_last: true,
                blocks: vec![crate::view::Block::Paragraph(text)],
                status: None,
                buttons: Vec::new(),
            },
        )
        .await
    }

    /// 发一个交互提示。通道按自身能力渲染：
    /// 有按钮 → inline keyboard；无按钮 → 文本 + 序号，用户回 `/approve <id>`。
    ///
    /// 默认实现走纯文本降级（保证新通道可用，不会静默丢失提示）。
    async fn deliver_prompt(
        &self,
        conversation: &str,
        prompt: crate::view::Prompt,
    ) -> Result<MsgRef, ChannelError> {
        let mut text = prompt.text;
        if !prompt.choices.is_empty() {
            text.push_str("\n\n");
            for (i, c) in prompt.choices.iter().enumerate() {
                let cmd = if c.value == "y" { "approve" } else { "deny" };
                text.push_str(&format!(
                    "{}) {}  —  `/{cmd} {}`\n",
                    i + 1,
                    c.label,
                    c.id
                ));
            }
        }
        let view = crate::view::View {
            shard: 0,
            is_last: true,
            blocks: vec![crate::view::Block::Paragraph(text)],
            status: None,
            buttons: Vec::new(),
        };
        self.deliver(conversation, view).await
    }

    /// 审批结束后的清理：优先删除提示消息；不支持删除的通道做文本覆盖。
    async fn clear_prompt(
        &self,
        conversation: &str,
        msg: &MsgRef,
        note: &str,
    ) -> Result<(), ChannelError> {
        let view = crate::view::View {
            shard: 0,
            is_last: true,
            blocks: vec![crate::view::Block::Paragraph(note.to_owned())],
            status: None,
            buttons: Vec::new(),
        };
        self.update(conversation, msg, view).await.map(|_| ())
    }

    /// 显示"对方正在输入"。默认无操作；Telegram 需每 ~4s 续一次。
    async fn typing(&self, _conversation: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    /// 结束"正在输入"。默认无操作（多数平台几秒后自己消失）；
    /// 微信要显式发 status=2，否则会多挂几秒。
    async fn typing_stop(&self, _conversation: &str) -> Result<(), ChannelError> {
        Ok(())
    }

    /// 给**入站消息**挂一个状态标记（表情反应）。语义是 best-effort：
    /// 失败只该记 trace，不得影响 run 的成败，也不得阻塞 turn（D24）。
    ///
    /// 默认实现即"没有该能力"——不支持 reaction 的通道什么都不用写。
    async fn react(
        &self,
        _conversation: &str,
        _msg: &MsgRef,
        _emoji: &str,
    ) -> Result<(), ChannelError> {
        Ok(())
    }

    /// 指令清单（不含斜杠）。通道可据此注册到平台（如 Telegram setMyCommands）。
    fn command_specs(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// 把指令注册到平台，失败不致命。
    async fn register_commands(&self) -> Result<(), ChannelError> {
        Ok(())
    }
}
