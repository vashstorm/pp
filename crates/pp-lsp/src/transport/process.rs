//! Startup, stderr aggregation and process-group reaping for managed stdio processes.

use anyhow::{Context, Result, bail};
use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::io::{reader_loop, writer_loop};
use super::{
    KILL_GRACE, NotificationSink, POLL, ProcessSlot, STDERR_LIMIT, Shared, TERM_GRACE, Transport,
    WRITE_QUEUE,
};

impl Transport {
    /// Start the language server process; only the last 64 KiB of stderr is kept.
    pub(crate) fn spawn(
        command: &Path,
        args: &[String],
        cwd: &Path,
        process: Arc<Mutex<ProcessSlot>>,
        sink: NotificationSink,
    ) -> Result<Self> {
        {
            let slot = process.lock().unwrap_or_else(|e| e.into_inner());
            anyhow::ensure!(!slot.closing, "LSP client is shutting down");
            anyhow::ensure!(
                slot.child.is_none(),
                "LSP server process has not been reaped"
            );
        }
        let child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting LSP server {}", command.display()))?;
        // Register the child as soon as it exists, so any later pipe/thread failure leaves a retryable slot
        // instead of handing an unreaped process back to the caller.
        {
            let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
            if slot.closing || slot.child.is_some() {
                drop(slot);
                // A process that was not registered successfully is reaped by this thread and never enters the registry's slot.
                let orphan = Arc::new(Mutex::new(ProcessSlot {
                    child: Some(child),
                    closing: true,
                }));
                terminate_slot(&orphan, Duration::ZERO)
                    .map_err(pp_core::cancellation::CleanupError)?;
                bail!("LSP client is shutting down");
            }
            slot.child = Some(child);
        }
        let (stdin, stdout, stderr) = {
            let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
            let child = slot
                .child
                .as_mut()
                .context("LSP server process was not registered")?;
            (
                child.stdin.take().context("LSP server stdin unavailable")?,
                child
                    .stdout
                    .take()
                    .context("LSP server stdout unavailable")?,
                child
                    .stderr
                    .take()
                    .context("LSP server stderr unavailable")?,
            )
        };

        let (write_tx, write_rx) = sync_channel::<Vec<u8>>(WRITE_QUEUE);
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(write_tx)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            callbacks: Mutex::new(VecDeque::new()),
            active: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            settings: Mutex::new(Value::Null),
            folders: Mutex::new(Vec::new()),
            registrations: Mutex::new(HashMap::new()),
            stderr: Mutex::new(Vec::new()),
            reason: Mutex::new(None),
        });

        let thread_shared = shared.clone();
        let threads = (move || -> Result<()> {
            let writer_shared = thread_shared.clone();
            std::thread::Builder::new()
                .name("pp-lsp-writer".into())
                .spawn(move || writer_loop(stdin, write_rx, writer_shared))
                .context("starting LSP writer thread")?;

            let reader_shared = thread_shared.clone();
            std::thread::Builder::new()
                .name("pp-lsp-reader".into())
                .spawn(move || reader_loop(stdout, reader_shared, sink))
                .context("starting LSP reader thread")?;

            let stderr_shared = thread_shared.clone();
            std::thread::Builder::new()
                .name("pp-lsp-stderr".into())
                .spawn(move || drain_stderr(stderr, stderr_shared))
                .context("starting LSP stderr thread")?;
            Ok(())
        })();
        if let Err(error) = threads {
            // When starting a thread fails, the writer must still see the queue disconnect, otherwise an already started writer
            // thread would block forever on recv; the process stays in the slot so the reap can be retried.
            *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = terminate_slot(&process, Duration::ZERO);
            return Err(error);
        }

        // While threads were starting, the cleanup path may already have set closing and reaped the process; in that case a transport
        // claiming to be usable must not be returned.
        let closing = {
            let slot = process.lock().unwrap_or_else(|e| e.into_inner());
            slot.closing
        };
        if closing {
            *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
            terminate_slot(&process, Duration::ZERO)
                .map_err(pp_core::cancellation::CleanupError)?;
            shared.note_close("LSP client is shutting down");
            bail!("LSP client is shutting down");
        }

        Ok(Self {
            process,
            shared,
            next_id: AtomicI64::new(1),
        })
    }

    /// Close stdin and wait for the process/process group to exit; keeps the slot and errors when reaping is unconfirmed.
    pub(crate) fn terminate(&self, grace: Duration) -> Result<()> {
        // Disconnect the write queue first: the writer thread exits and closes stdin, giving the server a chance to exit on its own.
        *self.shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
        terminate_slot(&self.process, grace)?;
        self.shared.note_close("LSP server was terminated");
        Ok(())
    }
}

