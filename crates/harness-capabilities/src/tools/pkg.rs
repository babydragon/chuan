//! `pkg`：系统包管理（apt）。**走 hostd 的提权通道**，不做任何提权兜底。
//!
//! 为什么要单独一个工具而不是让 agent 自己 `host` 里跑 apt：
//! - 需要 root，`host` 是服务账号身份；
//! - 装包是不可逆的（升级会替换库），必须先给主人看 `apt-get -s` 的**预览**；
//! - 预览与真装必须成对出现：`install` 会拒绝"没预览过"的包集合。
//!
//! v1 只做 `plan` / `install`。删包、升级整机一律不做（`remove`/`dist-upgrade`
//! 在 SBC 上等于赌博，见 D22）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 只读查询的输出上限：别让一次 `apt-cache search python` 把上下文冲爆
const SEARCH_MAX_LINES: usize = 30;
const SHOW_MAX_PKGS: usize = 5;

use async_trait::async_trait;
use harness_kernel::{
    Block, Domain, EffectClass, ErrorCode, Lane, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use crate::elevated::ElevatedClient;

/// 预览有效期：超过这个时间要求重新 plan（包源/依赖可能已经变了）
const PLAN_TTL: Duration = Duration::from_secs(600);

pub struct PkgTool {
    client: ElevatedClient,
    /// 已预览的包集合 → 预览时刻
    planned: Mutex<HashMap<String, Instant>>,
}

impl PkgTool {
    pub fn new(client: ElevatedClient) -> Self {
        Self { client, planned: Mutex::new(HashMap::new()) }
    }

    /// 包名白名单：`[a-z0-9][a-z0-9+.-]*`，可带 `=version`。
    /// 拒绝空格/斜杠/`$`——argv 直接 exec，没有 shell，但拒绝仍然有意义：
    /// 它挡住了"把一堆东西塞进一个参数"这种误用。
    pub fn valid_pkg(s: &str) -> bool {
        let (name, ver) = match s.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (s, None),
        };
        if name.is_empty() || name.len() > 128 {
            return false;
        }
        let ok_name = name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-'))
            && name.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let ok_ver = ver.is_some_and(|v| {
            !v.is_empty()
                && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '~' | ':' | '-'))
        }) || ver.is_none();
        ok_name && ok_ver
    }

    fn key(pkgs: &[String]) -> String {
        let mut v = pkgs.to_vec();
        v.sort();
        v.join(" ")
    }

    fn argv(action: &str, pkgs: &[String]) -> Vec<String> {
        let mut argv: Vec<String> = vec![
            "/usr/bin/apt-get".into(),
            "install".into(),
            if action == "plan" { "-s".into() } else { "-y".into() },
            "--no-install-recommends".into(),
        ];
        argv.extend(pkgs.iter().cloned());
        argv
    }

    /// 查询关键词：只接受包名里可能出现的字符，避免拿 apt 的正则去跑没人看得懂的东西。
    pub fn valid_query(q: &str) -> bool {
        !q.trim().is_empty()
            && q.len() <= 64
            && q.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '.' | '-' | '_')
            })
    }

    /// `apt-cache show` 的紧凑化：只留人做判断需要的几个字段。
    /// 返回 (每包一行 + 依赖行, 实际输出的包数)。
    pub fn compact_show(raw: &str, limit: usize) -> String {
        fn flush(cur: &mut Vec<(String, String)>, out: &mut String) -> bool {
            if cur.is_empty() {
                return false;
            }
            let get = |k: &str| {
                cur.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone()).unwrap_or_default()
            };
            let name = get("Package");
            if name.is_empty() {
                cur.clear();
                return false;
            }
            let ver = get("Version");
            let kb: u64 = get("Installed-Size").parse().unwrap_or(0);
            let deps: String = get("Depends").chars().take(120).collect();
            out.push_str(&format!(
                "{name} {ver}{}{}\n",
                if kb > 0 { format!("  ≈{} MB", (kb + 1023) / 1024) } else { String::new() },
                if deps.is_empty() { String::new() } else { format!("\n  依赖：{deps}") }
            ));
            cur.clear();
            true
        }

        let mut out = String::new();
        let mut n = 0usize;
        let mut cur: Vec<(String, String)> = Vec::new();
        for line in raw.lines() {
            if line.trim().is_empty() {
                if flush(&mut cur, &mut out) {
                    n += 1;
                }
                if n >= limit {
                    break;
                }
                continue;
            }
            if !line.starts_with(' ') {
                if let Some((k, v)) = line.split_once(':') {
                    cur.push((k.to_owned(), v.trim().to_owned()));
                }
            }
        }
        if n < limit && flush(&mut cur, &mut out) {
            n += 1;
        }
        if n > 0 && !out.ends_with('\n') {
            out.push('\n');
        }
        out
    }

    /// 只读查询：以服务账号身份直接跑 `apt-cache`，不经 hostd、不需要 root。
    async fn query(&self, argv: Vec<String>, timeout_ms: u64) -> Result<String, ToolError> {
        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let out = tokio::time::timeout(Duration::from_millis(timeout_ms), cmd.output())
            .await
            .map_err(|_| ToolError::new(ErrorCode::Timeout, "apt-cache 超时", true))?
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, format!("跑不了 apt-cache：{e}"), true))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() && text.trim().is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(ToolError::new(
                ErrorCode::Unavailable,
                format!("apt-cache 失败：{}", err.trim()),
                false,
            ));
        }
        Ok(text)
    }

    fn plan_age(&self, key: &str) -> Option<Duration> {
        let g = self.planned.lock().unwrap();
        g.get(key).map(|t| t.elapsed())
    }
}

