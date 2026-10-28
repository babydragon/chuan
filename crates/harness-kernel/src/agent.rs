//! 子 agent（前台委托）的契约：`DECISIONS.md` D19 / D20。
//!
//! 内核只定义"怎么派、怎么收"，不实现调度策略（那是 `Actor` 的事）。
//! 设计上只有三个硬约束，其余都是配置：
//!
//! 1. **单层**：`depth` 恒为 1；子 run 拿不到 `AgentSpawner`，所以递归在结构上不可能。
//! 2. **只回传结论**：父上下文增加的是一条 `msg.tool_result`，不是子会话的过程。
//! 3. **不能请求审批**：子 run 的审批门对 `ask` 一律拒绝（fail-closed）。

use tokio_util::sync::CancellationToken;

use crate::driver::UsageSnapshot;
use crate::ids::RunId;

/// 结论摘要上限。超出部分留在子会话的账本里，父上下文只拿引用。
pub const SUMMARY_CAP: usize = 2000;

/// 子 agent 在结论末尾用的分节标记：`[待父执行]`。
///
/// 子 agent 没有审批权，需要人确认的动作（发布技能、装依赖、发消息、打印）
/// 只能交回父会话。用固定标记而不是让模型自由发挥，是为了**能解析**：
/// 父上下文需要一条明确清单，而不是在几百字里找"还差一步"。
pub const PENDING_MARKER: &str = "[待父执行]";

/// 子 agent 的默认工具白名单（D19）：只有免审批域的工具。
///
/// 这不是"少给几个"的偏好，而是安全边界的一部分：`http` / `mcp` / `service`
/// 会带着宿主注入的凭据去改远端状态，`install_deps` / `publish_skill` 改本地，
/// 子 agent 都拿不到。要在某个部署里放宽，改 `[agents] tools`——但审核域仍会拦。
pub const DEFAULT_CHILD_TOOLS: &[&str] = &[
    "read", "write", "exec", "playbook", "web_fetch", "keep", "files",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentProfile {
    /// 配置里的档位名；`"inherit"` = 跟随主会话。
    pub name: String,
    /// 覆盖模型（None = 继承 `[engine] model`）。
    pub model: Option<String>,
    /// 允许的工具名（None = 内置白名单；只能更窄，不能更宽）。
    pub tools: Option<Vec<String>>,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self { name: "inherit".into(), model: None, tools: None }
    }
}

#[derive(Clone, Debug)]
pub struct AgentSpec {
    /// 父会话（`sub:` 的父节点）
    pub parent_conv: String,
    pub parent_run: RunId,
    /// 任务书：要做什么、按什么格式回。
    pub task: String,
    /// `/ws` 下的输入文件路径（只传引用，不搬内容）。
    pub inputs: Vec<String>,
    pub profile: AgentProfile,
    /// 父的系统指令。子 agent 复用它（前缀一致才能命中 prompt cache）。
    pub system_prompt: Option<String>,
    pub max_turns: u32,
    pub max_tokens: u64,
    pub wall_ms: u64,
    pub depth: u8,
    /// 接手上一次的工作：任务号（`task-…`）或子会话（`sub:…#n`）。
    /// 内核把它解析成一段"上次进展"简报，注进子 agent 的 system——
    /// 不给它整段历史，只给结论与产物路径，避免把 token 花在复述上。
    pub continue_from: Option<String>,
    /// 预分配的子会话 id（后台任务在入队时定下来，打断后才知道去续哪个）。
    pub child_conv: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Done,
    Budget,
    Timeout,
    Failed,
    Cancelled,
}

