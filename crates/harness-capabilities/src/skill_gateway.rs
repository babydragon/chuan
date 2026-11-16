//! 技能服务网关（D41）：沙盒里的脚本 ↔ 宿主侧的命名服务。
//!
//! 解决的问题：技能脚本需要调一个有鉴权的接口（Basic / Bearer / 签名），
//! 但凭据不能进沙盒（D18），而且默认 lane 没有网卡。
//!
//! 做法：每次 `run_skill` 在宿主侧起一组 **unix socket**，只监听本次执行
//! 声明过的服务；脚本把请求写成一行 JSON，宿主校验 method / path / header
//! 白名单后注入凭据、代发、留痕，再把应答写回。
//!
//! 三条硬边界（与 `http` 工具同一套 `Service` 配置）：
//! 1. 凭据只在宿主：调用方拿不到、也设不了鉴权头；
//! 2. 目的地由配置钉死：脚本只给 path，不给协议/域名；
//! 3. 脚本侧没有网卡：socket 在 `/ws` 下，与 netns 无关。
//!
//! 生命周期：执行结束（含失败/取消）即关闭并删除目录；残留目录由下次启动
//! 清理（见 `GatewaySession::sweep`）。

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::services::{GatewayRequest, HttpServiceTool, Service};

/// 一行请求的上限：JNAP 这类接口的包体都是几 KB。
const MAX_LINE: usize = 96 * 1024;
/// 单次执行里每个服务最多转发多少次。防止脚本把路由器当数据泵。
const MAX_REQUESTS: u32 = 64;
/// 沙盒里网关目录的固定位置（`<workspace>/.g/<id>` → `/ws/.g/<id>`）。
const SANDBOX_DIR: &str = "/ws/.g";

/// 一次技能执行的网关。Drop 时关闭监听、删除 socket。
pub struct GatewaySession {
    dir: PathBuf,
    /// 服务名 → 沙盒里的 socket 路径
    endpoints: BTreeMap<String, String>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl GatewaySession {
    /// 为 `services` 起一组 socket。任一服务未配置、未开放、或白名单为空都直接失败
    /// （fail closed：宁可不跑，也不猜）。
    pub async fn start(
        http: Arc<HttpServiceTool>,
        services: &[String],
        workspace: &str,
    ) -> Result<Self, String> {
        if services.is_empty() {
            return Err("没有声明任何服务".into());
        }
        let id = unique_id();
        let rel = format!(".g/{id}");
        let dir = Path::new(workspace).join(&rel);
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建网关目录失败：{e}"))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("设置网关目录权限失败：{e}"))?;

        let cancel = CancellationToken::new();
        // 先建 session 再开始监听：任何一步失败（`?`）都会走 Drop，清掉半成品目录。
        let mut session = Self {
            dir: dir.clone(),
            endpoints: BTreeMap::new(),
            cancel: cancel.clone(),
            tasks: Vec::new(),
        };
        for (i, name) in services.iter().enumerate() {
            let svc = http
                .get(name)
                .ok_or_else(|| format!("技能申请的服务 `{name}` 没有配置"))?;
            if !svc.expose_to_skills {
                return Err(format!(
                    "服务 `{name}` 没有对技能开放（配置里需要 expose_to_skills = true，且由主人批准）"
                ));
            }
            if svc.allow_methods.is_empty() || svc.allow_paths.is_empty() {
                return Err(format!(
                    "服务 `{name}` 已对技能开放，但 allow_methods / allow_paths 为空（拒绝以全放开兜底）"
                ));
            }
            let sock = dir.join(format!("{i}.sock"));
            let listener = UnixListener::bind(&sock)
                .map_err(|e| format!("监听 {} 失败：{e}", sock.display()))?;
            std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("设置 socket 权限失败：{e}"))?;
            session
                .endpoints
                .insert(name.clone(), format!("{SANDBOX_DIR}/{id}/{i}.sock"));
            session.tasks.push(tokio::spawn(serve(
                listener,
                svc,
                Arc::clone(&http),
                cancel.clone(),
            )));
        }
        Ok(session)
    }

    /// 服务名 → 沙盒内可见的 socket 路径（通过 stdin 的 `_gateway` 给脚本）。
    pub fn endpoints(&self) -> &BTreeMap<String, String> {
        &self.endpoints
    }