#[async_trait]
impl Tool for PkgTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "pkg".into(),
            description: "安装**系统包**（apt），需要主人授权，走提权通道（`chuan hostd`）。\
用于：python 大件（numpy/matplotlib/pandas）、系统工具（jq/sqlite3/gnuplot）、编译依赖。\
**先查再装**：`search`（关键词找包）→ `show`（版本/依赖/占用）→ `plan`（模拟安装）→ `install`。\
前两步只读、免审批、以服务账号身份执行（不经提权通道），最后一步才需要主人点头。\
注意 `[policy] host_elevated = deny` 会连 search/show 一起禁掉（域是工具级的）；\
想保留查包、只禁安装，把它设成 `ask` 即可（安装仍会逐条确认）。\
**不要**用它装 python 库到自己的环境——那用 `install_deps`。\
流程固定：先 `action=plan`（`apt-get -s` 预览，几秒）把结果给主人看，再 `action=install`。\
`install` 会拒绝没有预览过的包集合（预览 10 分钟内有效）。\
v1 只做 install：删包、整机升级一律不做（在 SBC 上等于赌博）。\
装完的东西在宿主上，沙盒里能不能用取决于它装在哪个前缀：python 包（`python3-*`）\
沙盒直接可用（venv 含系统 site-packages），命令行工具要把二进制所在目录挂进沙盒才看得到。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["search", "show", "plan", "install"],
                        "description": "search/show 只读、免审批；plan 预览；install 真装"
                    },
                    "query": { "type": "string", "description": "search 用：关键词（包名片段）" },
                    "packages": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "包名，如 python3-matplotlib；可带版本 python3-numpy=1.26.4-1"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 改系统状态，失败不自动重发
        EffectClass::External
    }

    fn domain(&self) -> Domain {
        Domain::HostElevated
    }

    fn tier(&self) -> u8 {
        1
    }

    fn lane(&self) -> Lane {
        // 要联网下载
        Lane::External
    }

    /// `plan` 是只读预览（`apt-get -s` 不改任何东西），不该占一次审批；
    /// 真装（`install`）仍然逐条问。策略域设成 `deny` 时这条不生效。
    fn skip_approval(&self, args: &serde_json::Value) -> bool {
        // 只读动作（预览/查询）不占审批；install 仍逐条问。
        matches!(
            args.get("action").and_then(|v| v.as_str()),
            Some("plan") | Some("search") | Some("show")
        )
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let pkgs: Vec<String> = args
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let age = self.plan_age(&Self::key(&pkgs));
        let tail = match age {
            Some(d) => format!("（{} 秒前已预览）", d.as_secs()),
            None => "（**未预览**——install 会被拒绝）".to_string(),
        };
        Some(format!("{action} 系统包：{} {tail}", pkgs.join(" ")))
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let pkgs: Vec<String> = args
            .get("packages")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if pkgs.is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "packages 为空", false));
        }
        if let Some(bad) = pkgs.iter().find(|p| !Self::valid_pkg(p)) {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("包名不合法：{bad}（只允许小写字母/数字/+.-，可带 =版本）"),
                false,
            ));
        }
        let key = Self::key(&pkgs);

        match action {
            "search" => {
                let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                if !Self::valid_query(q) {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "query 只接受小写字母/数字/+.-_（≤64），例如 matplotlib",
                        false,
                    ));
                }
                let text = self
                    .query(vec!["/usr/bin/apt-cache".into(), "search".into(), q.to_owned()], 20_000)
                    .await?;
                let all: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
                if all.is_empty() {
                    return Ok(ToolOutput {
                        content: vec![Block::Paragraph(format!(
                            "没有匹配的包：{q}（包列表过期时先 apt update，或换个关键词）"
                        ))],
                        doc_writes: Vec::new(),
                        artifacts: Vec::new(),
                    });
                }
                let mut body = String::new();
                for l in all.iter().take(SEARCH_MAX_LINES) {
                    body.push_str(l);
                    body.push('\n');
                }
                if all.len() > SEARCH_MAX_LINES {
                    body.push_str(&format!("…（共 {} 条，先看前 {SEARCH_MAX_LINES} 条）\n", all.len()));
                }
                Ok(ToolOutput {
                    content: vec![
                        Block::Paragraph(format!("共 {} 条（按包名+描述匹配）", all.len())),
                        Block::Code { lang: None, text: body },
                    ],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            "show" => {
                let picked: Vec<String> = pkgs.iter().take(SHOW_MAX_PKGS).cloned().collect();
                let mut argv: Vec<String> = vec![
                    "/usr/bin/apt-cache".into(),
                    "show".into(),
                ];
                argv.extend(picked.iter().cloned());
                let text = self.query(argv, 20_000).await?;
                let body = Self::compact_show(&text, SHOW_MAX_PKGS);
                if body.trim().is_empty() {
                    return Ok(ToolOutput {
                        content: vec![Block::Paragraph(format!(
                            "查不到这些包的元数据：{}",
                            picked.join(" ")
                        ))],
                        doc_writes: Vec::new(),
                        artifacts: Vec::new(),
                    });
                }
                Ok(ToolOutput {
                    content: vec![Block::Code { lang: None, text: body }],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            "plan" => {
                let argv = Self::argv("plan", &pkgs);
                let out = self.client.exec(&argv).await.map_err(|e| {
                    ToolError::new(ErrorCode::Unavailable, e, true)
                })?;
                if out.grant_index == 0 {
                    return Err(ToolError::new(
                        ErrorCode::NotPermitted,
                        out.error.unwrap_or_else(|| "提权通道拒绝（检查 [[host_elevated.grant]]）".into()),
                        false,
                    ));
                }
                if !out.ok {
                    return Err(ToolError::new(
                        ErrorCode::Unavailable,
                        format!(
                            "apt-get -s 失败（code={:?}）：{}",
                            out.code,
                            out.stderr.lines().take(6).collect::<Vec<_>>().join("\n")
                        ),
                        false,
                    ));
                }
                self.planned.lock().unwrap().insert(key, Instant::now());
                Ok(ToolOutput {
                    content: vec![Block::Code { lang: None, text: out.stdout }],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            "install" => {
                match self.plan_age(&key) {
                    None => {
                        return Err(ToolError::new(
                            ErrorCode::InvalidArgs,
                            "这个包集合还没预览过：先 action=plan 把 apt-get -s 的结果发给主人，再 install",
                            false,
                        ))
                    }
                    Some(d) if d > PLAN_TTL => {
                        self.planned.lock().unwrap().remove(&key);
                        return Err(ToolError::new(
                            ErrorCode::InvalidArgs,
                            format!("预览已过期（{} 分钟）：重新 plan 一次", d.as_secs() / 60),
                            false,
                        ));
                    }
                    Some(_) => {}
                }
                let argv = Self::argv("install", &pkgs);
                let out = self.client.exec(&argv).await.map_err(|e| {
                    ToolError::new(ErrorCode::Unavailable, e, true)
                })?;
                if let Some(e) = &out.error {
                    return Err(ToolError::new(ErrorCode::NotPermitted, e.clone(), false));
                }
                self.planned.lock().unwrap().remove(&key);

                let mut text = format!(
                    "exit={}\n命中授权：第 {} 条\n",
                    out.code.map(|c| c.to_string()).unwrap_or("signal".into()),
                    out.grant_index
                );
                if let Some(s) = &out.snapshot {
                    text.push_str(&format!("变更前快照：{s}\n"));
                }
                if !out.stdout.is_empty() {
                    text.push_str("--- stdout ---\n");
                    text.push_str(&out.stdout);
                }
                if !out.stderr.is_empty() {
                    text.push_str("\n--- stderr ---\n");
                    text.push_str(&out.stderr);
                }
                if out.truncated {
                    text.push_str("\n[输出已截断]");
                }
                Ok(ToolOutput {
                    content: vec![Block::Code { lang: None, text }],
                    doc_writes: Vec::new(),
                    artifacts: Vec::new(),
                })
            }
            other => Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("未知 action：{other}（plan / install）"),
                false,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PkgTool;

    #[test]
    fn package_names_are_whitelisted() {
        for ok in ["python3-matplotlib", "jq", "gcc-12", "libssl3", "python3-numpy=1:1.26.4-1"] {
            assert!(PkgTool::valid_pkg(ok), "{ok} 应通过");
        }
        for bad in ["", "-y", "Apt", "foo bar", "../../etc/passwd", "foo;rm", "a/b", "foo=1 2"] {
            assert!(!PkgTool::valid_pkg(bad), "{bad} 应被拒");
        }
    }

    #[test]
    fn query_is_restricted() {
        for ok in ["figlet", "matplotlib", "python3-", "gcc-12", "libssl3"] {
            assert!(PkgTool::valid_query(ok), "{ok} 应通过");
        }
        for bad in ["", "Figlet", "a b", "../x", "f*", "(.*)", &"x".repeat(65)] {
            assert!(!PkgTool::valid_query(bad), "{bad} 应被拒");
        }
    }

    #[test]
    fn compact_show_keeps_the_useful_fields() {
        let raw = "Package: figlet\nVersion: 2.2.5-3\nInstalled-Size: 42000\nDepends: libc6\n\nPackage: toilet\nVersion: 0.3-3\nInstalled-Size: 900\n";
        let out = PkgTool::compact_show(raw, 5);
        assert!(out.contains("figlet 2.2.5-3"), "{out}");
        assert!(out.contains("MB"), "应给人一个体积量级：{out}");
        assert!(out.contains("依赖：libc6"), "{out}");
        assert!(out.contains("toilet 0.3-3"), "{out}");
        // limit 生效
        let one = PkgTool::compact_show(raw, 1);
        assert!(!one.contains("toilet"), "{one}");
    }

    #[test]
    fn argv_keeps_the_grant_prefix() {
        // 授权表写的是 ["/usr/bin/apt-get","install"]，两种 action 都必须以它开头
        for a in ["plan", "install"] {
            let argv = PkgTool::argv(a, &["jq".into()]);
            assert_eq!(argv[0], "/usr/bin/apt-get");
            assert_eq!(argv[1], "install");
            assert!(argv.contains(&"jq".to_string()));
        }
    }

    #[test]
    fn key_is_order_insensitive() {
        assert_eq!(
            PkgTool::key(&["b".into(), "a".into()]),
            PkgTool::key(&["a".into(), "b".into()])
        );
    }
}
