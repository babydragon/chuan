//! 用 Store 的 kv 持久化 Telegram offset。

use std::sync::Arc;

use async_trait::async_trait;
use harness_channels::telegram::OffsetStore;
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
    async fn get(&self, _key: &str) -> Option<i64> {
        self.store
            .kv_get(&self.key)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
    }

    async fn set(&self, _key: &str, value: i64) {
        if let Err(e) = self.store.kv_set(&self.key, &value.to_string()).await {
            tracing::warn!(error = %e, "persist offset failed");
        }
    }
}

#[allow(dead_code)]
fn _unused(_: &mut dyn OffsetStore) {}
