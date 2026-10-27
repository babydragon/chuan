//! `remember`：把"关于这个家的结论"写进长期记忆（doc 容器）。

use async_trait::async_trait;
use harness_kernel::{
    Block, DocWrite, DocOp, DocId, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};

pub struct RememberTool;

fn fnv(bytes: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 同一个 (scope, subject, predicate) 永远得到同一个 doc id → upsert 覆盖。
pub fn fact_id(scope: &str, subject: &str, predicate: &str) -> DocId {
    DocId(format!("fact-{:016x}", fnv(&format!("{scope}|{subject}|{predicate}"))))
}

#[async_trait]
impl Tool for RememberTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "remember".into(),
            description: "把一条**长期有效**的事实写进记忆（例如打印机型号、孩子的班级）。\
只记结论，不要记流水账；不要记密码、令牌等敏感信息。".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "subject": { "type": "string", "description": "主体，如 打印机 / 女儿" },
                    "predicate": { "type": "string", "description": "属性，如 型号 / 班级" },
                    "value": { "type": "string" },
                    "scope": { "type": "string", "enum": ["family", "user"], "default": "family" }
                },
                "required": ["subject", "predicate", "value"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::Idempotent { key: "remember".into() }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let get = |k: &str| {
            args.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let subject = get("subject").ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidArgs, "需要 subject", false)
        })?;
        let predicate = get("predicate").ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidArgs, "需要 predicate", false)
        })?;
        let value = get("value").ok_or_else(|| {
            ToolError::new(ErrorCode::InvalidArgs, "需要 value", false)
        })?;
        let scope = get("scope").unwrap_or_else(|| "family".into());

        let id = fact_id(&scope, &subject, &predicate);
        let doc = DocWrite {
            id: id.clone(),
            kind: "fact".into(),
            scope: scope.clone(),
            payload: serde_json::json!({
                "subject": subject, "predicate": predicate, "value": value,
                "confidence": "observed",
                "source_call": ctx.call.0,
            }),
            version: 1,
            target: None,
            op: DocOp::Upsert,
        };
        Ok(ToolOutput {
            content: vec![Block::Paragraph(format!(
                "已记住（{scope}）：{subject} · {predicate} = {value}  [{}]",
                &id.0[..id.0.len().min(14)]
            ))],
            doc_writes: vec![doc],
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_kernel::{ConversationId, RunId, Workspace};

    #[test]
    fn fact_id_is_stable_and_scope_aware() {
        let a = fact_id("family", "打印机", "型号");
        let b = fact_id("family", "打印机", "型号");
        let c = fact_id("user:1", "打印机", "型号");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[tokio::test]
    async fn execute_returns_doc_write() {
        let env = crate::NullEnv;
        let ws = Workspace { run: RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            conv: ConversationId("c".into()),
            actor: "42",
            outbox: &harness_kernel::NullOutbox,
            docs: &harness_kernel::NullDocReader,
            run: RunId("r".into()),
            call: harness_kernel::CallId("call-1".into()),
            workspace: &ws,
            env: &env,
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let out = RememberTool
            .execute(
                serde_json::json!({"subject":"打印机","predicate":"型号","value":"HP M15"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(out.doc_writes.len(), 1);
        assert_eq!(out.doc_writes[0].kind, "fact");
        assert_eq!(out.doc_writes[0].scope, "family");
        assert_eq!(out.doc_writes[0].payload["value"], "HP M15");
    }
}
