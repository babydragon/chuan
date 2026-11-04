//! 用户可见文案（**发给通道的话**）。
//!
//! 只放"人看到的那一句"。三条边界：
//! 1. **模型可见的描述不在这里**：`ToolSpec.description`、`NotGranted` 之类是提示词/结构化
//!    数据，不属于文案；混在一起会让"改一句话"变成"改提示词"。
//! 2. **日志与错误不在这里**：`tracing` 与 `RunnerError` 是给运维的，可以写术语。
//! 3. **人话规则**（`GLOSSARY.md` §G.9）：不出现身份称谓（主人/家人）、不出现策略键
//!    （`[policy]`、`who="anyone"`）、不出现 shell。`copy::tests` 会扫一遍。
//!
//! 为什么单独一个模块：这些句子要被人反复读、被统一修改；散在 `actor.rs` 的 36 处
//! `reply()` 里，改一次措辞要翻五个函数。以后真要做多语言，换这一个模块的实现即可，
//! 不需要引框架——语料就这么点，`rust-i18n` 那种重依赖与 C5 的轻量取向冲突。

/// 越界被拦下的那句（D30 §8）。`detail` 由内核给，必须是"下一步能做什么"，
/// 而不是策略键——`Blocked` 的两个来源共用这一处，不许各写一份。
pub fn blocked(detail: &str) -> String {
    let head = "这个我现在没有权限做，已经记录下来了。";
    let detail = detail.trim();
    if detail.is_empty() {
        head.to_owned()
    } else {
        format!("{head}\n\n{detail}")
    }
}

/// 未绑定 / 未授权：只说"找管理员"，不提配置键。
pub const NOT_ALLOWED: &str = "⛔ 这个账号还没有被允许使用，请联系管理员。";

/// 模型服务商那边的额度用尽（D30 §4）：不写"配额周期"以外的实现词。
pub fn quota_exhausted(wait: &str, detail: &str) -> String {
    format!("⛔ 服务额度已经用完，这次任务停下了（不会自动重试）。{wait}\n\n{detail}")
}

/// 服务商没通过验证（key 失效/过期）。
pub const AUTH_FAILED: &str = "🔑 服务商那边没有通过验证（登录信息可能过期了），已停止。";

/// 一轮的内容超过模型一次能处理的量：给出"放文件里"的下一步，不甩 400。
pub const TOO_LONG: &str = "这一轮的内容太长，超出一次能处理的量，已经停下了。\n\
下一步：把长材料放进工作区，让我按文件去读，而不是整段贴进来。";

/// 任务卡住 / 超时：不写 `wall_clock_ms` / `max_turns` 这类配置名。
pub const STALLED: &str = "⏱ 任务卡住了：好几分钟没有任何进展，已经取消。可以换个说法再试。";
pub const OUT_OF_TIME: &str = "⏱ 这次做的时间太长了，已经停下。把任务拆小一点再试。";

/// `/usage` 的金额一栏：没配价格时别假装免费，也别甩配置键。
pub const MONEY_UNKNOWN: &str = "金额未统计";

/// 审批：只有更高权限的账号能批（D30 §2）。
pub const NOT_APPROVER: &str = "这个需要更高权限的账号来确认。";

/// 审批卡首行。
pub const APPROVAL_ASK: &str = "🔐 需要你确认";

pub const APPROVED: &str = "✅ 已允许（记录见账本）";
pub const DENIED: &str = "❌ 已拒绝";
pub const APPROVED_SHORT: &str = "✅ 已批准。";
pub const DENIED_SHORT: &str = "❌ 已拒绝。";
pub const ALREADY_RESOLVED: &str = "这条已经处理过了。";
pub const APPROVAL_MISSING: &str = "审批不存在或已过期";

/// 命令条目提案落盘失败 / 缺少安装器时的补充说明。
pub const NO_INSTALLER: &str = "（当前版本没有打开这个功能，未做改动）";
pub const PROPOSAL_GONE: &str = "⚠️ 这条提案已经不在了，没有做任何改动。";
pub const PROPOSAL_CHANGED: &str = "⚠️ 这条提案在确认之前被改过，出于安全考虑没有安装。";
pub const PROPOSAL_NO_NAME: &str = "⚠️ 这条提案缺少名称，未做改动。";
pub const PROPOSAL_DISCARDED: &str = "已放弃这条提案，没有生效。";

/// `label` 是提案里那句人话说明（没有就用名字）。
pub fn install_ok(label: &str) -> String {
    format!("✅ 已生效：{label}")
}

