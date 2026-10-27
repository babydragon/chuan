//! 代理策略：**谁能用、默认是否用**。
//!
//! 目标板上代理跑在宿主本地（`127.0.0.1:7890` 之类）。注意：联网 lane
//! 不隔离 netns，所以沙盒里的 `127.0.0.1` 就是宿主的 loopback，能直接连代理；
//! 离线 lane 没有网络，代理无意义。
//!
//! 三种模式：
//! - `Never`：永远直连
//! - `Auto`：按需（工具参数要求时才用；web_fetch 是"直连失败再试代理"）
//! - `Always`：一律走代理

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Never,
    Auto,
    Always,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "on" | "true" => Mode::Always,
            "never" | "off" | "false" => Mode::Never,
            _ => Mode::Auto,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProxyConfig {
    pub url: Option<String>,
    pub sandbox: Mode,
    pub web_fetch: Mode,
    pub services: Mode,
}

impl Default for Mode {
    fn default() -> Self {
        Mode::Auto
    }
}

impl ProxyConfig {
    pub fn enabled(&self) -> bool {
        self.url.as_deref().map(|u| !u.trim().is_empty()).unwrap_or(false)
    }

    /// 沙盒里要注入的环境变量：`proxy=true` 或 mode=Always 时给。
    pub fn sandbox_env(&self, requested: bool) -> Vec<(String, String)> {
        match (self.enabled(), self.sandbox) {
            (false, _) | (_, Mode::Never) => Vec::new(),
            (true, Mode::Always) => self.env(),
            (true, Mode::Auto) if requested => self.env(),
            _ => Vec::new(),
        }
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let Some(url) = &self.url else { return Vec::new() };
        vec![
            ("http_proxy".into(), url.clone()),
            ("https_proxy".into(), url.clone()),
            ("all_proxy".into(), url.clone()),
            // 让 localhost / 内网直连，免得把本机服务也绕进代理
            (
                "no_proxy".into(),
                "localhost,127.0.0.1,::1,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16".into(),
            ),
        ]
    }

    pub fn reqwest(&self) -> Option<reqwest::Proxy> {
        let url = self.url.as_ref()?;
        reqwest::Proxy::all(url).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(sandbox: Mode) -> ProxyConfig {
        ProxyConfig {
            url: Some("http://127.0.0.1:7890".into()),
            sandbox,
            ..Default::default()
        }
    }

    #[test]
    fn sandbox_env_follows_mode() {
        assert!(cfg(Mode::Never).sandbox_env(true).is_empty());
        assert!(cfg(Mode::Auto).sandbox_env(false).is_empty());
        assert_eq!(cfg(Mode::Auto).sandbox_env(true).len(), 4);
        assert_eq!(cfg(Mode::Always).sandbox_env(false).len(), 4);
        let none = ProxyConfig::default();
        assert!(none.sandbox_env(true).is_empty());
    }

    #[test]
    fn no_proxy_covers_lan_and_loopback() {
        let env = cfg(Mode::Always).env();
        let (_, np) = env.iter().find(|(k, _)| k == "no_proxy").unwrap();
        assert!(np.contains("127.0.0.1") && np.contains("192.168.0.0/16"));
    }

    #[test]
    fn mode_parsing() {
        assert_eq!(Mode::parse("always"), Mode::Always);
        assert_eq!(Mode::parse("NEVER"), Mode::Never);
        assert_eq!(Mode::parse(""), Mode::Auto);
    }
}
