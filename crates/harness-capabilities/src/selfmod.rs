//! 自改事务：agent 改自己（技能 / 规范 / 服务 / 角色 / 命令）时共用的机械动作。
//!
//! 统一的只是**事务语义**：原子写、覆盖前备份、保留 N 版、回滚取最新一份、
//! 删除进回收站、每次变更落一条账。**校验强度按载体各自决定**——
//! 命令条目要最严（argv 形状），规范只是文字，不该用同一把尺子。
//!
//! 这些动作以前散在各工具里：service/role 直接 `write`（断电留半截），
//! 删除不可恢复，账本里只有工具参数、看不到结果。这里收口。

use std::path::{Path, PathBuf};

use harness_kernel::{DocId, DocOp, DocWrite};
use sha2::{Digest, Sha256};

/// 每个文件保留几份历史版本。
pub const KEEP_BACKUPS: usize = 5;
/// 备份目录（相对于被管理文件所在的目录）。
pub const BACKUP_DIR: &str = ".backup";
/// 回收站目录：删除先进这里，不真删。
pub const TRASH_DIR: &str = ".trash";

/// 原子写：同目录临时文件 → fsync → rename → fsync 目录。
///
/// 断电/磁盘满时，要么是旧内容，要么是新内容，不会出现半截 TOML。
pub fn write_atomic(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("建目录失败：{e}"))?;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("写临时文件失败：{e}"))?;
        f.write_all(bytes.as_ref()).map_err(|e| format!("写入失败：{e}"))?;
        f.sync_all().map_err(|e| format!("落盘失败：{e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("替换失败：{e}")
    })?;
    if let Some(d) = path.parent().and_then(|dir| std::fs::File::open(dir).ok()) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// 覆盖前备份：`<backup_dir>/<文件名>.<毫秒>`。没有旧文件时返回 `None`。
pub fn backup_file(path: &Path, backup_dir: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(backup_dir).map_err(|e| format!("备份目录不可用：{e}"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("没有文件名")?;
    let dest = backup_dir.join(format!("{name}.{}", harness_kernel::now_ms()));
    std::fs::copy(path, &dest).map_err(|e| format!("备份失败：{e}"))?;
    prune_backups(backup_dir, &name);
    Ok(Some(dest))
}

/// 只保留最近 [`KEEP_BACKUPS`] 份。
pub fn prune_backups(backup_dir: &Path, file_name: &str) {
    let Ok(rd) = std::fs::read_dir(backup_dir) else { return };
    let mut items: Vec<(i64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let ts = name.strip_prefix(&format!("{file_name}."))?.parse::<i64>().ok()?;
            Some((ts, e.path()))
        })
        .collect();
    items.sort_by_key(|(ts, _)| *ts);
    let excess = items.len().saturating_sub(KEEP_BACKUPS);
    for (_, p) in items.into_iter().take(excess) {
        let _ = std::fs::remove_file(&p);
        // 技能备份是目录
        let _ = std::fs::remove_dir_all(&p);
    }
}

/// 最新一份备份：(时间戳, 路径)。
pub fn newest_backup(backup_dir: &Path, file_name: &str) -> Option<(i64, PathBuf)> {
    let rd = std::fs::read_dir(backup_dir).ok()?;
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let ts = name.strip_prefix(&format!("{file_name}."))?.parse::<i64>().ok()?;
            Some((ts, e.path()))
        })
        .max_by_key(|(ts, _)| *ts)
}

/// 删除进回收站：`<trash_dir>/<文件名>.<毫秒>`。返回落点，便于告诉用户怎么捞回来。
pub fn trash(path: &Path, trash_dir: &Path) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(trash_dir).map_err(|e| format!("回收站不可用：{e}"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or("没有文件名")?;
    let dest = trash_dir.join(format!("{name}.{}", harness_kernel::now_ms()));
    std::fs::rename(path, &dest).map_err(|e| format!("移入回收站失败：{e}"))?;
    Ok(Some(dest))
}

/// 每次变更的账：`<kind>:<name>`，payload 里有摘要与动作。
///
/// 为什么统一记：账本里只有工具参数时，事后回答不了"上周装的是哪一版"。
pub fn record(
    kind: &str,
    name: &str,
    action: &str,
    body: &[u8],
    extra: serde_json::Value,
) -> DocWrite {
    let digest: String = Sha256::digest(body)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    DocWrite {
        id: DocId(format!("{kind}:{name}")),
        kind: kind.to_owned(),
        scope: "config".into(),
        payload: serde_json::json!({
            "name": name,
            "action": action,
            "sha256": digest,
            "bytes": body.len(),
            "at": harness_kernel::now_ms(),
            "detail": extra,
        }),
        version: 1,
        target: None,
        op: DocOp::Upsert,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.toml");
        write_atomic(&f, b"v1").unwrap();
        write_atomic(&f, b"v2").unwrap();
        assert_eq!(std::fs::read(&f).unwrap(), b"v2");
        assert!(!dir.path().join(".a.toml.tmp").exists(), "临时文件必须清掉");
    }

    #[test]
    fn backup_keeps_only_five_versions() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.toml");
        let bak = dir.path().join(BACKUP_DIR);
        write_atomic(&f, b"v0").unwrap();
        for i in 0..8 {
            write_atomic(&f, format!("v{i}")).unwrap();
            backup_file(&f, &bak).unwrap();
            // 用不同文件内容保证时间戳可能相同也不影响：这里不依赖 ts 唯一
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let n = std::fs::read_dir(&bak).unwrap().count();
        assert!(n <= KEEP_BACKUPS, "备份应保留 ≤{KEEP_BACKUPS} 份，实际 {n}");
        assert!(newest_backup(&bak, "a.toml").is_some());
    }

    #[test]
    fn trash_moves_instead_of_delete() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.toml");
        write_atomic(&f, b"x").unwrap();
        let moved = trash(&f, &dir.path().join(TRASH_DIR)).unwrap().unwrap();
        assert!(!f.exists());
        assert!(moved.exists(), "回收站里必须留一份");
    }

    #[test]
    fn record_carries_digest_and_action() {
        let w = record("role", "coder", "add", b"body", serde_json::json!({"tools": ["read"]}));
        assert_eq!(w.id.0, "role:coder");
        assert_eq!(w.kind, "role");
        assert_eq!(w.payload["action"], "add");
        assert_eq!(w.payload["bytes"], 4);
        assert_eq!(w.payload["detail"]["tools"][0], "read");
    }
}
