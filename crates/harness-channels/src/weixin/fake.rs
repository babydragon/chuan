//! 测试替身：可编排入站消息、记录出站文本。

use std::sync::Mutex;

use async_trait::async_trait;

use super::api::{Api, ApiError, Item, Updates, WeixinMessage};

#[derive(Default)]
pub struct FakeApi {
    /// 待返回的批次，按顺序弹出
    pub batches: Mutex<Vec<Updates>>,
    /// 记录：(to, text, client_id)
    pub sent: Mutex<Vec<(String, String, String)>>,
    /// 下一次调用返回的错误（字符串描述）
    pub fail: Mutex<Option<&'static str>>,
    next_id: Mutex<i64>,
}

impl FakeApi {
    pub fn new() -> Self {
        Self { next_id: Mutex::new(1000), ..Default::default() }
    }

    /// 推一条文本入站。
    pub fn push_text(&self, message_id: i64, from: &str, text: &str, ctx: &str, at_ms: i64) {
        self.push_items(
            message_id,
            from,
            vec![Item { kind: 1, text_item: Some(super::api::TextItem { text: text.into() }), voice_item: None }],
            ctx,
            at_ms,
        );
    }

    pub fn push_items(
        &self,
        message_id: i64,
        from: &str,
        item_list: Vec<Item>,
        ctx: &str,
        at_ms: i64,
    ) {
        let mut g = self.batches.lock().unwrap();
        if g.is_empty() {
            g.push(Updates::default());
        }
        g[0].msgs.push(WeixinMessage {
            seq: message_id,
            message_id,
            from_user_id: from.into(),
            create_time_ms: at_ms,
            context_token: ctx.into(),
            item_list,
        });
    }

    pub fn sent_texts(&self) -> Vec<String> {
        self.sent.lock().unwrap().iter().map(|(_, t, _)| t.clone()).collect()
    }
}

#[async_trait]
impl Api for FakeApi {
    async fn get_updates(&self, _cursor: &str, _timeout_ms: i64) -> Result<Updates, ApiError> {
        if let Some(kind) = self.fail.lock().unwrap().take() {
            return match kind {
                "expired" => Err(ApiError::SessionExpired),
                other => Err(ApiError::Transport(other.into())),
            };
        }
        let next = {
            let mut g = self.batches.lock().unwrap();
            if g.is_empty() {
                None
            } else {
                Some(g.remove(0))
            }
        };
        match next {
            Some(u) => Ok(u),
            None => {
                // 模拟长轮询空转，别把测试转成忙等
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                Ok(Updates::default())
            }
        }
    }

    async fn send_text(
        &self,
        to: &str,
        _ctx: &str,
        text: &str,
        client_id: &str,
    ) -> Result<String, ApiError> {
        self.sent
            .lock()
            .unwrap()
            .push((to.to_owned(), text.to_owned(), client_id.to_owned()));
        let mut id = self.next_id.lock().unwrap();
        *id += 1;
        Ok(id.to_string())
    }
}
