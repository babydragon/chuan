//! 长轮询游标持久化：接口在通道层，实现由应用层提供。

use async_trait::async_trait;

#[async_trait]
pub trait OffsetStore: Send + Sync {
    async fn get(&self, key: &str) -> Option<String>;
    async fn set(&self, key: &str, value: &str);
}

/// 内存实现：测试与独立通道用。
#[derive(Default)]
pub struct MemoryOffsets {
    value: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

#[async_trait]
impl OffsetStore for MemoryOffsets {
    async fn get(&self, key: &str) -> Option<String> {
        self.value.lock().unwrap().get(key).cloned()
    }
    async fn set(&self, key: &str, value: &str) {
        self.value
            .lock()
            .unwrap()
            .insert(key.to_owned(), value.to_owned());
    }
}
