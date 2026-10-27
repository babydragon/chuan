//! 检索端口：只负责"给定查询返回相关文档 id"，不负责注入。

use async_trait::async_trait;

use crate::ids::DocId;

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub text: String,
    pub scopes: Vec<String>,
    pub kinds: Vec<String>,
    pub k: usize,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub doc_id: DocId,
    pub score: f32,
}

#[async_trait]
pub trait Retriever: Send + Sync + 'static {
    async fn search(&self, q: Query) -> Result<Vec<Hit>, String>;
}

/// 技能目录：只暴露一段可注入的文本，内核不解析技能格式。
pub trait SkillSource: Send + Sync {
    fn catalog(&self) -> String;
}
