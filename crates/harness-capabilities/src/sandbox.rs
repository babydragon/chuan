//! bwrap 执行环境。
//!
//! 挂载集与目标板实测一致（见 `docs/PLATFORM.md`）：
//! `/usr` `/etc` 只读，`/lib` `/lib64` `/bin` `/sbin` 按宿主形态（符号链接或目录）处理，
//! 只有 workspace 可写，`/home` `/root` `/var` `/mnt` 一律不可见，无网络。

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use harness_kernel::{EnvError, ExecOpts, ExecOutput, ExecutionEnv};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

const OUTPUT_CAP: usize = 64 * 1024;

/// 沙盒内 `/tmp` 的上限（字节）。tmpfs 计入服务的内存 cgroup：不限制时会长到
/// 内存的一半（板上 8GB → 3.9GB），脚本写满就把整个服务拖进 OOM。
/// 256MB 够放解压包与临时帧；大产物应写 `/ws`（宿主磁盘）。
const TMP_MAX_BYTES: &str = "268435456";

/// 额外挂载：把宿主服务（CUPS socket、D-Bus…）暴露进沙盒。
///
/// 这是**能力授权**，只能由主人写进配置：挂 `/run/cups` 意味着沙盒里任何代码
/// 都能提交/取消打印任务；挂 `docker.sock` 就是容器逃逸级别。默认空。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub ro: bool,
}

/// 不允许被覆盖的挂载点（改这些会破坏隔离本身）。
const RESERVED: &[&str] = &[
    "/", "/usr", "/etc", "/proc", "/dev", "/tmp", "/ws", "/inbox", "/skills", "/media", "/py",
    "/node", "/rt",
];

pub fn validate_mount(m: &Mount) -> Result<(), String> {
    if !m.dst.is_absolute() {
        return Err(format!("挂载目标必须是绝对路径：{}", m.dst.display()));
    }
    let dst = m.dst.to_string_lossy();
    for r in RESERVED {
        if dst == *r || (*r != "/" && dst.starts_with(&format!("{r}/"))) {
            return Err(format!("不能覆盖保留挂载点：{dst}"));
        }
    }
    if !m.src.exists() {
        return Err(format!("源路径不存在：{}", m.src.display()));
    }
    Ok(())
}

pub struct BwrapEnv {
    workspace: PathBuf,
    default_timeout: Duration,
    /// 只读输入目录（用户投递文件用），挂到 /inbox
    inbox: Option<PathBuf>,
    /// 技能目录（只读挂到 /skills）
    skills: Option<PathBuf>,
    /// 用户投递的附件（只读挂到 /media）
    media: Option<PathBuf>,
    /// 主人显式授权的额外挂载
    mounts: Vec<Mount>,
    /// 是否保留网络。默认 false：`--unshare-all` 会连 net 一起隔离。
    network: bool,
    /// 运行时目录（venv / node_modules）：只读挂 `/py`、`/node`；装包时整块可写挂 `/rt`
    runtime: Option<(PathBuf, bool)>,
    /// 宿主 lane（D22 的 L1）：Some 时走完全不同的挂载路径——
    /// 只挂授权表给的路径，不挂 `/usr` `/etc` 默认集，`/proc` 用宿主视图。
    host: Option<HostMounts>,
    /// 要在沙盒里**遮掉**的宿主目录（挂一个空 tmpfs 盖上去）。
    /// 典型用途：`/etc` 是整块只读挂进来的，凭据目录就在它下面——
    /// 不遮的话，沙盒里的脚本能直接读到 token（我们和 daemon 同 uid，权限位挡不住）。
    masked: Vec<PathBuf>,
}

