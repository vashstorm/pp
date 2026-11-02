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

use crate::cancellation::{CancellationToken, CleanupError};
use brush_core::{
    CleanupPermit, ManagedGroup, ManagedProcess, ProcessSupervisor, SpawnGroup, SpawnedProcess,
};
use rustix::process::{Pid, WaitId, WaitIdOptions, WaitIdStatus, WaitOptions, waitid, waitpid};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::unix::process::CommandExt,
    process::{Child, Command, ExitStatus},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

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

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Running,
    Stopping,
    Killing,
    Draining,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    TimedOut,
    Cancelled,
    Stopped,
}

impl StopReason {
    const fn termination(self) -> Termination {
        match self {
            Self::TimedOut => Termination::TimedOut,
            Self::Cancelled => Termination::Cancelled,
            Self::Stopped => Termination::Stopped,
        }
    }
}

struct State {
    phase: Phase,
    deadline: Instant,
    explicit_stop: bool,
    interpreter_done: bool,
    stop: Option<StopReason>,
    permit: Option<Arc<Permit>>,
    grace_deadline: Option<Instant>,
    errors: BTreeSet<String>,
    groups: BTreeMap<u64, GroupRecord>,
    procs: BTreeMap<i32, ProcessRecord>,
    /// Every PID this supervisor ever returned from `ManagedProcess::pid()`.
    /// Kept after reaping so a later `kill` on a finished job PID reports
    /// success instead of ESRCH or touching a reused PID.
    managed_pids: BTreeSet<i32>,
    next_key: u64,
}

