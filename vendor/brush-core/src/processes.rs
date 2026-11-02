//! Process management

use std::sync::Arc;
use std::task::Poll;

use futures::FutureExt;
use futures::future::poll_fn;

use crate::supervisor;
use crate::{error, sys};

/// A waitable future that will yield the results of a child process's execution.
pub(crate) type WaitableChildProcess = std::pin::Pin<
    Box<dyn futures::Future<Output = Result<std::process::Output, std::io::Error>> + Send + Sync>,
>;

/// Identifies a process group that spawned commands may join.
///
/// Joining is carried out through the supervised group handle when one exists,
/// so that the supervisor -- not a raw process group ID that may already have
/// been reused -- decides where the child lands.
#[derive(Clone)]
pub struct ProcessGroupRef {
    /// The numeric process group ID.
    pub pgid: i32,
    /// The supervised group handle, when the group is supervised.
    pub managed: Option<Arc<dyn supervisor::ManagedGroup>>,
}

/// The mechanism through which a child process is observed.
enum ChildProcessInner {
    /// A child owned by this shell through the platform process facilities.
    Legacy(WaitableChildProcess),
    /// A child owned and reaped by an external process supervisor.
    Managed {
        /// The managed child process.
        process: Arc<dyn supervisor::ManagedProcess>,
        /// This child's registration lease on its process group, if any.
        group: Option<Arc<dyn supervisor::ManagedGroup>>,
    },
}

/// Tracks a child process being awaited.
pub struct ChildProcess {
    /// The mechanism through which the child is observed.
    inner: ChildProcessInner,
    /// If available, the process ID of the child.
    pid: Option<sys::process::ProcessId>,
    /// If available, the process group ID of the child.
    pgid: Option<sys::process::ProcessId>,
    /// The terminal status, once known.
    status: Option<std::process::ExitStatus>,
}

#[allow(clippy::missing_const_for_fn)]
impl ChildProcess {
    /// Wraps a child process and its future.
    pub fn new(
        child: sys::process::Child,
        pid: Option<sys::process::ProcessId>,
        pgid: Option<sys::process::ProcessId>,
    ) -> Self {
        Self {
            inner: ChildProcessInner::Legacy(Box::pin(child.wait_with_output())),
            pid,
            pgid,
            status: None,
        }
    }

    /// Wraps a process spawned by a process supervisor.
    pub fn from_spawned(spawned: supervisor::SpawnedProcess) -> Self {
        let supervisor::SpawnedProcess { process, group } = spawned;

        Self {
            pid: Some(process.pid()),
            pgid: Some(process.pgid()),
            inner: ChildProcessInner::Managed {
                process,
                group: Some(group),
            },
            status: None,
        }
    }

    /// Returns the process's ID.
    pub const fn pid(&self) -> Option<sys::process::ProcessId> {
        self.pid
    }

    /// Returns the process's group ID.
    pub const fn pgid(&self) -> Option<sys::process::ProcessId> {
        self.pgid
    }

    /// Returns the process group this child belongs to, if known.
    pub fn group_ref(&self) -> Option<ProcessGroupRef> {
        let pgid = self.pgid?;

        let managed = match &self.inner {
            ChildProcessInner::Managed { group, .. } => group.clone(),
            ChildProcessInner::Legacy(_) => None,
        };

        Some(ProcessGroupRef { pgid, managed })
    }

    /// Releases this child's assembly-time lease on its process group.
    ///
    /// The lease only exists so that a pipeline is fully spawned before the
    /// supervisor may reap the group's leader; callers release it once the whole
    /// pipeline has been assembled, or before waiting on a standalone child.
    pub fn release_group_lease(&mut self) {
        if let ChildProcessInner::Managed { group, .. } = &mut self.inner {
            *group = None;
        }
    }

    /// Waits for the process to exit.
    pub async fn wait(&mut self) -> Result<ProcessWaitResult, error::Error> {
        if let Some(status) = self.status {
            return Ok(ProcessWaitResult::Completed(completed_output(status)));
        }

        if let ChildProcessInner::Managed { process, .. } = &self.inner {
            let process = process.clone();
            let status = poll_fn(move |cx| process.poll_exit(cx)).await?;
            self.status = Some(status);

            return Ok(ProcessWaitResult::Completed(completed_output(status)));
        }

        let ChildProcessInner::Legacy(exec_future) = &mut self.inner else {
            return Err(error::ErrorKind::InternalError(
                "unrecognized child process state".to_owned(),
            )
            .into());
        };

        #[allow(unused_mut, reason = "only mutated on some platforms")]
        let mut sigtstp = sys::signal::tstp_signal_listener()?;
        #[allow(unused_mut, reason = "only mutated on some platforms")]
        let mut sigchld = sys::signal::chld_signal_listener()?;

        #[allow(clippy::ignored_unit_patterns)]
        loop {
            tokio::select! {
                output = &mut *exec_future => {
                    break Ok(ProcessWaitResult::Completed(output?))
                },
                _ = sigtstp.recv() => {
                    break Ok(ProcessWaitResult::Stopped)
                },
                _ = sigchld.recv() => {
                    if sys::signal::poll_for_stopped_children()? {
                        break Ok(ProcessWaitResult::Stopped);
                    }
                },
                _ = sys::signal::await_ctrl_c() => {
                    // SIGINT got thrown. Handle it and continue looping. The child should
                    // have received it as well, and either handled it or ended up getting
                    // terminated (in which case we'll see the child exit).
                },
            }
        }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<std::process::Output, error::Error>> {
        if let Some(status) = self.status {
            return Some(Ok(completed_output(status)));
        }

        match &mut self.inner {
            ChildProcessInner::Legacy(exec_future) => exec_future
                .as_mut()
                .now_or_never()
                .map(|result| result.map_err(Into::into)),
            ChildProcessInner::Managed { process, .. } => {
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());

                match process.poll_exit(&mut cx) {
                    Poll::Ready(Ok(status)) => {
                        self.status = Some(status);
                        Some(Ok(completed_output(status)))
                    }
                    Poll::Ready(Err(err)) => Some(Err(err.into())),
                    Poll::Pending => None,
                }
            }
        }
    }
}

/// Builds the output record reported for a child that only exposes its status.
fn completed_output(status: std::process::ExitStatus) -> std::process::Output {
    std::process::Output {
        status,
        stdout: Vec::new(),
        stderr: Vec::new(),
    }
}

/// Represents the result of waiting for an executing process.
pub enum ProcessWaitResult {
    /// The process completed.
    Completed(std::process::Output),
    /// The process stopped and has not yet completed.
    Stopped,
}
