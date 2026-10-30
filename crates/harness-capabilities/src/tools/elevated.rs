//! `elevated`：提权通道的**通用**客户端（D22 L2）。
//!
//! 这里刻意**没有任何发行版语义**——不说 apt、不说 emerge、不说 `-y` 还是 `-s`：
//! "能跑什么"由 `[[host_elevated.grant]]` 的 argv 前缀决定，"要不要问"由条目上的
//! `approval` 决定，"变更前存什么"由 `snapshot_argv` 决定。三条都是数据。
//!
//! 所以换一台 Gentoo 不需要改 Rust：加 grant（`emerge`）、加 `[host.path]`（`/var/db/pkg` ro），
//! 再把 `emerge --pretend` 的用法写进 playbook/记忆即可。

use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, Domain, EffectClass, ErrorCode, Lane, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use crate::elevated::ElevatedClient;
use crate::grants::{Approval, Grants, GrantsCache};

pub struct ElevatedTool {
    client: ElevatedClient,
    cache: Arc<GrantsCache>,
}

impl ElevatedTool {
    pub fn new(client: ElevatedClient, cache: Arc<GrantsCache>) -> Self {
        Self { client, cache }
    }

    fn argv_of(args: &serde_json::Value) -> Result<Vec<String>, ToolError> {
        let argv: Vec<String> = args
            .get("argv")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        if argv.is_empty() {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                "argv 为空：这是一条要 root 执行的命令，用数组给出（不带 shell）",
                false,
            ));
        }
        if !argv[0].starts_with('/') {
            return Err(ToolError::new(
                ErrorCode::InvalidArgs,
                format!("argv[0] 必须是绝对路径，收到 {}", argv[0]),
                false,
            ));
        }
        Ok(argv)
    }

    /// 工具侧也匹配一次：为的是给出"哪条授权、还差什么"的**可读错误**，
    /// 真正的判定仍由 hostd 再做一遍（纵深防御，别指望工具层的校验当边界）。
    fn grant_hint(grants: &Grants, argv: &[String]) -> Result<usize, String> {
        if let Some(reason) = grants.elevated_error() {
            return Err(format!("提权授权不可用（{reason}）——修好配置再试，不会回退旧授权"));
        }
        if let Some((idx, _)) = grants.match_argv(argv) {
            return Ok(idx);
        }
        let list: Vec<String> = grants
            .elevated
            .iter()
            .map(|g| g.argv.join(" "))
            .collect();
        Err(format!(
            "不在 [[host_elevated.grant]] 里：{}\n现有授权：{}",
            argv.join(" "),
            if list.is_empty() { "（空）".to_string() } else { list.join("｜") }
        ))
    }
}

#[async_trait]
impl Tool for ElevatedTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "elevated".into(),
            description: "以 root 执行一条**已在授权表里登记过**的命令（D22 L2）。\
这里没有发行版语义：apt、emerge、systemctl、dispatch-conf 一视同仁，\
能不能跑只看 `[[host_elevated.grant]]` 里的 argv 前缀。\
**先只读预览、再动手**是使用者的责任：例如 `emerge --pretend`（只读，通常配 approval=always 免问）\
看完依赖，再用同样的 argv 去掉 `--pretend` 真装（配 approval=once 逐条问）。\
不带 shell、不支持管道重定向——要复杂逻辑就写成脚本，再授权执行那个脚本。\
被拒时不要换写法重试：按返回的「现有授权」清单，缺什么就让主人加一条 grant。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "完整命令数组，argv[0] 必须是绝对路径"
                    },
                    "why": { "type": "string", "description": "一句话说明这次要做什么（会显示在审批卡上）" }
                },
                "required": ["argv"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        EffectClass::External
    }

    fn domain(&self) -> Domain {
        Domain::HostElevated
    }

    fn tier(&self) -> u8 {
        1
    }

    fn lane(&self) -> Lane {
        Lane::External
    }

    /// 命中 `approval = "always"` 的条目时免问（例如 `emerge --pretend`）。
    /// 未命中一律交给策略（通常是 ask）；策略是 `deny` 时这里也不会生效。
    fn skip_approval(&self, args: &serde_json::Value) -> bool {
        let Ok(argv) = Self::argv_of(args) else { return false };
        let g = self.cache.current();
        Self::grant_hint(&g, &argv)
            .ok()
            .and_then(|idx| g.elevated.get(idx - 1))
            .map(|gr| gr.approval == Approval::Always)
            .unwrap_or(false)
    }

    fn approval_hint(&self, args: &serde_json::Value) -> Option<String> {
        let argv = Self::argv_of(args).ok()?;
        let g = self.cache.current();
        let idx = Self::grant_hint(&g, &argv).ok()?;
        let why = args.get("why").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = format!("第 {idx} 条授权：{}", argv.join(" "));
        if !why.is_empty() {
            out.push_str(&format!("\n为什么：{why}"));
        }
        if let Some(gr) = g.elevated.get(idx - 1) {
            if let Some(snap) = &gr.snapshot_argv {
                out.push_str(&format!("\n变更前会存快照：{}", snap.join(" ")));
            }
        }
        Some(out)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let argv = Self::argv_of(&args)?;
        let grants = self.cache.current();
        let idx = Self::grant_hint(&grants, &argv)
            .map_err(|e| ToolError::new(ErrorCode::NotPermitted, e, false))?;

        let out = self
            .client
            .exec(&argv)
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e, true))?;
        if let Some(e) = &out.error {
            return Err(ToolError::new(ErrorCode::NotPermitted, e.clone(), false));
        }

        let mut text = format!(
            "exit={}\n命中授权：第 {} 条\n",
            out.code.map(|c| c.to_string()).unwrap_or("signal".into()),
            out.grant_index.max(idx)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grants::from_toml;

    fn cache_with(toml: &str) -> (tempfile::TempDir, Arc<GrantsCache>) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("chuan.toml");
        std::fs::write(&p, toml).unwrap();
        let c = Arc::new(GrantsCache::new(&p));
        (d, c)
    }

    fn tool(c: Arc<GrantsCache>) -> ElevatedTool {
        ElevatedTool::new(ElevatedClient::new("/nonexistent/host.sock"), c)
    }

    const TOML: &str = r#"
