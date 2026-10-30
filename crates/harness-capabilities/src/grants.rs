//! 宿主授权表（D22）：`[host.path]` 与 `[[host_elevated.grant]]`。
//!
//! 三条设计前提：
//! 1. **是数据，不是代码**——加一个目录/一条 argv 只改 TOML，不重新编译；
//! 2. **热加载**——按 mtime 重读，改完不用重启（重启会打断正在跑的那一轮，而"开口子"恰好发生在此时）；
//! 3. **fail-closed**——文件解析失败时不是"回退上一版"，而是**全部拒绝**：
//!    回退会让"改坏配置"变成"权限悄悄回到旧状态"，比直接拒绝更难查。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Deserialize;

/// 无论怎么授权都看不见的路径（`[host.path]` 与临时挂载都不能覆盖）。
///
/// `services.d` 里写着 `token_file` 的宿主路径，`chuan.toml` 里有完整策略；
/// `secrets` 是 daemon 自己以 chuan 身份读的（`0440 root:chuan`），不遮等于白做 D18。
pub const PROTECTED: [&str; 3] = [
    "/etc/chuan/secrets",
    "/etc/chuan/services.d",
    "/etc/chuan/chuan.toml",
];

/// 永久不可见的目录（连授权表也不能放开）。`/run` 整块不挂是结构性决定：
/// 里面有 `credentials`、`docker.sock` 之类的东西，只按需挂单个 socket。
pub const NEVER_MOUNT: [&str; 2] = ["/run/credentials", "/run/user/0"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Ro,
    Rw,
}

impl Mode {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "ro" | "read" => Ok(Mode::Ro),
            "rw" | "write" => Ok(Mode::Rw),
            other => Err(format!("mode 只能是 ro / rw，收到 {other}")),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Approval {
    /// 每次调用都要人点头
    Once,
    /// 对完全相同的路径 / argv 前缀免问（由人手写，agent 不能申请）
    Always,
}

impl Approval {
    /// 路径的默认是 `always`：人既然把这一行写进配置，就是"长期可用"的意思。
    /// 想"每次点名才给"必须显式写 `approval = "once"`。
    fn parse_path(s: Option<&str>) -> Result<Self, String> {
        match s {
            None | Some("always") => Ok(Approval::Always),
            Some("once") => Ok(Approval::Once),
            Some(other) => Err(format!("approval 只能是 once / always，收到 {other}")),
        }
    }

