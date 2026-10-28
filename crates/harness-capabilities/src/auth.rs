//! 宿主侧的凭据解析与签名。
//!
//! 目标：**脚本/模型永远拿不到值**，但"把凭据放到请求的哪个位置"要能覆盖常见服务。
//! 支持三件事：
//!
//! 1. `token_file` 的行/键取值（第 1 行 token，第 2 行 secret，或 `key = value`）；
//! 2. 模板替换 `{token}` `{secret}` `{sign}` `{ts_s}` `{ts_ms}` `{path}`；
//! 3. HMAC 签名（钉钉 `{ts_ms}\n{secret}`、飞书 `{ts_s}\n{secret}`），
//!    结果按 base64 / base64url / hex 编码。
//!
//! 非目标：OAuth2 令牌交换（需要缓存与过期刷新）、AWS SigV4、OAuth1 这类
//! 多步签名协议——它们的复杂度不属于"一次 HTTP 调用"，见 `docs/DECISIONS.md` D18。

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

/// 凭据文件的内容：按行 + 可选的 `key = value` 映射。
#[derive(Clone, Debug, Default)]
pub struct SecretFile {
    lines: Vec<String>,
    kv: BTreeMap<String, String>,
}

impl SecretFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("读取凭据 {} 失败：{e}", path.display()))?;
        let mut lines = Vec::new();
        let mut kv = BTreeMap::new();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() && !v.is_empty() {
                    kv.insert(k.to_owned(), v.to_owned());
                }
            }
            lines.push(line.to_owned());
        }
        Ok(Self { lines, kv })
    }

    /// 按需取：优先键名，其次第 `nth` 个非空行（0 起）。
    pub fn get(&self, key: Option<&str>, nth: usize) -> Option<String> {
        if let Some(k) = key {
            if let Some(v) = self.kv.get(k) {
                return Some(v.clone());
            }
        }
        self.lines.get(nth).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// 原始行（用于把"已知值"从日志/响应里抹掉）。
    pub fn raw_lines(&self) -> &[String] {
        &self.lines
    }
}

