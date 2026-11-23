//! Killing 阶段单测：假 ProcessCell 与显式 deadline 驱动的可终止转移。
//!
//! 这些用例不创建真实子进程，直接构造 `State` 并调用 `advance_killing`，
//! 验证 SIGKILL 后的回收等待有界、等待者被唤醒、记录保持真实状态。

use super::super::process::{CellInner, ProcessCell, ProcessRecord};
use super::super::state::GroupRecord;
use super::{GRACE, KILL_ABANDONED, Phase, State};
use super::{abandon_killing, advance_killing, draining_reaches_finished};
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Instant,
};

/// 假 ProcessCell 的等待者：记录被唤醒次数，并可轮询最终结果。
struct WakeCounter(AtomicU64);

impl std::task::Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Release);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

fn killing_state(pid: i32, cell: Arc<ProcessCell>, kill_deadline: Option<Instant>) -> State {
    let mut state = State::new(None);
    state.phase = Phase::Killing;
    state.kill_deadline = kill_deadline;
    state.groups.insert(
        0,
        GroupRecord {
            pgid: pid,
            leader: pid,
            lease: Weak::new(),
        },
    );
    state.procs.insert(
        pid,
        ProcessRecord {
            cell,
            group: 0,
            leader: true,
            observed: false,
            reaped: false,
        },
    );
    state
}

fn fake_cell(pid: i32) -> Arc<ProcessCell> {
    Arc::new(ProcessCell {
        pid,
        pgid: pid,
        inner: Mutex::new(CellInner::default()),
    })
}

/// 到期仍未清空的注册表必须放弃回收：等待者被唤醒、未完成 cell 拿到清理
/// 错误、记录保持真实未回收状态，monitor 可从 Killing 进入可终止的 Draining。
#[test]
fn supervisor_killing_deadline_abandons_and_wakes_waiters() {
    let pid = 2_000_000_000;
    let cell = fake_cell(pid);
    let waiter = Arc::new(WakeCounter(AtomicU64::new(0)));
    let waker = Waker::from(waiter.clone());
    let mut cx = Context::from_waker(&waker);
    // 注册一个等待者：当前实现下 poll 必然 Pending。
    assert!(cell.poll(&mut cx).is_pending());

    let mut state = killing_state(pid, cell.clone(), Some(Instant::now() - GRACE));
    advance_killing(&mut state, Instant::now());

    assert_eq!(state.phase, Phase::Draining);
    assert!(
        state.abandoned,
        "the kill window must be recorded as abandoned"
    );
    assert!(state.errors.contains(KILL_ABANDONED));
    // 等待者被唤醒；未完成 cell 拿到清理错误而非伪造退出码。
    assert!(
        waiter.0.load(Ordering::Acquire) >= 1,
        "the waiting waker must be woken"
    );
    let error = match cell.poll(&mut cx) {
        Poll::Ready(Err(error)) => error,
        Poll::Ready(Ok(status)) => panic!("an unreclaimed cell faked exit {status:?}"),
        Poll::Pending => panic!("the unreclaimed cell must surface a cleanup error"),
    };
    let message = error.to_string();
    assert!(
        message.contains("not reclaimed after SIGKILL"),
        "unexpected cell error: {message}"
    );
    // 记录保持真实状态：未标 reaped，也未被移出注册表。
    let record = &state.procs[&pid];
    assert!(!record.reaped);
    assert!(!record.observed);
    assert!(state.groups.contains_key(&0));

    // Draining 终态判定：解释器已结束且已放弃回收时可以立即收尾。
    state.interpreter_done = true;
    assert!(draining_reaches_finished(&state));
}

/// 未到期时继续重发 SIGKILL；注册表清空后进入 Draining 而不放弃。
#[test]
fn supervisor_killing_within_window_resignals_and_drains_when_empty() {
    let pid = 2_000_000_000;
    // 仍在本进程组外：对不存在的组发送信号只会得到 ESRCH，不会误报错误。
    let mut state = killing_state(pid, fake_cell(pid), Some(Instant::now() + GRACE));
    advance_killing(&mut state, Instant::now());
    assert_eq!(state.phase, Phase::Killing);
    assert!(!state.abandoned);
    assert!(state.errors.is_empty());

    // 注册表清空后不再等待，进入 Draining。
    state.procs.clear();
    state.groups.clear();
    advance_killing(&mut state, Instant::now());
    assert_eq!(state.phase, Phase::Draining);
    assert!(!state.abandoned);
    assert!(state.errors.is_empty());
    state.interpreter_done = true;
    assert!(draining_reaches_finished(&state));
}

