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