impl BwrapEnv {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            default_timeout: Duration::from_secs(15),
            inbox: None,
            skills: None,
            media: None,
            mounts: Vec::new(),
            network: false,
            runtime: None,
            host: None,
            masked: Vec::new(),
        }
    }

    /// 宿主 lane：按授权表挂载，宿主 netns 可选，**不换 pid namespace**。
    ///
    /// 与 `new()` 的区别（每一条都是 D22 的边界）：
    /// - 不做 `--unshare-pid` / `--unshare-net`：要的是宿主进程表与宿主网络；
    /// - `/proc` 用 `--ro-bind`（`--proc` 会挂一份**新** procfs，看谁都只有自己）；
    /// - `/usr` `/etc` 等**不隐式挂**：装什么由授权表决定，漏了就该报 `NotGranted`。
    ///
    /// `mounts` 由调用方从 `grants` 生成，已经过 `PROTECTED` 校验；这里不再走
    /// `validate_mount`（它保留 `/usr` `/etc` `/proc`，那是普通沙盒的规则）。
    pub fn host(workspace: impl Into<PathBuf>, mounts: Vec<Mount>, net: bool) -> Self {
        Self {
            workspace: workspace.into(),
            default_timeout: Duration::from_secs(15),
            inbox: None,
            skills: None,
            media: None,
            mounts: Vec::new(),
            network: false,
            runtime: None,
            host: Some(HostMounts { mounts, net }),
            masked: Vec::new(),
        }
    }

    pub fn with_default_timeout(mut self, d: Duration) -> Self {
        self.default_timeout = d;
        self
    }

    /// 只读挂载用户的投递目录到 `/inbox`（宿主 /tmp、/home 仍不可见）。
    pub fn with_inbox(mut self, dir: impl Into<PathBuf>) -> Self {
        self.inbox = Some(dir.into());
        self
    }

    /// 只读挂载技能目录到 `/skills`。
    pub fn with_skills(mut self, dir: impl Into<PathBuf>) -> Self {
        self.skills = Some(dir.into());
        self
    }

    /// 额外挂载（配置里显式授权；不合法/源不存在的会被跳过并告警）。
    pub fn with_mounts(mut self, mounts: Vec<Mount>) -> Self {
        self.mounts = mounts.into_iter().filter(|m| match validate_mount(m) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "跳过非法挂载");
                false
            }
        }).collect();
        self
    }

    /// 只读挂载附件目录到 `/media`（通道收到的图片/PDF）。
    pub fn with_media(mut self, dir: impl Into<PathBuf>) -> Self {
        self.media = Some(dir.into());
        self
    }

    /// 保留网络（内网调试用）。**这会同时放开公网**：bwrap 只能整块隔离或放开。
    /// 遮掉某个宿主目录：沙盒里同名路径变成空目录。
    /// 用于凭据目录这类"被整块 /etc 带进来"的东西。
    pub fn with_masked_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.masked.push(dir.into());
        self
    }

    pub fn with_network(mut self, on: bool) -> Self {
        self.network = on;
        self
    }

    /// 运行时目录。`writable=false` 时按生态分别只读挂到 `/py`、`/node`；
    /// `writable=true` 时整块可写挂到 `/rt`（只给装包 lane 用）。
    pub fn with_runtime(mut self, dir: impl Into<PathBuf>, writable: bool) -> Self {
        self.runtime = Some((dir.into(), writable));
        self
    }

    /// 隔离参数：`--unshare-all` 会连网络一起隔离；要联网就得逐项列出。
    fn isolation_args(network: bool) -> &'static [&'static str] {
        if network {
            &[
                "--unshare-user",
                "--unshare-ipc",
                "--unshare-pid",
                "--unshare-uts",
                "--unshare-cgroup-try",
            ]
        } else {
            &["--unshare-all"]
        }
    }

    fn build_command(&self, argv: &[String], opts: &ExecOpts) -> Command {
        if let Some(h) = &self.host {
            return self.build_host_command(h, argv, opts);
        }
        let mut cmd = Command::new("bwrap");
        cmd.args(Self::isolation_args(self.network));
        cmd.args([
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/etc",
            "/etc",
        ]);

        // 合并 usr 的系统上 /lib 等是符号链接；不能直接 ro-bind。
        for dir in ["/lib", "/lib64", "/bin", "/sbin"] {
            let p = Path::new(dir);
            match std::fs::symlink_metadata(p) {
                Ok(m) if m.file_type().is_symlink() => {
                    if let Ok(target) = std::fs::read_link(p) {
                        cmd.arg("--symlink").arg(target).arg(p);
                    }
                }
                Ok(m) if m.is_dir() => {
                    cmd.arg("--ro-bind").arg(p).arg(p);
                }
                _ => {}
            }
        }

        // 凭据目录必须盖掉：/etc 是整块只读挂进来的，而 daemon 与沙盒同 uid，
        // 靠权限位分不开。挂空 tmpfs 后沙盒里看到的是空目录。
        for dir in &self.masked {
            if dir.exists() {
                // 空 tmpfs 盖住，并**重挂成只读**：沙盒里既看不到内容，也不能往这块
                // 内存里写东西——避免出现"看起来是个可写空目录"的误导。
                cmd.arg("--tmpfs").arg(dir);
                cmd.arg("--remount-ro").arg(dir);
            }
        }

        // 联网 lane 的 DNS：`/etc/resolv.conf` 常是指向 /run 的符号链接，
        // 而 /run 默认没挂 —— 只把指向的那个目录补进来，别把整个 /run 暴露了
        // （/run 里有 docker.sock 之类的套接字）。
        if self.network {
            if let Ok(target) = std::fs::canonicalize("/etc/resolv.conf") {
                if !target.starts_with("/etc") {
                    if let Some(dir) = target.parent() {
                        if dir.exists() {
                            cmd.arg("--ro-bind").arg(dir).arg(dir);
                        }
                    }
                }
            }
            // mDNS：`.local` 名字要靠宿主 avahi 的 socket（nss-mdns）解析。沙盒里
            // 看不到 /run，所以 `getent hosts printer.local` 一定失败——而家里大量
            // 设备（打印机 dnssd://、Home Assistant、智能插座）都用 .local 发布。
            // 只挂这一个目录，别把整个 /run 暴露（里面有 docker.sock 之类）。
            //
            // 代价：这条 lane 里的代码能连上 avahi，理论上可以发布/撤销 mDNS 记录。
            // lan lane 本身就要逐条审批，且只在需要时用。
            let avahi = Path::new("/run/avahi-daemon");
            if avahi.exists() {
                cmd.arg("--ro-bind").arg(avahi).arg(avahi);
            }
        }
        // /tmp 是 tmpfs，算在服务的内存 cgroup 里：不设上限时它会长到内存的一半
        // （板上 8GB → 3.9GB），脚本写满就把整个服务拖进 OOM。这里按字节限死，
        // 大产物应该写 /ws（宿主磁盘）。
        cmd.args(["--proc", "/proc", "--dev", "/dev"]);
        cmd.args(["--size", TMP_MAX_BYTES]);
        cmd.arg("--tmpfs").arg("/tmp");
        cmd.arg("--dir").arg("/ws");
        cmd.arg("--bind").arg(&self.workspace).arg("/ws");
        if let Some(inbox) = &self.inbox {
            if inbox.exists() {
                cmd.arg("--ro-bind").arg(inbox).arg("/inbox");
            }
        }
        if let Some(skills) = &self.skills {
            if skills.exists() {
                cmd.arg("--ro-bind").arg(skills).arg("/skills");
            }
        }
        if let Some(media) = &self.media {
            if media.exists() {
                cmd.arg("--ro-bind").arg(media).arg("/media");
            }
        }
        for m in &self.mounts {
            cmd.arg(if m.ro { "--ro-bind" } else { "--bind" }).arg(&m.src).arg(&m.dst);
        }
        let mut path = "/usr/local/bin:/usr/bin:/bin".to_owned();
        if let Some((rt, writable)) = &self.runtime {
            if *writable {
                if rt.exists() {
                    cmd.arg("--bind").arg(rt).arg("/rt");
                }
            } else {
                for (sub, at) in [("py", "/py"), ("node", "/node")] {
                    let d = rt.join(sub);
                    if d.exists() {
                        cmd.arg("--ro-bind").arg(&d).arg(at);
                    }
                }
                if rt.join("py/bin").exists() {
                    path = format!("/py/bin:{path}");
                }
            }
        }
        cmd.arg("--chdir").arg(opts.cwd.clone().unwrap_or_else(|| "/ws".into()));
        cmd.args([
            "--clearenv",
            "--setenv",
            "PATH",
            path.as_str(),
            "--setenv",
            "HOME",
            "/ws",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TMPDIR",
            "/tmp",
        ]);
        for (k, v) in &opts.env {
            cmd.arg("--setenv").arg(k).arg(v);
        }
        cmd.args(argv);
        cmd
    }
}

