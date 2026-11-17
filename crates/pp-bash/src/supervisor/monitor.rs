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
use super::{
    ACTIVE_POLL, ACTIVE_POLL_MAX, GRACE, IDLE_POLL, Permit, Phase, Shared, State, StopReason,
};

/// active 轮询的指数退避：基线 5ms，无状态变化时逐轮翻倍到 50ms 上限。
/// 退避只是为了减少空转唤醒，任何注册表或事件变化都让它回到基线。
struct Backoff {
    next: Duration,
}

impl Backoff {
    const fn new() -> Self {
        Self { next: ACTIVE_POLL }
    }

    fn reset(&mut self) {
        self.next = ACTIVE_POLL;
    }

    /// 取本轮间隔，并为下一轮翻倍（不超过上限）。
    fn advance(&mut self) -> Duration {
        let interval = self.next;
        self.next = interval.saturating_mul(2).min(ACTIVE_POLL_MAX);
        interval
    }
}

/// 本轮应等待的时长。
///
/// `active` 为真（有未观察的子进程，或正在 TERM/KILL 升级）时使用会退避的
/// active 间隔；否则固定用空闲间隔。`changed` 为真表示自上一轮以来注册表被
/// 观察/回收改变，或发生过一次唤醒事件（新注册、显式停止、解释器结束、租约
/// 释放）：此时立即回到基线，不把刚发生的变化推到下一个长间隔之后。
fn poll_interval(active: bool, changed: bool, backoff: &mut Backoff) -> Duration {
    if changed {
        backoff.reset();
    }
    if active {
        backoff.advance()
    } else if changed {
        // 空闲阶段也只在事件后立刻复查一次，随后回到空闲间隔。
        ACTIVE_POLL
    } else {
        IDLE_POLL
    }
}

/// 退避不能推迟 TERM/KILL 升级或超时判定：本轮睡眠不跨过最近的待处理截止
/// 时间，并保留下限 `ACTIVE_POLL` 以免截止时间已过时忙等。
fn clamp_to_pending_deadline(state: &State, now: Instant, interval: Duration) -> Duration {
    let deadline = match state.phase {
        // `Killing` 沿用已过的 grace 截止时间，因此仍按基线重发 SIGKILL。
        Phase::Stopping | Phase::Killing => state.grace_deadline,
        Phase::Running if !state.interpreter_done => state.deadline,
        _ => None,
    };
    match deadline {
        Some(deadline) => interval.min(deadline.saturating_duration_since(now).max(ACTIVE_POLL)),
        None => interval,
    }
}

pub(super) fn monitor(
    shared: Arc<Shared>,
    execution: CancellationToken,
    parent: CancellationToken,
    stop: CancellationToken,
) {
    let mut backoff = Backoff::new();
    // 上一轮结束时观察到的唤醒计数；跨轮对比即可覆盖“唤醒发生在采样之后、
    // 等待之前”这种不会让条件变量再唤醒一次的窗口。
    let mut seen = shared.generation();
    loop {
        let mut state = lock(&shared.state);
        let progressed = reap(&mut state);
        let phase_before = state.phase;

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
        let active = matches!(state.phase, Phase::Stopping | Phase::Killing)
            || state.procs.values().any(|process| !process.observed);
        let generation = shared.generation();
        let changed = progressed || state.phase != phase_before || generation != seen;
        seen = generation;
        let interval = poll_interval(active, changed, &mut backoff);
        let interval = clamp_to_pending_deadline(&state, Instant::now(), interval);
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
///
/// 返回本轮是否改动了注册表（观察/回收/清理失败）：供 monitor 决定继续退避
/// 还是回到 5ms 基线。常态的空转轮询（子进程仍在跑、组还有成员）不算变化。
fn reap(state: &mut State) -> bool {
    let mut changed = false;
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
                    changed = true;
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
                changed = true;
            }
            Err(error) => {
                state
                    .errors
                    .insert(format!("observing external process {pid}: {error}"));
                // 观察失败不在退避范围内反复推迟：按基线重试并上报。
                changed = true;
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
            changed = true;
            continue;
        };
        match waitpid(Some(raw), WaitOptions::empty()) {
            Ok(_) => {
                state.procs.remove(&pid);
                changed = true;
            }
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::CHILD) => {
                state.procs.remove(&pid);
                changed = true;
            }
            Err(error) => {
                state
                    .errors
                    .insert(format!("reaping external process {pid}: {error}"));
                changed = true;
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
                changed = true;
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
            changed = true;
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
                    changed = true;
                }
                Err(error) => {
                    state
                        .errors
                        .insert(format!("reaping process group leader {leader}: {error}"));
                    changed = true;
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
                changed = true;
            }
        }
    }
    changed
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

#[cfg(test)]
mod tests {
    use super::*;

    fn millis(duration: Duration) -> u64 {
        duration.as_millis() as u64
    }

    /// 空闲期轮询间隔必须逐轮提升到上界，并在任何状态变化后立刻回到基线。
    #[test]
    fn supervisor_poll_backoff_escalates_and_resets_on_state_change() {
        let mut backoff = Backoff::new();
        let quiet_polls: Vec<u64> = (0..6)
            .map(|_| millis(poll_interval(true, false, &mut backoff)))
            .collect();
        assert_eq!(quiet_polls, vec![5, 10, 20, 40, 50, 50]);

        assert_eq!(
            millis(poll_interval(true, true, &mut backoff)),
            5,
            "a state change must return to the baseline interval"
        );
        assert_eq!(millis(poll_interval(true, false, &mut backoff)), 10);

        // 空闲阶段不受 active 退避影响；事件后的第一次复查仍用基线间隔。
        assert_eq!(millis(poll_interval(false, false, &mut backoff)), 50);
        assert_eq!(millis(poll_interval(false, true, &mut backoff)), 5);
        assert_eq!(
            millis(poll_interval(true, true, &mut backoff)),
            5,
            "an idle event must also reset the active backoff"
        );
    }

    /// 退避的睡眠不得跨过待处理截止时间，也不能因为截止时间已过而忙等。
    #[test]
    fn supervisor_poll_interval_never_passes_a_pending_deadline() {
        let now = Instant::now();

        let mut running = State::new(Some(now + Duration::from_millis(30)));
        assert_eq!(
            millis(clamp_to_pending_deadline(&running, now, ACTIVE_POLL_MAX)),
            30
        );

        // 解释器已结束时超时截止不再需要精确唤醒。
        running.interpreter_done = true;
        assert_eq!(
            clamp_to_pending_deadline(&running, now, ACTIVE_POLL_MAX),
            ACTIVE_POLL_MAX
        );

        // TERM/KILL 升级窗口：截止时间临近就按时唤醒；已经过时按基线重发 KILL。
        let mut stopping = State::new(None);
        stopping.phase = Phase::Stopping;
        stopping.grace_deadline = Some(now + Duration::from_millis(400));
        assert_eq!(
            clamp_to_pending_deadline(&stopping, now, ACTIVE_POLL_MAX),
            ACTIVE_POLL_MAX
        );
        stopping.grace_deadline = Some(now - Duration::from_millis(1));
        assert_eq!(
            clamp_to_pending_deadline(&stopping, now, ACTIVE_POLL_MAX),
            ACTIVE_POLL
        );
        stopping.phase = Phase::Killing;
        assert_eq!(
            clamp_to_pending_deadline(&stopping, now, ACTIVE_POLL_MAX),
            ACTIVE_POLL
        );
    }
}
