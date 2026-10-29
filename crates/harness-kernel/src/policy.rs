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
    /// 宿主执行（L1）：bwrap + 路径白名单 + 宿主 netns，服务账号身份
    Host,
    /// 宿主提权（L2）：经 `chuan hostd`，argv 授权表
    HostElevated,
    /// 宿主只读观测（L3）：`/proc`、`/sys`、`systemctl status`
    HostObserve,
}

/// `[policy]` 的运行时形态。
#[derive(Clone, Debug)]
pub struct ApprovalPolicy {
    pub sandbox: Verdict,
    pub self_modify: Verdict,
    pub outbound: Verdict,
    pub lan: Verdict,
    pub install: Verdict,
    /// 宿主执行（L1）：默认逐条问
    pub host: Verdict,
    /// 宿主提权（L2）：默认关闭
    pub host_elevated: Verdict,
    /// 宿主只读观测（L3）：默认放行（读 `/proc` 本身不构成越界）
    pub host_observe: Verdict,
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
            host: Verdict::Ask,
            host_elevated: Verdict::Deny,
            host_observe: Verdict::Allow,
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
            Domain::Host => self.host,
            Domain::HostElevated => self.host_elevated,
            Domain::HostObserve => self.host_observe,
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
        assert_eq!(p.verdict("host", Domain::Host), Verdict::Ask);
        assert_eq!(p.verdict("hostd", Domain::HostElevated), Verdict::Deny);
        // 读 /proc 不算越界；但 L1/L2 不行
        assert_eq!(p.verdict("sys", Domain::HostObserve), Verdict::Allow);
        assert_eq!(p.verdict("lan_exec", Domain::Lan), Verdict::Ask);
        assert_eq!(p.verdict("install_deps", Domain::Install), Verdict::Ask);
    }

    #[test]
    fn name_list_only_tightens() {
        let p = ApprovalPolicy {
            extra_tools: vec!["write".into(), "hostd".into(), "sys".into()],
            ..Default::default()
        };
        assert_eq!(p.verdict("write", Domain::Sandbox), Verdict::Ask);
        // 域本身是 Deny，名单不能把它放成 Ask
        assert_eq!(p.verdict("hostd", Domain::HostElevated), Verdict::Deny);
        // 域是 Allow，名单可以把它提成 Ask
        assert_eq!(p.verdict("sys", Domain::HostObserve), Verdict::Ask);
    }
}