/// 未经 begin_killing 的状态也必须有界：缺失 deadline 时补上，不会无限重发。
#[test]
fn supervisor_killing_without_deadline_gets_one_and_stays_bounded() {
    let pid = 2_000_000_000;
    let mut state = killing_state(pid, fake_cell(pid), None);
    let now = Instant::now();
    advance_killing(&mut state, now);
    assert_eq!(state.phase, Phase::Killing);
    assert_eq!(
        state.kill_deadline,
        Some(now.checked_add(GRACE).unwrap_or(now)),
        "a missing kill deadline must be filled in bounded"
    );
    assert!(!state.abandoned);
}

/// 到期时若所有注册进程都已观察为退出、且组内无存活成员，剩下的只是等待
/// 解释器释放组租约并回收 anchor：SIGKILL 已生效，不得判为回收失败。
/// 阻塞的调用方（如输出回调）只影响解释器进度，不是内核拒绝回收。
#[test]
fn supervisor_killing_deadline_waits_for_interpreter_bookkeeping() {
    let pid = 2_000_000_000;
    let cell = fake_cell(pid);
    cell.complete(super::super::process::CellOutcome::Status(
        std::process::ExitStatus::default(),
    ));
    let mut state = killing_state(pid, cell, Some(Instant::now() - GRACE));
    state.procs.get_mut(&pid).expect("record").observed = true;

    let now = Instant::now();
    advance_killing(&mut state, now);

    assert_eq!(state.phase, Phase::Killing);
    assert!(
        !state.abandoned,
        "reclamation awaiting the interpreter must not be abandoned"
    );
    assert!(state.errors.is_empty());
    assert_eq!(
        state.kill_deadline,
        Some(now.checked_add(GRACE).unwrap_or(now)),
        "the kill window must be refreshed for in-progress bookkeeping"
    );
    // 解释器释放租约后注册表清空，正常进入 Draining 且不报清理错误。
    state.procs.clear();
    state.groups.clear();
    advance_killing(&mut state, Instant::now());
    assert_eq!(state.phase, Phase::Draining);
    assert!(!state.abandoned);
    assert!(state.errors.is_empty());
}

/// 组内仍有存活成员时，即使登记的直接子进程都已观察退出，到期也必须放弃
/// 回收：这是真正不可回收的场景。
#[test]
fn supervisor_killing_deadline_abandons_when_group_members_stay_alive() {
    let pid = 2_000_000_000;
    let cell = fake_cell(pid);
    cell.complete(super::super::process::CellOutcome::Status(
        std::process::ExitStatus::default(),
    ));
    let mut state = killing_state(pid, cell, Some(Instant::now() - GRACE));
    state.procs.get_mut(&pid).expect("record").observed = true;
    // 宿主进程组必然有存活成员，代表“组内仍有不可杀的成员”。
    state.groups.get_mut(&0).expect("group").pgid = super::host_pgid();

    advance_killing(&mut state, Instant::now());

    assert_eq!(state.phase, Phase::Draining);
    assert!(state.abandoned);
    assert!(state.errors.contains(KILL_ABANDONED));
}

/// 幂等性：已完成的 cell 保持真实退出状态，放弃回收不覆盖真实结果，也不重复
/// 写入清理错误。
#[test]
fn supervisor_killing_abandon_is_idempotent_for_completed_cells() {
    let pid = 2_000_000_000;
    let cell = fake_cell(pid);
    cell.complete(super::super::process::CellOutcome::Status(
        std::process::ExitStatus::default(),
    ));
    let mut state = killing_state(pid, cell.clone(), Some(Instant::now() - GRACE));
    abandon_killing(&mut state);
    abandon_killing(&mut state);
    assert!(state.abandoned);
    let waiter = Arc::new(WakeCounter(AtomicU64::new(0)));
    let waker = Waker::from(waiter.clone());
    let mut cx = Context::from_waker(&waker);
    // 已完成 cell 不被清理错误覆盖：仍报告真实退出状态 0。
    assert!(matches!(cell.poll(&mut cx), Poll::Ready(Ok(_))));
    assert_eq!(waiter.0.load(Ordering::Acquire), 0, "no waker to wake");
}
