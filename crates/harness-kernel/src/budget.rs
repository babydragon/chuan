//! 用量预算（`DECISIONS.md` D20）：按 token 收口，不看钱。
//!
//! 金额要等 `[pricing]` 落地才有意义，而且各家价目随时变；token 数是本地能算准的，
//! 所以闸门先建在 token 上。`0` 一律表示"不限"，避免默认配置把系统锁死。

use crate::ids::Timestamp;
use crate::record::{UsageSplit, UsageTotal};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BudgetLimits {
    /// 本地自然日内允许的 token 总量（prompt+completion）
    pub daily_tokens: u64,
    /// 本地自然月内允许的 token 总量
    pub monthly_tokens: u64,
}

impl BudgetLimits {
    pub fn unlimited(&self) -> bool {
        self.daily_tokens == 0 && self.monthly_tokens == 0
    }
}

/// 超限说明。`None` = 还能跑。
///
/// 文案直接给主人看，所以必须写清"用了多少 / 上限多少 / 改哪个配置"。
pub fn exceeded(
    limits: &BudgetLimits,
    day: &UsageTotal,
    month: &UsageTotal,
    split: &UsageSplit,
) -> Option<String> {
    if limits.daily_tokens > 0 && day.tokens() >= limits.daily_tokens {
        return Some(format!(
            "今日额度已用完（{} / {} token，其中子 agent {}）。\n\
             明天再试，或调大 `[budget] daily_tokens`。",
            day.tokens(),
            limits.daily_tokens,
            split.agent.tokens()
        ));
    }
    if limits.monthly_tokens > 0 && month.tokens() >= limits.monthly_tokens {
        return Some(format!(
            "本月额度已用完（{} / {} token）。\n调大 `[budget] monthly_tokens` 后无需重启。",
            month.tokens(),
            limits.monthly_tokens
        ));
    }
    None
}

/// 预算剩余（`/usage` 用）。`None` = 不限。
pub fn remaining(limit: u64, used: u64) -> Option<u64> {
    (limit > 0).then(|| limit.saturating_sub(used))
}

/// 当前时间窗的起点（本地日 / 本地月）。
pub fn windows(now: Timestamp, tz_min: i32) -> (Timestamp, Timestamp) {
    (
        crate::schedule::local_day_start(now, tz_min),
        crate::schedule::local_month_start(now, tz_min),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn total(p: u64, c: u64) -> UsageTotal {
        UsageTotal { prompt_tokens: p, completion_tokens: c, cost_est: 0.0 }
    }

    #[test]
    fn zero_means_unlimited() {
        let l = BudgetLimits::default();
        assert!(l.unlimited());
        assert!(exceeded(&l, &total(9_999_999, 0), &total(9_999_999, 0), &UsageSplit::default())
            .is_none());
    }

    #[test]
    fn daily_limit_reports_used_and_remedy() {
        let l = BudgetLimits { daily_tokens: 100, monthly_tokens: 0 };
        let split = UsageSplit { agent: total(10, 15), ..Default::default() };
        let msg = exceeded(&l, &total(80, 25), &total(0, 0), &split).unwrap();
        assert!(msg.contains("105 / 100"), "{msg}");
        assert!(msg.contains("子 agent 25"), "{msg}");
        assert!(msg.contains("daily_tokens"), "{msg}");
    }

    #[test]
    fn monthly_limit_only_after_daily_passes() {
        let l = BudgetLimits { daily_tokens: 1_000, monthly_tokens: 50 };
        let msg = exceeded(&l, &total(10, 10), &total(40, 20), &UsageSplit::default()).unwrap();
        assert!(msg.contains("本月"), "{msg}");
        assert!(msg.contains("60 / 50"), "{msg}");
    }

    #[test]
    fn remaining_and_windows() {
        assert_eq!(remaining(0, 5), None);
        assert_eq!(remaining(100, 30), Some(70));
        assert_eq!(remaining(100, 130), Some(0));

        let (d, m) = windows(1_793_000_000_000, 480);
        assert!(m <= d, "月初不可能晚于今日起点");
        // 都落在本地 00:00（+08:00 → UTC 前一天 16:00）
        assert_eq!((d + 8 * 3600 * 1000).rem_euclid(86_400_000), 0);
        assert_eq!((m + 8 * 3600 * 1000).rem_euclid(86_400_000), 0);
    }
}