[[host_elevated.grant]]
argv = ["/usr/bin/emerge", "--pretend"]
approval = "always"

[[host_elevated.grant]]
argv = ["/usr/bin/emerge"]
approval = "once"
snapshot_argv = ["/usr/bin/qlist", "-I"]
"#;

    #[test]
    fn always_grant_skips_approval() {
        let (_d, c) = cache_with(TOML);
        let t = tool(c);
        // 命中最长前缀：--pretend 那条是 always
        assert!(t.skip_approval(&serde_json::json!({
            "argv": ["/usr/bin/emerge", "--pretend", "dev-lang/go"]
        })));
        // 真装那条是 once → 要问
        assert!(!t.skip_approval(&serde_json::json!({
            "argv": ["/usr/bin/emerge", "dev-lang/go"]
        })));
    }

    #[test]
    fn unknown_argv_is_refused_with_the_grant_list() {
        let (_d, c) = cache_with(TOML);
        let t = tool(c);
        assert!(!t.skip_approval(&serde_json::json!({
            "argv": ["/usr/bin/rm", "-rf", "/"]
        })));
        let hint = t
            .approval_hint(&serde_json::json!({"argv": ["/usr/bin/emerge"]}))
            .unwrap();
        assert!(hint.contains("第 2 条"), "{hint}");
        assert!(hint.contains("qlist"), "卡片要提示会存快照：{hint}");
    }

    #[test]
    fn argv_must_be_absolute_and_non_empty() {
        let (_d, c) = cache_with(TOML);
        let t = tool(c);
        assert!(t.skip_approval(&serde_json::json!({"argv": []})) == false);
        assert!(t.approval_hint(&serde_json::json!({"argv": ["emerge"]})).is_none());
    }

    #[test]
    fn snapshot_argv_parses_and_legacy_alias_still_works() {
        let g = from_toml(TOML).unwrap();
        assert_eq!(g.elevated[1].snapshot_argv.as_ref().unwrap()[0], "/usr/bin/qlist");

        let legacy = r#"
[[host_elevated.grant]]
argv = ["/usr/bin/apt-get", "install"]
snapshot = "dpkg"
"#;
        let g2 = from_toml(legacy).unwrap();
        assert_eq!(
            g2.elevated[0].snapshot_argv.as_ref().unwrap(),
            &vec!["/usr/bin/dpkg".to_string(), "--get-selections".to_string()]
        );
    }

    #[test]
    fn both_snapshot_forms_is_an_error() {
        let bad = r#"
[[host_elevated.grant]]
argv = ["/usr/bin/x"]
snapshot = "dpkg"
snapshot_argv = ["/usr/bin/y"]
"#;
        let g = from_toml(bad).unwrap();
        let err = g.elevated_error.clone().expect("应标记提权段作废");
        assert!(err.contains("只能写一个"), "{err}");
    }
}
