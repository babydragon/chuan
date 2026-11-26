//! 投递失败的"留底"：一句短提示存进 doc，等通道恢复后再送到。
//!
//! 背景：通道挂掉时，"失败提示"和正文走的是同一条发送路径，所以提示本身也会失败。
//! 用户那边看到的就是彻头彻尾的静默——2026-11-26 的 502 事故就是这样：
//! run 因发送失败被杀，本该解释失败的那句话也没发出去。
//!
//! 三条规则：
//! 1. **只存提示，不存正文**：正文丢了可以重问；"完全没有反馈"才是真正的问题。
//! 2. **每个会话最多一条**：补发的是"刚才那条没成"，堆积多条只会变成刷屏。
//! 3. **有寿命**：超过 [`MAX_AGE_MS`] 或重试次数用尽就丢弃，只留日志——
//!    两天后再收到"上次那条没跑成"比没有更让人困惑。
//!
//! 这是投递状态，不是结论：真正的状态仍在 Journal；doc 只是"还没送到的一句话"。

use std::collections::HashMap;
use std::sync::Arc;

use crate::channel::Channel;
use crate::ids::{now_ms, ConversationId, DocId, RunId, Timestamp};
use crate::record::{Doc, DocQuery};
use crate::store::Store;

pub const KIND: &str = "notice";
pub const SCOPE: &str = "notice";

/// 单条提示最多尝试多少次（2 分钟一拍 ≈ 40 分钟）；之后丢弃。
pub const MAX_ATTEMPTS: u32 = 20;
/// 超过一天的提示不再补发：太旧的消息补上去只会让人困惑。
pub const MAX_AGE_MS: Timestamp = 24 * 60 * 60 * 1000;

/// doc 容器用的合成会话：提示不属于任何一轮对话。
const OWNER: &str = "__notices__";

/// 一条待补发的提示。
#[derive(Clone, Debug)]
pub struct Pending {
    pub id: DocId,
    pub channel: String,
    pub conversation: String,
    pub text: String,
    pub attempts: u32,
    pub created_at: Timestamp,
    version: u64,
    valid_from: Timestamp,
}

/// 同一会话只用一条 doc：后来的提示覆盖前一条（旧的那句已经过时）。
pub fn doc_id(channel: &str, conversation: &str) -> DocId {
    DocId(format!("notice:{channel}:{conversation}"))
}

/// 从 `ConversationId`（`{channel}:{conversation}:{epoch}`）拆出通道与会话。
///
/// 只用于重启恢复：`open_runs` 只给得到 conv id，而补发要知道发回哪里。
/// 形状不认识就返回 `None`——宁可少发一句，也不要发错人。
pub fn split_conv(conv: &str) -> Option<(&str, &str)> {
    let (head, epoch) = conv.rsplit_once(':')?;
    if epoch.is_empty() || !epoch.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (channel, conversation) = head.split_once(':')?;
    if channel.is_empty() || conversation.is_empty() {
        return None;
    }
    Some((channel, conversation))
}

/// 排一条提示。失败只记日志：连"留底"都写不进去时，没有更好的办法。
pub async fn enqueue(store: &dyn Store, channel: &str, conversation: &str, text: &str) -> bool {
    if channel.is_empty() || conversation.is_empty() || text.trim().is_empty() {
        return false;
    }
    let id = doc_id(channel, conversation);
    let version = match store.begin_read().await {
        Ok(mut rd) => rd
            .get_doc(id.clone())
            .await
            .ok()
            .flatten()
            .map(|d| d.version + 1)
            .unwrap_or(1),
        Err(_) => 1,
    };
    let now = now_ms();
    let doc = Doc {
        id,
        kind: KIND.into(),
        scope: SCOPE.into(),
        payload: serde_json::json!({
            "channel": channel,
            "conversation": conversation,
            "text": text,
            "attempts": 0,
            "created_at": now,
        }),
        version,
        valid_from: now,
        valid_to: None,
    };
    let mut tx = match store.begin_write().await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "写失败提示失败");
            return false;
        }
    };
    if let Err(e) = tx
        .upsert_doc(
            ConversationId(OWNER.into()),
            RunId(OWNER.into()),
            doc,
        )
        .await
    {
        tracing::warn!(error = %e, "写失败提示失败");
        return false;
    }
    if let Err(e) = tx.commit().await {
        tracing::warn!(error = %e, "写失败提示失败");
        return false;
    }
    true
}

