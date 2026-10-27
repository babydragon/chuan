//! 运行时选择驱动：echo（离线）/ 真实 OpenAI 兼容。

use std::sync::Arc;

use async_trait::async_trait;
use harness_engine::{EchoDriver, ModelDriver, OpenAiChat, ReqwestTransport};
use harness_kernel::{
    EngineError, EventSink, Message, ToolSpec, Turn, TurnDriver, TurnSpec,
};

pub enum AnyDriver {
    Echo(EchoDriver),
    Model(ModelDriver<ReqwestTransport>),
}

#[async_trait]
impl TurnDriver for AnyDriver {
    async fn begin(
        &self,
        spec: TurnSpec,
        history: &[Message],
        tools: &[ToolSpec],
        sink: EventSink,
    ) -> Result<Box<dyn Turn>, EngineError> {
        match self {
            AnyDriver::Echo(d) => d.begin(spec, history, tools, sink).await,
            AnyDriver::Model(d) => d.begin(spec, history, tools, sink).await,
        }
    }
}

/// provider 预设：base_url / 默认模型 / key 文件名
pub struct Preset {
    pub base_url: &'static str,
    pub model: &'static str,
    pub key_file: &'static str,
}

pub fn preset(name: &str) -> Option<Preset> {
    match name {
        "zai" => Some(Preset {
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            model: "glm-5.3-flash",
            key_file: "zai_key",
        }),
        "opencode-go" => Some(Preset {
            base_url: "https://opencode.ai/zen/go/v1",
            model: "deepseek-v4.1-flash",
            key_file: "opencode_go_key",
        }),
        _ => None,
    }
}

pub fn build(
    name: &str,
    base_url: Option<&str>,
    model: Option<&str>,
    api_key: &str,
    proxy: Option<&str>,
    max_retries: u32,
    backoff_ms: u64,
) -> Result<AnyDriver, String> {
    if name == "echo" {
        return Ok(AnyDriver::Echo(EchoDriver));
    }
    let p = preset(name).ok_or_else(|| format!("unknown provider: {name}"))?;
    let base = base_url.unwrap_or(p.base_url).to_owned();
    let model = model.unwrap_or(p.model).to_owned();
    let transport =
        Arc::new(ReqwestTransport::new_with_proxy(&base, api_key, proxy).map_err(|e| e.to_string())?);
    let mut driver = ModelDriver::new(transport, Arc::new(OpenAiChat), model)
        .with_retry(harness_engine::driver::RetryPolicy {
            max_attempts: max_retries.max(1),
            base_backoff_ms: backoff_ms.max(50),
        });
    // opencode-go 需要会话路由头，否则 400 MissingSessionID
    if name == "opencode-go" {
        // 绑会话（不是进程）：跨 turn 命中同一后端与缓存
        driver = driver.with_session_header("x-opencode-session");
    }
    println!("provider={name} base={base}");
    Ok(AnyDriver::Model(driver))
}
