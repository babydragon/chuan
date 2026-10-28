//! `task`：父 agent 管理自己派出去的后台任务（P4.3）。
//!
//! 为什么要有这个工具：`delegate(background=true)` 之后父只能干等，
//! 用户问"怎么样了"它只能编。现在它可以自己查、自己停——但**禁止轮询**：
//! 任务完成会推消息，查一次两次足够。
//!
//! 边界：
//! - `tier = 1`：子 agent 拿不到（它自己就是被管理的一方）；
//! - 只列/只停**自己这个实例**派出的任务，不跨用户；
//! - 前台委托是阻塞的，模型在被阻塞时无法调用任何工具——那只能靠 `/cancel`
//!   或角色的 `wall_ms`，这个工具管不到。

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

const MAX_LIST: usize = 10;

pub struct TaskTool;

fn short(s: &str, n: usize) -> String {
    let one: String = s.lines().next().unwrap_or("").chars().take(n).collect();
    one
}

#[async_trait]
impl Tool for TaskTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".into(),
            description: "查看/取消你自己派出去的后台任务。\n\
                用法：list（最近几条）、status（看某一条的结论）、cancel（停掉它）。\n\
                不要轮询：任务完成会主动推消息；一次对话查一两次足够了。\n\
                想停正在前台跑的任务，只能让主人发 /cancel——你被阻塞时调不了任何工具。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "status", "cancel"] },
                    "id": { "type": "string", "description": "任务号（task-…），status/cancel 必填" }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 取消是有状态的，崩溃后不自动重放
        EffectClass::External
    }

    fn tier(&self) -> u8 {
        1
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(spawner) = ctx.spawner else {
            return Err(ToolError::new(
                ErrorCode::NotPermitted,
                "当前运行不能管理任务（子 agent 与定时任务都不行）",
                false,
            ));
        };
        let action = args["action"].as_str().unwrap_or("");
        let id = args["id"].as_str().unwrap_or("").trim().to_owned();

        let text = match action {
            "list" => {
                let tasks = spawner
                    .tasks()
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
                if tasks.is_empty() {
                    "没有后台任务。".to_owned()
                } else {
                    let mut out = String::new();
                    for t in tasks.iter().rev().take(MAX_LIST) {
                        out.push_str(&format!(
                            "- `{}` · {} · {}\n",
                            t.id,
                            t.status,
                            short(&t.text, 50)
                        ));
                    }
                    out.push_str("\n看结论：task(action=\"status\", id=…)；停掉：task(action=\"cancel\", id=…)");
                    out
                }
            }
            "status" => {
                if id.is_empty() {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "status 需要 id", false));
                }
                let tasks = spawner
                    .tasks()
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
                let Some(t) = tasks.iter().find(|t| t.id == id) else {
                    return Err(ToolError::new(
                        ErrorCode::NotFound,
                        format!("没有这个任务：{id}"),
                        false,
                    ));
                };
                let mut out = format!("`{}` · {}\n任务：{}\n", t.id, t.status, short(&t.text, 200));
                if let Some(r) = &t.result {
                    out.push_str(&format!("\n结论：{r}\n"));
                }
                if let Some(c) = &t.child_conv {
                    out.push_str(&format!("\n完整过程在子会话 `{c}`（主人可用 /agents show 查看）"));
                }
                out
            }
            "cancel" => {
                if id.is_empty() {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "cancel 需要 id", false));
                }
                spawner
                    .cancel_task(&id)
                    .await
                    .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?
            }
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("action 只能是 list/status/cancel，收到 {other}"),
                    false,
                ))
            }
        };

        Ok(ToolOutput {
            content: vec![Block::Paragraph(text)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}
