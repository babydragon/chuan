//! L1 宿主执行环境（D22）：`BwrapEnv::host` + 授权表校验。
//!
//! 这里只做三件事：
//! 1. 按授权表生成挂载集（不在表里的路径**不会**出现，出错就是 `NotGranted`）；
//! 2. 遮蔽强制项（`secrets` / `services.d` / `chuan.toml`），调用级不可覆盖；
//! 3. 过滤环境变量——`env -i` 起步，只放行非敏感键。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use harness_kernel::{EnvError, ExecOpts, ExecOutput, ExecutionEnv, GrantKind};
use tokio_util::sync::CancellationToken;

use crate::grants::{normalize, Grants, GrantsCache, Mode, NEVER_MOUNT, PROTECTED};
use crate::sandbox::{BwrapEnv, Mount};

/// 按 PATH 顺序找命令时看的目录（都要先被授权才可用）
const EXE_DIRS: [&str; 5] = ["/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"];

pub struct HostEnv {
    workspace: PathBuf,
    grants: Arc<GrantsCache>,
}

impl HostEnv {
    pub fn new(workspace: impl Into<PathBuf>, grants: Arc<GrantsCache>) -> Self {
        Self { workspace: workspace.into(), grants }
    }

    /// 用指定的授权表执行（`host` 工具带 `grant[]` 时传合并后的临时表）。
    pub async fn exec_with(
        &self,
        argv: &[String],
        opts: ExecOpts,
        grants: &Grants,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, EnvError> {
        if argv.is_empty() {
            return Err(EnvError::Io("empty argv".into()));
        }
        if let Some(reason) = &grants.locked {
            return Err(EnvError::NotGranted {
                kind: GrantKind::Path,
                item: "(授权表)".into(),
                hint: format!("{reason}；修好配置再试，不会回退旧授权"),
            });
        }
        resolve_exe(grants, &argv[0])?;

        // 文件级遮蔽要一个 0 字节的普通文件（见 sandbox::append_masks）。
        // 放在 workspace 的**父目录**（= state 根）：放 workspace 里会让每个 run 的
        // `/ws` 都多一个来历不明的空文件。
        let state_root = self.workspace.parent().unwrap_or(&self.workspace);
        let empty = state_root.join(".mask-empty");
        if !empty.exists() {
            let _ = std::fs::create_dir_all(&self.workspace);
            let _ = std::fs::write(&empty, b"");
        }
        let mut env = BwrapEnv::host(&self.workspace, mounts_for(grants), grants.net_host)
            .with_mask_file(empty);
        // 遮蔽必须显式接上：`/etc` 整块挂进来时，secrets / services.d / chuan.toml 就在下面。
        // （这一行漏掉过一次：授权表、masks_for、单测都在，但没人调用它，于是 L1 里
        //   `cat /etc/chuan/chuan.toml` 真的成功了——2026-10-29 板上发现。）
        for m in masks_for(grants) {
            env = env.with_masked_dir(m);
        }
        let mut opts = opts;
        opts.env = filter_env(opts.env);
        env.exec(argv, opts, cancel).await
    }
}

/// 这次调用要不要打扰人：有点名的临时挂载就要（它们必然是 `once` + 只读）。
pub fn needs_approval(grants: &Grants) -> bool {
    !grants.ephemeral.is_empty()
}

#[async_trait]
impl ExecutionEnv for HostEnv {
    async fn exec(
        &self,
        argv: &[String],
        opts: ExecOpts,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, EnvError> {
        let grants = self.grants.current();
        self.exec_with(argv, opts, &grants, cancel).await
    }
}

/// 授权表 → 挂载集。
///
/// **只有 `approval = "always"` 的持久条目 + 本次点名的临时挂载会进挂载集**：
/// `once` 的含义是"默认看不见，必须每次点名"（点名 → 只读 → 弹审批卡）。
/// 这样"给 agent 一个长期可写目录、但不把 /home 一起敞开"才成立。
///
/// 不存在的路径静默跳过（配置里留了一个尚未创建的目录不该让整条 lane 挂掉）。
pub fn mounts_for(grants: &Grants) -> Vec<Mount> {
    let mut out = Vec::new();
    for g in grants
        .paths
        .iter()
        .filter(|g| g.approval == crate::grants::Approval::Always)
        .chain(grants.ephemeral.iter())
    {
        if !g.path.exists() {
            continue;
        }
        // 加载时查过，但根可能在之后被换成软链（有人把目录替换成 `-> /`）。
        // 这里再查一次：宁可这次少挂一个目录，也不能把根挂进来。
        if let Err(e) = crate::grants::ensure_no_symlink_components(&g.path) {
            tracing::warn!(error = %e, "跳过软链挂载点");
            continue;
        }
        out.push(Mount { src: g.path.clone(), dst: g.path.clone(), ro: g.mode == Mode::Ro });
    }
    out
}

/// 遮蔽哪些：三个强制项里，只要有一个已被授权挂进来（比如 `/etc`），就得盖掉。
pub fn masks_for(grants: &Grants) -> Vec<PathBuf> {
    let all: Vec<&str> = PROTECTED.iter().chain(NEVER_MOUNT.iter()).copied().collect();
    masks_for_in(grants, &all)
}

/// 便于测试：给定主路径与遮蔽项，算"哪些需要盖掉"。
///
/// 判据是"**父目录被授权了**"——只有整块挂进来才需要盖；父目录没挂，路径根本不存在。
fn masks_for_in(grants: &Grants, protected: &[&str]) -> Vec<PathBuf> {
    protected
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists() && grants.paths.iter().any(|g| p.starts_with(&g.path)))
        .collect()
}

/// 解析 `argv[0]`：绝对路径要求本身被授权；裸名字在已授权的标准目录里找。
///
/// 这一步只为了给出**结构化**的失败原因，真正执行仍由 bwrap 内的 PATH 解析。
fn resolve_exe(grants: &Grants, prog: &str) -> Result<PathBuf, EnvError> {
    let not_granted = |item: String, hint: String| EnvError::NotGranted {
        kind: GrantKind::Path,
        item,
        hint,
    };

    if prog.contains('/') {
        let p = normalize(prog).map_err(|e| not_granted(prog.to_owned(), e))?;
        match grants.authorize_path(&p.to_string_lossy(), Mode::Ro) {
            Ok(_) => Ok(p),
            Err(hint) => Err(not_granted(p.display().to_string(), hint)),
        }
    } else {
        for dir in EXE_DIRS {
            let cand = Path::new(dir).join(prog);
            if cand.exists() && grants.best_path(&cand).is_some() {
                return Ok(cand);
            }
        }
        Err(not_granted(
            prog.to_owned(),
            format!(
                "命令 {prog} 不在已授权的目录里；[host.path] 需要包含 {} 之一，或改用绝对路径",
                EXE_DIRS.join(" / ")
            ),
        ))
    }
}

/// `env -i` 起步后只放行非敏感键。名字里带 KEY/TOKEN/SECRET/PASSWORD/CREDENTIAL 的一律丢弃——
/// 宿主 lane 与 daemon 同 uid，环境变量是最后一道"别手滑"的闸（D15）。
pub fn filter_env(env: Vec<(String, String)>) -> Vec<(String, String)> {
    const BAD: [&str; 5] = ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    env.into_iter()
        .filter(|(k, _)| {
            let up = k.to_ascii_uppercase();
            !BAD.iter().any(|b| up.contains(b))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g() -> Grants {
        crate::grants::from_toml(
            r#"
[host]
net = "host"
[host.path]
"/usr" = { mode = "ro" }
"/etc" = { mode = "ro" }
"/var/lib/chuan/timers" = { mode = "rw" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn mounts_follow_the_table_only() {
        let m = mounts_for(&g());
        let dsts: Vec<String> = m.iter().map(|x| x.dst.display().to_string()).collect();
        assert!(dsts.contains(&"/usr".to_string()));
        assert!(dsts.contains(&"/etc".to_string()));
        // 没授权的东西绝不出现
        assert!(!dsts.iter().any(|d| d == "/home"));
        assert!(!dsts.iter().any(|d| d == "/var/log"));
        // 模式跟着表走
        let timers = m.iter().find(|x| x.dst == Path::new("/var/lib/chuan/timers"));
        if let Some(t) = timers {
            assert!(!t.ro, "timers 是 rw");
        }
    }

    #[test]
    fn exe_outside_grants_is_structured() {
        let grants = g();
        // 裸名字：/bin/sh 在 /usr/bin 或 /bin 下，已授权
        assert!(resolve_exe(&grants, "sh").is_ok());
        // 未授权的绝对路径
        match resolve_exe(&grants, "/opt/tool/run") {
            Err(EnvError::NotGranted { kind, item, hint }) => {
                assert_eq!(kind, GrantKind::Path);
                assert_eq!(item, "/opt/tool/run");
                assert!(hint.contains("不在 [host.path] 里"), "{hint}");
            }
            other => panic!("应拒绝，得到 {other:?}"),
        }
    }

    #[test]
    fn locked_table_denies_everything() {
        let grants = Grants::locked("配置写坏了");
        let err = resolve_exe(&grants, "sh").unwrap_err();
        assert!(matches!(err, EnvError::NotGranted { .. }));
    }

    #[test]
    fn secrets_are_always_masked_when_etc_is_granted() {
        let masks = masks_for(&g());
        // 板上才存在这些目录；存在就必须遮
        for p in &masks {
            assert!(PROTECTED.iter().any(|b| Path::new(b) == p.as_path()));
        }
    }

    #[test]
    fn mask_list_follows_granted_parents() {
        let d = tempfile::tempdir().unwrap();
        let etc = d.path().join("etc");
        std::fs::create_dir_all(etc.join("secrets")).unwrap();
        std::fs::write(etc.join("chuan.toml"), b"token = 1").unwrap();
        let toml = format!("[host.path]\n\"{}\" = {{ mode = \"ro\" }}\n", etc.display());
        let grants = crate::grants::from_toml(&toml).unwrap();

        let secrets = etc.join("secrets");
        let cfg = etc.join("chuan.toml");
        let masks = masks_for_in(
            &grants,
            &[secrets.to_str().unwrap(), cfg.to_str().unwrap(), "/nonexistent/x"],
        );
        assert_eq!(masks.len(), 2, "两个存在的都必须遮，不存在的不算");
        assert!(masks.contains(&secrets));
        assert!(masks.contains(&cfg));
    }

    #[test]
    fn env_filter_drops_sensitive_names() {
        let out = filter_env(vec![
            ("PATH".into(), "/usr/bin".into()),
            ("http_proxy".into(), "http://127.0.0.1:7890".into()),
            ("CHUAN_API_KEY".into(), "sk-xxx".into()),
            ("telegram_token".into(), "123:abc".into()),
            ("MY_SECRET".into(), "s".into()),
        ]);
        let keys: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["PATH", "http_proxy"]);
    }
}
