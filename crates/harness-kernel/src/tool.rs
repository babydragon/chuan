use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::effect::EffectClass;
use crate::env::ExecutionEnv;
use crate::error::ToolError;
use crate::ids::{CallId, ConversationId, RunId};

/// 工作区按 run 创建；P0 为单可写根（三段式挂载列 P2）。
#[derive(Clone, Debug)]
pub struct Workspace {
    pub run: RunId,
    pub root: String,
}
use crate::record::DocWrite;
use crate::view::Block;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// 任意 JSON Schema（运行时生成，不强绑类型）。
    pub parameters: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ToolCallRequest {
    pub call: CallId,
    pub name: String,
    pub args: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ArtifactWrite {
    pub path: String,
    pub bytes_sha256: String,
    pub kind: String,
}

/// 一条待批准的条目提案（草稿文件里读出来的）。
#[derive(Clone, Debug)]
pub struct PendingCommand {
    pub name: String,
    /// 安装提案的正文字；`None` = 删除提案
    pub body: Option<String>,
    pub hash: String,
    /// 提案人写的一句人话（审批通知只显示它，不显示命令与参数）
    pub summary: String,
}

/// 命令条目的安装/删除（D31）。**由应用层实现**：内核只负责"主人批准之后调用它"，
/// 不碰 TOML 格式与目录布局（分层纪律：kernel 的依赖树里没有 toml / 文件路径知识）。
pub trait CommandAdmin: Send + Sync {
    /// 待批准的提案（草稿目录）。运行结束后内核据此建 `Permission` 审批。
    fn pending(&self) -> Vec<PendingCommand>;
    fn install(&self, name: &str, body: &str) -> Result<(), String>;
    /// 丢掉草稿（无论批准与否，处理完都要清）。
    fn discard(&self, name: &str) -> Result<(), String>;
    fn remove(&self, name: &str) -> Result<(), String>;
}

/// 命令条目的判定（D31）。`NotApplicable` = 这不是条目工具。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntryCheck {
    NotApplicable,
    /// 命中一条对当前主体开放的条目，且参数都在域内。
    Allowed,
    /// 条目工具，但这条不合法（表外名字 / 参数越界 / 条目只对 owner 开放）。
    Denied(String),
}

#[derive(Clone, Debug, Default)]
pub struct ToolOutput {
    pub content: Vec<Block>,
    /// 由内核在负责提交的事务里写入，工具本身不接触存储。
    pub doc_writes: Vec<DocWrite>,
    pub artifacts: Vec<ArtifactWrite>,
}

/// 工具把产物送到用户的通道（发文件/图片）。
///
/// 和 `Channel` 分开：工具拿不到整个通道，只能"发一个文件到当前会话"。
#[async_trait]
pub trait Outbox: Send + Sync {
    async fn send_file(
        &self,
        path: &str,
        name: &str,
        mime: &str,
        caption: &str,
    ) -> Result<(), String>;
}

/// 没有通道可用时的兜底（例如定时任务里的工具调用）。
pub struct NullOutbox;

#[async_trait]
impl Outbox for NullOutbox {
    async fn send_file(
        &self,
        _path: &str,
        _name: &str,
        _mime: &str,
        _caption: &str,
    ) -> Result<(), String> {
        Err("当前没有可用的发送通道".into())
    }
}

/// 工具读文档（文件库/记忆检索）的窄接口。写走 `ToolOutput.doc_writes`，读走这里。
#[async_trait]
pub trait DocReader: Send + Sync {
    async fn list(&self, kind: &str, limit: usize) -> Result<Vec<crate::record::Doc>, String>;
    async fn get(&self, id: &str) -> Result<Option<crate::record::Doc>, String>;
    async fn delete(&self, id: &str) -> Result<bool, String>;
}

pub struct NullDocReader;

#[async_trait]
impl DocReader for NullDocReader {
    async fn list(&self, _kind: &str, _limit: usize) -> Result<Vec<crate::record::Doc>, String> {
        Ok(Vec::new())
    }
    async fn get(&self, _id: &str) -> Result<Option<crate::record::Doc>, String> {
        Ok(None)
    }
    async fn delete(&self, _id: &str) -> Result<bool, String> {
        Ok(false)
    }
}