    /// 清掉上次异常退出留下的网关目录（只动 `.g/` 下自己的子目录）。
    pub fn sweep(workspace: &str) {
        let root = Path::new(workspace).join(".g");
        let Ok(rd) = std::fs::read_dir(&root) else { return };
        let now = SystemTime::now();
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            let old = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|d| d.as_secs() > 3600)
                .unwrap_or(false);
            if old {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

impl Drop for GatewaySession {
    fn drop(&mut self) {
        self.cancel.cancel();
        for t in &self.tasks {
            t.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ (d.as_secs() << 8))
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos & 0xffff_ffff)
}

async fn serve(
    listener: UnixListener,
    svc: Service,
    http: Arc<HttpServiceTool>,
    cancel: CancellationToken,
) {
    let mut served = 0u32;
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => break,
            r = listener.accept() => r,
        };
        let Ok((stream, _)) = accepted else { break };
        if served >= MAX_REQUESTS {
            let _ = write_reply(
                stream,
                &serde_json::json!({ "error": "本次执行对该服务的调用次数已达上限" }),
            )
            .await;
            continue;
        }
        served += 1;
        let _ = handle(stream, &svc, &http).await;
    }
}

async fn handle(stream: UnixStream, svc: &Service, http: &HttpServiceTool) -> Result<(), String> {
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r.take(MAX_LINE as u64));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .map_err(|e| format!("读取请求失败：{e}"))?;
    if line.trim().is_empty() {
        return write_reply(w, &serde_json::json!({ "error": "空请求" })).await;
    }
    let req: GatewayRequest = match serde_json::from_str(&line) {
        Ok(v) => v,
        Err(e) => {
            return write_reply(w, &serde_json::json!({ "error": format!("请求不是合法 JSON：{e}") }))
                .await;
        }
    };
    let out = match http.gateway_send(svc, req).await {
        Ok(r) => serde_json::to_value(r).unwrap_or_default(),
        Err(e) => serde_json::json!({ "error": e }),
    };
    write_reply(w, &out).await
}

async fn write_reply<W: tokio::io::AsyncWrite + Unpin>(
    mut w: W,
    value: &serde_json::Value,
) -> Result<(), String> {
    let mut text = serde_json::to_string(value).map_err(|e| e.to_string())?;
    text.push('\n');
    w.write_all(text.as_bytes())
        .await
        .map_err(|e| format!("写回应失败：{e}"))?;
    w.flush().await.map_err(|e| format!("写回应失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::{GatewayRequest, Service};
    use std::collections::BTreeMap;

    fn service(name: &str) -> Service {
        Service {
            name: name.into(),
            base: "http://127.0.0.1:1".into(),
            expose_to_skills: true,
            allow_methods: vec!["POST".into()],
            allow_paths: vec!["/JNAP/".into()],
            header_allow: BTreeMap::from([(
                "X-JNAP-Action".to_owned(),
                vec!["GetDeviceInfo".to_owned()],
            )]),
            ..Default::default()
        }
    }

    #[test]
    fn path_allowlist_is_prefix_only_for_trailing_slash() {
        let allowed = vec!["/JNAP/".to_owned(), "/exact".to_owned()];
        assert!(crate::services::gateway_path_allowed("/JNAP/", &allowed));
        assert!(crate::services::gateway_path_allowed("/JNAP/x", &allowed));
        assert!(crate::services::gateway_path_allowed("/exact", &allowed));
        assert!(!crate::services::gateway_path_allowed("/exactly", &allowed));
        assert!(!crate::services::gateway_path_allowed("/other", &allowed));
    }

    #[test]
    fn session_refuses_closed_service() {
        let mut svc = service("velop");
        svc.expose_to_skills = false;
        let http = HttpServiceTool::new(vec![svc], "/tmp/gw-test.log").unwrap();
        let dir = std::env::temp_dir().join(format!("chuan-gw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(GatewaySession::start(http, &["velop".into()], dir.to_str().unwrap()))
            .err()
            .unwrap();
        assert!(err.contains("expose_to_skills"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn request_defaults_to_post() {
        let req: GatewayRequest = serde_json::from_str(r#"{"path":"/JNAP/"}"#).unwrap();
        assert_eq!(req.method, "POST");
        assert!(req.body.is_none());
    }
}