/// 宿主 lane 的挂载集（由授权表生成）
pub struct HostMounts {
    pub mounts: Vec<Mount>,
    pub net: bool,
}

impl BwrapEnv {
    fn build_host_command(&self, h: &HostMounts, argv: &[String], opts: &ExecOpts) -> Command {
        let mut cmd = Command::new("bwrap");
        // 与普通沙盒共用 userns/ipc/uts/cgroup，但**不** unshare pid 与 net
        cmd.args([
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
            "--unshare-user",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
        ]);
        if !h.net {
            cmd.arg("--unshare-net");
        }
        for m in &h.mounts {
            cmd.arg(if m.ro { "--ro-bind" } else { "--bind" }).arg(&m.src).arg(&m.dst);
        }
        // 合并 usr 的系统上 /bin /lib 是符号链接：只挂 /usr 时，缺了它们连动态链接器
        // 都找不到，表现是 execvp 返回 ENOENT（看着像"命令不存在"，其实是 /lib64/ld-linux 缺了）。
        if h.mounts.iter().any(|m| m.dst == Path::new("/usr")) {
            for d in ["/lib", "/lib64", "/bin", "/sbin"] {
                if let Ok(target) = std::fs::read_link(d) {
                    let t = target.to_string_lossy();
                    // 只补指向 usr/ 的链接；别的目标不在授权范围内，补了也是断链
                    if t.starts_with("usr/") || t.starts_with("/usr/") {
                        cmd.arg("--symlink").arg(target).arg(d);
                    }
                }
            }
        }
        // 宿主进程视图：这是 L3 能读到别的进程的前提
        if !h.mounts.iter().any(|m| m.dst == Path::new("/proc")) {
            cmd.args(["--ro-bind", "/proc", "/proc"]);
        }
        // 遮蔽必须在挂载之后：/etc 整块挂进来时，凭据目录就在它下面
        for dir in &self.masked {
            if dir.exists() {
                cmd.arg("--tmpfs").arg(dir);
                cmd.arg("--remount-ro").arg(dir);
            }
        }
        cmd.args(["--dev", "/dev", "--size", TMP_MAX_BYTES]);
        cmd.arg("--tmpfs").arg("/tmp");
        // /ws 仍是 agent 自己的地盘（脚本可以先写这里，再让 timer 从 /var/lib/chuan/timers 跑）
        let _ = std::fs::create_dir_all(&self.workspace);
        cmd.arg("--dir").arg("/ws");
        cmd.arg("--bind").arg(&self.workspace).arg("/ws");
        cmd.arg("--chdir").arg(opts.cwd.clone().unwrap_or_else(|| "/ws".into()));
        cmd.args([
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
            "--setenv",
            "HOME",
            "/var/lib/chuan",
            "--setenv",
            "LANG",
            "C.UTF-8",
            "--setenv",
            "TMPDIR",
            "/tmp",
        ]);
        // `systemctl --user` 依赖 XDG_RUNTIME_DIR 找 /run/user/<uid> 的控制 socket；
        // 不设的话会直接报 "$XDG_RUNTIME_DIR not defined"（板上实测）。
        let runtime = format!("/run/user/{}", unsafe { libc::getuid() });
        cmd.arg("--setenv").arg("XDG_RUNTIME_DIR").arg(runtime);
        for (k, v) in &opts.env {
            cmd.arg("--setenv").arg(k).arg(v);
        }
        cmd.args(argv);
        cmd
    }
}