    /// 提权命令的默认是 `once`：argv 授权是危险动作，默认逐条问。
    fn parse_argv(s: Option<&str>) -> Result<Self, String> {
        match s {
            None | Some("once") => Ok(Approval::Once),
            Some("always") => Ok(Approval::Always),
            Some(other) => Err(format!("approval 只能是 once / always，收到 {other}")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PathGrant {
    pub path: PathBuf,
    pub mode: Mode,
    pub approval: Approval,
}

#[derive(Clone, Debug)]
pub struct ArgvGrant {
    /// 前缀匹配：`["/usr/bin/apt-get", "install"]` 命中 `apt-get install -y foo`
    pub argv: Vec<String>,
    pub approval: Approval,
    /// 变更前快照的**命令**（data，不是发行版判断）：例如 `["/usr/bin/dpkg","--get-selections"]`、
    /// `["/usr/bin/qlist","-I"]`。hostd 执行前跑它、把 stdout 存进 pkg-snapshots。
    /// 旧写法 `snapshot = "dpkg"` 仍然认，等价于内置的 dpkg 命令。
    pub snapshot_argv: Option<Vec<String>>,
    /// 兼容旧配置：`snapshot = "dpkg"`
    pub snapshot: Option<String>,
    pub note: Option<String>,
}

/// 生效的授权表。`locked` 非空表示解析失败，一切请求都拒绝。
#[derive(Clone, Debug)]
pub struct Grants {
    pub net_host: bool,
    pub paths: Vec<PathGrant>,
    /// 本次调用点名要的额外只读路径（不落盘、不影响下一轮）。
    /// 与 `paths` 分开存，是因为**挂载集只包含 `paths` 里 `always` 的条目 + 这里**。
    pub ephemeral: Vec<PathGrant>,
    pub elevated: Vec<ArgvGrant>,
    /// 审批卡勾"本对话有效"的上限
    pub conversation_ttl_s: u64,
    /// 单次调用能带几条临时挂载
    pub max_ephemeral: usize,
    pub locked: Option<String>,
}

impl Default for Grants {
    fn default() -> Self {
        Self {
            net_host: false,
            paths: Vec::new(),
            ephemeral: Vec::new(),
            elevated: Vec::new(),
            conversation_ttl_s: 1800,
            max_ephemeral: 8,
            locked: None,
        }
    }
}

impl Grants {
    /// 解析失败时的形态：任何请求都拒绝，并带上原因。
    pub fn locked(reason: impl Into<String>) -> Self {
        Self { locked: Some(reason.into()), ..Default::default() }
    }

    pub fn is_locked(&self) -> bool {
        self.locked.is_some()
    }

    /// `capability list` 的第一行：**哪个文件、什么时候、几条**
    pub fn provenance(&self, path: &Path, mtime: Option<SystemTime>) -> String {
        let mtime = mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| format!("{}s", d.as_secs()))
            .unwrap_or_else(|| "?".into());
        format!(
            "来源：{}｜mtime={mtime}（与 `stat -c %Y` 对比）｜路径 {} 条，提权 {} 条{}",
            path.display(),
            self.paths.len(),
            self.elevated.len(),
            if let Some(r) = &self.locked { format!("｜**锁定**：{r}") } else { String::new() }
        )
    }

    /// 找覆盖这个路径的授权（**最长前缀**生效：`/var/lib/chuan/timers` 优先于 `/var/lib/chuan`）。
    pub fn best_path(&self, path: &Path) -> Option<&PathGrant> {
        self.ephemeral
            .iter()
            .chain(self.paths.iter())
            .filter(|g| path.starts_with(&g.path))
            .max_by_key(|g| g.path.components().count())
    }

    /// 只包含持久表的版本（用于"这条授权是第几条"的审计编号）
    fn best_path_persistent(&self, path: &Path) -> Option<&PathGrant> {
        self.paths
            .iter()
            .filter(|g| path.starts_with(&g.path))
            .max_by_key(|g| g.path.components().count())
    }

    /// 审批卡上要显示的"命中第几条"（从 1 开始，与日志里的 `grant_index` 一致）。
    pub fn path_index(&self, grant: &PathGrant) -> usize {
        self.paths.iter().position(|g| std::ptr::eq(g, grant)).map(|i| i + 1).unwrap_or(0)
    }

    /// argv 前缀匹配。要求逐项相等（不做 glob、不看参数个数）：
    /// 一条 `["/usr/bin/apt-get","install"]` 就是"apt-get 装东西"，后面跟什么都不影响匹配——
    /// 这是这个设计的已知弱点，靠逐次审批 + `-s` 预览兜底（见 D22「代价」）。
    pub fn match_argv(&self, argv: &[String]) -> Option<(usize, &ArgvGrant)> {
        self.elevated
            .iter()
            .enumerate()
            .find(|(_, g)| !g.argv.is_empty() && argv.starts_with(&g.argv))
            .map(|(i, g)| (i + 1, g))
    }

    /// 把调用参数里的临时挂载并进来（只读、限量、不可碰 PROTECTED / NEVER_MOUNT）。
    ///
    /// 返回新的 `Grants`：调用方用它构造**一次性**的执行环境，不落盘、不影响下一轮。
    pub fn with_ephemeral(&self, extra: &[String]) -> Result<Grants, String> {
        if extra.is_empty() {
            return Ok(self.clone());
        }
        if extra.len() > self.max_ephemeral {
            return Err(format!(
                "一次最多带 {} 条临时挂载，收到 {} 条：请拆成多次调用，或写进授权表",
                self.max_ephemeral,
                extra.len()
            ));
        }
        let mut out = self.clone();
        for raw in extra {
            let p = normalize(raw)?;
            ensure_not_protected(&p)?;
            ensure_no_symlink_components(&p)?;
            // 点名一个不存在的路径 = 白弹一次审批卡，然后 cat 报 "文件不存在"，
            // 人会以为是权限问题。直接在这里说清楚。
            if !p.exists() {
                return Err(format!(
                    "{} 在本机不存在：临时挂载只接受已存在的路径。\
                     如果本来就想知道文件在不在，直接在命令里 ls 或 test -e，不要点名挂载",
                    p.display()
                ));
            }
            if out.best_path(&p).is_some_and(|g| g.path == p) {
                continue; // 已经在表里（或已经点名过），不必重复
            }
            out.ephemeral.push(PathGrant { path: p, mode: Mode::Ro, approval: Approval::Once });
        }
        Ok(out)
    }

    /// 宿主 cue：某个路径能不能读 / 写。失败时给出**结构化**提示（`NotGranted` 用）。
    pub fn authorize_path(&self, raw: &str, want: Mode) -> Result<(PathBuf, Approval, usize), String> {
        if let Some(reason) = &self.locked {
            return Err(format!("授权表不可用（{reason}）——修好配置再试，不会回退旧授权"));
        }
        let p = normalize(raw)?;
        ensure_not_protected(&p)?;
        let Some(g) = self.best_path(&p) else {
            return Err(format!(
                "{} 不在 [host.path] 里；已授权（ro）：{}",
                p.display(),
                self.ro_summary()
            ));
        };
        if want == Mode::Rw && g.mode == Mode::Ro {
            return Err(format!(
                "{} 只被授权为 ro；要写必须由主人在 [host.path] 里改成 rw",
                p.display()
            ));
        }
        let idx = match self.paths.iter().position(|x| std::ptr::eq(x, g)) {
            Some(i) => i + 1,
            // 临时挂载：审计里记 0（卡片上直接列路径）
            None => 0,
        };
        Ok((p, g.approval, idx))
    }

    fn ro_summary(&self) -> String {
        let mut out: Vec<String> = self
            .paths
            .iter()
            .filter(|g| g.mode == Mode::Ro)
            .map(|g| g.path.display().to_string())
            .collect();
        out.sort();
        if out.is_empty() {
            return "（空）".into();
        }
        if out.len() > 12 {
            out.truncate(12);
            out.push("…".into());
        }
        out.join(" ")
    }
}

/// 规范化为绝对路径：拒绝 `..`（防"用相对路径跳出授权目录"），
/// 不强制要求存在（会写入新文件的路径也要能判断），但会解析存在的部分。
pub fn normalize(raw: &str) -> Result<PathBuf, String> {
    normalize_for_uid(raw, unsafe { libc::getuid() })
}

/// `%U` 展开成**服务账号**的 uid，而不是"读这份配置的进程"的 uid。
///
/// 这一点踩过：`hostd` 以 root 运行，也读同一份 TOML；用 `getuid()` 会展开成
/// `/run/user/0`，命中强制遮蔽 → 整张表判解析失败，连 `host_elevated` 一起废掉
/// （2026-10-30 板上：elevated 报"授权表不可用 /run/user/0"）。
pub fn normalize_for_uid(raw: &str, uid: u32) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("路径为空".into());
    }
    if raw.contains('\0') {
        return Err("路径包含 NUL".into());
    }
    let raw = if raw.contains("%U") {
        raw.replace("%U", &uid.to_string())
    } else {
        raw.to_owned()
    };
    let p = Path::new(&raw);
    if !p.is_absolute() {
        return Err(format!("需要绝对路径，收到 {raw}"));
    }
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(x) => out.push(x),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("路径不允许包含 ..：{raw}"));
            }
            Component::Prefix(_) => return Err(format!("不支持的路径前缀：{raw}")),
        }
    }
    Ok(out)
}

