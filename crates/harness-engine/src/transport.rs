//! 传输抽象：只搬运字节，不认识任何协议。P0 用脚本化传输做离线验证。

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use harness_kernel::EngineError;

#[derive(Clone, Debug, Default)]
pub struct HttpRequest {
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

#[async_trait]
pub trait SseBytes: Send {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, EngineError>>;
}

#[async_trait]
pub trait SseTransport: Send + Sync {
    async fn open(&self, req: HttpRequest) -> Result<Box<dyn SseBytes>, EngineError>;
}

/// 按脚本回放；记录收到的请求供测试断言。
#[derive(Default)]
pub struct ScriptedTransport {
    responses: Mutex<VecDeque<Vec<Vec<u8>>>>,
    pub requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    pub fn new(responses: Vec<Vec<Vec<u8>>>) -> Self {
        Self { responses: Mutex::new(responses.into()), requests: Mutex::new(Vec::new()) }
    }
}

struct ScriptedBytes {
    chunks: VecDeque<Vec<u8>>,
}

#[async_trait]
impl SseBytes for ScriptedBytes {
    async fn next_chunk(&mut self) -> Option<Result<Vec<u8>, EngineError>> {
        self.chunks.pop_front().map(Ok)
    }
}

#[async_trait]
impl SseTransport for ScriptedTransport {
    async fn open(&self, req: HttpRequest) -> Result<Box<dyn SseBytes>, EngineError> {
        self.requests.lock().unwrap().push(req);
        let chunks = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| EngineError::Provider("no scripted response left".into()))?;
        Ok(Box::new(ScriptedBytes { chunks: chunks.into() }))
    }
}