struct Capped {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_capped<R: AsyncReadExt + Unpin>(mut r: Option<R>) -> Capped {
    let mut out = Vec::new();
    let mut truncated = false;
    if let Some(reader) = r.as_mut() {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if out.len() + n > OUTPUT_CAP {
                        let room = OUTPUT_CAP.saturating_sub(out.len());
                        out.extend_from_slice(&buf[..room]);
                        truncated = true;
                        // 继续读但丢弃，避免子进程因管道满而阻塞
                        continue;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                Err(_) => break,
            }
        }
    }
    Capped { bytes: out, truncated }
}

#[async_trait]
impl ExecutionEnv for BwrapEnv {
    async fn exec(
        &self,
        argv: &[String],
        opts: ExecOpts,
        cancel: CancellationToken,
    ) -> Result<ExecOutput, EnvError> {
        if argv.is_empty() {
            return Err(EnvError::Io("empty argv".into()));
        }
        std::fs::create_dir_all(&self.workspace)
            .map_err(|e| EnvError::Unavailable(format!("workspace: {e}")))?;

        let mut cmd = self.build_command(argv, &opts);
        cmd.stdin(if opts.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.kill_on_drop(true);

        // 独立进程组，超时才能杀掉整棵树；编译/装包类任务降优先级，
        // 避免把 TG 长轮询与 timer 饿到秒级抖动（D11）。
        let nice = opts.nice;
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if nice {
                    // 尽力而为：nice 失败（权限/平台）不该让任务跑不起来。
                    libc::setpriority(libc::PRIO_PROCESS, 0, 19);
                }
                Ok(())
            });
        }

        let mut child = cmd.spawn().map_err(|e| EnvError::Unavailable(e.to_string()))?;
        let pid = child.id().unwrap_or(0);
        if let Some(input) = opts.stdin.clone() {
            if let Some(mut si) = child.stdin.take() {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = si.write_all(input.as_bytes()).await;
                });
            }
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let out_task = tokio::spawn(read_capped(stdout));
        let err_task = tokio::spawn(read_capped(stderr));

        let timeout = if opts.timeout_ms > 0 {
            Duration::from_millis(opts.timeout_ms)
        } else {
            self.default_timeout
        };

        let status = tokio::select! {
            _ = cancel.cancelled() => {
                kill_group(pid);
                let _ = child.wait().await;
                return Err(EnvError::Cancelled);
            }
            r = tokio::time::timeout(timeout, child.wait()) => match r {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => return Err(EnvError::Io(e.to_string())),
                Err(_) => {
                    kill_group(pid);
                    let _ = child.wait().await;
                    return Err(EnvError::Timeout(timeout.as_millis() as u64));
                }
            }
        };

        let out = out_task.await.unwrap_or(Capped { bytes: Vec::new(), truncated: false });
        let err = err_task.await.unwrap_or(Capped { bytes: Vec::new(), truncated: false });

        Ok(ExecOutput {
            code: status.code(),
            stdout: String::from_utf8_lossy(&out.bytes).into_owned(),
            stderr: String::from_utf8_lossy(&err.bytes).into_owned(),
            truncated: out.truncated || err.truncated,
        })
    }
}

fn kill_group(pid: u32) {
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}