/// 失败原因进日志，不给用户看英文的系统报错。
pub const INSTALL_FAILED: &str = "⚠️ 没有装成，这次的改动没有生效（原因已记在日志里）。";

pub fn remove_ok(label: &str) -> String {
    format!("✅ 已删除：{label}")
}

pub const REMOVE_FAILED: &str = "⚠️ 没有删掉（原因已记在日志里）。";

/// 给 owner 的提案通知（新增/删除一项能力）。`label` 是提案人写的人话说明。
pub fn command_proposal(what: &str, label: &str, id: &str) -> String {
    format!(
        "🔐 有一项新能力等你确认（{what}）：\n\n{label}\n\n\
         同意：/approve {id}\n拒绝：/deny {id}\n\n不确认就不会生效。"
    )
}

/// 定时任务到点遇到需要确认的步骤（D30 §7）。不点名具体工具——那是内部实现。
pub fn scheduled_needs_approval() -> String {
    "⏰ 有条定时任务要做的某一步需要你确认，但到点没人能确认，这一步已经跳过了。\n\n\
     把它改成不需要确认的做法，或者等你在线时手动做一次。"
        .to_owned()
}

/// 定时任务整轮没跑成。
pub fn scheduled_outcome(title: &str, what: &str) -> String {
    let title: String = title.chars().take(40).collect();
    format!("⏰ 定时任务「{title}」{what}。\n\n查看或取消：/reminders")
}

/// 有人建了"到点会自动处理"的任务时给 owner 的通知。
pub fn member_model_reminder(text: &str, next: &str) -> String {
    format!(
        "⏰ 有人设了一个到点自动处理的任务：{text}\n\n\
         下次 {next}；处理时用的是对方自己的权限。\n不需要的话，用 /reminders 取消。"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples() -> Vec<String> {
        vec![
            blocked(""),
            blocked("没有这个命令条目：x"),
            NOT_APPROVER.to_owned(),
            APPROVAL_ASK.to_owned(),
            APPROVED.to_owned(),
            DENIED.to_owned(),
            ALREADY_RESOLVED.to_owned(),
            APPROVAL_MISSING.to_owned(),
            NO_INSTALLER.to_owned(),
            PROPOSAL_GONE.to_owned(),
            PROPOSAL_CHANGED.to_owned(),
            PROPOSAL_DISCARDED.to_owned(),
            install_ok("打印文件"),
            INSTALL_FAILED.to_owned(),
            remove_ok("打印文件"),
            REMOVE_FAILED.to_owned(),
            command_proposal("新增", "打印文件，纸型 A4/照片", "appr-1"),
            scheduled_needs_approval(),
            scheduled_outcome("看看老师消息", "被权限挡住"),
            member_model_reminder("看看老师消息", "2026-11-03 08:00"),
            NOT_ALLOWED.to_owned(),
            AUTH_FAILED.to_owned(),
            STALLED.to_owned(),
            OUT_OF_TIME.to_owned(),
            MONEY_UNKNOWN.to_owned(),
            quota_exhausted("约 5 分钟后可再试", "upstream said: quota"),
        ]
    }

    /// 文案规则（`GLOSSARY.md` §G.9）：不写身份称谓、不写策略键、不把手滑写成 shell。
    #[test]
    fn copy_is_neutral() {
        for s in samples() {
            for bad in [
                "主人",
                "家人",
                "爸爸",
                "妈妈",
                "[policy]",
                "who =",
                "sh -lc",
                "sudo",
                // 不懂计算机的人读不懂的实现名词（`/xxx` 这类指令除外）
                "service",
                "http",
                "exec",
                "read",
                "write",
                "commands.d",
                "policy",
                "token",
                "argv",
                "JSON",
                "MCP",
                "沙盒",
                "指纹",
                "权限点",
            ] {
                assert!(!s.contains(bad), "文案里不该出现 {bad:?}：{s}");
            }
        }
    }

    #[test]
    fn blocked_keeps_head_when_no_detail() {
        assert_eq!(blocked(""), "这个我现在没有权限做，已经记录下来了。");
        assert!(blocked("缺 x").starts_with("这个我现在没有权限做"));
        assert!(blocked("缺 x").ends_with("缺 x"));
    }

    #[test]
    fn scheduled_outcome_truncates_title() {
        let long = "很".repeat(100);
        let s = scheduled_outcome(&long, "失败");
        assert!(s.contains("失败"));
        assert!(s.chars().count() < 140, "{s}");
    }
}
