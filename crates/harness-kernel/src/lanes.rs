//! 并发车道（`DECISIONS.md` D11）：按**资源类型**限流，不按请求数。
//!
//! 目标机只有 0.3–0.6 个现代核。放开并发只会让 TG 长轮询与 timer 抖动，
//! 所以配额按"这件事在吃什么"分：
//!
//! - `io`：本地文件、短命令、沙盒里的轻活（默认 2）
//! - `compile`：编译/装包/技能 dry-run 这类长 CPU 活（默认 1）
//! - `external`：模型请求（默认 1；同时是 provider 侧限流的第一道闸）
//! - `agent`：子 agent 派生（默认 1；P4.1 起使用）
//!
//! 等待可被取消：`/cancel` 与超时必须能立刻退出排队，否则一个长编译会把
//! 排在后面的 run 全挂住。

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

/// 车道。与日志字段 `lane` 一一对应。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Io,
    Compile,
    External,
    Agent,
}

impl Lane {
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::Io => "io",
            Lane::Compile => "compile",
            Lane::External => "external",
            Lane::Agent => "agent",
        }
    }

    fn idx(self) -> usize {
        match self {
            Lane::Io => 0,
            Lane::Compile => 1,
            Lane::External => 2,
            Lane::Agent => 3,
        }
    }
}

/// 配额。0 会被抬到 1（车道不能"完全关闭"，否则一次请求就死锁）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaneLimits {
    pub io: usize,
    pub compile: usize,
    pub external: usize,
    pub agent: usize,
}

impl Default for LaneLimits {
    fn default() -> Self {
        Self { io: 2, compile: 1, external: 1, agent: 1 }
    }
}

/// 运行时车道表。`Clone` 共享同一组信号量。
#[derive(Clone, Debug)]
pub struct Lanes {
    limits: LaneLimits,
    sems: [Arc<Semaphore>; 4],
}

impl Default for Lanes {
    fn default() -> Self {
        Self::new(LaneLimits::default())
    }
}

impl Lanes {
    pub fn new(limits: LaneLimits) -> Self {
        let mk = |n: usize| Arc::new(Semaphore::new(n.max(1)));
        let sems = [
            mk(limits.io),
            mk(limits.compile),
            mk(limits.external),
            mk(limits.agent),
        ];
        Self { limits, sems }
    }

    pub fn limits(&self) -> LaneLimits {
        self.limits
    }

    /// 当前空闲配额（`/status` 用）。
    pub fn available(&self, lane: Lane) -> usize {
        self.sems[lane.idx()].available_permits()
    }

    /// 排队拿许可；取消时立刻返回，不把 run 留在队列里。
    pub async fn acquire(
        &self,
        lane: Lane,
        cancel: &CancellationToken,
    ) -> Result<LaneGuard, LaneCancelled> {
        let sem = Arc::clone(&self.sems[lane.idx()]);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(LaneCancelled(lane)),
            p = sem.acquire_owned() => p
                .map(|permit| LaneGuard { lane, _permit: permit })
                .map_err(|_| LaneCancelled(lane)),
        }
    }

    /// 不等待：拿不到就返回 `None`。后台任务用它给前台让路（P4.3）。
    pub fn try_acquire(&self, lane: Lane) -> Option<LaneGuard> {
        self.sems[lane.idx()]
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| LaneGuard { lane, _permit: permit })
    }
}

/// 持有期间占着配额；`Drop` 即释放。
pub struct LaneGuard {
    lane: Lane,
    _permit: OwnedSemaphorePermit,
}

impl LaneGuard {
    pub fn lane(&self) -> Lane {
        self.lane
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("lane {lane} wait cancelled", lane = .0.as_str())]
pub struct LaneCancelled(pub Lane);

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn limits(compile: usize) -> LaneLimits {
        LaneLimits { io: 2, compile, external: 1, agent: 1 }
    }

    #[tokio::test]
    async fn compile_lane_is_exclusive() {
        let lanes = Lanes::new(limits(1));
        let c = CancellationToken::new();
        let first = lanes.acquire(Lane::Compile, &c).await.unwrap();
        assert_eq!(lanes.available(Lane::Compile), 0);

        let pending = tokio::time::timeout(
            Duration::from_millis(100),
            lanes.acquire(Lane::Compile, &c),
        )
        .await;
        assert!(pending.is_err(), "compile=1 时第二个必须排队");

        drop(first);
        let second = lanes
            .acquire(Lane::Compile, &c)
            .await
            .expect("释放后应立刻拿到");
        assert_eq!(second.lane(), Lane::Compile);
    }

    #[tokio::test]
    async fn lanes_are_independent() {
        let lanes = Lanes::new(limits(1));
        let c = CancellationToken::new();
        let _compile = lanes.acquire(Lane::Compile, &c).await.unwrap();
        // 编译占满不影响模型请求与 io
        let _external = lanes.acquire(Lane::External, &c).await.unwrap();
        let _io = lanes.acquire(Lane::Io, &c).await.unwrap();
    }

    #[tokio::test]
    async fn waiting_is_cancellable() {
        let lanes = Lanes::new(limits(1));
        let c = CancellationToken::new();
        let _held = lanes.acquire(Lane::Compile, &c).await.unwrap();

        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            c2.cancel();
        });
        let got = lanes.acquire(Lane::Compile, &c).await;
        assert_eq!(got.err(), Some(LaneCancelled(Lane::Compile)));
    }

    #[tokio::test]
    async fn zero_becomes_one() {
        let lanes = Lanes::new(LaneLimits { io: 0, compile: 0, external: 0, agent: 0 });
        assert_eq!(lanes.available(Lane::Io), 1);
        assert_eq!(lanes.available(Lane::Compile), 1);
    }

    #[tokio::test]
    async fn try_acquire_does_not_wait() {
        let lanes = Lanes::new(limits(1));
        let c = CancellationToken::new();
        let _held = lanes.acquire(Lane::Compile, &c).await.unwrap();
        assert!(lanes.try_acquire(Lane::Compile).is_none());
        assert!(lanes.try_acquire(Lane::Io).is_some());
    }
}
