use std::collections::HashMap;
use std::sync::Arc;

use crate::tool::{Tool, ToolSpec};

/// 运行时装配的工具表。按角色裁剪、按名字覆盖都在这一层做（P0 只做装配）。
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

    /// 稳定顺序，保证请求前缀稳定（prompt cache 友好）。
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut v: Vec<ToolSpec> = self.tools.values().map(|t| t.spec()).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
