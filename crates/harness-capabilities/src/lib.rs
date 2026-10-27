//! 能力层：沙盒、内置工具、技能、打印。P0 待实现 `BwrapEnv` 与 `exec`。

use async_trait::async_trait;
use harness_kernel::{env::{ExecOpts, ExecOutput}, EnvError, ExecutionEnv};

pub mod admin;
pub mod install;
pub mod library;
pub mod mcp;
pub mod net;
pub mod payload;
pub mod playbook;
pub mod services;
pub mod spill;
pub mod sandbox;
pub mod skills;
pub mod tools;

pub use admin::ServiceTool;
pub use install::InstallTool;
pub use library::{FilesTool, KeepTool};
pub use mcp::{McpCallTool, McpHub, McpServer};
pub use net::{NetLogTool, WebFetchTool};
pub use tools::remind::RemindTool;
pub use tools::send::SendFileTool;
pub use playbook::PlaybookTool;
pub use services::{HttpServiceTool, Service};
pub use sandbox::{BwrapEnv, Mount, validate_mount};
pub use skills::{PublishSkillTool, RollbackSkillTool, RunSkillTool, SkillCatalog};
pub use tools::{ExecTool, ReadTool, RememberTool, WriteTool};

/// 未接入沙盒时的占位：任何执行都明确失败，绝不静默放行。
pub struct NullEnv;

#[async_trait]
impl ExecutionEnv for NullEnv {
    async fn exec(
        &self,
        _argv: &[String],
        _opts: ExecOpts,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<ExecOutput, EnvError> {
        Err(EnvError::Unavailable("sandbox not wired".into()))
    }
}
