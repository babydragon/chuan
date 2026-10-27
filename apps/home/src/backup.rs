//! 备份与保留策略：按天数 + 总量双重约束（SBC 上磁盘比天数更紧）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug)]
pub struct BackupPolicy {
    pub dir: PathBuf,
    pub keep_days: u64,
    pub keep_min: usize,
    pub max_total_mb: u64,
}

impl BackupPolicy {
    pub fn from_env() -> Self {
        let dir = std::env::var("CHUAN_BACKUP_DIR")
            .unwrap_or_else(|_| "/tmp/opencode/chuan/backups".into());
        let keep_days = env_u64("CHUAN_BACKUP_KEEP_DAYS", 7);
        let keep_min = env_u64("CHUAN_BACKUP_KEEP_MIN", 2) as usize;
        let max_total_mb = env_u64("CHUAN_BACKUP_MAX_MB", 256);
        Self { dir: dir.into(), keep_days, keep_min, max_total_mb }
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn next_path(dir: &Path) -> PathBuf {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dir.join(format!("chuan-{secs}.db"))
}

struct Entry {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
}

fn list(dir: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        if !(name.starts_with("chuan-") && name.ends_with(".db")) {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        out.push(Entry {
            path: p,
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
        });
    }
    out.sort_by(|a, b| b.mtime.cmp(&a.mtime)); // 新 → 旧
    out
}

/// 先按天数删，再按总量删；最近 `keep_min` 份永不删除。
pub fn prune(policy: &BackupPolicy) -> std::io::Result<usize> {
    let mut entries = list(&policy.dir);
    let now = SystemTime::now();
    let mut removed = 0;

    let mut kept: Vec<Entry> = Vec::new();
    for (i, e) in entries.drain(..).enumerate() {
        let too_old = now
            .duration_since(e.mtime)
            .map(|d| d > Duration::from_secs(policy.keep_days * 86_400))
            .unwrap_or(false);
        if i < policy.keep_min || !too_old {
            kept.push(e);
        } else {
            std::fs::remove_file(&e.path)?;
            removed += 1;
        }
    }

    let mut total: u64 = kept.iter().map(|e| e.size).sum();
    let cap = policy.max_total_mb * 1024 * 1024;
    while total > cap && kept.len() > policy.keep_min {
        let oldest = kept.pop().expect("len > keep_min");
        std::fs::remove_file(&oldest.path)?;
        total -= oldest.size;
        removed += 1;
    }
    Ok(removed)
}
