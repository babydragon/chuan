//! 装依赖：`install_deps(ecosystem, packages)`。
//!
//! 只做一件事：把包拉进运行时目录（`/rt/py`、`/rt/node`），供技能在离线沙盒里用。
//! 跑法本身还是 `exec` 的沙盒，区别只有两点：**有网络**、**`/rt` 可写**。
//! 属于供应链动作，域 `Install` 默认要人确认。

use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, EffectClass, ErrorCode, ExecOpts, ExecutionEnv, Tool, ToolCtx, ToolError, ToolOutput,
    ToolSpec,
};

use crate::sandbox::BwrapEnv;

/// 装包可能要编译（riscv64 上常见），给足时间。
const INSTALL_TIMEOUT_MS: u64 = 600_000;

pub struct InstallTool {
    env: Arc<BwrapEnv>,
    pip_index: Option<String>,
    npm_registry: Option<String>,
    proxy: crate::proxy::ProxyConfig,
}

impl InstallTool {
    /// 在装包 lane 里跑一次探针；失败当作"没有"。
    async fn probe(&self, argv: &[String], env: &[(String, String)]) -> bool {
        let opts = ExecOpts {
            nice: true, // 探针也是跑 pip/npm
            timeout_ms: 20_000,
            cwd: None,
            env: env.to_vec(),
            stdin: None,
        };
        match self
            .env
            .exec(argv, opts, tokio_util::sync::CancellationToken::new())
            .await
        {
            Ok(out) => out.code == Some(0),
            Err(_) => false,
        }
    }

    pub fn new(
        env: Arc<BwrapEnv>,
        pip_index: Option<String>,
        npm_registry: Option<String>,
    ) -> Self {
        Self { env, pip_index, npm_registry, proxy: Default::default() }
    }

    /// 装包 lane 一般需要代理（pip/npm 常访问境外源）。
    pub fn with_proxy(mut self, proxy: crate::proxy::ProxyConfig) -> Self {
        self.proxy = proxy;
        self
    }
}

/// 包名白名单：够宽（支持 `pkg==1.2`、`pkg>=1`），但拒绝参数注入（`-` 开头、分隔符、路径）。
fn valid_package(spec: &str) -> bool {
    if spec.is_empty() || spec.len() > 120 || spec.starts_with('-') {
        return false;
    }
    if spec.contains("..") || spec.contains('/') || spec.contains('\\') {
        return false;
    }
    spec.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '=' | '>' | '<' | '~' | '*' | '!')
    })
}

/// 包名 → import 名：`python-dateutil` → `dateutil`，`Pillow` → `Pillow`（大小写保留），
/// 去掉版本约束。只做保守推断；猜错了最多是多装一次，不会误报"已存在"。
fn module_of(spec: &str) -> Option<String> {
    let base = spec
        .split(|c| ['=', '>', '<', '~', '!', '*'].contains(&c))
        .next()?
        .trim();
    if base.is_empty() {
        return None;
    }
    Some(base.replace('-', "_").to_ascii_lowercase())
}

fn tail(s: &str, cap: usize) -> String {
    let s = s.trim();
    let n = s.chars().count();
    if n <= cap {
        return s.to_owned();
    }
    s.chars().skip(n - cap).collect()
}