/// 把"用户可能贴进来的东西"归一成真正的 token。
///
/// 人会直接粘整条 Webhook URL——那就别要求他裁。规则：
/// - 带 `access_token=` 的：取该参数的值（钉钉）；
/// - 是 URL 但没有该参数：取最后一段路径（飞书 hook id 就在路径里）；
/// - `access_token=xxx` 这种裸参数：取等号右边；
/// - 其它情况原样返回（裸 token、SEC 开头的密钥都不动）。
pub fn normalize_token(raw: &str) -> String {
    let s = raw.trim();
    if s.is_empty() {
        return String::new();
    }
    if let Some(pos) = s.find("access_token=") {
        let rest = &s[pos + "access_token=".len()..];
        let end = rest.find(['&', '#', ' ', '\n']).unwrap_or(rest.len());
        let v = &rest[..end];
        if !v.is_empty() {
            return v.to_owned();
        }
    }
    if s.starts_with("http://") || s.starts_with("https://") {
        let no_q = s.split(['?', '#']).next().unwrap_or(s);
        let seg = no_q.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        if !seg.is_empty() {
            return seg.to_owned();
        }
    }
    s.to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encode {
    Base64,
    Base64Url,
    Hex,
}

impl Encode {
    pub fn parse(s: &str) -> Encode {
        match s.trim().to_ascii_lowercase().as_str() {
            "hex" => Encode::Hex,
            "base64url" | "base64_url" | "urlsafe" => Encode::Base64Url,
            _ => Encode::Base64,
        }
    }

    fn apply(self, bytes: &[u8]) -> String {
        match self {
            Encode::Base64 => base64::engine::general_purpose::STANDARD.encode(bytes),
            Encode::Base64Url => base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
            Encode::Hex => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
}

/// 签名配置。算法固定 HMAC-SHA256（钉钉/飞书/绝大多数 webhook 都是它）。
#[derive(Clone, Debug)]
pub struct SignConfig {
    /// secret 在凭据文件里的键名；不写则取第 2 行
    pub secret_key: Option<String>,
    /// 待签串模板，可用 `{ts_s}` `{ts_ms}` `{secret}` `{token}` `{path}`
    pub canonical: String,
    pub encode: Encode,
}

impl Default for SignConfig {
    fn default() -> Self {
        // 钉钉：timestamp(毫秒) + "\n" + secret
        Self { secret_key: None, canonical: "{ts_ms}\n{secret}".into(), encode: Encode::Base64 }
    }
}

/// 一次请求的模板变量。
#[derive(Clone, Debug, Default)]
pub struct Vars {
    pub token: Option<String>,
    pub secret: Option<String>,
    pub sign: Option<String>,
    pub ts_ms: i64,
    pub ts_s: i64,
    pub path: String,
}

impl Vars {
    pub fn new(ts_ms: i64, path: &str) -> Self {
        Self { ts_ms, ts_s: ts_ms / 1000, path: path.to_owned(), ..Default::default() }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        match name {
            "token" => self.token.clone(),
            "secret" => self.secret.clone(),
            "sign" => self.sign.clone(),
            "ts_ms" => Some(self.ts_ms.to_string()),
            "ts_s" => Some(self.ts_s.to_string()),
            "path" => Some(self.path.clone()),
            _ => None,
        }
    }

    /// 替换 `{name}` 形式的占位符。未知占位符原样保留（便于报错时看出写错了）。
    pub fn render(&self, tpl: &str) -> String {
        let mut out = String::with_capacity(tpl.len());
        let mut rest = tpl;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let Some(close) = after.find('}') else {
                out.push_str(&rest[open..]);
                return out;
            };
            let name = &after[..close];
            match self.lookup(name) {
                Some(v) => out.push_str(&v),
                None => {
                    out.push('{');
                    out.push_str(name);
                    out.push('}');
                }
            }
            rest = &after[close + 1..];
        }
        out.push_str(rest);
        out
    }

    /// 本次请求里所有"不能外泄的值"，用于把响应/错误/日志里的回显抹掉。
    pub fn secrets(&self) -> Vec<String> {
        [self.token.clone(), self.secret.clone(), self.sign.clone()]
            .into_iter()
            .flatten()
            .filter(|v| v.chars().count() >= 6)
            .collect()
    }
}

/// 按配置算签名；缺 secret 时明确报错（而不是签出一个空串）。
pub fn sign(cfg: &SignConfig, vars: &Vars) -> Result<String, String> {
    let secret = vars
        .secret
        .as_deref()
        .ok_or("需要签名的服务必须在凭据文件第 2 行（或 secret_key 指定的键）提供 secret")?;
    let msg = vars.render(&cfg.canonical);
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|e| format!("签名密钥不可用：{e}"))?;
    mac.update(msg.as_bytes());
    Ok(cfg.encode.apply(&mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_file(text: &str) -> SecretFile {
        let dir = std::env::temp_dir().join(format!(
            "chuan-auth-{}-{:?}",
            std::process::id(),
            text.len()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s");
        std::fs::write(&p, text).unwrap();
        SecretFile::load(&p).unwrap()
    }

    #[test]
    fn reads_lines_and_kv() {
        let f = secret_file("token123\nsec456\n");
        assert_eq!(f.get(None, 0).as_deref(), Some("token123"));
        assert_eq!(f.get(None, 1).as_deref(), Some("sec456"));

        let f = secret_file("# 注释\naccess_token = abc\nsec = xyz\n");
        assert_eq!(f.get(Some("access_token"), 0).as_deref(), Some("abc"));
        assert_eq!(f.get(Some("sec"), 0).as_deref(), Some("xyz"));
    }

    #[test]
    fn renders_placeholders() {
        let mut v = Vars::new(1_700_000_000_123, "/robot/send");
        v.token = Some("T".into());
        assert_eq!(v.render("{token}:{ts_s}:{ts_ms}:{path}"), "T:1700000000:1700000000123:/robot/send");
        // 未知占位符原样保留，方便看出配置写错
        assert_eq!(v.render("{nope}"), "{nope}");
    }

    #[test]
    fn dingtalk_signature_matches_known_shape() {
        // 钉钉：base64(hmac_sha256(secret, "{ts_ms}\n{secret}"))
        let mut v = Vars::new(1_700_000_000_000, "/");
        v.secret = Some("SECtest".into());
        let cfg = SignConfig { secret_key: None, canonical: "{ts_ms}\n{secret}".into(), encode: Encode::Base64 };
        let s = sign(&cfg, &v).unwrap();
        // 用同一实现独立复算，确认不是空串/hex
        let mut mac = Hmac::<Sha256>::new_from_slice(b"SECtest").unwrap();
        mac.update(b"1700000000000\nSECtest");
        let expect = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        assert_eq!(s, expect);
        assert!(s.ends_with('=') || s.len() >= 40);
    }

    #[test]
    fn feishu_uses_seconds_and_body_placement() {
        let mut v = Vars::new(1_700_000_000_999, "/hook");
        v.secret = Some("S".into());
        let cfg = SignConfig { secret_key: None, canonical: "{ts_s}\n{secret}".into(), encode: Encode::Base64 };
        let s = sign(&cfg, &v).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(b"S").unwrap();
        mac.update(b"1700000000\nS");
        assert_eq!(s, base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()));
    }

    #[test]
    fn missing_secret_is_an_error() {
        let v = Vars::new(1, "/");
        assert!(sign(&SignConfig::default(), &v).is_err());
    }

    #[test]
    fn secrets_lists_values_for_scrubbing() {
        let mut v = Vars::new(1, "/");
        v.token = Some("abcdef123456".into());
        v.sign = Some("short".into()); // 太短，不参与
        assert_eq!(v.secrets(), vec!["abcdef123456".to_string()]);
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::normalize_token;

    #[test]
    fn dingtalk_full_url_yields_access_token() {
        let url = "https://oapi.dingtalk.com/robot/send?access_token=abc123def";
        assert_eq!(normalize_token(url), "abc123def");
        // 带额外参数也只取值本身
        let url = "https://oapi.dingtalk.com/robot/send?access_token=abc123&x=1";
        assert_eq!(normalize_token(url), "abc123");
    }

    #[test]
    fn feishu_full_url_yields_hook_id() {
        let url = "https://open.feishu.cn/open-apis/bot/v2/hook/1234-abcd";
        assert_eq!(normalize_token(url), "1234-abcd");
    }

    #[test]
    fn bare_values_stay_untouched() {
        assert_eq!(normalize_token("abc123def"), "abc123def");
        assert_eq!(normalize_token("SECdeadbeef"), "SECdeadbeef");
        assert_eq!(normalize_token("access_token=abc"), "abc");
        assert_eq!(normalize_token("  "), "");
    }
}
