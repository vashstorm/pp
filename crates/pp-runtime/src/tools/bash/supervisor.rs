//! External process supervision for the embedded shell.
//!
//! The embedded interpreter never owns a shell process of its own. Every
//! external program it starts is a direct child of pp and is registered here
//! before it can be observed by the interpreter. A dedicated monitor thread
//! observes exits with `waitid(P_PID, ..., WNOWAIT)`, keeps exited group
//! leaders as unreaped anchors while their group is still live, and owns the
//! TERM/KILL escalation for timeouts, cancellation and explicit stops.
//!
//! Only the monitor thread reaps children. `signal_pid`/`signal_group` take the
//! same registry lock as spawning and reaping, so a signal is either sent to a
//! still-registered process group or rejected after it has been dismantled.

use brush_core::{
    CleanupPermit, ManagedGroup, ManagedProcess, ProcessSupervisor, SpawnGroup, SpawnedProcess,
};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::CleanupError;
use std::{
    io,
    os::unix::process::CommandExt,
    process::Command,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod monitor;
mod process;
mod state;

use monitor::{
    check_spawn_allowed, host_pgid, lock, monitor, raw_kill, signal_group_locked, spawn_closed,
    terminate_unregistered,
};
use process::{CellInner, ProcessCell, ProcessHandle, ProcessRecord};
use state::{GroupLease, GroupRecord, Phase, Shared, State, StopReason};

/// How often the monitor checks child state while a direct child is unobserved
/// or a stop is in progress.
const ACTIVE_POLL: Duration = Duration::from_millis(5);
/// Idle poll interval when there is nothing to reap.
const IDLE_POLL: Duration = Duration::from_millis(50);
/// Grace period between the TERM/cleanup permit deadline and SIGKILL.
const GRACE: Duration = Duration::from_secs(1);

/// How a supervised run ended. `Exited` means the interpreter itself finished
/// naturally; every other variant selected a stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Termination {
    Exited,
    TimedOut,
    Cancelled,
    Stopped,
}

/// Result of waiting for the supervisor monitor to reach `Finished`.
pub(crate) struct SupervisorOutcome {
    pub(crate) termination: Termination,
    /// Failure to observe, signal or reap an external process. Hosts must
    /// surface this as an error instead of reporting a clean run.
    pub(crate) cleanup_error: Option<CleanupError>,
}

/// One supervisor per shell run.
///
/// Construct it before creating the embedded shell, pass the `Arc` to
/// `Shell::set_process_supervisor`, and put [`Supervisor::execution_token`] on
/// the shell's `ExecutionParameters`. When the interpreter has finished and
/// the host's output readers have drained, call
/// [`Supervisor::interpreter_finished`] and then [`Supervisor::wait`].
pub(crate) struct Supervisor {
    shared: Arc<Shared>,
    /// Cancelled by the monitor when a stop is selected. The core's cooperative
    /// cancellation checks observe this token.
    execution: CancellationToken,
    /// Host-supplied explicit stop input (`Termination::Stopped`).
    #[cfg(test)]
    stop: CancellationToken,
    monitor: Mutex<Option<JoinHandle<()>>>,
}

