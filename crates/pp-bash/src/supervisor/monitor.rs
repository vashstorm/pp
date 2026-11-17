//! 监督器 monitor 线程：观察、信号升级与回收。

use brush_core::CleanupPermit;
use pp_core::cancellation::CancellationToken;
use rustix::process::{Pid, WaitId, WaitIdOptions, WaitIdStatus, WaitOptions, waitid, waitpid};
use std::{
    collections::BTreeSet,
    io,
    process::{Child, ExitStatus},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use super::process::CellOutcome;
use super::{ACTIVE_POLL, GRACE, IDLE_POLL, Permit, Phase, Shared, State, StopReason};

pub(super) fn monitor(
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
                    } else if !state.interpreter_done
                        && state.deadline.is_some_and(|deadline| now >= deadline)
                    {
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

pub(super) fn signal_group_locked(
    state: &State,
    key: u64,
    pgid: i32,
    signal: i32,
) -> io::Result<()> {
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

pub(super) fn check_spawn_allowed(
    state: &State,
    cleanup: Option<&dyn CleanupPermit>,
) -> io::Result<()> {
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

pub(super) fn spawn_closed(reason: &str) -> io::Error {
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

pub(super) fn terminate_unregistered(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn host_pgid() -> i32 {
    // SAFETY: getpgrp has no preconditions.
    unsafe { libc::getpgrp() }
}

pub(super) fn raw_kill(pid: i32, signal: i32) -> io::Result<()> {
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
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
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
