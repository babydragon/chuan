use serde::{Deserialize, Serialize};

/// 副作用等级：决定崩溃后能否重跑。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectClass {
    /// 无副作用，可无条件重跑。
    Pure,
    /// 幂等：重跑等价于一次执行；`key` 用于跨重新规划的核对。
    Idempotent { key: String },
    /// 外部副作用：至多一次，永不自动重发。
    External,
}

impl EffectClass {
    pub fn may_retry_after_crash(&self) -> bool {
        matches!(self, EffectClass::Pure | EffectClass::Idempotent { .. })
    }
}