impl Supervisor {
    /// 创建带可选截止时间的监督器；None 仅关闭超时，不影响取消和显式停止。
    pub(crate) fn new_with_stop(
        deadline: Option<Instant>,
        parent_cancellation: CancellationToken,
        stop_cancellation: CancellationToken,
    ) -> io::Result<Arc<Self>> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::new(deadline)),
            wake: Condvar::new(),
        });
        let execution = CancellationToken::new();
        let cancelled_at_start = parent_cancellation.is_cancelled();
        let supervisor = Arc::new(Self {
            shared: shared.clone(),
            execution: execution.clone(),
            #[cfg(test)]
            stop: stop_cancellation.clone(),
            monitor: Mutex::new(None),
        });
        let monitor_shared = shared;
        let monitor_execution = execution;
        let monitor_parent = parent_cancellation;
        let monitor_stop = stop_cancellation;
        let handle = thread::Builder::new()
            .name("pp-supervisor".into())
            .spawn(move || {
                monitor(
                    monitor_shared,
                    monitor_execution,
                    monitor_parent,
                    monitor_stop,
                )
            })
            .map_err(|error| io::Error::other(format!("starting process supervisor: {error}")))?;
        *lock(&supervisor.monitor) = Some(handle);
        if cancelled_at_start {
            supervisor.execution.cancel();
        }
        Ok(supervisor)
    }

    /// Convenience constructor with a fresh stop token.
    #[cfg(test)]
    pub(crate) fn new(
        deadline: Instant,
        parent_cancellation: CancellationToken,
    ) -> io::Result<Arc<Self>> {
        Self::new_with_stop(
            Some(deadline),
            parent_cancellation,
            CancellationToken::new(),
        )
    }

    /// Convenience constructor: the deadline is `timeout` from now and the
    /// stop token is fresh.
    #[cfg(test)]
    pub(crate) fn with_timeout(
        timeout: Duration,
        parent_cancellation: CancellationToken,
    ) -> io::Result<Arc<Self>> {
        let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "command timeout is too large")
        })?;
        Self::new(deadline, parent_cancellation)
    }

    /// Token to hand to the embedded shell's `ExecutionParameters`.
    pub(crate) fn execution_token(&self) -> CancellationToken {
        self.execution.clone()
    }

    /// Requests an explicit stop (background task stop, shutdown, user cancel).
    ///
    /// The monitor publishes the cleanup permit and sends TERM before it
    /// cancels the interpreter token, so a TERM trap can still use its permit.
    #[cfg(test)]
    pub(crate) fn request_stop(&self) {
        {
            let mut state = lock(&self.shared.state);
            if state.phase == Phase::Running {
                state.explicit_stop = true;
            }
        }
        // An explicit request is also visible through the host-facing stop
        // token; the monitor still publishes the permit and sends TERM first.
        self.stop.cancel();
        self.shared.wake.notify_all();
    }

    /// Marks the interpreter (all shell/logical tasks and output writes) as
    /// finished. The monitor may then reclaim remaining external groups and
    /// reach `Finished`.
    pub(crate) fn interpreter_finished(&self) {
        lock(&self.shared.state).interpreter_done = true;
        self.shared.wake.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn is_finished(&self) -> bool {
        lock(&self.shared.state).phase == Phase::Finished
    }

    /// Waits for `Finished` and reports the chosen termination plus any cleanup
    /// failure. Call `interpreter_finished` first; otherwise this blocks while
    /// the interpreter is still considered active.
    pub(crate) fn wait(&self) -> SupervisorOutcome {
        {
            // Holding the handle lock while joining also serializes concurrent
            // waiters: the later one only observes a finished monitor.
            let mut monitor = lock(&self.monitor);
            if let Some(handle) = monitor.take()
                && handle.join().is_err()
            {
                lock(&self.shared.state)
                    .errors
                    .insert("process supervisor thread panicked".into());
            }
        }
        let state = lock(&self.shared.state);
        let termination = state
            .stop
            .map(StopReason::termination)
            .unwrap_or(Termination::Exited);
        let cleanup_error = (!state.errors.is_empty()).then(|| {
            CleanupError(anyhow::anyhow!(
                state.errors.iter().cloned().collect::<Vec<_>>().join("; ")
            ))
        });
        SupervisorOutcome {
            termination,
            cleanup_error,
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        {
            let mut state = lock(&self.shared.state);
            if state.phase == Phase::Running {
                state.explicit_stop = true;
            }
            // No host remains to observe interpreter progress; do not leave the
            // monitor waiting on it.
            state.interpreter_done = true;
        }
        self.execution.cancel();
        self.shared.wake.notify_all();
        if let Some(handle) = lock(&self.monitor).take() {
            let _ = handle.join();
        }
    }
}

impl ProcessSupervisor for Supervisor {
    fn spawn(
        &self,
        mut command: Command,
        group: SpawnGroup,
        cleanup: Option<&dyn CleanupPermit>,
    ) -> io::Result<SpawnedProcess> {
        // Spawn and registration share one lock with the stop phase and with
        // signal delivery, so a child is either registered before a stop starts
        // or rejected before it is actually started.
        let mut state = lock(&self.shared.state);
        check_spawn_allowed(&state, cleanup)?;

        let join = match &group {
            SpawnGroup::New => None,
            SpawnGroup::Join(lease) => {
                let key = lease.key();
                let pgid = lease.pgid();
                let record = state
                    .groups
                    .get(&key)
                    .ok_or_else(|| spawn_closed("the process group is no longer registered"))?;
                if record.pgid != pgid {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "process group lease does not match the registry",
                    ));
                }
                if pgid == host_pgid() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "refusing to join the host process group",
                    ));
                }
                Some((key, pgid))
            }
        };

        let new_key = match &group {
            SpawnGroup::New => {
                let key = state.next_key;
                state.next_key = key.checked_add(1).ok_or_else(|| {
                    io::Error::other("process group generation counter exhausted")
                })?;
                Some(key)
            }
            SpawnGroup::Join(_) => None,
        };

        let requested_pgid = join.map(|(_, pgid)| pgid);
        // Append to the core's pre_exec hook; do not replace it. Signal
        // dispositions, the virtual umask and rlimits are core's job. This
        // closure only creates/joins the supervisor's process group.
        // SAFETY: the closure only calls the async-signal-safe setpgid and
        // allocates nothing. It performs no locking in the child.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, requested_pgid.unwrap_or(0)) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command.spawn()?;
        let pid = child.id() as i32;
        let expected_pgid = requested_pgid.unwrap_or(pid);
        if expected_pgid == host_pgid() {
            terminate_unregistered(child);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to create the host process group",
            ));
        }
        // Set the group from the parent too: the child's pre_exec may not have
        // run yet when spawn returns. EACCES (child already exec'd) and ESRCH
        // (child already gone) are harmless; the verification below decides.
        let _ = unsafe { libc::setpgid(pid, expected_pgid) };
        match unsafe { libc::getpgid(pid) } {
            actual if actual == expected_pgid => {}
            actual if actual < 0 => {
                let error = io::Error::last_os_error();
                terminate_unregistered(child);
                return Err(error);
            }
            actual => {
                terminate_unregistered(child);
                return Err(io::Error::other(format!(
                    "external process {pid} joined process group {actual} instead of {expected_pgid}"
                )));
            }
        }
        // The monitor reaps by pid; the std Child no longer owns reaping.
        drop(child);

        let cell = Arc::new(ProcessCell {
            pid,
            pgid: expected_pgid,
            inner: Mutex::new(CellInner::default()),
        });
        state.managed_pids.insert(pid);
        let lease: Arc<dyn ManagedGroup> = match group {
            SpawnGroup::New => {
                let key = new_key.expect("a new group key was allocated");
                let handle = Arc::new(GroupLease {
                    key,
                    pgid: expected_pgid,
                    shared: Arc::downgrade(&self.shared),
                });
                state.groups.insert(
                    key,
                    GroupRecord {
                        pgid: expected_pgid,
                        leader: pid,
                        lease: Arc::downgrade(&handle),
                    },
                );
                state.procs.insert(
                    pid,
                    ProcessRecord {
                        cell: cell.clone(),
                        group: key,
                        leader: true,
                        observed: false,
                        reaped: false,
                    },
                );
                let handle: Arc<dyn ManagedGroup> = handle;
                handle
            }
            SpawnGroup::Join(lease) => {
                let key = join.expect("the joined group was resolved").0;
                state.procs.insert(
                    pid,
                    ProcessRecord {
                        cell: cell.clone(),
                        group: key,
                        leader: false,
                        observed: false,
                        reaped: false,
                    },
                );
                lease
            }
        };
        drop(state);
        self.shared.wake.notify_all();

        let process: Arc<dyn ManagedProcess> = Arc::new(ProcessHandle { cell });
        Ok(SpawnedProcess {
            process,
            group: lease,
        })
    }

    fn signal_pid(&self, pid: i32, signal: i32) -> io::Result<()> {
        if signal != 0 && (pid <= 0 || pid == std::process::id() as i32 || pid == host_pgid()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "refusing to signal the host process",
            ));
        }
        // PID signals and reaping are serialized by the same registry lock.
        let state = lock(&self.shared.state);
        if state.managed_pids.contains(&pid) {
            return match state.procs.get(&pid) {
                Some(record) if !record.reaped => raw_kill(pid, signal),
                // The job exists but is already finished (or its registry
                // entry and group were reclaimed). Report success without
                // touching a possibly reused PID.
                Some(_) | None => Ok(()),
            };
        }
        raw_kill(pid, signal)
    }

    fn signal_group(&self, group: &dyn ManagedGroup, signal: i32) -> io::Result<()> {
        let key = group.key();
        let pgid = group.pgid();
        let state = lock(&self.shared.state);
        let record = state.groups.get(&key).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "process group is no longer registered",
            )
        })?;
        if record.pgid != pgid {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "process group lease does not match the registry",
            ));
        }
        signal_group_locked(&state, key, pgid, signal)
    }

    fn cleanup_permit(&self) -> Option<Arc<dyn CleanupPermit>> {
        let state = lock(&self.shared.state);
        if state.phase != Phase::Stopping {
            return None;
        }
        let permit = state.permit.as_ref()?;
        if permit.is_revoked() || Instant::now() >= permit.deadline() {
            return None;
        }
        let permit: Arc<dyn CleanupPermit> = permit.clone();
        Some(permit)
    }
}

