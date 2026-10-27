//! 身份：**用户（User）与通道地址（Binding）分离**。
//!
//! 现在只有一个真实用户，但结构上已经允许「一个人对应多个通道」：
//! 今天 `telegram:81865154`，以后加 `webui:u1`、`feishu:ou_xxx` 都只是多一条绑定。
//!
//! 这里只做**解析**（channel+addr → user），不做隔离：隔离要动
//! 记忆 scope、workspace 挂载、审批归属，等第二个真人出现再说。

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub channel: String,
    pub addr: String,
}

impl Binding {
    pub fn parse(s: &str) -> Option<Self> {
        let (channel, addr) = s.split_once(':')?;
        let (channel, addr) = (channel.trim(), addr.trim());
        if channel.is_empty() || addr.is_empty() {
            return None;
        }
        Some(Self { channel: channel.to_owned(), addr: addr.to_owned() })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: Option<String>,
    pub channels: Vec<Binding>,
}

/// 配置里的一张表：谁是 owner、他绑了哪些通道。
#[derive(Clone, Debug, Default)]
pub struct Directory {
    pub users: Vec<User>,
    pub default_user: Option<String>,
}

impl Directory {
    /// 找不到绑定时用什么。`None` = 退化成把地址本身当用户（单用户时的旧行为）。
    pub fn with_default(mut self, id: impl Into<String>) -> Self {
        self.default_user = Some(id.into());
        self
    }

    /// (channel, addr) → user id。
    pub fn resolve(&self, channel: &str, addr: &str) -> String {
        for u in &self.users {
            if u.channels
                .iter()
                .any(|b| b.channel == channel && b.addr == addr)
            {
                return u.id.clone();
            }
        }
        self.default_user.clone().unwrap_or_else(|| addr.to_owned())
    }

    /// 这个用户绑了哪些通道地址（提醒默认投这些）。
    pub fn channels(&self, user: &str) -> Vec<Binding> {
        self.users
            .iter()
            .find(|u| u.id == user)
            .map(|u| u.channels.clone())
            .unwrap_or_default()
    }

    pub fn name(&self, user: &str) -> Option<String> {
        self.users
            .iter()
            .find(|u| u.id == user)
            .and_then(|u| u.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Directory {
        Directory {
            users: vec![User {
                id: "owner".into(),
                name: Some("主人".into()),
                channels: vec![
                    Binding::parse("telegram:81865154").unwrap(),
                    Binding::parse("webui:u1").unwrap(),
                ],
            }],
            default_user: None,
        }
    }

    #[test]
    fn resolves_each_channel_to_the_same_user() {
        let d = dir();
        assert_eq!(d.resolve("telegram", "81865154"), "owner");
        assert_eq!(d.resolve("webui", "u1"), "owner");
        assert_eq!(d.channels("owner").len(), 2);
    }

    #[test]
    fn unknown_addr_falls_back() {
        let d = dir();
        assert_eq!(d.resolve("telegram", "999"), "999");
        let d = dir().with_default("owner");
        assert_eq!(d.resolve("telegram", "999"), "owner");
    }

    #[test]
    fn binding_parse_shapes() {
        assert_eq!(
            Binding::parse("telegram:123"),
            Some(Binding { channel: "telegram".into(), addr: "123".into() })
        );
        assert!(Binding::parse("nocolon").is_none());
        assert!(Binding::parse(":123").is_none());
    }
}
