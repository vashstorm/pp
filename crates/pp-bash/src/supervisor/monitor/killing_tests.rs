//! Killing-phase unit tests: terminable transitions driven by fake `ProcessCell`s and explicit deadlines.
//!
//! These cases create no real child process; they build `State` directly and call `advance_killing` to
//! verify that reclamation after SIGKILL waits with a bound, that waiters are woken, and that records keep
//! their real state.

mod integration {
    use super::super::super::process::{CellInner, ProcessCell, ProcessRecord};
    use super::super::super::state::GroupRecord;
    use super::super::{GRACE, KILL_ABANDONED, Phase, State};
    use super::super::{abandon_killing, advance_killing, draining_reaches_finished};
    use std::{
        sync::{
            Arc, Mutex, Weak,
            atomic::{AtomicU64, Ordering},
        },
        task::{Context, Poll, Waker},
        time::Instant,
    };

    /// Waiter of a fake `ProcessCell`: counts wakeups and can poll for the final result.
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

    /// A registry still not empty at the deadline must abandon reclamation: waiters are woken, unfinished cells
    /// get a cleanup error, records keep their real unreclaimed state, and the monitor can move from Killing to a
    /// terminable Draining.
    #[test]
    fn supervisor_killing_deadline_window_and_missing_deadline() {
        {
            let pid = 2_000_000_000;
            let cell = fake_cell(pid);
            let waiter = Arc::new(WakeCounter(AtomicU64::new(0)));
            let waker = Waker::from(waiter.clone());
            let mut cx = Context::from_waker(&waker);
            // Register a waiter: under the current implementation poll is necessarily Pending.
            assert!(cell.poll(&mut cx).is_pending());

            let mut state = killing_state(pid, cell.clone(), Some(Instant::now() - GRACE));
            advance_killing(&mut state, Instant::now());

            assert_eq!(state.phase, Phase::Draining);
            assert!(
                state.abandoned,
                "the kill window must be recorded as abandoned"
            );
            assert!(state.errors.contains(KILL_ABANDONED));
            // The waiter is woken; the unfinished cell receives a cleanup error instead of a fabricated exit code.
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
            // The record keeps its real state: not marked reaped, and not removed from the registry.
            let record = &state.procs[&pid];
            assert!(!record.reaped);
            assert!(!record.observed);
            assert!(state.groups.contains_key(&0));

            // Draining terminal-state decision: with the interpreter finished and reclamation abandoned the run can
            // finish immediately.
            state.interpreter_done = true;
            assert!(draining_reaches_finished(&state));
        }
        {
            let pid = 2_000_000_000;
            // Still outside this process group: signaling a nonexistent group only yields ESRCH, never a false error.
            let mut state = killing_state(pid, fake_cell(pid), Some(Instant::now() + GRACE));
            advance_killing(&mut state, Instant::now());
            assert_eq!(state.phase, Phase::Killing);
            assert!(!state.abandoned);
            assert!(state.errors.is_empty());

            // Once the registry is empty there is nothing left to wait for, so Draining follows.
            state.procs.clear();
            state.groups.clear();
            advance_killing(&mut state, Instant::now());
            assert_eq!(state.phase, Phase::Draining);
            assert!(!state.abandoned);
            assert!(state.errors.is_empty());
            state.interpreter_done = true;
            assert!(draining_reaches_finished(&state));
        }
        {
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
    }

    /// When the deadline expires while every registered process has been observed as exited and the group has no
    /// live members, what remains is only waiting for the interpreter to release the group lease and reclaim the
    /// anchor: SIGKILL already took effect, so this must not be judged a reclamation failure.
    #[test]
    fn supervisor_killing_bookkeeping_group_alive_and_idempotency() {
        {
            let pid = 2_000_000_000;
            let cell = fake_cell(pid);
            cell.complete(super::super::super::process::CellOutcome::Status(
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
            // Once the interpreter releases the lease the registry empties, Draining is entered normally and no cleanup
            // error is reported.
            state.procs.clear();
            state.groups.clear();
            advance_killing(&mut state, Instant::now());
            assert_eq!(state.phase, Phase::Draining);
            assert!(!state.abandoned);
            assert!(state.errors.is_empty());
        }
        {
            let pid = 2_000_000_000;
            let cell = fake_cell(pid);
            cell.complete(super::super::super::process::CellOutcome::Status(
                std::process::ExitStatus::default(),
            ));
            let mut state = killing_state(pid, cell, Some(Instant::now() - GRACE));
            state.procs.get_mut(&pid).expect("record").observed = true;
            // The host process group necessarily has live members, standing in for "the group still has members that
            // cannot be killed".
            state.groups.get_mut(&0).expect("group").pgid = super::super::host_pgid();

            advance_killing(&mut state, Instant::now());

            assert_eq!(state.phase, Phase::Draining);
            assert!(state.abandoned);
            assert!(state.errors.contains(KILL_ABANDONED));
        }
        {
            let pid = 2_000_000_000;
            let cell = fake_cell(pid);
            cell.complete(super::super::super::process::CellOutcome::Status(
                std::process::ExitStatus::default(),
            ));
            let mut state = killing_state(pid, cell.clone(), Some(Instant::now() - GRACE));
            abandon_killing(&mut state);
            abandon_killing(&mut state);
            assert!(state.abandoned);
            let waiter = Arc::new(WakeCounter(AtomicU64::new(0)));
            let waker = Waker::from(waiter.clone());
            let mut cx = Context::from_waker(&waker);
            // A finished cell is not overwritten by the cleanup error: it still reports the real exit status 0.
            assert!(matches!(cell.poll(&mut cx), Poll::Ready(Ok(_))));
            assert_eq!(waiter.0.load(Ordering::Acquire), 0, "no waker to wake");
        }
    }
}
