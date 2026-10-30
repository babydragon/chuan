//! `remember`：把"关于这个家的结论"写进长期记忆（doc 容器）。

use async_trait::async_trait;
use harness_kernel::{
    Block, DocWrite, DocOp, DocId, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};

pub struct RememberTool {
    /// 身份表：用来把"人"解析成跨通道稳定的 user id（D26）。
    /// 单测/子 agent 没有身份表时退化成地址本身。
    identity: Option<std::sync::Arc<harness_kernel::Directory>>,
}

impl Default for RememberTool {
    fn default() -> Self {
        Self { identity: None }
    }
}

impl RememberTool {
    pub fn with_identity(mut self, d: std::sync::Arc<harness_kernel::Directory>) -> Self {
        self.identity = Some(d);
        self
    }

    /// `user` scope 一律写成 `user:<id>`，且 id 与通道无关：
    /// 同一个人在 TG / 微信 / 两个 TG 号上看到的是同一份个人记忆。
    fn user_scope(&self, ctx: &ToolCtx<'_>) -> String {
        let channel = ctx.conv.0.split(':').next().unwrap_or_default();
        match &self.identity {
            Some(d) => format!("user:{}", d.resolve(channel, ctx.actor)),
            None => format!("user:{}", ctx.actor),
        }
    }
}

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
                    "scope": { "type": "string", "enum": ["family", "user"], "default": "family",
                        "description": "全家都适用的写 family；只跟某一个人有关的写 user（系统会自动关联到人，跨通道共享）" }
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
        // 注意：`user` 不能直接当 scope 用——记忆注入按 `user:<id>` 查，
        // 直接写 "user" 会写进一个永远读不回来的作用域（旧实现的 bug）。
        let scope = match get("scope").as_deref() {
            None | Some("family") => "family".to_string(),
            Some("user") => self.user_scope(ctx),
            Some(other) => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("scope 只能是 family / user，收到 {other}"),
                    false,
                ))
            }
        };

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

    /// `scope=user` 必须落成 `user:<user id>`，且 id 与通道无关（D26）。
    #[tokio::test]
    async fn user_scope_maps_to_user_id() {
        let dir = std::sync::Arc::new(harness_kernel::Directory {
            users: vec![harness_kernel::User {
                id: "owner".into(),
                name: None,
                channels: vec![
                    harness_kernel::Binding::parse("telegram:42").unwrap(),
                    harness_kernel::Binding::parse("weixin:abc@im.wechat").unwrap(),
                ],
            }],
        });
        let tool = RememberTool::default().with_identity(dir);
        let env = crate::NullEnv;
        let ws = Workspace { run: RunId("r".into()), root: "/tmp".into() };
        for (conv, actor) in [("telegram:42:0", "42"), ("weixin:abc@im.wechat:0", "abc@im.wechat")] {
            let ctx = ToolCtx {
                system_prompt: "",
                spawner: None,
                conv: ConversationId(conv.into()),
                actor,
                outbox: &harness_kernel::NullOutbox,
                docs: &harness_kernel::NullDocReader,
                run: RunId("r".into()),
                call: harness_kernel::CallId("call-1".into()),
                workspace: &ws,
                env: &env,
                cancel: tokio_util::sync::CancellationToken::new(),
            };
            let out = tool
                .execute(
                    serde_json::json!({"subject":"我","predicate":"过敏","value":"花生","scope":"user"}),
                    &ctx,
                )
                .await
                .unwrap();
            assert_eq!(out.doc_writes[0].scope, "user:owner", "conv={conv}");
        }
    }

    #[tokio::test]
    async fn execute_returns_doc_write() {
        let env = crate::NullEnv;
        let ws = Workspace { run: RunId("r".into()), root: "/tmp".into() };
        let ctx = ToolCtx {
            system_prompt: "",
            spawner: None,
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
        let out = RememberTool::default()
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
