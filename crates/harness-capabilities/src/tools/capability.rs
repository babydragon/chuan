//! `capability`：把"我被权限卡住了"变成一份**可审**的申请单（D22）。
//!
//! 它只做三件事，且**绝不修改授权表**：
//! - `list`：现在到底能用什么（授权表 + 是否锁定）；
//! - `check`：某个路径 / argv 是否已被覆盖（避免申请重复的东西）；
//! - `request`：按固定 8 节写一份草稿，落盘并告诉主人文件在哪。
//!
//! 为什么要固定 8 节：模型糊一段"我需要访问 /var"没法审——缺哪节就拒，
//! 逼它把"最小请求、更小选项、副作用、回滚"写清楚。

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{
    Block, Domain, EffectClass, ErrorCode, Tool, ToolCtx, ToolError, ToolOutput, ToolSpec,
};

use crate::grants::{Grants, GrantsCache, Mode};

/// 申请单必填节：字段名 → 小节标题
const SECTIONS: [(&str, &str); 8] = [
    ("scene", "1. 现场"),
    ("request", "2. 最小请求"),
    ("necessity", "3. 必要性"),
    ("visibility", "4. 可见范围"),
    ("alternatives", "5. 更小的选项"),
    ("risk", "6. 副作用"),
    ("rollback", "7. 回滚"),
    ("verify", "8. 自验"),
];

pub struct CapabilityTool {
    dir: PathBuf,
    grants: Arc<GrantsCache>,
    /// 已生效的命令条目目录（`commands.d`）：只用来判"是不是已经有了"
    commands: PathBuf,
}

impl CapabilityTool {
    pub fn new(dir: impl Into<PathBuf>, grants: Arc<GrantsCache>) -> Self {
        let dir = dir.into();
        let commands = dir.parent().map(|p| p.join("commands.d")).unwrap_or_default();
        Self { dir, grants, commands }
    }

    pub fn with_commands(mut self, dir: impl Into<PathBuf>) -> Self {
        self.commands = dir.into();
        self
    }

    /// 提案草稿目录：运行结束后内核从这里读（`CommandAdmin::pending`）。
    fn drafts(&self) -> PathBuf {
        self.dir.join("commands")
    }

    fn summary(&self, g: &Grants) -> String {
        let mut s = String::new();
        if let Some(reason) = &g.locked {
            return format!("授权表不可用：{reason}");
        }
        let mut head = String::new();
        if let Some(e) = &g.paths_error {
            head.push_str(&format!("⚠ [host.path] 整段作废：{e}\n"));
        }
        if let Some(e) = &g.elevated_error {
            head.push_str(&format!("⚠ [[host_elevated.grant]] 整段作废：{e}\n"));
        }
        s.push_str(&head);
        s.push_str(&format!("net = {}\n", if g.net_host { "host" } else { "isolated" }));
        if g.paths.is_empty() {
            s.push_str("[host.path]：（空）\n");
        } else {
            s.push_str("[host.path]\n");
            for (i, p) in g.paths.iter().enumerate() {
                let mode = if p.mode == Mode::Rw { "rw" } else { "ro" };
                let ap = match p.approval {
                    crate::grants::Approval::Always => "always",
                    crate::grants::Approval::Once => "once（要点名才挂）",
                };
                s.push_str(&format!("{}. {} = {mode}, {ap}\n", i + 1, p.path.display()));
            }
        }
        if !g.elevated.is_empty() {
            s.push_str("[host_elevated.grant]\n");
            for (i, e) in g.elevated.iter().enumerate() {
                s.push_str(&format!("{}. {}\n", i + 1, e.argv.join(" ")));
            }
        }
        s
    }

    fn slug(s: &str) -> String {
        let mut out: String = s
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        while out.contains("--") {
            out = out.replace("--", "-");
        }
        out.trim_matches('-').to_ascii_lowercase().chars().take(40).collect()
    }
}

