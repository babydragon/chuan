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

impl Domain {
    pub fn as_str(self) -> &'static str {
        match self {
            Domain::Sandbox => "sandbox",
            Domain::SelfModify => "self_modify",
            Domain::Outbound => "outbound",
            Domain::Lan => "lan",
            Domain::Install => "install",
            Domain::Host => "host",
            Domain::HostElevated => "host_elevated",
            Domain::HostObserve => "host_observe",
        }
    }

    /// 家人（member）默认也能用的域：不需要任何审批。
    /// 其余域对 member 一律 `Blocked`（D30 §1）——除非走命令条目或工具声明了 `member_safe`。
    pub fn family_default(self) -> bool {
        matches!(self, Domain::Sandbox | Domain::Outbound | Domain::HostObserve)
    }
}

/// 越界且没有合法路径可走时的原因（D30）。给文案与审计用，不是给人看的完整句子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    /// 家人触发到角色上限之外：不建审批行，记提案并通知主人
    MemberCeiling,
    /// 主人也拿不到：表外动作 / 参数越界 / 没有对应授权
    NotAuthorized,
}

/// 门的判定结果（D30）。**不要用 bool**：`false` 既可能意味着"继续跑"，也可能意味着"结束 run"。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateDecision {
    Allow,
    /// 这条路不存在（域关闭 / 参数非法）。模型看到"被拒绝"，run 继续。
    Deny(String),
    /// 逐条确认：只对主人；member 永远到不了这里。
    Ask,
    /// 终态：结束该 run（D30 §5）。
    Blocked { reason: BlockedReason, hint: String },
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

    /// 运行时判定（`DECISIONS.md` D30 §1，顺序不可颠倒）：
    ///
    /// ```text
    /// Deny（域关闭，天花板，对所有人）
    ///   → 数据化免审（bounded：工具自证 / 命令条目命中）→ Allow
    ///   → member 且域不在家人默认集且工具未声明 member_safe → Blocked
    ///   → Ask（只对 owner）
    /// ```
    ///
    /// - `bounded`：工具自证"这一条结构上有界"（`Tool::skip_approval`，例如授权表里的
    ///   `always` 路径）。**对 member 不生效**——否则家人会经 `host` 拿到宿主 shell。
    /// - `entry`：命中**命令条目**且该条目对当前主体开放（D31 的 `who`）。条目是主人写死的
    ///   一行 argv，对家人也有效，所以它单独一个开关。
    /// - `member_safe`：工具声明"对家人也安全"（目前只有 `remind`），仍需 `bounded` 才放行。
    ///
    /// 三者都**只能把 Ask 放成 Allow**，绝不能放开 Deny。
    pub fn decide(
        &self,
        role: crate::identity::ActorRole,
        tool: &str,
        domain: Domain,
        bounded: bool,
        entry: bool,
        member_safe: bool,
    ) -> GateDecision {
        if self.for_domain(domain) == Verdict::Deny {
            return GateDecision::Deny(format!(
                "{} 域已关闭（[policy] {} = \"deny\"）",
                domain.as_str(),
                domain.as_str()
            ));
        }
        let verdict = self.verdict(tool, domain);
        match role {
            crate::identity::ActorRole::Owner => match verdict {
                Verdict::Allow => GateDecision::Allow,
                Verdict::Deny => GateDecision::Deny(format!("{} 域已关闭", domain.as_str())),
                Verdict::Ask if bounded || entry => GateDecision::Allow,
                Verdict::Ask => GateDecision::Ask,
            },
            crate::identity::ActorRole::Member => {
                if verdict == Verdict::Allow {
                    return GateDecision::Allow;
                }
                if entry || (member_safe && bounded) {
                    return GateDecision::Allow;
                }
                GateDecision::Blocked {
                    reason: BlockedReason::MemberCeiling,
                    // 给用户看的一句话，不写策略键与内部术语（细节在 tracing 与账本里）。
                    hint: "这个动作超出了当前账号允许的范围。".to_owned(),
                }
            }
        }
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

    use crate::identity::ActorRole;

    #[test]
    fn decide_owner_keeps_old_semantics() {
        let p = ApprovalPolicy::default();
        let owner = ActorRole::Owner;
        assert_eq!(p.decide(owner, "exec", Domain::Sandbox, false, false, false), GateDecision::Allow);
        assert_eq!(p.decide(owner, "publish_skill", Domain::SelfModify, false, false, false), GateDecision::Ask);
        // 数据化免审：只能把 Ask 放成 Allow
        assert_eq!(p.decide(owner, "lan_exec", Domain::Lan, true, false, false), GateDecision::Allow);
        assert!(matches!(
            p.decide(owner, "hostd", Domain::HostElevated, true, false, true),
            GateDecision::Deny(_)
        ));
    }

    #[test]
    fn deny_is_a_ceiling_bounded_cannot_relax_it() {
        let p = ApprovalPolicy { lan: Verdict::Deny, ..Default::default() };
        assert!(matches!(
            p.decide(ActorRole::Owner, "lan_command", Domain::Lan, true, false, false),
            GateDecision::Deny(_)
        ));
        assert!(matches!(
            p.decide(ActorRole::Member, "lan_command", Domain::Lan, true, true, false),
            GateDecision::Deny(_)
        ));
    }

    #[test]
    fn member_is_limited_to_family_domains_and_bounded_entries() {
        let p = ApprovalPolicy::default();
        let m = ActorRole::Member;
        // 家人默认可用
        assert_eq!(p.decide(m, "exec", Domain::Sandbox, false, false, false), GateDecision::Allow);
        assert_eq!(p.decide(m, "web_fetch", Domain::Outbound, false, false, false), GateDecision::Allow);
        assert_eq!(p.decide(m, "sys", Domain::HostObserve, false, false, false), GateDecision::Allow);

        // 命令条目（entry）对家人也免审——打印靠这条；`bounded` 单独不给家人开门
        assert_eq!(
            p.decide(m, "lan_command", Domain::Lan, false, true, false),
            GateDecision::Allow
        );
        assert!(matches!(
            p.decide(m, "lan_command", Domain::Lan, true, false, false),
            GateDecision::Blocked { reason: BlockedReason::MemberCeiling, .. }
        ));

        // lan 的原始路径不行；host 的 skip_approval 也不能放给家人
        assert!(matches!(
            p.decide(m, "lan_exec", Domain::Lan, false, false, false),
            GateDecision::Blocked { reason: BlockedReason::MemberCeiling, .. }
        ));
        assert!(
            matches!(
                p.decide(m, "host", Domain::Host, true, false, false),
                GateDecision::Blocked { .. }
            ),
            "host 的 always 路径不能让家人拿到宿主 shell"
        );

        // 只有显式声明 member_safe 且自证有界才放（remind）
        assert_eq!(p.decide(m, "remind", Domain::SelfModify, true, false, true), GateDecision::Allow);
        assert!(matches!(
            p.decide(m, "remind", Domain::SelfModify, false, false, true),
            GateDecision::Blocked { .. }
        ));
    }
}