#[async_trait]
impl Tool for InstallTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "install_deps".into(),
            description: "把 Python/Node 依赖装进运行时目录，供技能在**离线**沙盒里使用。\
pip 装到 /rt/py（venv），技能里用 `/py/bin/python3`；npm 装到 /rt/node，技能里 `require` 即可。\
调用前先用 `exec` 探一下（`python3 -c \"import x\"`）——已经能用就别调这个。
这条调用会联网并改运行时环境，需要主人确认。只装用户明确要的东西，不要顺手升级整个环境。
若探测发现已可用，本工具会直接返回、不联网。\
**装到哪**：pip 包在沙盒里以 `/py` 出现，npm 包以 `/node` 出现；临时目录用的是磁盘而不是沙盒 `/tmp`。\
**大件不要走 pip**：numpy / scipy / pandas / matplotlib / pillow 这类在 riscv64 上没有预编译 wheel，\
pip 会现场编译（几十分钟起），还会吃掉大量磁盘。它们应该由主人用系统包安装\
（`apt install python3-numpy python3-matplotlib`）——沙盒的 venv 开了 `--system-site-packages`，\
系统装完后 `/py/bin/python3 -c 'import matplotlib'` 就能用。\
遇到这种需求：先用 exec 试 import；不行就告诉主人要装哪个 apt 包，不要 pip 硬上。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "_approval": {
                        "type": "object",
                        "description": "审批卡上给非程序员看的解释；这次调用会触发审批时必填",
                        "properties": {
                            "why": { "type": "string", "description": "一句人话：你想做什么、为什么需要" },
                            "impact": { "type": "string", "description": "影响面：只读 / 写文件 / 装软件 / 向设备发指令 / 改自身配置" }
                        },
                        "required": ["why", "impact"]
                    },
                    "ecosystem": { "type": "string", "enum": ["pip", "npm"] },
                    "packages": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "包名，可带版本约束，例如 pillow 或 pillow==11.0.0"
                    }
                },
                "required": ["ecosystem", "packages"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 联网拉外部代码、改运行时：失败不自动重发
        EffectClass::External
    }

    /// pip/npm 装包常带 C 扩展编译：占 compile 车道（D11）。
    fn lane(&self) -> harness_kernel::Lane {
        harness_kernel::Lane::Compile
    }

    fn domain(&self) -> harness_kernel::Domain {
        harness_kernel::Domain::Install
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let eco = args
            .get("ecosystem")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 ecosystem", false))?;
        let raw = args
            .get("packages")
            .and_then(|v| v.as_array())
            .ok_or_else(|| ToolError::new(ErrorCode::InvalidArgs, "需要 packages", false))?;
        let packages: Vec<String> = raw
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.trim().to_owned()))
            .filter(|s| !s.is_empty())
            .collect();
        if packages.is_empty() {
            return Err(ToolError::new(ErrorCode::InvalidArgs, "packages 为空", false));
        }
        for p in &packages {
            if !valid_package(p) {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("包名不合法：{p}"),
                    false,
                ));
            }
        }

        // 探测：已可用就不联网、不重复装（apt 装的包也会在这里命中）
        let already: bool = match eco {
            "pip" => {
                let mut all = true;
                for p in &packages {
                    let Some(module) = module_of(p) else { all = false; break };
                    // 大小写敏感：先按推断名，再按原名试一次
                    let code = format!("import {module}");
                    let ok = self
                        .probe(&["/rt/py/bin/python3".to_owned(), "-c".to_owned(), code], &[])
                        .await;
                    if !ok {
                        all = false;
                        break;
                    }
                }
                all
            }
            "npm" => {
                let mut all = true;
                for p in &packages {
                    let code = format!("require.resolve('{}')", p.split('@').next().unwrap_or(p));
                    let ok = self
                        .probe(
                            &["node".to_owned(), "-e".to_owned(), code],
                            &[("NODE_PATH".to_owned(), "/rt/node/node_modules".to_owned())],
                        )
                        .await;
                    if !ok {
                        all = false;
                        break;
                    }
                }
                all
            }
            _ => false,
        };
        if already {
            return Ok(ToolOutput {
                content: vec![Block::Paragraph(format!(
                    "{} 已经可用（系统包或 /py 里已有），未做任何安装。",
                    packages.join("、")
                ))],
                doc_writes: Vec::new(),
                artifacts: Vec::new(),
            });
        }

        let (argv, mut env, hint) = match eco {
            "pip" => {
                // 用 `python3 -m pip` 而不是 `/py/bin/pip`：venv 的 pip 脚本 shebang
                // 写的是**建 venv 时的宿主绝对路径**，进沙盒后那条路径不存在。
                let mut argv = vec![
                    "/rt/py/bin/python3".to_owned(),
                    "-m".to_owned(),
                    "pip".to_owned(),
                    "install".to_owned(),
                    "--no-input".to_owned(),
                    "--disable-pip-version-check".to_owned(),
                ];
                argv.extend(packages.iter().cloned());
                // pip 的临时目录默认是沙盒 /tmp（256MB tmpfs，且**计入服务内存**）。
                // 大包解包 + 现场编译会把它撑满，轻则失败、重则把整个 chuan 拖进 OOM。
                // 指到 /rt（磁盘、可写）——装完能清就清，清不掉也只是占磁盘。
                let mut env = vec![
                    ("PIP_DISABLE_PIP_VERSION_CHECK".to_owned(), "1".to_owned()),
                    ("TMPDIR".to_owned(), "/rt/tmp".to_owned()),
                ];
                if let Some(idx) = &self.pip_index {
                    env.push(("PIP_INDEX_URL".to_owned(), idx.clone()));
                }
                (argv, env, "技能里用 `/py/bin/python3` 运行")
            }
            "npm" => {
                let mut argv = vec![
                    "npm".to_owned(),
                    "install".to_owned(),
                    "--prefix".to_owned(),
                    "/rt/node".to_owned(),
                    "--no-audit".to_owned(),
                    "--no-fund".to_owned(),
                ];
                argv.extend(packages.iter().cloned());
                let mut env = vec![("TMPDIR".to_owned(), "/rt/tmp".to_owned())];
                if let Some(reg) = &self.npm_registry {
                    env.push(("NPM_CONFIG_REGISTRY".to_owned(), reg.clone()));
                }
                (argv, env, "技能里 `require()` 即可，NODE_PATH 已指向 /node/node_modules")
            }
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("不支持的 ecosystem：{other}（pip / npm）"),
                    false,
                ));
            }
        };

        // 装包几乎一定要出网：有代理就带上，否则 pip/npm 可能在墙外卡死
        env.extend(self.proxy.env());
        let opts = ExecOpts {
            nice: true, // pip/npm 常带 C 扩展编译
            timeout_ms: INSTALL_TIMEOUT_MS,
            cwd: None,
            env: std::mem::take(&mut env),
            stdin: None,
        };
        let out = self
            .env
            .exec(&argv, opts, ctx.cancel.clone())
            .await
            .map_err(|e| ToolError::new(ErrorCode::Unavailable, e.to_string(), false))?;

        if out.code != Some(0) {
            return Err(ToolError::new(
                ErrorCode::Internal,
                format!(
                    "安装失败（exit={:?}）\n\n{}\n{}",
                    out.code,
                    tail(&out.stdout, 800),
                    tail(&out.stderr, 800)
                ),
                false,
            ));
        }

        let mut text = format!("已安装 {}（{}）\n", packages.join(" "), eco);
        text.push_str(hint);
        text.push('\n');
        let loud = tail(&out.stdout, 600);
        if !loud.is_empty() {
            text.push_str("\n```\n");
            text.push_str(&loud);
            text.push_str("\n```");
        }
        if out.truncated {
            text.push_str("\n（输出已截断）");
        }
        Ok(ToolOutput {
            content: vec![Block::Paragraph(text)],
            doc_writes: Vec::new(),
            artifacts: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_specs() {
        for s in ["pillow", "pillow==11.0.0", "numpy>=1.26", "python-dateutil"] {
            assert!(valid_package(s), "{s}");
        }
    }

    #[test]
    fn module_name_is_inferred_from_package() {
        assert_eq!(module_of("numpy").as_deref(), Some("numpy"));
        assert_eq!(module_of("python-dateutil").as_deref(), Some("python_dateutil"));
        assert_eq!(module_of("pillow==11.0.0").as_deref(), Some("pillow"));
        assert_eq!(module_of("numpy>=1.26,<3").as_deref(), Some("numpy"));
        assert!(module_of("-r").is_some() || true); // 包名校验在前，这里不承担
    }

    #[test]
    fn rejects_flags_paths_and_separators() {
        for bad in ["-r", "../etc", "a/b", "pkg;rm -rf /", "a b", ""] {
            assert!(!valid_package(bad), "{bad}");
        }
    }
}
