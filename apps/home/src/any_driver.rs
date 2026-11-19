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

/// provider 预设：base_url / 默认模型。
///
/// 空串 = **必须由配置提供**（通用 OpenAI 兼容入口就是这种）。
/// key 文件不在这里：统一走 `[engine] api_key_file`，否则两处真相。
pub struct Preset {
    pub base_url: &'static str,
    pub model: &'static str,
}

pub fn preset(name: &str) -> Option<Preset> {
    match name {
        "zai" => Some(Preset {
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            model: "glm-5.3-flash",
        }),
        "opencode-go" => Some(Preset {
            base_url: "https://opencode.ai/zen/go/v1",
            model: "deepseek-v4.1-flash",
        }),
        // 通用 OpenAI 兼容端点：厂商官方、云厂商兼容模式、第三方中转站都用它。
        // 不绑死任何厂商专属头，base_url 与 model 必须由配置给（见 build 的校验）。
        "openai" => Some(Preset { base_url: "", model: "" }),
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
    let p = preset(name).ok_or_else(|| {
        format!("unknown provider: {name}（可选 echo / zai / opencode-go / openai + base_url）")
    })?;
    let base = resolve_arg(base_url, p.base_url).ok_or_else(|| {
        format!("provider={name} 需要 [engine] base_url（任意 OpenAI 兼容端点，含 /v1）")
    })?;
    let model = resolve_arg(model, p.model).ok_or_else(|| {
        format!("provider={name} 需要 [engine] model（写成厂商文档里的模型 id）")
    })?;
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

/// 配置值优先；没给就用 preset 的默认值；两者都空 → None（由调用方报错）。
fn resolve_arg(given: Option<&str>, preset_default: &'static str) -> Option<String> {
    given
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| (!preset_default.is_empty()).then(|| preset_default.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 通用入口：不写 base_url / model 时必须给出能照做的提示，而不是 panic 或发空请求。
    #[test]
    fn generic_provider_requires_base_url_and_model() {
        let e = build("openai", None, Some("qwen3.5-max"), "k", None, 1, 100).err().unwrap();
        assert!(e.contains("base_url"), "{e}");
        let e = build("openai", Some("https://example.com/v1"), None, "k", None, 1, 100)
            .err()
            .unwrap();
        assert!(e.contains("model"), "{e}");
        assert!(build(
            "openai",
            Some("https://example.com/v1"),
            Some("qwen3.5-max"),
            "k",
            None,
            1,
            100
        )
        .is_ok());
    }

    /// 内置预设仍然自带 base_url / model，不用配置。
    #[test]
    fn known_presets_need_no_config() {
        assert!(build("zai", Some("https://example.com/v1"), None, "k", None, 1, 100).is_ok());
        assert!(preset("zai").is_some_and(|p| !p.base_url.is_empty() && !p.model.is_empty()));
        assert!(preset("nope").is_none());
    }
}
