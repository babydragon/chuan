//! 测试替身：可编排更新、记录发送/编辑、可注入 429。

use std::sync::{Mutex, atomic::{AtomicI64, Ordering}};

use async_trait::async_trait;
use harness_kernel::{EditOutcome, MsgRef};

use super::api::{Api, ApiError, Update};

#[derive(Default)]
pub struct FakeApi {
    pub updates: Mutex<Vec<Update>>,
    /// 发出去的文件名（sendDocument / sendPhoto）
    pub files: Mutex<Vec<String>>,
    pub sent: Mutex<Vec<(i64, String)>>,
    pub edited: Mutex<Vec<(i64, String)>>,
    pub deny_edit: Mutex<bool>,
    /// 下一次调用返回 429
    pub rate_limit_once: Mutex<Option<u32>>,
    next_msg_id: AtomicI64,
}

impl FakeApi {
    pub fn new() -> Self {
        Self { next_msg_id: AtomicI64::new(100), ..Default::default() }
    }

    pub fn push_update(&self, u: Update) {
        self.updates.lock().unwrap().push(u);
    }
}

#[async_trait]
impl Api for FakeApi {
    async fn get_me(&self) -> Result<String, ApiError> {
        Ok("fake_bot".into())
    }

    async fn get_updates(&self, _offset: i64, _timeout_s: u32) -> Result<Vec<Update>, ApiError> {
        if let Some(ms) = self.rate_limit_once.lock().unwrap().take() {
            return Err(ApiError::RetryAfter(ms));
        }
        let out = {
            let mut g = self.updates.lock().unwrap();
            g.drain(..).collect::<Vec<_>>()
        };
        if out.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(out)
    }

    async fn send_message(&self, chat: i64, text: &str, _html: bool) -> Result<MsgRef, ApiError> {
        self.sent.lock().unwrap().push((chat, text.to_owned()));
        let id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        Ok(MsgRef(id.to_string()))
    }

    async fn edit_message(
        &self,
        chat: i64,
        _msg: &MsgRef,
        text: &str,
        _html: bool,
    ) -> Result<EditOutcome, ApiError> {
        if *self.deny_edit.lock().unwrap() {
            return Ok(EditOutcome::TooOld);
        }
        self.edited.lock().unwrap().push((chat, text.to_owned()));
        Ok(EditOutcome::Edited)
    }

    async fn answer_callback(&self, _id: &str, _text: &str) -> Result<(), ApiError> {
        Ok(())
    }

    async fn set_my_commands(&self, _commands: &[(String, String)]) -> Result<(), ApiError> {
        Ok(())
    }

    async fn send_chat_action(&self, _chat: i64, _action: &str) -> Result<(), ApiError> {
        Ok(())
    }

    async fn delete_message(&self, _chat: i64, _msg: &MsgRef) -> Result<(), ApiError> {
        Ok(())
    }

    async fn edit_message_with_buttons(
        &self,
        _chat: i64,
        _msg: &MsgRef,
        text: &str,
        _buttons: &[Vec<(String, String)>],
    ) -> Result<(), ApiError> {
        self.edited.lock().unwrap().push((0, text.to_owned()));
        Ok(())
    }

    async fn send_document(
        &self,
        _chat: i64,
        _path: &std::path::Path,
        name: &str,
        _mime: &str,
        _caption: &str,
    ) -> Result<MsgRef, ApiError> {
        self.files.lock().unwrap().push(name.to_owned());
        Ok(MsgRef("doc-1".into()))
    }

    async fn send_photo(
        &self,
        _chat: i64,
        path: &std::path::Path,
        _caption: &str,
    ) -> Result<MsgRef, ApiError> {
        self.files
            .lock()
            .unwrap()
            .push(path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default());
        Ok(MsgRef("photo-1".into()))
    }

    async fn download_file(&self, _file_id: &str) -> Result<Vec<u8>, ApiError> {
        Ok(b"fake-media".to_vec())
    }

    async fn remove_buttons(&self, _chat: i64, _msg: &MsgRef) -> Result<(), ApiError> {
        Ok(())
    }

    async fn edit_card(&self, _chat: i64, _msg: &MsgRef, _text: &str) -> Result<(), ApiError> {
        Ok(())
    }

    async fn send_card(
        &self,
        chat: i64,
        text: &str,
        _buttons: &[Vec<(String, String)>],
    ) -> Result<MsgRef, ApiError> {
        self.sent.lock().unwrap().push((chat, text.to_owned()));
        let id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        Ok(MsgRef(id.to_string()))
    }
}
