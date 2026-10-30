//! 能力层：沙盒（bwrap）、内置工具、命名服务（http/mcp）、技能、playbook、凭据注入。
//!
//! 边界：这里只放"通用传输与执行"，领域能力（打印、某家 API 的语义）归技能与 playbook（D17）；
//! 凭据的值只在宿主侧使用，永远不进沙盒（D18）。

use async_trait::async_trait;
use harness_kernel::{env::{ExecOpts, ExecOutput}, EnvError, ExecutionEnv};

pub mod admin;
pub mod auth;
pub mod elevated;
pub mod grants;
pub mod host;
pub mod hostd;
pub mod install;
pub mod library;
pub mod mcp;
pub mod net;
pub mod payload;
pub mod playbook;
pub mod services;
pub mod spill;
pub mod proxy;
pub mod sandbox;
pub mod roles;
pub mod skills;
pub mod tools;

pub use admin::ServiceTool;
pub use grants::{Approval, ArgvGrant, Grants, GrantsCache, Mode, PathGrant};
pub use elevated::ElevatedClient;
pub use host::HostEnv;
pub use install::InstallTool;
pub use library::{FilesTool, KeepTool};
pub use mcp::{McpCallTool, McpHub, McpServer};
pub use net::{NetLogTool, WebFetchTool};
pub use roles::{DiskRoles, RoleTool};
pub use tools::remind::{RemindersTool, RemindTool};
pub use tools::send::SendFileTool;
pub use tools::task::TaskTool;
pub use playbook::PlaybookTool;
pub use services::{HttpServiceTool, Service};
pub use proxy::{Mode as ProxyMode, ProxyConfig};
pub use sandbox::{BwrapEnv, Mount, validate_mount};
pub use skills::{PublishSkillTool, RollbackSkillTool, RunSkillTool, SkillCatalog};
pub use tools::{
    CapabilityTool, DelegateTool, ElevatedTool, ExecTool, HostTool, ReadTool, RememberTool, SysTool,
    WriteTool,
};

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

/// 给外部调用方（如 apps/home 构造 MCP 服务）用的代理构造器，
/// 避免把 reqwest 泄漏给应用层的类型签名。
pub fn proxy_for(url: &str) -> Option<reqwest::Proxy> {
    reqwest::Proxy::all(url).ok()
}