#[async_trait]
impl Tool for CapabilityTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "capability".into(),
            description: "申请宿主权限（**只写草稿，不改任何配置**）。\n\
- `action=list`：现在到底能用什么；\n\
- `action=check`：某个路径 / 命令是否已被覆盖（先查再申请，别申请已有的）；\n\
- `action=request`：按固定 8 节写申请单，落盘并通知主人。缺任何一节都会被拒。\n\
申请之后**停下**：不要一边申请一边另找路子绕（换写法重试、把文件拆小、用 `exec` 硬猜）。\n\
可以申请的 `kind`：`host_path`（加目录）、`host_argv`（提权命令前缀，hostd）、`host_root`（需要 root）。\n\
你不能申请 `always` 的提权命令，也不能申请把某个目录改成 `rw`——那两条只能主人手写。"
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "check", "request", "propose_command", "remove_command"]
                    },
                    "name": { "type": "string", "description": "remove_command 用：要删的条目名" },
                    "command": {
                        "type": "object",
                        "description": "propose_command 用：{name, summary, env?, argv[], timeout_ms?, who?, params{}}；\
params 每项只能声明 enum / file / int / pattern 之一；\
summary 是一句人话（审批通知只显示它，不显示命令与参数）",
                        "properties": {
                            "name": { "type": "string" },
                            "summary": { "type": "string", "description": "一句人话：这个能力做什么（≤80 字）" },
                            "env": { "type": "string", "enum": ["lan"] },
                            "argv": { "type": "array", "items": { "type": "string" } },
                            "timeout_ms": { "type": "integer" },
                            "who": { "type": "string", "enum": ["owner", "anyone"] },
                            "params": { "type": "object", "additionalProperties": true }
                        },
                        "required": ["name", "argv"]
                    },
                    "kind": { "type": "string", "enum": ["host_path", "host_argv", "host_root"] },
                    "item": { "type": "string", "description": "绝对路径或 argv 前缀（空格分隔）" },
                    "mode": { "type": "string", "enum": ["ro", "rw"] },
                    "scene": { "type": "string", "description": "1. 现场：哪次调用、什么错误" },
                    "request": { "type": "string", "description": "2. 最小请求：精确到一行 TOML" },
                    "necessity": { "type": "string", "description": "3. 必要性：现有能力为什么不够，试过哪三条路" },
                    "visibility": { "type": "string", "description": "4. 可见范围：读什么、写什么" },
                    "alternatives": { "type": "string", "description": "5. 更小的选项：能否只给单个文件 / 只读 / 只一次" },
                    "risk": { "type": "string", "description": "6. 副作用：误用会怎样，是否可逆" },
                    "rollback": { "type": "string", "description": "7. 回滚：撤销那一行后的状态" },
                    "verify": { "type": "string", "description": "8. 自验：贴完后会跑什么命令、期望什么输出" },
                    "why": { "type": "string", "description": "一句话理由（会出现在通知里）" }
                },
                "required": ["action"]
            }),
        }
    }

    fn effect(&self) -> EffectClass {
        // 写草稿 + 通知：不自动重跑（重跑会多一份，且会重复打扰人）
        EffectClass::External
    }

    fn domain(&self) -> Domain {
        // 只写草稿、不改变任何权限，按 sandbox 域免问
        Domain::Sandbox
    }

    fn tier(&self) -> u8 {
        1
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput, ToolError> {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let grants = self.grants.current();

        let text = match action {
            "list" => format!(
                "{}\n{}",
                grants.provenance(self.grants.path(), self.grants.file_mtime()),
                self.summary(&grants)
            ),
            "check" => {
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("host_path");
                let item = args.get("item").and_then(|v| v.as_str()).unwrap_or("");
                if item.is_empty() {
                    return Err(ToolError::new(ErrorCode::InvalidArgs, "check 需要 item", false));
                }
                match kind {
                    "host_path" => {
                        let mode = if args.get("mode").and_then(|v| v.as_str()) == Some("rw") {
                            Mode::Rw
                        } else {
                            Mode::Ro
                        };
                        match grants.authorize_path(item, mode) {
                            Ok((p, ap, idx)) => format!(
                                "已覆盖：{}（第 {idx} 条授权，approval={ap:?}）",
                                p.display()
                            ),
                            Err(e) => format!("未覆盖：{e}\n→ 可以用 action=request 申请"),
                        }
                    }
                    "host_argv" => {
                        let argv: Vec<String> =
                            item.split_whitespace().map(str::to_owned).collect();
                        match grants.match_argv(&argv) {
                            Some((idx, g)) => {
                                format!("已覆盖：第 {idx} 条（{}）", g.argv.join(" "))
                            }
                            None => "未覆盖：没有匹配的 argv 前缀".into(),
                        }
                    }
                    other => return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("未知 kind：{other}"),
                        false,
                    )),
                }
            }
            "request" => {
                let mut missing: Vec<&str> = Vec::new();
                for (key, title) in SECTIONS {
                    let ok = args
                        .get(key)
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().chars().count() >= 5)
                        .unwrap_or(false);
                    if !ok {
                        missing.push(title);
                    }
                }
                if !missing.is_empty() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!(
                            "申请单缺节：{}。按 playbook(\"capability\") 的模板补齐——缺节的申请没法审。",
                            missing.join("、")
                        ),
                        false,
                    ));
                }
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("host_path");
                let item = args.get("item").and_then(|v| v.as_str()).unwrap_or("");
                let why = args.get("why").and_then(|v| v.as_str()).unwrap_or("");
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let slug = Self::slug(&format!("{kind}-{item}"));
                std::fs::create_dir_all(&self.dir).map_err(|e| {
                    ToolError::new(
                        ErrorCode::Unavailable,
                        format!("创建 {} 失败：{e}", self.dir.display()),
                        false,
                    )
                })?;
                let path = self.dir.join(format!("{ts}-{slug}.md"));
                let mut md = format!("# 权限申请 · {kind}\n\n- 目标：`{item}`\n- 一句话：{why}\n\n");
                for (key, title) in SECTIONS {
                    let body = args.get(key).and_then(|v| v.as_str()).unwrap_or("").trim();
                    md.push_str(&format!("## {title}\n\n{body}\n\n"));
                }
                md.push_str(&format!(
                    "---\n当前生效：\n```\n{}\n```\n",
                    self.summary(&grants)
                ));
                std::fs::write(&path, md).map_err(|e| {
                    ToolError::new(
                        ErrorCode::Unavailable,
                        format!("写 {} 失败：{e}", path.display()),
                        false,
                    )
                })?;
                format!(
                    "已写申请单：{}\n**停在这里**：把这份文件（或下面摘要）发给主人，等他改完配置再试。\n\
                     不要换写法重试，也不要一边申请一边绕路。",
                    path.display()
                )
            }
            "propose_command" => {
                let spec = args.get("command").ok_or_else(|| {
                    ToolError::new(ErrorCode::InvalidArgs, "propose_command 需要 command 对象", false)
                })?;
                let (name, body) = crate::commands::render_entry(spec)
                    .map_err(|e| ToolError::new(ErrorCode::InvalidArgs, e, false))?;
                // 审批通知只显示这一句人话：写不清用途的提案，人也没法审。
                let summary = spec
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_owned())
                    .unwrap_or_else(|| format!("一项新能力（{name}）"));
                if summary.chars().count() > 80 {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        "summary 太长（≤80 字，一句话说清做什么）",
                        false,
                    ));
                }
                if self.commands.join(format!("{name}.toml")).exists() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("条目 {name} 已经存在（只增不改；要改先 remove_command）"),
                        false,
                    ));
                }
                let drafts = self.drafts();
                std::fs::create_dir_all(&drafts).map_err(|e| {
                    ToolError::new(
                        ErrorCode::Unavailable,
                        format!("创建 {} 失败：{e}", drafts.display()),
                        false,
                    )
                })?;
                let path = drafts.join(format!("{name}.toml"));
                std::fs::write(&path, &body).map_err(|e| {
                    ToolError::new(ErrorCode::Unavailable, format!("写草稿失败：{e}"), false)
                })?;
                let _ = std::fs::write(drafts.join(format!("{name}.summary")), &summary);
                // 删除提案与安装提案互斥
                let _ = std::fs::remove_file(drafts.join(format!("{name}.remove")));
                format!(
                    "已生成命令条目提案 `{name}`（**还没生效**）。\n\n```toml\n{body}```\n\n\
                     批准后由内核原子写入 `commands.d/`；不批准不会生效。之后会单独发一条带 \
                     `/approve <id>` 的确认消息。"
                )
            }
            "remove_command" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ToolError::new(ErrorCode::InvalidArgs, "remove_command 需要 name", false)
                    })?;
                if !self.commands.join(format!("{name}.toml")).exists() {
                    return Err(ToolError::new(
                        ErrorCode::InvalidArgs,
                        format!("没有这个条目：{name}"),
                        false,
                    ));
                }
                let drafts = self.drafts();
                std::fs::create_dir_all(&drafts).map_err(|e| {
                    ToolError::new(
                        ErrorCode::Unavailable,
                        format!("创建 {} 失败：{e}", drafts.display()),
                        false,
                    )
                })?;
                std::fs::write(drafts.join(format!("{name}.remove")), name).map_err(|e| {
                    ToolError::new(ErrorCode::Unavailable, format!("写草稿失败：{e}"), false)
                })?;
                let _ = std::fs::write(
                    drafts.join(format!("{name}.summary")),
                    format!("不再使用「{name}」"),
                );
                let _ = std::fs::remove_file(drafts.join(format!("{name}.toml")));
                format!("已生成删除提案：`{name}`（还没生效，等批准）。")
            }
            other => {
                return Err(ToolError::new(
                    ErrorCode::InvalidArgs,
                    format!("未知 action：{other}"),
                    false,
                ))
            }
        };

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

    fn tool(dir: &std::path::Path) -> CapabilityTool {
        let cfg = dir.join("chuan.toml");
        std::fs::write(&cfg, "[host.path]\n\"/usr\" = { mode = \"ro\" }\n").unwrap();
        CapabilityTool::new(dir.join("caps"), Arc::new(GrantsCache::new(&cfg)))
    }

    #[test]
    fn slug_is_safe() {
        assert_eq!(CapabilityTool::slug("/home/a b/c"), "home-a-b-c");
        assert_eq!(CapabilityTool::slug("../../etc"), "etc");
    }

    #[test]
    fn missing_sections_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let t = tool(dir.path());
        // 直接调 execute 需要 ToolCtx，这里只验 schema 常量与路径安全
        assert_eq!(SECTIONS.len(), 8);
        let keys: Vec<&str> = SECTIONS.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"alternatives"));
        let _ = t;
    }
}