/// 授权可以指向遮蔽目录的**父目录**（`/etc` 是正常授权，里面的凭据目录由 `masks_for` 盖掉），
/// 但不能指向遮蔽目录本身或它下面的东西——那等于直接要 token。
/// 根目录单独拒绝：`/` 会把遮蔽的意义抹掉（还能顺带看到 `/run/credentials`）。
/// 路径链上不允许出现符号链接。
///
/// 这一条挡的是"授权 `~/.local/share/foo`，但 foo 是指向 `/` 的软链"——
/// `--bind` 会跟着源路径走，等于把整个根挂进去。同理，沙盒里一个指向
/// 遮蔽目录的软链也会让 `PROTECTED` 形同虚设（路径相同，仍然指向被遮的那棵）。
///
/// 只检查**已存在**的前缀：`/var/lib/chuan/timers/新脚本.sh` 的后半段还没创建，
/// 允许通过；等它真出现时，`authorize_path` 每次调用都会再查一遍。
pub fn ensure_no_symlink_components(p: &Path) -> Result<(), String> {
    let mut cur = PathBuf::from("/");
    for c in p.components().skip(1) {
        let Component::Normal(name) = c else { continue };
        cur.push(name);
        match std::fs::symlink_metadata(&cur) {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&cur)
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "?".into());
                return Err(format!(
                    "{} 是符号链接（-> {target}）；授权路径不允许经过软链，请直接写真实目录",
                    cur.display()
                ));
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

fn ensure_not_protected(p: &Path) -> Result<(), String> {
    if p == Path::new("/") {
        return Err("不允许授权根目录 /：请逐条给具体目录".into());
    }
    for bad in PROTECTED.iter().chain(NEVER_MOUNT.iter()) {
        let b = Path::new(bad);
        if p.starts_with(b) {
            return Err(format!(
                "{} 落在强制遮蔽目录 {} 里，不可授权（D22）",
                p.display(),
                bad
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 解析 + 热加载
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct HostFile {
    #[serde(default)]
    net: Option<String>,
    #[serde(default)]
    path: BTreeMap<String, PathEntry>,
    #[serde(default)]
    conversation: Option<ConversationFile>,
    #[serde(default)]
    ephemeral: Option<EphemeralFile>,
}

#[derive(Deserialize)]
struct PathEntry {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    approval: Option<String>,
}

#[derive(Deserialize)]
struct ConversationFile {
    #[serde(default)]
    ttl_s: Option<u64>,
}

#[derive(Deserialize)]
struct EphemeralFile {
    #[serde(default)]
    max: Option<usize>,
}

#[derive(Deserialize)]
struct ElevFile {
    #[serde(default)]
    grant: Vec<GrantFile>,
}

#[derive(Deserialize)]
struct GrantFile {
    argv: Vec<String>,
    #[serde(default)]
    approval: Option<String>,
    #[serde(default)]
    snapshot: Option<String>,
    #[serde(default)]
    snapshot_argv: Option<Vec<String>>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct RootFile {
    #[serde(default)]
    host: HostFile,
    #[serde(default)]
    host_elevated: Option<ElevFile>,
}

/// 从一个 TOML 文本构造授权表。任何一项写错都整体拒绝（不做"部分生效"）。
pub fn from_toml(text: &str) -> Result<Grants, String> {
    from_toml_for_uid(text, unsafe { libc::getuid() })
}

/// `uid` 用于展开 `%U`：主进程传自己的 uid，`hostd` 传 `--allow-uid`。
pub fn from_toml_for_uid(text: &str, uid: u32) -> Result<Grants, String> {
    let f: RootFile = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut g = Grants {
        net_host: f.host.net.as_deref() == Some("host"),
        ..Default::default()
    };
    if let Some(c) = &f.host.conversation {
        if let Some(t) = c.ttl_s {
            g.conversation_ttl_s = t;
        }
    }
    if let Some(e) = &f.host.ephemeral {
        if let Some(m) = e.max {
            g.max_ephemeral = m;
        }
    }
    for (raw, entry) in &f.host.path {
        let p = normalize_for_uid(raw, uid)?;
        ensure_not_protected(&p)?;
        ensure_no_symlink_components(&p)?;
        let mode = Mode::parse(entry.mode.as_deref().unwrap_or("ro"))?;
        let approval = Approval::parse_path(entry.approval.as_deref())?;
        if mode == Mode::Rw && NEVER_MOUNT.iter().any(|b| p.starts_with(b)) {
            return Err(format!("{raw} 不允许 rw"));
        }
        // rw + once 没有意义：临时挂载只能 ro，rw 只能来自持久表，
        // 而持久表里的 rw 就是"一直可写"——写错代价不可逆，必须当场说清。
        if mode == Mode::Rw && approval == Approval::Once {
            return Err(format!(
                "{raw} 是 rw，但 approval = \"once\" 表示\"每次点名才挂\"且只能 ro；\
                 要长期可写请改成 approval = \"always\""
            ));
        }
        g.paths.push(PathGrant { path: p, mode, approval });
    }
    // 长路径排前面没有意义（匹配是 longest-prefix），但排序让审批卡的"第 N 条"稳定
    g.paths.sort_by(|a, b| a.path.cmp(&b.path));

    if let Some(e) = &f.host_elevated {
        for gr in &e.grant {
            if gr.argv.is_empty() {
                return Err("host_elevated.grant.argv 不能为空".into());
            }
            if !gr.argv[0].starts_with('/') {
                return Err(format!(
                    "host_elevated.grant.argv[0] 必须是绝对路径，收到 {}",
                    gr.argv[0]
                ));
            }
            let snap_argv = match (&gr.snapshot_argv, gr.snapshot.as_deref()) {
                (Some(a), None) => {
                    if a.is_empty() || !a[0].starts_with('/') {
                        return Err("host_elevated.grant.snapshot_argv 必须是绝对路径命令".into());
                    }
                    Some(a.clone())
                }
                (Some(_), Some(_)) => {
                    return Err("snapshot 与 snapshot_argv 只能写一个（前者是旧的别名）".into())
                }
                (None, Some("dpkg")) => {
                    Some(vec!["/usr/bin/dpkg".into(), "--get-selections".into()])
                }
                (None, Some(other)) => {
                    return Err(format!(
                        "未知的 snapshot 类型：{other}；请改用 snapshot_argv = [\"/path/to/cmd\"]"
                    ))
                }
                (None, None) => None,
            };
            g.elevated.push(ArgvGrant {
                argv: gr.argv.clone(),
                approval: Approval::parse_argv(gr.approval.as_deref())?,
                snapshot_argv: snap_argv,
                snapshot: gr.snapshot.clone(),
                note: gr.note.clone(),
            });
        }
    }
    Ok(g)
}

/// 带 mtime 缓存的授权表。改文件后**下一次调用**就生效，无需重启。
pub struct GrantsCache {
    path: PathBuf,
    /// 展开 `%U` 用的 uid（主进程 = 自己；hostd = --allow-uid）
    uid: u32,
    state: Mutex<(Option<SystemTime>, Grants)>,
}

impl GrantsCache {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::with_uid(path, unsafe { libc::getuid() })
    }

    pub fn with_uid(path: impl Into<PathBuf>, uid: u32) -> Self {
        Self {
            path: path.into(),
            uid,
            state: Mutex::new((None, Grants::locked("尚未加载"))),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 文件的 mtime（给 `capability list` 显示"我到底读的是哪份、什么时候的"）
    pub fn file_mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(&self.path).and_then(|m| m.modified()).ok()
    }

    /// 取当前生效的授权表。文件不存在 → 空表（一切拒绝，但不算"坏"）；
    /// 解析失败 → `locked`（所有请求都会被拒，并带上原因）。
    pub fn current(&self) -> Grants {
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        let mut st = self.state.lock().unwrap();
        if st.0 == mtime && st.0.is_some() {
            return st.1.clone();
        }
        let fresh = match (&mtime, std::fs::read_to_string(&self.path)) {
            (None, _) => Grants::locked(format!("授权表 {} 不存在", self.path.display())),
            (_, Ok(text)) => match from_toml_for_uid(&text, self.uid) {
                Ok(g) => g,
                Err(e) => Grants::locked(format!("{} 解析失败：{e}", self.path.display())),
            },
            (_, Err(e)) => Grants::locked(format!("读取 {} 失败：{e}", self.path.display())),
        };
        // 热加载要留痕：否则"我改了没生效"只能靠猜（板上 2026-10-29 就踩过）。
        match &fresh.locked {
            Some(reason) => tracing::warn!(
                path = %self.path.display(),
                "host 授权表加载失败（全部拒绝）：{reason}"
            ),
            None => tracing::info!(
                path = %self.path.display(),
                paths = fresh.paths.len(),
                elevated = fresh.elevated.len(),
                "host 授权表已加载/重新加载"
            ),
        }
        *st = (mtime, fresh.clone());
        fresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[host]
net = "host"
[host.path]
"/usr" = { mode = "ro" }
"/home" = { mode = "ro", approval = "once" }
"/var/lib/chuan/timers" = { mode = "rw", approval = "always" }
[host.conversation]
ttl_s = 600
[host.ephemeral]
max = 2
[[host_elevated.grant]]
argv = ["/usr/bin/apt-get", "install"]
approval = "once"
snapshot = "dpkg"
"#;

    fn grants() -> Grants {
        from_toml(SAMPLE).expect("sample 应能解析")
    }

    #[test]
    fn parses_paths_and_argv() {
        let g = grants();
        assert!(g.net_host);
        assert_eq!(g.paths.len(), 3);
        assert_eq!(g.conversation_ttl_s, 600);
        assert_eq!(g.max_ephemeral, 2);
        let (idx, gr) = g.match_argv(&["/usr/bin/apt-get".into(), "install".into(), "-y".into()]).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(gr.snapshot.as_deref(), Some("dpkg"));
        // 不是前缀就不匹配（remove 不能借用 install 的授权）
        assert!(g.match_argv(&["/usr/bin/apt-get".into(), "remove".into()]).is_none());
    }

    #[test]
    fn longest_prefix_wins() {
        let g = grants();
        let p = normalize("/var/lib/chuan/timers/a.sh").unwrap();
        let hit = g.best_path(&p).unwrap();
        assert_eq!(hit.mode, Mode::Rw);
        assert_eq!(hit.approval, Approval::Always);

        let p2 = normalize("/usr/bin/sh").unwrap();
        assert_eq!(g.best_path(&p2).unwrap().mode, Mode::Ro);
    }

    #[test]
    fn ro_path_denies_write_with_hint() {
        let g = grants();
        let err = g.authorize_path("/usr/bin/sh", Mode::Rw).unwrap_err();
        assert!(err.contains("只被授权为 ro"), "{err}");
        assert!(g.authorize_path("/usr/bin/sh", Mode::Ro).is_ok());
    }

    #[test]
    fn unknown_path_lists_what_is_granted() {
        let g = grants();
        let err = g.authorize_path("/opt/foo", Mode::Ro).unwrap_err();
        assert!(err.contains("不在 [host.path] 里"), "{err}");
        assert!(err.contains("/usr"), "{err}");
    }

    #[test]
    fn protected_paths_cannot_be_granted() {
        let bad = r#"
[host.path]
"/etc/chuan/secrets" = { mode = "ro" }
"#;
        let err = from_toml(bad).unwrap_err();
        assert!(err.contains("强制遮蔽"), "{err}");

        let g = grants();
        // 临时挂载也不能指着遮蔽目录本身
        assert!(g.with_ephemeral(&["/etc/chuan/services.d".into()]).is_err());
        assert!(g.with_ephemeral(&["/etc/chuan/secrets/token".into()]).is_err());
        // 父目录可以授权（靠 masks_for 盖掉里面的凭据），根目录不行
        assert!(g.with_ephemeral(&["/etc".into()]).is_ok());
        assert!(g.with_ephemeral(&["/".into()]).is_err());
    }

    #[test]
    fn ephemeral_requires_an_existing_path() {
        let g = grants();
        let err = g.with_ephemeral(&["/opt/definitely-not-here".into()]).unwrap_err();
        assert!(err.contains("不存在"), "{err}");
        // 存在的路径照常（/usr 一定有）
        assert!(g.with_ephemeral(&["/usr".into()]).is_ok());
    }

    #[test]
    fn ephemeral_is_ro_limited_and_non_persistent() {
        let g = grants();
        let d = tempfile::tempdir().unwrap();
        let data = d.path().join("data");
        std::fs::create_dir(&data).unwrap();
        std::fs::write(data.join("report.csv"), b"a,b").unwrap();
        let item = data.display().to_string();

        let more = g.with_ephemeral(&[item.clone()]).unwrap();
        let file = data.join("report.csv").display().to_string();
        let (_, approval, idx) = more.authorize_path(&file, Mode::Ro).unwrap();
        assert_eq!(approval, Approval::Once);
        assert_eq!(idx, 0, "临时挂载在审计里记 0（不是持久表的第 N 条）");
        // rw 永远不行：临时挂载只读
        assert!(more.authorize_path(&file, Mode::Rw).is_err());
        // 原表不受影响
        assert!(g.authorize_path(&file, Mode::Ro).is_err());
        // 超出 max=2（都指向已存在路径，确保是因为数量被拒）
        assert!(g
            .with_ephemeral(&[item.clone(), "/usr".into(), "/etc".into()])
            .is_err());
    }

    #[test]
    fn traversal_is_rejected() {
        assert!(normalize("/opt/../etc/passwd").is_err());
        assert!(normalize("relative/x").is_err());
        assert_eq!(normalize("/opt//x/./y").unwrap(), PathBuf::from("/opt/x/y"));
    }

    #[test]
    fn symlink_grants_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink("/etc", &link).unwrap();

        // 授权一个指向 /etc 的软链 → 等于把 /etc 挂进来，必须拒绝
        let toml = format!("[host.path]\n\"{}\" = {{ mode = \"ro\" }}\n", link.display());
        let err = from_toml(&toml).unwrap_err();
        assert!(err.contains("符号链接"), "{err}");

        // 临时挂载同理
        let g = from_toml("[host.path]\n\"/usr\" = { mode = \"ro\" }\n").unwrap();
        let err = g.with_ephemeral(&[link.display().to_string()]).unwrap_err();
        assert!(err.contains("符号链接"), "{err}");

        // 真实目录照常
        let toml = format!("[host.path]\n\"{}\" = {{ mode = \"rw\" }}\n", real.display());
        assert!(from_toml(&toml).is_ok());
    }

    #[test]
    fn symlinks_inside_a_granted_dir_are_fine() {
        // 授权目录**内部**的软链是正常的（`/usr/bin/sh -> bash`），
        // 我们要挡的是"授权根本身是软链"，以及"根在加载后被换成软链"。
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir(&ws).unwrap();
        std::os::unix::fs::symlink("real", ws.join("alias")).unwrap();
        let toml = format!("[host.path]\n\"{}\" = {{ mode = \"rw\" }}\n", ws.display());
        let g = from_toml(&toml).unwrap();
        assert!(g.authorize_path(&ws.join("alias/x").to_string_lossy(), Mode::Ro).is_ok());
    }

    #[test]
    fn bad_toml_locks_everything() {
        let g = from_toml("[host.path]\n\"/usr\" = { mode = \"rwx\" }\n");
        assert!(g.is_err());
        let locked = Grants::locked("坏了");
        assert!(locked.authorize_path("/usr", Mode::Ro).is_err());
    }

    #[test]
    fn shipped_example_parses() {
        // 出厂配置必须能解析：否则主机 lane 一启动就是 locked，而原因只有日志里能看到。
        let text = include_str!("../../../deploy/chuan.toml.example");
        let g = from_toml(text).expect("chuan.toml.example 的 [host] 段应能解析");
        assert!(g.locked.is_none());
        assert!(g.paths.iter().any(|p| p.path == Path::new("/usr")));
        let uid = unsafe { libc::getuid() };
        assert!(
            g.paths.iter().any(|p| p.path == Path::new(&format!("/run/user/{uid}"))),
            "%U 应被替换成当前 uid"
        );
    }

    #[test]
    fn cache_reloads_when_mtime_changes() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("chuan.toml");
        std::fs::write(&f, "[host.path]\n\"/usr\" = { mode = \"ro\" }\n").unwrap();
        let c = GrantsCache::new(&f);
        assert!(c.current().authorize_path("/usr", Mode::Ro).is_ok());
        assert!(c.current().authorize_path("/opt", Mode::Ro).is_err());

        // 加一行（模拟人手改），mtime 变 → 下一次调用生效
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, "[host.path]\n\"/usr\" = { mode = \"ro\" }\n\"/opt\" = { mode = \"ro\" }\n")
            .unwrap();
        assert!(c.current().authorize_path("/opt", Mode::Ro).is_ok());

        // 写坏 → 全部拒绝（不回退旧版）
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, "[host.path\n").unwrap();
        assert!(c.current().is_locked());
        assert!(c.current().authorize_path("/usr", Mode::Ro).is_err());
    }
}