impl AgentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Done => "done",
            AgentStatus::Budget => "budget",
            AgentStatus::Timeout => "timeout",
            AgentStatus::Failed => "failed",
            AgentStatus::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Conclusion {
    pub status: AgentStatus,
    /// ≤ [`SUMMARY_CAP`] 字符；完整内容在 `child_conv` 的账本里。
    pub summary: String,
    /// 子会话 id（`sub:<parent>#<n>`），用于查完整过程。
    pub child_conv: String,
    pub child_run: String,
    /// 子 agent 产出的文件路径（`/ws/...`）。
    pub artifacts: Vec<String>,
    /// 需要父会话替你完成的动作（发布技能、装依赖、发消息、打印…）。
    pub pending_actions: Vec<String>,
    pub usage: UsageSnapshot,
}

impl Conclusion {
    /// 给模型看的一段话。刻意把"过程在哪"写出来，避免它以为这就是全部。
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(match self.status {
            AgentStatus::Done => "",
            AgentStatus::Budget => "⚠️ 子任务因预算耗尽中止。\n",
            AgentStatus::Timeout => "⚠️ 子任务超时中止。\n",
            AgentStatus::Failed => "⚠️ 子任务失败。\n",
            AgentStatus::Cancelled => "⚠️ 子任务被取消。\n",
        });
        s.push_str(&self.summary);
        if !self.artifacts.is_empty() {
            s.push_str("\n\n产物：\n");
            for a in &self.artifacts {
                s.push_str("- ");
                s.push_str(a);
                s.push('\n');
            }
        }
        if !self.pending_actions.is_empty() {
            s.push_str("\n\n待父执行（子 agent 无权完成，需要你在本会话处理）：\n");
            for a in &self.pending_actions {
                s.push_str("- ");
                s.push_str(a);
                s.push('\n');
            }
        }
        s.push_str(&format!("\n（完整过程在子会话 {}）", self.child_conv));
        s
    }
}

/// 角色：一组"提示词 + 模型 + 工具子集 + 预算"。
///
/// 只能收窄，不能扩权：工具最终还要与 `[agents] tools` 白名单取交集，
/// 且 `tier >= 1`（含 `delegate`）始终被剔除。提示词追加在父前缀之后，
/// 不破坏 prompt cache。
#[derive(Clone, Debug, Default)]
pub struct RoleSpec {
    /// 一句话说明（注入父的 system prompt，让模型知道有这个角色）
    pub description: Option<String>,
    /// 角色说明；`None` = 不加（继承父的 system 指令）
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    /// 该角色可用的工具；`None` = 跟随 `[agents] tools`
    pub tools: Option<Vec<String>>,
    /// 0 = 跟随 `[agents]` 的默认值
    pub max_turns: u32,
    pub max_tokens: u64,
    pub wall_ms: u64,
}

/// 角色来源：配置里写死的 + 目录里可热加载的。
///
/// 每次调用重新读盘（几个文件、几 KB），所以 agent 用 `role` 工具注册完，
/// 下一次 delegate 就能用，不需要重启。
pub trait RoleSource: Send + Sync {
    fn all(&self) -> Vec<(String, RoleSpec)>;
}

/// 委托闸门与默认预算（`[agents]`）。0 值一律按默认处理，避免配置漏项就把子 agent 关死。
#[derive(Clone, Debug)]
pub struct AgentLimits {
    pub enabled: bool,
    /// 子 agent 能看到的工具名（默认 [`DEFAULT_CHILD_TOOLS`]）。
    /// 最终工具集还会再过 tier 过滤和审批域，放宽这里不等于放宽权限。
    pub tools: Vec<String>,
    /// 单个 run 最多派几个子 agent
    pub max_per_run: usize,
    pub max_turns: u32,
    pub max_tokens: u64,
    pub wall_ms: u64,
    /// 前台静默多少秒后才启动后台任务（0 = 不等待）。
    ///
    /// 原来的 30s 太保守：用户明确说了"慢慢做"，还要再等半分钟纯属浪费。
    /// 5s 只用来错开"连着发两条消息"，真正的互斥由「没有 run 在跑」+ 车道保证。
    pub background_idle_s: u64,
    /// 可用角色（`[agents.roles.<name>]`）。空 = 只能用 `inherit`。
    pub roles: std::collections::HashMap<String, RoleSpec>,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            enabled: true,
            tools: DEFAULT_CHILD_TOOLS.iter().map(|s| (*s).to_owned()).collect(),
            max_per_run: 2,
            max_turns: 6,
            max_tokens: 20_000,
            wall_ms: 120_000,
            background_idle_s: 5,
            roles: std::collections::HashMap::new(),
        }
    }
}