struct Permit {
    deadline: Instant,
    revoked: AtomicBool,
}

impl CleanupPermit for Permit {
    fn deadline(&self) -> Instant {
        self.deadline
    }

    fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brush_core::{ManagedProcess, ProcessSupervisor};
    use std::process::{ExitStatus, Stdio};
    use std::task::{Context, Poll, Waker};

    fn command(script: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn wait_exit(process: &dyn ManagedProcess) -> ExitStatus {
        let mut context = Context::from_waker(Waker::noop());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Poll::Ready(result) = process.poll_exit(&mut context) {
                return result.expect("external process wait");
            }
            assert!(Instant::now() < deadline, "external process did not exit");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal 0 only queries permission/existence.
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn wait_until(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition was not reached");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn supervisor() -> Arc<Supervisor> {
        Supervisor::with_timeout(Duration::from_secs(30), CancellationToken::new())
            .expect("supervisor")
    }

    #[test]
    fn embedded_shell_supervisor_managed_spawn_waits_and_reaps() {
        let supervisor = supervisor();
        let spawned = supervisor
            .spawn(command("exit 7"), SpawnGroup::New, None)
            .expect("spawn");
        let pid = spawned.process.pid();
        assert!(pid > 0);
        assert_eq!(spawned.process.pgid(), pid);
        assert!(process_exists(pid));

        let status = wait_exit(spawned.process.as_ref());
        assert_eq!(status.code(), Some(7));
        let process = spawned.process.clone();
        drop(spawned.group);
        drop(spawned.process);
        supervisor.interpreter_finished();
        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Exited);
        assert!(outcome.cleanup_error.is_none());
        wait_until(|| !process_exists(pid));
        // Registry removal must not invalidate the cached terminal status or
        // make a later kill on the returned PID fail.
        assert_eq!(wait_exit(process.as_ref()).code(), Some(7));
        assert!(supervisor.signal_pid(pid, libc::SIGTERM).is_ok());
        assert!(supervisor.signal_pid(pid, 0).is_ok());
    }

    #[test]
    fn embedded_shell_supervisor_group_lease_anchors_leader_until_group_is_empty() {
        let supervisor = supervisor();
        let leader = supervisor
            .spawn(command("sleep 0.2; exit 5"), SpawnGroup::New, None)
            .expect("spawn leader");
        let leader_pid = leader.process.pid();
        let pgid = leader.group.pgid();

        let joined = supervisor
            .spawn(command("exit 3"), SpawnGroup::Join(leader.group), None)
            .expect("spawn joined stage");
        assert_eq!(joined.process.pgid(), pgid);

        assert_eq!(wait_exit(joined.process.as_ref()).code(), Some(3));
        // The lease keeps the exited leader as an anchor even though the joined
        // stage is already reaped.
        assert_eq!(wait_exit(leader.process.as_ref()).code(), Some(5));
        assert!(process_exists(leader_pid), "leader anchor was reaped early");

        drop(joined.group);
        drop(leader.process);
        supervisor.interpreter_finished();
        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Exited);
        assert!(outcome.cleanup_error.is_none());
        wait_until(|| !process_exists(leader_pid));
    }

