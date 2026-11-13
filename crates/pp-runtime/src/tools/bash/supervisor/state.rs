//! 监督器共享状态、停止原因与进程组租约。

use brush_core::ManagedGroup;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Condvar, Mutex, Weak},
    time::Instant,
};

use super::Permit;
use super::process::ProcessRecord;

pub(super) struct Shared {
    pub(super) state: Mutex<State>,
    pub(super) wake: Condvar,
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
            shared.wake.notify_all();
        }
    }
}

pub(super) struct GroupRecord {
    pub(super) pgid: i32,
    pub(super) leader: i32,
    /// The interpreter's group handle; present while any Arc clone is alive.
    pub(super) lease: Weak<GroupLease>,
}