pub struct ToolCtx<'a> {
    pub conv: ConversationId,
    /// 发起者（通道内的用户标识）；多通道/多用户时用它解析 User
    pub actor: &'a str,
    pub run: RunId,
    pub call: CallId,
    pub workspace: &'a Workspace,
    pub env: &'a dyn ExecutionEnv,
    pub outbox: &'a dyn Outbox,
    pub docs: &'a dyn DocReader,
    /// 本 run 的 system 指令。子 agent 复用它以命中前缀缓存（D19）。
    pub system_prompt: &'a str,
    /// 只有主会话 run 才有；`None` = 这个 run 不能派生（子 agent / 定时任务）。
    pub spawner: Option<&'a dyn crate::agent::AgentSpawner>,
    pub cancel: tokio_util::sync::CancellationToken,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    fn effect(&self) -> EffectClass;

    /// 工具档位：`0` 通用（子 agent 也能拿到）、`1` 仅主会话。
    /// 用于子 agent 的工具裁剪（`DECISIONS.md` D19）。**不进 wire**，只在内核内排序/过滤。
    fn tier(&self) -> u8 {
        0
    }

    /// 并发车道（`DECISIONS.md` D11）。默认 `io`；编译/装包类长任务声明 `compile`。
    fn lane(&self) -> crate::lanes::Lane {
        crate::lanes::Lane::Io
    }

    /// 审批卡上给人看的一句话（例：把解析后的触发时间写出来）。
    /// 默认 None → 退化为"工具 + 参数"的机械展示。
    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let _ = args;
        None
    }

    /// 本次调用是否可以免审批。
    ///
    /// 只有工具**自己核对过授权表**才能返回 true（例如 `host` 发现这次只用到
    /// `approval = "always"` 的路径、没有临时挂载）。它只能把 `Ask` 放成"放行"，
    /// **不能**绕过 `Deny`——策略天花板仍在（D22）。每次跳过都会记日志。
    fn skip_approval(&self, _args: &serde_json::Value) -> bool {
        false
    }

    /// 这一条对**当前主体**是否免审。默认与角色无关，回落到 [`Tool::skip_approval`]。
    ///
    /// 只有"同一条调用对家人和对主人风险不同"的工具才需要覆盖它——目前只有 `remind`：
    /// 静态 `message` / 钉住 `skill` 到点不碰工具面，谁建都行；`text` 形态到点要跑一轮模型
    /// （带着工具面），只允许主人建（D30 §7）。
    fn preapproved_for(
        &self,
        args: &serde_json::Value,
        _role: crate::identity::ActorRole,
    ) -> bool {
        self.skip_approval(args)
    }

    /// 这一条调用是否命中**命令条目**（D31），以及该条目对当前主体是否开放。
    ///
    /// 只有持有条目表的工具（`lan_command`、将来的 `host_command`）会返回非 `NotApplicable`。
    /// `Denied` 表示"是条目工具，但这条不合法"——门会直接转成 `Blocked` 终态，
    /// **不会发卡**（D31 §5：表外动作不许换路径重试）。
    fn command_entry(&self, _args: &serde_json::Value, _role: crate::identity::ActorRole) -> EntryCheck {
        EntryCheck::NotApplicable
    }

    /// 家人（member）能否在不经审批的情况下用这个工具（D30）。
    ///
    /// **默认 false（fail closed）**。只有"效果只作用于发起者本人、且能由数据判定"的工具
    /// 才该返回 true（目前只有 `remind`）。它不能替代命令条目：
    /// 对家人开放一类动作，正确做法是写一条 `who = "anyone"` 的命令条目（D31），
    /// 而不是在这里放松——这里放松等于对所有家人永久放开这个工具。
    fn member_safe(&self) -> bool {
        false
    }

    /// 能力域：决定要不要人确认（见 `policy`）。默认沙盒内。
    fn domain(&self) -> crate::policy::Domain {
        crate::policy::Domain::Sandbox
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError>;
}
