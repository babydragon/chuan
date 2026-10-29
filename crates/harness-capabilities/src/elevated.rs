//! `hostd` 的客户端（跑在 chuan 侧）。
//!
//! 一次请求一个连接、一行 JSON、一行响应。没有连接池：提权调用本来就低频，
//! 复用连接只会让"谁在什么时候用了什么授权"更难对账。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// apt 装大包可能几分钟；留足余量，真正的上限由 hostd 侧超时决定
const REPLY_TIMEOUT: Duration = Duration::from_secs(360);

#[derive(Debug, Clone, Deserialize)]
pub struct ElevatedOut {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    /// 命中的授权条目（1 起），0 = 未命中
    pub grant_index: usize,
    pub error: Option<String>,
    /// 执行前的快照文件（`snapshot = "dpkg"` 才有）
    #[serde(default)]
    pub snapshot: Option<String>,
}

#[derive(Clone)]
pub struct ElevatedClient {
    socket: PathBuf,
}

impl ElevatedClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into() }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub async fn exec(&self, argv: &[String]) -> Result<ElevatedOut, String> {
        if argv.is_empty() {
            return Err("argv 为空".into());
        }
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| format!("连接 hostd 超时（{}）——服务没起？", self.socket.display()))?
            .map_err(|e| {
                format!(
                    "连接 {} 失败：{e}。确认 chuan-hostd.service 在跑（没有它就没有提权通道）",
                    self.socket.display()
                )
            })?;

        let (rd, mut wr) = stream.into_split();
        let body = serde_json::json!({ "argv": argv });
        wr.write_all(body.to_string().as_bytes()).await.map_err(|e| e.to_string())?;
        wr.write_all(b"\n").await.map_err(|e| e.to_string())?;
        wr.flush().await.map_err(|e| e.to_string())?;

        let mut line = String::new();
        tokio::time::timeout(REPLY_TIMEOUT, BufReader::new(rd).read_line(&mut line))
            .await
            .map_err(|_| "hostd 响应超时".to_string())?
            .map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| format!("hostd 响应不是 JSON：{e}（{line:.200}）"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_socket_gives_actionable_error() {
        let c = ElevatedClient::new("/nonexistent/host.sock");
        let e = c.exec(&["/bin/true".into()]).await.unwrap_err();
        assert!(e.contains("chuan-hostd.service"), "{e}");
    }
}