/// 一条后台任务的状态快照（给模型看的，不是给存储看的）。
#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub id: String,
    /// `pending` / `running` / `done` / `failed` / `cancelled` / `interrupted`
    pub status: String,
    pub text: String,
    pub created_ms: i64,
    pub started_ms: Option<i64>,
    /// 子会话 id：想知道"它到底干了什么"，用这个去查（`/agents show`）
    pub child_conv: Option<String>,
    /// 结论摘要（完成/失败时才有）
    pub result: Option<String>,
}

#[async_trait::async_trait]
pub trait AgentSpawner: Send + Sync {
    async fn spawn(
        &self,
        spec: AgentSpec,
        cancel: CancellationToken,
    ) -> Result<Conclusion, AgentError>;

    /// 排一个后台任务（P4.3）：立刻返回任务号，结果之后推回原会话。
    /// 默认不支持——只有内核的 actor 实现提供。
    async fn enqueue(&self, spec: AgentSpec) -> Result<String, AgentError> {
        let _ = spec;
        Err(AgentError::Other("当前实现不支持后台任务".into()))
    }

    /// 自己派出去的后台任务（父 agent 管理用；不要拿来轮询）。
    async fn tasks(&self) -> Result<Vec<TaskInfo>, AgentError> {
        Err(AgentError::Other("当前实现不支持任务查询".into()))
    }

