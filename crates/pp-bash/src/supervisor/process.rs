//! 受监督子进程的 poll 单元与注册记录。

use brush_core::ManagedProcess;
use std::{
    io,
    process::ExitStatus,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};

use super::lock;

#[derive(Clone)]
pub(super) enum CellOutcome {
    Status(ExitStatus),
    Failed(io::ErrorKind, String),
}

#[derive(Default)]
pub(super) struct CellInner {
    pub(super) outcome: Option<CellOutcome>,
    pub(super) wakers: Vec<Waker>,
}

pub(super) struct ProcessCell {
    pub(super) pid: i32,
    pub(super) pgid: i32,
    pub(super) inner: Mutex<CellInner>,
}

impl ProcessCell {
    pub(super) fn complete(&self, outcome: CellOutcome) {
        let mut inner = lock(&self.inner);
        if inner.outcome.is_some() {
            return;
        }
        inner.outcome = Some(outcome);
        let wakers = std::mem::take(&mut inner.wakers);
        drop(inner);
        for waker in wakers {
            waker.wake();
        }
    }

    pub(super) fn complete_error(&self, kind: io::ErrorKind, message: String) {
        self.complete(CellOutcome::Failed(kind, message));
    }

    pub(super) fn poll(&self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        let mut inner = lock(&self.inner);
        match &inner.outcome {
            Some(CellOutcome::Status(status)) => Poll::Ready(Ok(*status)),
            Some(CellOutcome::Failed(kind, message)) => {
                Poll::Ready(Err(io::Error::new(*kind, message.clone())))
            }
            None => {
                if !inner.wakers.iter().any(|waker| waker.will_wake(cx.waker())) {
                    inner.wakers.push(cx.waker().clone());
                }
                Poll::Pending
            }
        }
    }
}

pub(super) struct ProcessHandle {
    pub(super) cell: Arc<ProcessCell>,
}

impl ManagedProcess for ProcessHandle {
    fn pid(&self) -> i32 {
        self.cell.pid
    }

    fn pgid(&self) -> i32 {
        self.cell.pgid
    }

    fn poll_exit(&self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        self.cell.poll(cx)
    }
}

pub(super) struct ProcessRecord {
    pub(super) cell: Arc<ProcessCell>,
    pub(super) group: u64,
    pub(super) leader: bool,
    pub(super) observed: bool,
    pub(super) reaped: bool,
}