/// Reap a server process slot: the root exiting does not mean its descendants closed the pipes, so the whole
/// process group must be confirmed gone. The registry also uses this when cancelling/closing during initialization, so it does not depend on
/// `Transport` (whose `shared` is not available yet).
///
/// On failure `slot.child` is kept so the caller can retry; an unconfirmed reap is never reported as success.
pub(crate) fn terminate_slot(process: &Arc<Mutex<ProcessSlot>>, grace: Duration) -> Result<()> {
    let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
    slot.closing = true;
    let Some(child) = slot.child.as_mut() else {
        return Ok(());
    };
    let group = child.id();
    // Within the grace period, first let the root exit by itself (it usually does once stdin is closed).
    let _ = wait_child(child, grace)?;
    // The root exiting does not mean its descendants closed the pipes: signal for reaping as long as the group is still there.
    // Probe "still present" first (EPERM counts as present) before deciding to signal, so no signal is sent to a
    // pgid that is already gone (the pgid may have been reused).
    if group_present(group)? {
        signal_group(group, Signal::TERM)?;
        let _ = wait_group(child, group, TERM_GRACE)?;
    }
    if !group_settled(group)? {
        signal_group(group, Signal::KILL)?;
        let _ = wait_group(child, group, KILL_GRACE)?;
    }
    if !group_settled(group)? {
        bail!("LSP server process group {group} did not exit");
    }
    // A gone group or one with only zombies means the root has exited too; confirm and reap at the end so no
    // zombie process is left behind.
    if !wait_child(child, KILL_GRACE)? {
        bail!("LSP server process {group} did not exit");
    }
    slot.child = None;
    Ok(())
}

/// Wait for the root to exit within the window; returns whether it was reaped.
fn wait_child(child: &mut Child, window: Duration) -> Result<bool> {
    let deadline = Instant::now() + window;
    loop {
        if child
            .try_wait()
            .context("checking LSP server process")?
            .is_some()
        {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

/// Wait for the whole process group to settle within the window while continuing to reap the root.
fn wait_group(child: &mut Child, group: u32, window: Duration) -> Result<bool> {
    let deadline = Instant::now() + window;
    loop {
        let _ = child.try_wait().context("checking LSP server process")?;
        if group_settled(group)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

fn signal_group(id: u32, signal: Signal) -> Result<()> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(());
    };
    match kill_process_group(group, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        // macOS returns EPERM for a group with only zombies left; once the leader has exited/been reaped, cleanup is
        // considered complete (zombie members cannot run and cannot answer signals).
        Err(Errno::PERM) if cfg!(target_os = "macos") => {
            if group_leader_gone(group)? {
                Ok(())
            } else {
                bail!("cannot signal the LSP server process group {id}");
            }
        }
        Err(error) => Err(error).context("terminating LSP process group"),
    }
}

/// Whether the process group still exists: EPERM also counts as present, so a signal must be sent before deciding it settled, avoiding mistaking
/// "only zombies plus live descendants" for empty.
fn group_present(id: u32) -> Result<bool> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(false);
    };
    match test_kill_process_group(group) {
        Ok(()) => Ok(true),
        Err(Errno::SRCH) => Ok(false),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(true),
        Err(error) => Err(error).context("checking LSP server process group"),
    }
}

/// Whether the process group has no runnable members left: ESRCH, or a rejected signal on macOS (only zombies).
/// This does not equal "clean success" — the caller must still confirm the root was reaped.
fn group_settled(id: u32) -> Result<bool> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(true);
    };
    match test_kill_process_group(group) {
        Ok(()) => Ok(false),
        Err(Errno::SRCH) => Ok(true),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(true),
        Err(error) => Err(error).context("checking LSP server process group"),
    }
}

