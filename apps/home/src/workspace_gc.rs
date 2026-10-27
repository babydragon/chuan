//! workspace 清理：`/ws` 是草稿区，不是档案库。
//!
//! 两条约束（SBC 上磁盘比天数更紧，和备份同理）：
//! - 超过 `keep_days` 的文件删掉；
//! - 总量超过 `max_bytes` 时，从最旧的开始删到限额以内。
//!
//! 只动 workspace 内部；不跟随符号链接；清完顺手删空目录（根目录保留）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub removed_files: usize,
    pub removed_bytes: u64,
    pub kept_files: usize,
    pub kept_bytes: u64,
}

#[derive(Clone, Debug)]
struct Entry {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
}

fn collect(root: &Path, out: &mut Vec<Entry>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            // 不跟随也不删：避免把工作区外的目标一起带走
            continue;
        }
        let p = e.path();
        if ft.is_dir() {
            collect(&p, out);
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        out.push(Entry {
            path: p,
            mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            size: meta.len(),
        });
    }
}

fn prune_empty_dirs(root: &Path) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        prune_empty_dirs(&p);
        // 空目录才删；`publish/` 里还有草稿时会被保留
        let _ = std::fs::remove_dir(&p);
    }
}

/// 一次清理。返回删了多少（便于记账，不静默）。
pub fn sweep(root: &Path, keep_days: u64, max_bytes: u64, now: SystemTime) -> Report {
    let mut entries = Vec::new();
    collect(root, &mut entries);

    let mut report = Report::default();
    // 0 = 该维度不设限（与 wall_clock_ms 的约定一致）
    let cutoff = if keep_days == 0 {
        None
    } else {
        now.checked_sub(Duration::from_secs(keep_days.saturating_mul(86_400)))
    };

    // ① 过期
    let mut survivors = Vec::new();
    for e in entries {
        let expired = cutoff.map(|c| e.mtime < c).unwrap_or(false);
        if expired {
            if std::fs::remove_file(&e.path).is_ok() {
                report.removed_files += 1;
                report.removed_bytes += e.size;
                continue;
            }
        }
        survivors.push(e);
    }

    // ② 总量：从最旧的删到限额内
    let mut total: u64 = survivors.iter().map(|e| e.size).sum();
    if total > max_bytes {
        survivors.sort_by_key(|e| e.mtime);
        let mut i = 0;
        while total > max_bytes && i < survivors.len() {
            let e = &survivors[i];
            if std::fs::remove_file(&e.path).is_ok() {
                total -= e.size;
                report.removed_files += 1;
                report.removed_bytes += e.size;
            }
            i += 1;
        }
        survivors.drain(0..i);
    }

    report.kept_files = survivors.len();
    report.kept_bytes = total;
    prune_empty_dirs(root);
    report
}

/// 现在就跑一次（启动时 + 周期任务共用）。
pub fn sweep_now(root: &Path, keep_days: u64, max_mb: u64) -> Report {
    let cap = if max_mb == 0 { u64::MAX } else { max_mb.saturating_mul(1024 * 1024) };
    sweep(root, keep_days, cap, SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, bytes: usize, age_days: u64) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, vec![b'x'; bytes]).unwrap();
        if age_days > 0 {
            let when = SystemTime::now() - Duration::from_secs(age_days * 86_400);
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(when).unwrap();
        }
        p
    }

    #[test]
    fn removes_expired_files_and_keeps_fresh_ones() {
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), "old.txt", 10, 30);
        let fresh = write(dir.path(), "fresh.txt", 10, 0);
        let r = sweep_now(dir.path(), 7, 100);
        assert!(!old.exists(), "过期文件应删除");
        assert!(fresh.exists(), "新文件应保留");
        assert_eq!(r.removed_files, 1);
        assert_eq!(r.kept_files, 1);
    }

    #[test]
    fn enforces_total_size_from_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "a.txt", 100, 3);
        let b = write(dir.path(), "b.txt", 100, 2);
        let c = write(dir.path(), "c.txt", 100, 1);
        // 300 字节 > 150 → 删最旧的两个
        let r = sweep_now(dir.path(), 7, 0);
        assert_eq!(r.removed_files, 0, "max_mb=0 表示不按总量删");
        assert!(a.exists() && b.exists() && c.exists());

        let r = sweep(dir.path(), 7, 150, SystemTime::now());
        assert!(!a.exists() && !b.exists(), "从最旧的开始删");
        assert!(c.exists(), "最新的留下");
        assert_eq!(r.kept_bytes, 100);
    }

    #[test]
    fn cleans_empty_dirs_but_keeps_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "publish/text-upper/skill.json", 5, 30);
        sweep_now(dir.path(), 7, 100);
        assert!(dir.path().exists(), "根目录保留");
        assert!(!dir.path().join("publish").exists(), "空目录应清掉");
    }

    #[test]
    fn leaves_non_empty_dirs_alone() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "keep/a.txt", 5, 0);
        sweep_now(dir.path(), 7, 100);
        assert!(dir.path().join("keep/a.txt").exists());
    }
}
