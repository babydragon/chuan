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

/// 配置里的一张表：谁是主人、他绑了哪些通道。
///
/// 一人多账号、多人并存都只是多写几条 `channels`。
/// **没有"兜底用户"**：找不到绑定就是未授权（A 段的需求，D26）。
#[derive(Clone, Debug, Default)]
pub struct Directory {
    pub users: Vec<User>,
}

impl Directory {
    /// (channel, addr) → user id；**没绑过就是 None**。
    pub fn lookup(&self, channel: &str, addr: &str) -> Option<String> {
        self.users
            .iter()
            .find(|u| {
                u.channels
                    .iter()
                    .any(|b| b.channel == channel && b.addr == addr)
            })
            .map(|u| u.id.clone())
    }

    /// 宽松版：找不到就把地址本身当 id（**只用于展示/记账，不用于授权**）。
    pub fn resolve(&self, channel: &str, addr: &str) -> String {
        self.lookup(channel, addr).unwrap_or_else(|| addr.to_owned())
    }

    /// 某个通道上允许的地址（白名单唯一来源，D26）。
    pub fn allowed_on(&self, channel: &str) -> Vec<String> {
        self.users
            .iter()
            .flat_map(|u| u.channels.iter())
            .filter(|b| b.channel == channel)
            .map(|b| b.addr.clone())
            .collect()
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
    fn unknown_addr_is_not_bound() {
        let d = dir();
        assert_eq!(d.lookup("telegram", "999"), None, "没绑过就是未授权");
        // 宽松版只用于记账/展示
        assert_eq!(d.resolve("telegram", "999"), "999");
    }

    #[test]
    fn one_user_many_accounts_and_many_users() {
        let d = Directory {
            users: vec![
                User {
                    id: "owner".into(),
                    name: None,
                    channels: vec![
                        Binding::parse("telegram:1").unwrap(),
                        Binding::parse("telegram:2").unwrap(),
                        Binding::parse("weixin:a@im.wechat").unwrap(),
                    ],
                },
                User {
                    id: "kid".into(),
                    name: None,
                    channels: vec![Binding::parse("telegram:3").unwrap()],
                },
            ],
        };
        // 同一个人两个 TG 号 → 同一个 user
        assert_eq!(d.lookup("telegram", "1").as_deref(), Some("owner"));
        assert_eq!(d.lookup("telegram", "2").as_deref(), Some("owner"));
        // 多人
        assert_eq!(d.lookup("telegram", "3").as_deref(), Some("kid"));
        // 白名单按通道派生
        assert_eq!(d.allowed_on("telegram"), vec!["1", "2", "3"]);
        assert_eq!(d.allowed_on("weixin"), vec!["a@im.wechat"]);
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