/// The leader has exited (not yet reaped) or was already reaped; ECHILD means it was already waited for.
fn group_leader_gone(group: Pid) -> io::Result<bool> {
    loop {
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(Errno::INTR) => continue,
            Err(Errno::CHILD) => return Ok(true),
            Err(error) => return Err(error.into()),
        }
    }
}

fn drain_stderr(mut stderr: impl Read, shared: Arc<Shared>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let mut tail = shared.stderr.lock().unwrap_or_else(|e| e.into_inner());
                tail.extend_from_slice(&buffer[..count]);
                if tail.len() > STDERR_LIMIT {
                    let excess = tail.len() - STDERR_LIMIT;
                    tail.drain(..excess);
                }
            }
        }
    }
}

#[cfg(test)]
mod integration {
    use super::super::fixtures::*;
    use super::*;

    fn child_in_own_group(script: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn shell")
    }

    /// Observe the leader exit with WNOWAIT without reaping it, simulating "the root has exited while the slot still holds its handle".
    fn wait_leader_exit(group: u32) {
        let pid = Pid::from_raw(group as i32).expect("valid group id");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(Errno::INTR) => {}
                Err(error) => panic!("waitid failed: {error}"),
            }
            assert!(Instant::now() < deadline, "shell did not exit in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lsp_transport_terminate_slot_marks_closing_and_is_idempotent() {
        let process = empty_slot();
        terminate_slot(&process, Duration::ZERO).expect("first terminate");
        assert!(process.lock().unwrap().closing);
        terminate_slot(&process, Duration::ZERO).expect("second terminate");
        assert!(process.lock().unwrap().child.is_none());
    }

    #[test]
    fn lsp_transport_terminate_slot_reclaims_descendants_after_root_exit() {
        let child = child_in_own_group("sleep 60 & exit 0");
        let group = child.id();
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: Some(child),
            closing: false,
        }));
        wait_leader_exit(group);
        // The root has exited but is still a zombie; the background sleep still holds the same process group.
        terminate_slot(&process, Duration::ZERO).expect("terminate");
        assert!(process.lock().unwrap().child.is_none());
        assert!(group_settled(group).expect("probe group"));
    }

    #[test]
    fn lsp_transport_terminate_slot_kills_group_after_root_was_reaped() {
        let mut child = child_in_own_group("sleep 60 & exit 0");
        let group = child.id();
        wait_leader_exit(group);
        // The caller reaps the root first: Child still keeps the id and the cached exit status.
        assert!(child.try_wait().expect("reap root").is_some());
        assert!(group_present(group).expect("probe group"));
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: Some(child),
            closing: false,
        }));
        terminate_slot(&process, Duration::ZERO).expect("terminate");
        assert!(process.lock().unwrap().child.is_none());
        assert!(group_settled(group).expect("probe group"));
    }

    #[test]
    fn lsp_transport_spawn_refuses_after_cleanup_marks_slot_closing() {
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: true,
        }));
        let error = match Transport::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 60".to_owned()],
            Path::new("/"),
            process,
            no_sink(),
        ) {
            Ok(_) => panic!("spawn must be refused while closing"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("shutting down"), "{error}");
    }

    #[test]
    fn lsp_transport_terminate_reclaims_the_whole_group() {
        let process = empty_slot();
        let transport = Transport::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 60".to_owned()],
            Path::new("/"),
            process.clone(),
            no_sink(),
        )
        .expect("spawn server");
        let group = process
            .lock()
            .unwrap()
            .child
            .as_ref()
            .expect("registered child")
            .id();
        transport
            .terminate(Duration::ZERO)
            .expect("terminate server");
        assert!(process.lock().unwrap().child.is_none());
        assert!(transport.is_closed());
        assert!(group_settled(group).expect("probe group"));
    }
}