impl State {
    fn new(deadline: Instant) -> Self {
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

    fn has_live_work(&self) -> bool {
        !self.groups.is_empty() || !self.procs.is_empty()
    }
}

/// A spawn/terminate lease for one process group. Arc clones given to the
/// interpreter keep the group alive; the registry only holds a Weak handle, so
/// `Drop` runs when the final clone is released and never takes the registry
/// lock (which a caller may already hold inside `spawn`).
struct GroupLease {
    key: u64,
    pgid: i32,
    shared: Weak<Shared>,
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

struct GroupRecord {
    pgid: i32,
    leader: i32,
    /// The interpreter's group handle; present while any Arc clone is alive.
    lease: Weak<GroupLease>,
}

#[derive(Clone)]
enum CellOutcome {
    Status(ExitStatus),
    Failed(io::ErrorKind, String),
}

#[derive(Default)]
struct CellInner {
    outcome: Option<CellOutcome>,
    wakers: Vec<Waker>,
}

struct ProcessCell {
    pid: i32,
    pgid: i32,
    inner: Mutex<CellInner>,
}

impl ProcessCell {
    fn complete(&self, outcome: CellOutcome) {
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

    fn complete_error(&self, kind: io::ErrorKind, message: String) {
        self.complete(CellOutcome::Failed(kind, message));
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<io::Result<ExitStatus>> {
        let mut inner = lock(&self.inner);
        match &inner.outcome {
            Some(CellOutcome::Status(status)) => Poll::Ready(Ok(status.clone())),
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

struct ProcessHandle {
    cell: Arc<ProcessCell>,
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

struct ProcessRecord {
    cell: Arc<ProcessCell>,
    group: u64,
    leader: bool,
    observed: bool,
    reaped: bool,
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
    /// Creates a supervisor whose deadline is an absolute instant. The
    /// host-supplied stop token records `Termination::Stopped` when cancelled.
    pub(crate) fn new_with_stop(
        deadline: Instant,
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
        Self::new_with_stop(deadline, parent_cancellation, CancellationToken::new())
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
            if let Some(handle) = monitor.take() {
                if handle.join().is_err() {
                    lock(&self.shared.state)
                        .errors
                        .insert("process supervisor thread panicked".into());
                }
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

fn monitor(
    shared: Arc<Shared>,
    execution: CancellationToken,
    parent: CancellationToken,
    stop: CancellationToken,
) {
    loop {
        let mut state = lock(&shared.state);
        reap(&mut state);

        let now = Instant::now();
        match state.phase {
            Phase::Running => {
                // A naturally finished interpreter wins over a deadline that
                // elapsed in the same poll, but an explicit stop or parent
                // cancellation that already happened still wins over natural
                // completion.
                let stop_requested = state.explicit_stop || stop.is_cancelled();
                let natural = state.interpreter_done && !stop_requested && !parent.is_cancelled();
                if natural {
                    if state.has_live_work() {
                        begin_reclaim(&mut state, now);
                    } else {
                        state.phase = Phase::Draining;
                    }
                } else {
                    let reason = if parent.is_cancelled() {
                        Some(StopReason::Cancelled)
                    } else if !state.interpreter_done && now >= state.deadline {
                        Some(StopReason::TimedOut)
                    } else if stop_requested {
                        Some(StopReason::Stopped)
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        begin_stopping(&mut state, reason, now);
                        // Cooperative cancellation for the interpreter.
                        execution.cancel();
                    } else if state.interpreter_done {
                        // Unreachable with the conditions above, but keep the
                        // drain path total.
                        if state.has_live_work() {
                            begin_reclaim(&mut state, now);
                        } else {
                            state.phase = Phase::Draining;
                        }
                    }
                }
            }
            Phase::Stopping => {
                if now >= state.grace_deadline.unwrap_or(now) {
                    // The cleanup window is over: revoke new cleanup spawns and
                    // kill everything still registered.
                    if let Some(permit) = state.permit.take() {
                        permit.revoked.store(true, Ordering::Release);
                    }
                    state.phase = Phase::Killing;
                    signal_registered(&mut state, libc::SIGKILL);
                }
            }
            Phase::Killing => {
                if state.has_live_work() {
                    // A descendant can be forked between TERM and the first
                    // KILL; repeat the group/direct PID kill until the registry
                    // is empty.
                    signal_registered(&mut state, libc::SIGKILL);
                } else {
                    state.phase = Phase::Draining;
                }
            }
            Phase::Draining => {
                if state.interpreter_done && !state.has_live_work() {
                    state.phase = Phase::Finished;
                }
            }
            Phase::Finished => break,
        }

        if state.phase == Phase::Finished {
            break;
        }
        let interval = if matches!(state.phase, Phase::Stopping | Phase::Killing)
            || state.procs.values().any(|process| !process.observed)
        {
            ACTIVE_POLL
        } else {
            IDLE_POLL
        };
        state = wait_timeout(&shared, state, interval);
    }
}

fn begin_stopping(state: &mut State, reason: StopReason, now: Instant) {
    state.phase = Phase::Stopping;
    state.stop = Some(reason);
    let deadline = now.checked_add(GRACE).unwrap_or(now);
    state.grace_deadline = Some(deadline);
    state.permit = Some(Arc::new(Permit {
        deadline,
        revoked: AtomicBool::new(false),
    }));
    signal_registered(state, libc::SIGTERM);
}

/// Normal interpreter completion with external processes still alive: no stop
/// reason is recorded, but leftovers are terminated with the usual grace.
fn begin_reclaim(state: &mut State, now: Instant) {
    state.phase = Phase::Stopping;
    state.grace_deadline = Some(now.checked_add(GRACE).unwrap_or(now));
    signal_registered(state, libc::SIGTERM);
}

fn signal_registered(state: &mut State, signal: i32) {
    let groups: Vec<(u64, i32)> = state
        .groups
        .iter()
        .map(|(&key, group)| (key, group.pgid))
        .collect();
    for (key, pgid) in groups {
        if let Err(error) = signal_group_locked(state, key, pgid, signal) {
            state
                .errors
                .insert(format!("signaling process group {pgid}: {error}"));
        }
    }

    // Group signaling misses direct children that moved to another group;
    // signal every registered process that has not been observed as exited.
    let pids: Vec<i32> = state
        .procs
        .iter()
        .filter(|(_, process)| !process.observed && !process.reaped)
        .map(|(&pid, _)| pid)
        .collect();
    for pid in pids {
        if let Err(error) = raw_kill(pid, signal)
            && error.raw_os_error() != Some(libc::ESRCH)
        {
            state
                .errors
                .insert(format!("signaling external process {pid}: {error}"));
        }
    }
}

fn signal_group_locked(state: &State, key: u64, pgid: i32, signal: i32) -> io::Result<()> {
    if pgid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process group id",
        ));
    }
    if signal != 0 && pgid == host_pgid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to signal the host process group",
        ));
    }
    match raw_kill_group(pgid, signal) {
        Ok(()) => Ok(()),
        // The group is already gone: leaving, not a failure.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        // On Darwin an EPERM can mean "only pinned zombies remain"; it is not
        // proof that the group is empty, so consult the live-member query.
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
            let pinned: BTreeSet<i32> = state
                .procs
                .values()
                .filter(|process| process.group == key && process.observed && !process.reaped)
                .map(|process| process.cell.pid)
                .collect();
            match group_members_live(pgid, &pinned) {
                Ok(false) => Ok(()),
                Ok(true) => Err(error),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn check_spawn_allowed(state: &State, cleanup: Option<&dyn CleanupPermit>) -> io::Result<()> {
    if !state.errors.is_empty() {
        return Err(spawn_closed("supervision previously failed"));
    }
    let now = Instant::now();
    match state.phase {
        Phase::Running => {
            if let Some(permit) = cleanup
                && (permit.is_revoked() || now >= permit.deadline())
            {
                return Err(spawn_closed("cleanup permit expired"));
            }
            Ok(())
        }
        Phase::Stopping => {
            let Some(permit) = cleanup else {
                return Err(spawn_closed("run is stopping"));
            };
            let Some(active) = state.permit.as_ref() else {
                return Err(spawn_closed("run is stopping"));
            };
            if permit.is_revoked() || active.is_revoked() || now >= active.deadline() {
                return Err(spawn_closed("cleanup permit expired"));
            }
            Ok(())
        }
        Phase::Killing | Phase::Draining | Phase::Finished => Err(spawn_closed("run is stopping")),
    }
}

fn spawn_closed(reason: &str) -> io::Error {
    io::Error::other(format!("external process spawn is closed: {reason}"))
}

/// Observes and reaps direct children. Leader anchors are reaped only after the
/// group has no live members and no spawn leases.
fn reap(state: &mut State) {
    // Observe exits without reaping them so a leader remains a WNOWAIT anchor.
    let pids: Vec<i32> = state
        .procs
        .iter()
        .filter(|(_, process)| !process.observed)
        .map(|(&pid, _)| pid)
        .collect();
    for pid in pids {
        let Some(raw) = Pid::from_raw(pid) else {
            continue;
        };
        match waitid(
            WaitId::Pid(raw),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(info)) => {
                let status = exit_status(&info);
                if let Some(record) = state.procs.get_mut(&pid) {
                    record.cell.complete(CellOutcome::Status(status));
                    record.observed = true;
                }
            }
            Ok(None) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::CHILD) => {
                let mut leader = false;
                if let Some(record) = state.procs.get_mut(&pid) {
                    leader = record.leader;
                    record.cell.complete_error(
                        io::ErrorKind::Other,
                        format!("external process {pid} was reaped before it could be observed"),
                    );
                    record.observed = true;
                    record.reaped = true;
                }
                if !leader {
                    state.procs.remove(&pid);
                }
                state.errors.insert(format!(
                    "observing external process {pid}: no child process"
                ));
            }
            Err(error) => {
                state
                    .errors
                    .insert(format!("observing external process {pid}: {error}"));
            }
        }
    }

    // A non-leader direct child can be reaped as soon as its status is cached.
    let reaps: Vec<i32> = state
        .procs
        .iter()
        .filter(|(_, process)| !process.leader && process.observed && !process.reaped)
        .map(|(&pid, _)| pid)
        .collect();
    for pid in reaps {
        let Some(raw) = Pid::from_raw(pid) else {
            state.procs.remove(&pid);
            continue;
        };
        match waitpid(Some(raw), WaitOptions::empty()) {
            Ok(_) => {
                state.procs.remove(&pid);
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::CHILD) => {
                state.procs.remove(&pid);
            }
            Err(error) => {
                state
                    .errors
                    .insert(format!("reaping external process {pid}: {error}"));
            }
        }
    }

    let keys: Vec<u64> = state.groups.keys().copied().collect();
    for key in keys {
        let Some(group) = state.groups.get(&key) else {
            continue;
        };
        let pgid = group.pgid;
        let leader = group.leader;
        let leases = group.lease.strong_count();

        let (observed, reaped) = match state.procs.get(&leader) {
            Some(record) => (record.observed, record.reaped),
            None => {
                // The leader disappeared without passing through the monitor
                // (only possible after a reaping failure); drop the untrackable
                // group record.
                state.groups.remove(&key);
                continue;
            }
        };
        if !observed {
            continue;
        }
        if reaped {
            // The leader was reaped outside this monitor; its pgid is no longer
            // safe to query or signal.
            state.procs.remove(&leader);
            state.groups.remove(&key);
            continue;
        }
        let pending_direct_child = state
            .procs
            .values()
            .any(|process| process.group == key && !process.leader && !process.reaped);
        if pending_direct_child || leases > 0 {
            continue;
        }

        let pinned: BTreeSet<i32> = state
            .procs
            .values()
            .filter(|process| process.group == key && process.observed && !process.reaped)
            .map(|process| process.cell.pid)
            .collect();
        match group_members_live(pgid, &pinned) {
            Ok(false) => match reap_pid(leader) {
                Ok(()) => {
                    state.procs.remove(&leader);
                    state.groups.remove(&key);
                }
                Err(error) => {
                    state
                        .errors
                        .insert(format!("reaping process group leader {leader}: {error}"));
                }
            },
            Ok(true) => {}
            Err(error) => {
                state
                    .errors
                    .insert(format!("checking process group {pgid}: {error}"));
                // Do not keep an undecidable tombstone forever: kill the group
                // and release the anchor, while reporting the cleanup failure.
                if let Err(error) = raw_kill_group(pgid, libc::SIGKILL)
                    && !matches!(error.raw_os_error(), Some(libc::ESRCH | libc::EPERM))
                {
                    state
                        .errors
                        .insert(format!("cleaning up process group {pgid}: {error}"));
                }
                match reap_pid(leader) {
                    Ok(()) => {
                        state.procs.remove(&leader);
                        state.groups.remove(&key);
                    }
                    Err(error) => {
                        state
                            .errors
                            .insert(format!("reaping process group leader {leader}: {error}"));
                    }
                }
            }
        }
    }
}

fn reap_pid(pid: i32) -> io::Result<()> {
    let Some(raw) = Pid::from_raw(pid) else {
        return Ok(());
    };
    loop {
        match waitpid(Some(raw), WaitOptions::empty()) {
            Ok(_) | Err(rustix::io::Errno::CHILD) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn exit_status(info: &WaitIdStatus) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;

    if let Some(code) = info.exit_status() {
        ExitStatus::from_raw(code << 8)
    } else if let Some(signal) = info.terminating_signal() {
        ExitStatus::from_raw(signal)
    } else {
        ExitStatus::from_raw(0)
    }
}

fn terminate_unregistered(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn host_pgid() -> i32 {
    // SAFETY: getpgrp has no preconditions.
    unsafe { libc::getpgrp() }
}

fn raw_kill(pid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: kill is called with a checked positive pid.
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn raw_kill_group(pgid: i32, signal: i32) -> io::Result<()> {
    // SAFETY: pgid is checked positive; kill(-pgid) targets that group only.
    let result = unsafe { libc::kill(-pgid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn wait_timeout<'a>(
    shared: &'a Shared,
    guard: MutexGuard<'a, State>,
    interval: Duration,
) -> MutexGuard<'a, State> {
    match shared.wake.wait_timeout(guard, interval) {
        Ok((guard, _)) => guard,
        Err(poisoned) => poisoned.into_inner().0,
    }
}

// A poisoned registry must not prevent process reclamation.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(target_os = "linux")]
fn group_members_live(pgid: i32, pinned: &BTreeSet<i32>) -> io::Result<bool> {
    use std::fs;

    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
            continue;
        };
        if pid <= 0 || pinned.contains(&pid) {
            continue;
        }
        let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        // Field 2 (comm) may contain spaces and parentheses; parse after the
        // last ')' to reach state, ppid, pgrp.
        let Some(end) = stat.rfind(')') else {
            continue;
        };
        let mut fields = stat[end + 1..].split_whitespace();
        let state = fields.next();
        let _ppid = fields.next();
        let pgrp = fields.next().and_then(|value| value.parse::<i32>().ok());
        if state != Some("Z") && pgrp == Some(pgid) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn group_members_live(pgid: i32, pinned: &BTreeSet<i32>) -> io::Result<bool> {
    use std::{ffi::c_void, mem::size_of};

    #[repr(C)]
    struct ProcBsdShortInfo {
        pbsi_pid: u32,
        pbsi_ppid: u32,
        pbsi_pgid: u32,
        pbsi_status: u32,
        pbsi_comm: [libc::c_char; 16],
        pbsi_flags: u32,
        pbsi_uid: u32,
        pbsi_gid: u32,
        pbsi_ruid: u32,
        pbsi_rgid: u32,
        pbsi_svuid: u32,
        pbsi_svgid: u32,
        pbsi_rfu: u32,
    }

    // SAFETY: these are the documented libproc entry points used only with
    // correctly sized, initialized buffers.
    unsafe extern "C" {
        fn proc_listpgrppids(
            pgrp: libc::c_int,
            buffer: *mut libc::c_int,
            buffersize: libc::c_int,
        ) -> libc::c_int;
        fn proc_pidinfo(
            pid: libc::c_int,
            flavor: libc::c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: libc::c_int,
        ) -> libc::c_int;
    }

    const PROC_PIDT_SHORTBSDINFO: libc::c_int = 13;
    const SZOMB: u32 = 5;
    const MAX_MEMBERS: usize = 1 << 16;

    let mut capacity = 64usize;
    loop {
        let mut buffer = vec![0 as libc::c_int; capacity];
        let bytes = capacity * size_of::<libc::c_int>();
        // SAFETY: buffer is valid for `bytes` and the API returns a pid count.
        let count = unsafe { proc_listpgrppids(pgid, buffer.as_mut_ptr(), bytes as libc::c_int) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = count as usize;
        if count >= capacity {
            // The list may be truncated; grow and retry instead of treating a
            // full buffer as an empty group.
            if capacity >= MAX_MEMBERS {
                return Err(io::Error::other("process group member list is too large"));
            }
            capacity *= 2;
            continue;
        }
        for &pid in &buffer[..count] {
            if pid <= 0 || pinned.contains(&pid) {
                continue;
            }
            let mut info = std::mem::MaybeUninit::<ProcBsdShortInfo>::uninit();
            // SAFETY: the flavor writes at most size_of::<ProcBsdShortInfo>().
            let written = unsafe {
                proc_pidinfo(
                    pid,
                    PROC_PIDT_SHORTBSDINFO,
                    0,
                    info.as_mut_ptr().cast(),
                    size_of::<ProcBsdShortInfo>() as libc::c_int,
                )
            };
            if written <= 0 {
                let error = io::Error::last_os_error();
                // The member left between listing and querying; that is not an
                // empty-group proof but simply a vanished member.
                if error.raw_os_error() == Some(libc::ESRCH) {
                    continue;
                }
                return Err(error);
            }
            if (written as usize) < size_of::<ProcBsdShortInfo>() {
                return Err(io::Error::other("short process info record"));
            }
            // SAFETY: a full record was written.
            let info = unsafe { info.assume_init() };
            if info.pbsi_pgid == pgid as u32 && info.pbsi_status != SZOMB {
                return Ok(true);
            }
        }
        return Ok(false);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn group_members_live(_pgid: i32, _pinned: &BTreeSet<i32>) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process group liveness is not implemented on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use brush_core::{ManagedProcess, ProcessSupervisor};
    use std::process::Stdio;

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
        let supervisor = Supervisor::new_with_stop(
            Instant::now() + Duration::from_secs(30),
            cancellation.clone(),
            stop.clone(),
        )
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
        let supervisor = Supervisor::new_with_stop(
            Instant::now() + Duration::from_secs(30),
            CancellationToken::new(),
            stop.clone(),
        )
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
