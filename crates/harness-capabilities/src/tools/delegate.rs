//! `delegate`：派一个子 agent 做独立子任务，只把结论收回来（D19）。
//!
//! 它不是一个"更聪明的工具"，而是一次**上下文隔离**：子 agent 有自己的会话与账本，
//! 父上下文只增加一条结论。所以它的适用面比想象中窄——见 description 里写死的判断标准，
//! 那是给模型看的，不是给人看的。
//!
//! 三个结构性保证（不靠提示词）：
//! - `tier = 1`：子 agent 的工具集里没有它，递归不可能发生；
//! - `ctx.spawner` 为 `None` 时直接报错（定时任务、子 agent 自身的 run）；
//! - 子 run 的审批门对 `ask` 一律拒绝，所以它拿不到 self_modify/install/host_exec。

use async_trait::async_trait;
use harness_kernel::{
    AgentProfile, AgentSpec, Block, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};

const MAX_TASK: usize = 4_000;
const MAX_INPUTS: usize = 20;

pub struct DelegateTool;

#[async_trait]
impl Tool for DelegateTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "delegate".into(),
            description: "把一个独立子任务派给子 agent，只收回结论（过程不进本会话）。\n\
                适合：需要读大量材料/多次试错/多轮排查，而结果只是一段结论或几个文件。\n\
                不适合：一两步就能答的、需要用户确认的、需要发消息/打印/改配置的（子 agent 没有审批权）。
\
                有专门角色时用 role 指定（如写代码用 coder），它决定提示词、模型与可用工具。\n\
                子 agent 看不到本会话历史，所以 task 里必须把前提写全；要它看文件就放进 inputs。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "要它做什么 + 期望的输出格式（它看不到本会话历史）"
                    },
                    "inputs": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "/ws 下已有的文件路径（只传路径，不搬内容）"
                    },
                    "role": {
                        "type": "string",
                        "description": "角色名（配置里的 [agents.roles.<name>]，如 coder / tutor）；不填 = 通用"
                    },
                    "continue_from": {
                        "type": "string",
                        "description": "接手上一次的产出：任务号（task-…）或上个子会话（sub:…#n）。新子 agent 会拿到上次的结论摘要，不必从零开始"
                    },
                    "background": {
                        "type": "boolean",
                        "description": "true = 排成后台任务，立刻返回任务号，结果之后推回本会话（适合几十秒以上的活）"
                    }
                },
                "required": ["task"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 子 agent 会执行命令、可能改文件：崩了不自动重跑。
        EffectClass::External
    }

    /// 只在主会话出现；子 agent 的工具集里没有它 → 结构上禁止递归。
    fn tier(&self) -> u8 {
        1
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let task = args["task"].as_str().unwrap_or_default().trim();
        if task.is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "task 不能为空", false));
        }
        if task.chars().count() > MAX_TASK {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "task 太长，请精简到 4000 字内", false));
        }
        let inputs: Vec<String> = args["inputs"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| s.starts_with("/ws/"))
                    .take(MAX_INPUTS)
                    .map(|s| s.to_owned())
                    .collect()
            })
            .unwrap_or_default();
        // 角色只能**点名**：内容（提示词/模型/工具）全在配置里，模型改不了。
        let profile = args["role"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(|s| AgentProfile { name: s.trim().to_owned(), ..Default::default() })
            .unwrap_or_default();

        let Some(spawner) = ctx.spawner else {
            return Err(ToolError::new(
                ErrorCode::NotPermitted,
                "当前运行不能派生子 agent（子 agent 与定时任务都不能再派生）",
                false,
            ));
        };

        let spec = AgentSpec {
            parent_conv: ctx.conv.0.clone(),
            parent_run: ctx.run.clone(),
            task: task.to_owned(),
            inputs,
            profile,
            system_prompt: Some(ctx.system_prompt.to_owned()),
            max_turns: 0,   // 0 = 用运行时的默认值（由 spawner 填）
            max_tokens: 0,
            wall_ms: 0,
            depth: 1,
            continue_from: args["continue_from"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_owned()),
            child_conv: None,
        };

        if args["background"].as_bool().unwrap_or(false) {
            let id = spawner
                .enqueue(spec)
                .await
                .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(format!(
                    "已排入后台任务 `{id}`。\n它会在你这里安静下来后开始跑，完成后把结论推回本会话。\n查询/取消：/tasks"
                ))],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }

        let conclusion = spawner
            .spawn(spec, ctx.cancel.clone())
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;

        Ok(ToolOutput {
            content: vec![Block::Paragraph(conclusion.render())],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}
