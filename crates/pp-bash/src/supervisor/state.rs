//! 监督器共享状态、停止原因与进程组租约。

use brush_core::ManagedGroup;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use super::Permit;
use super::process::ProcessRecord;

pub(super) struct Shared {
    pub(super) state: Mutex<State>,
    pub(super) wake: Condvar,
    /// monitor 唤醒计数：每次触发唤醒都会递增。monitor 用它区分“被事件唤醒”
    /// 与“轮询超时”，从而在状态变化后回到 5ms 基线而不是继续退避。
    generation: AtomicU64,
}

impl Shared {
    pub(super) fn new(deadline: Option<Instant>) -> Self {
        Self {
            state: Mutex::new(State::new(deadline)),
            wake: Condvar::new(),
            generation: AtomicU64::new(0),
        }
    }

    /// 唤醒 monitor 并记下一次状态变化事件（新注册、显式停止、解释器结束、
    /// 进程组租约释放）。只递增原子计数并唤醒条件变量，不取注册表锁。
    pub(super) fn notify(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.wake.notify_all();
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Running,
    Stopping,
    Killing,
    Draining,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StopReason {
    TimedOut,
    Cancelled,
    Stopped,
}

impl StopReason {
    pub(super) const fn termination(self) -> super::Termination {
        match self {
            Self::TimedOut => super::Termination::TimedOut,
            Self::Cancelled => super::Termination::Cancelled,
            Self::Stopped => super::Termination::Stopped,
        }
    }
}

pub(super) struct State {
    pub(super) phase: Phase,
    pub(super) deadline: Option<Instant>,
    pub(super) explicit_stop: bool,
    pub(super) interpreter_done: bool,
    pub(super) stop: Option<StopReason>,
    pub(super) permit: Option<Arc<Permit>>,
    pub(super) grace_deadline: Option<Instant>,
    /// SIGKILL 升级后的独立回收截止时间；到期且外部进程仍未被 SIGKILL 终止时
    /// 放弃回收（仅剩解释器待释放的组租约/anchor 时刷新窗口继续等待，不算
    /// 回收失败）。它不复用早已过期的 `grace_deadline`，约束 Killing 阶段的
    /// 等待上界。
    pub(super) kill_deadline: Option<Instant>,
    /// SIGKILL 宽限期已过而外部进程仍未退出：注册表仍保留未回收记录，终态判定
    /// 豁免 live work 检查，记录保持真实的观察/reaped 状态。
    pub(super) abandoned: bool,
    pub(super) errors: BTreeSet<String>,
    pub(super) groups: BTreeMap<u64, GroupRecord>,
    pub(super) procs: BTreeMap<i32, ProcessRecord>,
    /// Every PID this supervisor ever returned from `ManagedProcess::pid()`.
    /// Kept after reaping so a later `kill` on a finished job PID reports
    /// success instead of ESRCH or touching a reused PID.
    pub(super) managed_pids: BTreeSet<i32>,
    pub(super) next_key: u64,
}

impl State {
    pub(super) fn new(deadline: Option<Instant>) -> Self {
        Self {
            phase: Phase::Running,
            deadline,
            explicit_stop: false,
            interpreter_done: false,
            stop: None,
            permit: None,
            grace_deadline: None,
            kill_deadline: None,
            abandoned: false,
            errors: BTreeSet::new(),
            groups: BTreeMap::new(),
            procs: BTreeMap::new(),
            managed_pids: BTreeSet::new(),
            next_key: 0,
        }
    }

    pub(super) fn has_live_work(&self) -> bool {
        !self.groups.is_empty() || !self.procs.is_empty()
    }
}

/// A spawn/terminate lease for one process group. Arc clones given to the
/// interpreter keep the group alive; the registry only holds a Weak handle, so
/// `Drop` runs when the final clone is released and never takes the registry
/// lock (which a caller may already hold inside `spawn`).
pub(super) struct GroupLease {
    pub(super) key: u64,
    pub(super) pgid: i32,
    pub(super) shared: Weak<Shared>,
}

impl ManagedGroup for GroupLease {
    fn pgid(&self) -> i32 {
        self.pgid
    }

    fn key(&self) -> u64 {
        self.key
    }
}

impl Drop for GroupLease {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.notify();
        }
    }
}

pub(super) struct GroupRecord {
    pub(super) pgid: i32,
    pub(super) leader: i32,
    /// The interpreter's group handle; present while any Arc clone is alive.
    pub(super) lease: Weak<GroupLease>,
}
