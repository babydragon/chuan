//! 模型连通性探针：一次真实流式请求，打印增量与终稿。
//!
//! 环境变量：LLM_BASE_URL、LLM_MODEL、LLM_KEY_FILE（或 LLM_API_KEY）

use std::sync::Arc;

use harness_engine::{ModelDriver, OpenAiChat, ReqwestTransport};
use harness_kernel::{
    ConversationId, EventSink, Message, Role, RunId, Step, StreamEvent, TurnDriver, TurnSpec,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = std::env::var("LLM_BASE_URL")?;
    let model = std::env::var("LLM_MODEL")?;
    let key = match std::env::var("LLM_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            let path = std::env::var("LLM_KEY_FILE")?;
            let p = if let Some(rest) = path.strip_prefix("~/") {
                format!("{}/{rest}", std::env::var("HOME")?)
            } else {
                path
            };
            std::fs::read_to_string(&p)?.trim().to_owned()
        }
    };

    let transport = Arc::new(ReqwestTransport::new(&base, key)?);
    let mut driver = ModelDriver::new(transport, Arc::new(OpenAiChat), model.clone());
    if std::env::var("LLM_SESSION_HEADER").ok().as_deref() == Some("1") {
        driver = driver.with_header("x-opencode-session", "chuan-probe");
    }

    let pair = EventSink::channel(256);
    let sink = pair.sink;
    let mut rx = pair.rx;
    let started = std::time::Instant::now();
    let printer = tokio::spawn(async move {
        let mut text = String::new();
        let mut tools = 0;
        let mut first: Option<std::time::Duration> = None;
        while let Some(ev) = rx.recv().await {
            match ev {
                StreamEvent::TextDelta(t) => {
                    if first.is_none() {
                        first = Some(started.elapsed());
                    }
                    print!("{t}");
                    text.push_str(&t);
                }
                StreamEvent::ReasoningDelta(_) => {}
                StreamEvent::TurnStarted { .. } => {}
                StreamEvent::Restarted => {}
                StreamEvent::ToolStarted { name, .. } => {
                    tools += 1;
                    println!("\n[tool] {name}");
                }
                StreamEvent::ToolFinished { .. } => {}
            }
        }
        (text, tools, first)
    });

    let use_tools = std::env::var("LLM_TOOLS").ok().as_deref() == Some("1");
    let prompt = if use_tools {
        "请调用 exec 工具执行 echo hi，不要只用文字回答。"
    } else {
        "用一句话介绍你自己，并说出你的模型名。"
    };
    let history = vec![Message {
        seq: 1,
        turn: None,
        role: Role::User,
        payload: serde_json::json!({ "text": prompt }),
    }];
    let tools = if use_tools {
        vec![harness_kernel::ToolSpec {
            name: "exec".into(),
            description: "在沙盒里执行命令".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "argv": { "type": "array", "items": { "type": "string" } } },
                "required": ["argv"]
            }),
        }]
    } else {
        Vec::new()
    };

    let mut turn = driver
        .begin(
            TurnSpec {
                conv: ConversationId("probe".into()),
                run: RunId("probe-1".into()),
                model: model.clone(),
                max_turns: 4,
                wall_clock_ms: 60_000,
                system_prompt: std::env::var("LLM_SYSTEM").ok(),
                images: Vec::new(),
            },
            &history,
            &tools,
            sink,
        )
        .await?;

    let step = turn.step().await?;
    let usage = turn.usage();
    drop(turn);

    let (streamed, tools, first) = printer.await?;
    println!(
        "\n---\nmodel={model}\nttft={:?} total={:?}\nstreamed_chars={} tool_events={tools}",
        first,
        started.elapsed(),
        streamed.chars().count()
    );
    println!("usage={:?}", usage.records.iter().map(|u| (u.prompt_tokens, u.completion_tokens)).collect::<Vec<_>>());
    match step {
        Step::Done { text, finish } => println!("finish={finish:?} final_chars={}", text.chars().count()),
        Step::Tools { calls, .. } => {
            println!("tool_calls={}", calls.len());
            for c in calls {
                println!("  name={} args={}", c.name, c.args);
            }
        }
        Step::Model { turn_no } => println!("unexpected model step {turn_no}"),
    }
    Ok(())
}