    /// 取消一条后台任务（pending 直接作废，running 打断）。
    async fn cancel_task(&self, id: &str) -> Result<String, AgentError> {
        let _ = id;
        Err(AgentError::Other("当前实现不支持任务取消".into()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("子 agent 未启用")]
    Disabled,
    #[error("不允许递归派生（depth={0}）")]
    Depth(u8),
    #[error("本次 run 已派满 {0} 个子 agent")]
    Quota(usize),
    #[error("子 agent 排队被取消")]
    Cancelled,
    #[error("{0}")]
    Other(String),
}

/// 截断到 [`SUMMARY_CAP`]，按字符（不是字节）切，避免切坏中文。
pub fn clip_summary(s: &str) -> String {
    if s.chars().count() <= SUMMARY_CAP {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(SUMMARY_CAP).collect();
    out.push_str("…（已截断）");
    out
}

/// 容错版标记识别：`[待父执行]`、`3) [待父执行]：说明`、`## [待父执行]` 都算。
fn is_pending_marker(line: &str) -> bool {
    let t = line.trim().trim_start_matches(['#', ' ']);
    let t = t.trim_start_matches(|c: char| c.is_ascii_digit() || matches!(c, ')' | '.' | '、' | '-'));
    t.trim_start().starts_with(PENDING_MARKER)
}

/// 从结论里切出 `[待父执行]` 块：返回（正文, 动作清单）。
///
/// 规则刻意宽松：标记行之后、连续的 `-`/`*`/`1.` 列表项都算动作；
/// 遇到非列表行就回到正文（模型偶尔会在后面补一句解释，不该丢）。
/// 没有标记时原样返回。
pub fn split_pending(text: &str) -> (String, Vec<String>) {
    let mut body: Vec<&str> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    let mut in_block = false;

    for line in text.lines() {
        if is_pending_marker(line) {
            in_block = true;
            continue;
        }
        if in_block {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let bullet = t
                .strip_prefix("- ")
                .or_else(|| t.strip_prefix("* "))
                .or_else(|| {
                    let (head, rest) = t.split_once(". ")?;
                    head.chars().all(|c| c.is_ascii_digit()).then_some(rest)
                });
            if let Some(item) = bullet {
                let item = item.trim();
                if !item.is_empty() && item != "无" {
                    items.push(item.chars().take(200).collect());
                }
                continue;
            }
            // 非列表行：块到此结束，这行归正文
            in_block = false;
        }
        body.push(line);
    }

    while body.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        body.pop();
    }
    items.truncate(8);
    (body.join("\n"), items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_clipping_is_char_safe() {
        let s = "中".repeat(SUMMARY_CAP + 10);
        let c = clip_summary(&s);
        assert!(c.chars().count() <= SUMMARY_CAP + 6, "{}", c.chars().count());
        assert!(c.ends_with("（已截断）"));
        assert!(c.starts_with("中中中"));
    }

    #[test]
    fn pending_block_is_extracted_and_stripped() {
        let text = "结论：已写好技能草稿。\n产物：/ws/publish/demo/\n\n[待父执行]\n- publish_skill(demo)\n2. install_deps(pip, requests)\n";
        let (body, items) = split_pending(text);
        assert_eq!(items, vec!["publish_skill(demo)", "install_deps(pip, requests)"]);
        assert!(!body.contains("待父执行"), "{body}");
        assert!(body.contains("产物：/ws/publish/demo/"), "{body}");
        assert!(!body.ends_with('\n'), "尾部空行应去掉: {body:?}");
    }

    #[test]
    fn pending_block_with_none_is_empty() {
        let (body, items) = split_pending("结论：没事可做。\n[待父执行]\n- 无\n");
        assert!(items.is_empty());
        assert_eq!(body, "结论：没事可做。");
    }

    #[test]
    fn text_after_pending_block_returns_to_body() {
        let (body, items) = split_pending("结论：A\n[待父执行]\n- publish_skill(x)\n补充一句说明。\n");
        assert_eq!(items, vec!["publish_skill(x)"]);
        assert!(body.contains("补充一句说明。"), "{body}");
        assert!(body.contains("结论：A"), "{body}");
    }

    #[test]
    fn marker_tolerates_list_numbering_and_headings() {
        let (body, items) = split_pending("## 结论：A\n3) [待父执行]：需要你处理的\n- publish_skill(x)\n");
        assert_eq!(items, vec!["publish_skill(x)"]);
        assert!(body.contains("## 结论：A"), "{body}");
        assert!(!body.contains("待父执行"), "{body}");
    }

    #[test]
    fn no_marker_means_no_change() {
        let (body, items) = split_pending("就是一段普通结论\n第二行");
        assert!(items.is_empty());
        assert_eq!(body, "就是一段普通结论\n第二行");
    }

    #[test]
    fn pending_items_are_capped() {
        let mut t = String::from("[待父执行]\n");
        for i in 0..12 {
            t.push_str(&format!("- action{i}\n"));
        }
        let (_, items) = split_pending(&t);
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn default_child_tools_carry_no_approval_domains() {
        for banned in [
            "http", "mcp", "service", "install_deps", "publish_skill", "rollback_skill",
            "lan_exec", "delegate",
        ] {
            assert!(
                !DEFAULT_CHILD_TOOLS.contains(&banned),
                "{banned} 属于审批域或父级专用，不能出现在子 agent 默认工具集里"
            );
        }
    }

    #[test]
    fn render_marks_non_done_states() {
        let c = Conclusion {
            status: AgentStatus::Budget,
            summary: "只查到一半".into(),
            child_conv: "sub:c#1".into(),
            child_run: "r1".into(),
            artifacts: vec!["/ws/a.txt".into()],
            pending_actions: vec!["publish_skill(demo)".into()],
            usage: UsageSnapshot::default(),
        };
        let t = c.render();
        assert!(t.contains("预算耗尽"));
        assert!(t.contains("/ws/a.txt"));
        assert!(t.contains("sub:c#1"));
    }
}
