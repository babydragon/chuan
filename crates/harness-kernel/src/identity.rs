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
    /// 主人是谁（D30）：`[identity] owner = "<user-id>"`。
    /// `None` = 没配主人，此时所有人一律按 member 处理（fail closed）。
    pub owner: Option<String>,
}

/// 主体角色（D30）。**不要叫 `Role`**：`record::Role` 已经是消息角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActorRole {
    Owner,
    Member,
}

impl ActorRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorRole::Owner => "owner",
            ActorRole::Member => "member",
        }
    }
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

    /// 这个 user 是主人还是成员（D30）。
    ///
    /// fail closed：配了身份表却没有 `owner`、或这个 id 不是 owner，都算 member。
    /// 例外是**空表**：那是"没配置身份"的中立模式（应用层拒绝空表启动，
    /// 内核也不再做未绑定拦截），此时按单用户处理，否则所有审批都会静默变 Blocked。
    /// 注意这不是"授权"——未绑定的地址在入口就被拦掉了（D26）。
    pub fn role(&self, user: &str) -> ActorRole {
        if self.users.is_empty() {
            return ActorRole::Owner;
        }
        match &self.owner {
            // 必须真的绑定过：owner 字段写了个不存在的 id 时，谁都不算主人（fail closed）。
            Some(owner) if owner == user && self.users.iter().any(|u| &u.id == user) => {
                ActorRole::Owner
            }
            _ => ActorRole::Member,
        }
    }

    /// 主人是否配置且确实绑定过（启动时校验用）。
    pub fn owner_problem(&self) -> Option<String> {
        match &self.owner {
            None => Some("缺少 [identity] owner（没有审批人，越界动作只能被拒绝）".into()),
            Some(o) if !self.users.iter().any(|u| &u.id == o) => {
                Some(format!("[identity] owner = \"{o}\" 不在 [[identity.user]] 里"))
            }
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> Directory {
        Directory {
            owner: Some("owner".into()),
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
            owner: Some("owner".into()),
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
    fn role_is_owner_or_member_and_fails_closed() {
        let d = dir();
        assert_eq!(d.role("owner"), ActorRole::Owner);
        assert_eq!(d.role("kid"), ActorRole::Member);
        assert_eq!(d.role("telegram:81865154"), ActorRole::Member, "通道地址不是 user id");

        // 空表 = 未配置身份（中立模式）：按单用户处理
        assert_eq!(Directory::default().role("anyone"), ActorRole::Owner);

        // 配了人但没配 owner：谁都不是主人（fail closed），且启动校验要能报出来
        let none = Directory { users: d.users.clone(), owner: None };
        assert_eq!(none.role("owner"), ActorRole::Member);
        assert!(none.owner_problem().unwrap().contains("owner"));

        // owner 指向一个不存在的 user：同样是配置问题
        let bad = Directory { users: d.users.clone(), owner: Some("ghost".into()) };
        assert_eq!(bad.role("ghost"), ActorRole::Member);
        assert!(bad.owner_problem().unwrap().contains("不在"));
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
