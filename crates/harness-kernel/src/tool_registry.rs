use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::{Tool, ToolSpec};

/// 运行时装配的工具表。按角色裁剪、按名字覆盖都在这一层做。
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.spec().name, tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    /// 全量清单（主会话用）。顺序 = `(tier, name)`。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.specs_for(u8::MAX)
    }

    /// 按档位裁剪：只保留 `tier <= max_tier`。
    ///
    /// 排序键是 `(tier, name)` 而不是 `name`——这样裁掉高 tier 就等于**削尾部**，
    /// 剩下的数组与全量清单逐字节同前缀，provider 的 prompt cache 才不会全废。
    pub fn specs_for(&self, max_tier: u8) -> Vec<ToolSpec> {
        let mut v: Vec<(u8, ToolSpec)> = self
            .tools
            .values()
            .filter(|t| t.tier() <= max_tier)
            .map(|t| (t.tier(), t.spec()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        v.into_iter().map(|(_, s)| s).collect()
    }

    /// 只保留 `tier <= max_tier` 的工具（子 agent 用）。
    pub fn filtered(&self, max_tier: u8) -> ToolRegistry {
        let tools = self
            .tools
            .iter()
            .filter(|(_, t)| t.tier() <= max_tier)
            .map(|(k, t)| (k.clone(), Arc::clone(t)))
            .collect();
        ToolRegistry { tools }
    }

    /// 只保留点名的工具（profile 的 `tools`）。名字不存在就忽略，不报错——
    /// 技能/MCP 工具会随配置变，硬报错会让 profile 变脆。
    pub fn only(&self, names: &[String]) -> ToolRegistry {
        let tools = names
            .iter()
            .filter_map(|n| self.tools.get(n).map(|t| (n.clone(), Arc::clone(t))))
            .filter(|(_, t)| t.tier() == 0)
            .collect();
        ToolRegistry { tools }
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::EffectClass;
    use crate::error::ToolError;
    use crate::tool::{ToolCtx, ToolOutput};
    use async_trait::async_trait;

    struct Stub {
        name: &'static str,
        tier: u8,
    }

    #[async_trait]
    impl Tool for Stub {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.to_owned(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        fn effect(&self) -> EffectClass {
            EffectClass::Pure
        }
        fn tier(&self) -> u8 {
            self.tier
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolCtx<'_>,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::default())
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        for (name, tier) in [("zeta", 0), ("beta", 0), ("delegate", 1), ("yank", 1), ("alpha", 0)] {
            r.register(Arc::new(Stub { name, tier }));
        }
        r
    }

    #[test]
    fn sorts_by_tier_then_name() {
        let names: Vec<String> = registry().specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, ["alpha", "beta", "zeta", "delegate", "yank"]);
    }

    /// 裁剪必须是"削尾部"：tier 0 的清单与全量清单的前 N 项逐项相同。
    #[test]
    fn trimming_keeps_prefix_stable() {
        let all = registry().specs();
        let trimmed = registry().specs_for(0);
        assert_eq!(trimmed.len(), 3);
        for (a, b) in trimmed.iter().zip(all.iter()) {
            assert_eq!(a.name, b.name);
            assert_eq!(a.description, b.description);
        }
    }
}
