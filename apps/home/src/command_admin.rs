//! 命令条目的安装器（`DECISIONS.md` D31 §7）。
//!
//! 内核（`harness-kernel`）只认 trait，不认目录与 TOML；真正的落盘在这里：
//! - 草稿目录：`capability` 写的提案（`<config>/capabilities/commands/`）
//! - 生效目录：`<config>/commands.d/`，只在主人批准后才写
//!
//! 两份目录分开，是为了让"提案"与"能力"在文件系统上也不可混淆：
//! agent 能写前者（经工具），后者只有这里（被批准后）会碰。

use std::path::PathBuf;

use harness_capabilities::commands;
use harness_kernel::{CommandAdmin, PendingCommand};

pub struct CommandFiles {
    /// 生效目录 `commands.d/`
    dir: PathBuf,
    /// 草稿目录 `<config>/capabilities/commands/`
    drafts: PathBuf,
}

/// 提案的人话说明（没有就用名字兜底，别让通知空着）。
fn read_summary(dir: &std::path::Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{name}.summary")))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("一项新能力（{name}）"))
}

impl CommandFiles {
    pub fn new(dir: impl Into<PathBuf>, drafts: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), drafts: drafts.into() }
    }
}

impl CommandAdmin for CommandFiles {
    fn pending(&self) -> Vec<PendingCommand> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&self.drafts) else {
            return out; // 没有草稿目录 = 没有提案，不是错误
        };
        for e in rd.flatten() {
            let path = e.path();
            let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            match path.extension().and_then(|x| x.to_str()) {
                Some("toml") => {
                    if let Ok(body) = std::fs::read_to_string(&path) {
                        out.push(PendingCommand {
                            name: name.to_owned(),
                            hash: commands::body_hash(&body),
                            summary: read_summary(&self.drafts, name),
                            body: Some(body),
                        });
                    }
                }
                Some("remove") => out.push(PendingCommand {
                    name: name.to_owned(),
                    // 删除提案没有正文：指纹取自"要删的名字"，批准后同样要能对账
                    hash: commands::body_hash(&format!("remove:{name}")),
                    summary: read_summary(&self.drafts, name),
                    body: None,
                }),
                _ => {}
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn install(&self, name: &str, body: &str) -> Result<(), String> {
        commands::install_atomic(&self.dir, name, body).map(|_| ())
    }

    fn discard(&self, name: &str) -> Result<(), String> {
        // 两种草稿都清：批准安装时可能还留着旧的 .remove
        let _ = std::fs::remove_file(self.drafts.join(format!("{name}.toml")));
        let _ = std::fs::remove_file(self.drafts.join(format!("{name}.remove")));
        let _ = std::fs::remove_file(self.drafts.join(format!("{name}.summary")));
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), String> {
        commands::remove_entry(&self.dir, name)
    }
}
