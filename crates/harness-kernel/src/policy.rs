//! 审批策略：轴是"执行域"（能力越界），不是命令/工具黑名单。
//!
//! 为什么不能按命令判：模型为了达成目标会自己写脚本走 `exec`，
//! 黑名单永远追不上；而 `exec` 在沙盒里本来就出不去（/home 不可见、
//! /etc 只读、无网络、无凭据）。所以真正需要人点头的只有"出沙盒"的动作，
//! 而不是"沙盒里跑了什么"。

use serde::{Deserialize, Serialize};

/// 单个执行域的处置方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// 直接执行
    Allow,
    /// 逐条确认
    Ask,
    /// 一律拒绝（不是"问"，是"不允许"）
    Deny,
}

/// 能力域。工具用 `Tool::domain()` 声明自己属于哪一类。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Domain {
    /// 沙盒内：/ws 可写，宿主不可见，无网络
    Sandbox,
    /// 改自身：技能目录、配置、代码
    SelfModify,
    /// 出网（宿主代取，受域名白名单约束）
    Outbound,
    /// 内网调试：有网卡（局域网与公网都可达），看不到宿主文件
    Lan,
    /// 装依赖：拉外部包写进运行时目录（供应链动作）
    Install,
    /// 宿主执行 / 提权
    HostExec,
}

/// `[policy]` 的运行时形态。
#[derive(Clone, Debug)]
pub struct ApprovalPolicy {
    pub sandbox: Verdict,
    pub self_modify: Verdict,
    pub outbound: Verdict,
    pub lan: Verdict,
    pub install: Verdict,
    pub host_exec: Verdict,
    /// 额外要求确认的工具名：只能加严，不能放松域的 Deny
    pub extra_tools: Vec<String>,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self {
            sandbox: Verdict::Allow,
            self_modify: Verdict::Ask,
            outbound: Verdict::Allow,
            lan: Verdict::Ask,
            install: Verdict::Ask,
            host_exec: Verdict::Deny,
            extra_tools: Vec::new(),
        }
    }
}

impl ApprovalPolicy {
    pub fn for_domain(&self, domain: Domain) -> Verdict {
        match domain {
            Domain::Sandbox => self.sandbox,
            Domain::SelfModify => self.self_modify,
            Domain::Outbound => self.outbound,
            Domain::Lan => self.lan,
            Domain::Install => self.install,
            Domain::HostExec => self.host_exec,
        }
    }

    /// 某次调用的最终处置。名单只能把 Allow 提成 Ask，不能把 Deny 放开。
    pub fn verdict(&self, tool: &str, domain: Domain) -> Verdict {
        let base = self.for_domain(domain);
        if base == Verdict::Deny {
            return Verdict::Deny;
        }
        if self.extra_tools.iter().any(|t| t == tool) {
            return Verdict::Ask;
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_allow_sandbox_and_ask_self_modify() {
        let p = ApprovalPolicy::default();
        assert_eq!(p.verdict("exec", Domain::Sandbox), Verdict::Allow);
        assert_eq!(p.verdict("publish_skill", Domain::SelfModify), Verdict::Ask);
        assert_eq!(p.verdict("host_exec", Domain::HostExec), Verdict::Deny);
        assert_eq!(p.verdict("lan_exec", Domain::Lan), Verdict::Ask);
        assert_eq!(p.verdict("install_deps", Domain::Install), Verdict::Ask);
    }

    #[test]
    fn name_list_only_tightens() {
        let p = ApprovalPolicy {
            extra_tools: vec!["write".into(), "host_exec".into()],
            ..Default::default()
        };
        assert_eq!(p.verdict("write", Domain::Sandbox), Verdict::Ask);
        // 域本身是 Deny，名单不能把它放成 Ask
        assert_eq!(p.verdict("host_exec", Domain::HostExec), Verdict::Deny);
    }
}
