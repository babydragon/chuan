//! 用 Store 的 kv 持久化通道游标（Telegram offset / 微信 get_updates_buf）。

use std::sync::Arc;

use async_trait::async_trait;
use harness_channels::offsets::OffsetStore;
use harness_kernel::Store;
use harness_store::SqliteStore;

pub struct StoreOffsets {
    store: Arc<SqliteStore>,
    key: String,
}

impl StoreOffsets {
    pub fn new(store: Arc<SqliteStore>, key: impl Into<String>) -> Self {
        Self { store, key: key.into() }
    }
}

#[async_trait]
impl OffsetStore for StoreOffsets {
    async fn get(&self, _key: &str) -> Option<String> {
        self.store.kv_get(&self.key).await.ok().flatten()
    }

    async fn set(&self, _key: &str, value: &str) {
        if let Err(e) = self.store.kv_set(&self.key, value).await {
            tracing::warn!(error = %e, "persist offset failed");
        }
    }
}

#[allow(dead_code)]
fn _unused(_: &mut dyn OffsetStore) {}
