use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($($name:ident),*) => {$(
        #[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.0) }
        }

        impl From<&str> for $name {
            fn from(v: &str) -> Self { Self(v.to_owned()) }
        }
    )*};
}

id_type!(
    ConversationId,
    RunId,
    TurnId,
    CallId,
    TaskId,
    ApprovalId,
    DocId,
    ChannelId,
    MsgRef,
    ArtifactId
);

pub type Timestamp = i64;
pub type Seq = i64;

/// 当前 UTC 毫秒。整个进程统一用它，便于测试替换。
pub fn now_ms() -> Timestamp {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