/// 重启恢复：把"上次被打断"的消息排进提示队列。
///
/// 调用点在进程启动、`open_runs()` 之后——那时还没有通道，所以只能留底，
/// 由 actor 起来后统一补发。
pub async fn enqueue_restart(store: &dyn Store, conv: &ConversationId) {
    let Some((channel, conversation)) = split_conv(&conv.0) else {
        tracing::debug!(conv = %conv.0, "无法解析会话，跳过重启提示");
        return;
    };
    enqueue(store, channel, conversation, crate::copy::RESTART_INTERRUPTED).await;
}

pub async fn list(store: &dyn Store) -> Vec<Pending> {
    let Ok(mut rd) = store.begin_read().await else {
        return Vec::new();
    };
    let docs = rd
        .list_docs(DocQuery {
            scopes: vec![SCOPE.into()],
            kinds: vec![KIND.into()],
            limit: 50,
        })
        .await
        .unwrap_or_default();
    docs.into_iter()
        .filter_map(|d| {
            Some(Pending {
                id: d.id.clone(),
                channel: d.payload["channel"].as_str()?.to_owned(),
                conversation: d.payload["conversation"].as_str()?.to_owned(),
                text: d.payload["text"].as_str()?.to_owned(),
                attempts: d.payload["attempts"].as_u64().unwrap_or(0) as u32,
                created_at: d.payload["created_at"].as_i64().unwrap_or(0),
                version: d.version,
                valid_from: d.valid_from,
            })
        })
        .collect()
}

/// 扫一遍待补发的提示并尝试送达。
///
/// - `only`：只处理某个会话（下一条入站到达时先补发，避免"新消息被旧提示淹没"）。
/// - 返回成功送达的条数。
///
/// 这里**不用** `reply()`：那条路径带长退避，会把 actor 的主循环卡住。
/// 补发失败只把尝试次数 +1，交给下一个 2 分钟窗口。
pub async fn drain(
    store: &dyn Store,
    channels: &HashMap<String, Arc<dyn Channel>>,
    only: Option<(&str, &str)>,
    now: Timestamp,
) -> usize {
    let mut delivered = 0;
    for p in list(store).await {
        if let Some((ch, conv)) = only {
            if p.channel != ch || p.conversation != conv {
                continue;
            }
        }
        let expired = p.created_at > 0 && now.saturating_sub(p.created_at) > MAX_AGE_MS;
        if p.text.trim().is_empty() || expired || p.attempts >= MAX_ATTEMPTS {
            tracing::warn!(
                channel = %p.channel,
                conversation = %p.conversation,
                attempts = p.attempts,
                "失败提示过期或重试用尽，丢弃"
            );
            let _ = store.delete_doc(&p.id).await;
            continue;
        }
        let Some(ch) = channels.get(&p.channel) else {
            tracing::warn!(channel = %p.channel, "没有这个通道，失败提示留在队列里");
            continue;
        };
        let view = crate::view::View {
            shard: 0,
            is_last: true,
            blocks: vec![crate::view::Block::Paragraph(p.text.clone())],
            status: None,
            buttons: Vec::new(),
        };
        match ch.deliver(&p.conversation, view).await {
            Ok(_) => {
                tracing::info!(channel = %p.channel, conversation = %p.conversation, "失败提示已补发");
                let _ = store.delete_doc(&p.id).await;
                delivered += 1;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    channel = %p.channel,
                    conversation = %p.conversation,
                    "失败提示补发失败，稍后再试"
                );
                let doc = Doc {
                    id: p.id.clone(),
                    kind: KIND.into(),
                    scope: SCOPE.into(),
                    payload: serde_json::json!({
                        "channel": p.channel,
                        "conversation": p.conversation,
                        "text": p.text,
                        "attempts": p.attempts + 1,
                        "created_at": p.created_at,
                    }),
                    version: p.version + 1,
                    valid_from: p.valid_from,
                    valid_to: None,
                };
                if let Ok(mut tx) = store.begin_write().await {
                    let _ = tx
                        .upsert_doc(
                            ConversationId(OWNER.into()),
                            RunId(OWNER.into()),
                            doc,
                        )
                        .await;
                    let _ = tx.commit().await;
                }
            }
        }
    }
    delivered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_conversation_without_guessing() {
        assert_eq!(split_conv("telegram:1000:3"), Some(("telegram", "1000")));
        assert_eq!(split_conv("__reminders__"), None);
        assert_eq!(split_conv("telegram:1000"), None);
        assert_eq!(split_conv("telegram::3"), None);
    }

    #[test]
    fn doc_id_is_per_conversation() {
        assert_eq!(doc_id("telegram", "1000").0, "notice:telegram:1000");
        assert_ne!(doc_id("telegram", "1000"), doc_id("telegram", "1001"));
    }
}