    #[test]
    fn embedded_shell_supervisor_join_after_leader_exit() {
        let supervisor = supervisor();
        let leader = supervisor
            .spawn(command("exit 5"), SpawnGroup::New, None)
            .expect("spawn leader");
        let pgid = leader.group.pgid();

        // Observe the leader's exit while the assembly lease still holds the
        // zombie anchor. A later pipeline stage must still be able to join the
        // group by pgid.
        assert_eq!(wait_exit(leader.process.as_ref()).code(), Some(5));
        let joined = supervisor
            .spawn(command("exit 3"), SpawnGroup::Join(leader.group), None)
            .expect("join after leader exit");
        assert_eq!(joined.process.pgid(), pgid);
        assert_eq!(wait_exit(joined.process.as_ref()).code(), Some(3));

        drop(joined.group);
        drop(leader.process);
        supervisor.interpreter_finished();
        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Exited);
        assert!(outcome.cleanup_error.is_none());
    }

    #[test]
    fn embedded_shell_supervisor_stop_escalates_and_closes_spawning() {
        let supervisor = supervisor();
        let spawned = supervisor
            .spawn(command("trap '' TERM; sleep 30"), SpawnGroup::New, None)
            .expect("spawn");
        let pid = spawned.process.pid();

        supervisor.request_stop();
        let permit = loop {
            if let Some(permit) = supervisor.cleanup_permit() {
                break permit;
            }
            assert!(
                !supervisor.is_finished(),
                "stop finished before a permit existed"
            );
            thread::sleep(Duration::from_millis(5));
        };

        // Spawns inside the TERM cleanup window are allowed.
        let cleanup = supervisor
            .spawn(command("exit 0"), SpawnGroup::New, Some(permit.as_ref()))
            .expect("cleanup spawn");
        drop(cleanup.group);
        drop(cleanup.process);

        wait_until(|| permit.is_revoked());
        assert!(supervisor.cleanup_permit().is_none());
        assert!(
            supervisor
                .spawn(command("exit 0"), SpawnGroup::New, Some(permit.as_ref()))
                .is_err()
        );
        assert!(
            supervisor
                .spawn(command("exit 0"), SpawnGroup::New, None)
                .is_err()
        );

        drop(spawned.group);
        drop(spawned.process);
        supervisor.interpreter_finished();
        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Stopped);
        assert!(outcome.cleanup_error.is_none());
        wait_until(|| !process_exists(pid));
    }

    #[test]
    fn embedded_shell_supervisor_parent_cancellation_reports_cancelled() {
        let cancellation = CancellationToken::new();
        let stop = CancellationToken::new();
        let supervisor = Supervisor::new_with_stop(None, cancellation.clone(), stop.clone())
            .expect("supervisor");
        let spawned = supervisor
            .spawn(command("trap '' TERM; sleep 30"), SpawnGroup::New, None)
            .expect("spawn");
        let pid = spawned.process.pid();
        // Both inputs are cancelled: parent cancellation must win.
        cancellation.cancel();
        stop.cancel();
        supervisor.interpreter_finished();
        drop(spawned.group);
        drop(spawned.process);

        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Cancelled);
        assert!(outcome.cleanup_error.is_none());
        wait_until(|| !process_exists(pid));
    }

    #[test]
    fn embedded_shell_supervisor_stop_token_reports_stopped() {
        let stop = CancellationToken::new();
        let supervisor = Supervisor::new_with_stop(None, CancellationToken::new(), stop.clone())
            .expect("supervisor");
        let spawned = supervisor
            .spawn(command("trap '' TERM; sleep 30"), SpawnGroup::New, None)
            .expect("spawn");
        let pid = spawned.process.pid();
        // The host-facing stop token alone selects Stopped, without request_stop.
        stop.cancel();
        supervisor.interpreter_finished();
        drop(spawned.group);
        drop(spawned.process);

        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Stopped);
        assert!(outcome.cleanup_error.is_none());
        wait_until(|| !process_exists(pid));
    }

    #[test]
    fn embedded_shell_supervisor_refuses_to_signal_the_host() {
        let supervisor = supervisor();
        let host = std::process::id() as i32;
        assert!(supervisor.signal_pid(host, libc::SIGTERM).is_err());
        assert!(supervisor.signal_pid(0, libc::SIGTERM).is_err());
        assert!(supervisor.signal_pid(-1, libc::SIGTERM).is_err());
        // Queries remain available.
        assert!(supervisor.signal_pid(host, 0).is_ok());

        supervisor.interpreter_finished();
        let outcome = supervisor.wait();
        assert_eq!(outcome.termination, Termination::Exited);
        assert!(outcome.cleanup_error.is_none());
    }
}
