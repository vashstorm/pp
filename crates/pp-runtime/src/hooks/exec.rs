//! Bounded command hook executor: stdin written once with JSON, capped stdout, stderr with the tail only.
//!
//! Unlike the Bash tool (embedded brush) and MCP/LSP (long-lived protocols), this is a one-shot
//! short-lived process: a single thread alternately writes stdin, reads both pipes and polls for
//! exit, every wait is bounded by a deadline and cancellation, and any failure path reaps the whole
//! process group before returning.

use anyhow::{Context, Result, bail};
use pp_core::cancellation::{CancellationToken, Cancelled, CleanupError};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// stdout cap: exceeding it aborts as an execution failure and never reads unbounded output into memory.
pub(super) const STDOUT_LIMIT: usize = 1024 * 1024;
/// Only the tail of stderr is kept, for diagnostics and the exit-2 reason.
pub(super) const STDERR_LIMIT: usize = 64 * 1024;

const POLL: Duration = Duration::from_millis(10);
const TERM_GRACE: Duration = Duration::from_millis(1000);
const KILL_GRACE: Duration = Duration::from_millis(1000);
const READ_CHUNK: usize = 8 * 1024;
/// Window left for pipe EOF after root exits; descendants may still hold the pipes open.
const DRAIN_GRACE: Duration = Duration::from_millis(1000);

pub(super) struct Request<'a> {
    pub(super) command: &'a str,
    /// `Some` execs directly without a shell; `None` uses `bash -c`.
    pub(super) args: Option<&'a [String]>,
    pub(super) timeout: Duration,
    pub(super) cwd: &'a Path,
    pub(super) input: &'a [u8],
}

#[derive(Debug)]
pub(super) struct Outcome {
    /// `None` when terminated by a signal.
    pub(super) code: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

struct Guard {
    child: Option<Child>,
    group: u32,
}

impl Guard {
    /// probe → TERM → wait → KILL → wait → reap root.
    /// Any failure in the reaping phase (probing, signalling, waiting for exit) means the process
    /// group cannot be confirmed gone and is wrapped uniformly into the single `CleanupError`, so the
    /// caller must stop the task; otherwise `onFailure: continue` would let an unreaped process through
    /// as an ordinary hook failure.
    fn cleanup(&mut self) -> Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        let group = self.group;
        terminate(child, group).map_err(|error| anyhow::Error::new(CleanupError(error)))?;
        self.child = None;
        Ok(())
    }
}

/// Every action of the reaping phase; errors are escalated to `CleanupError` by `Guard::cleanup`.
fn terminate(child: &mut Child, group: u32) -> Result<()> {
    if group_present(group)? {
        signal_group(group, Signal::TERM)?;
        let _ = wait_group(child, group, TERM_GRACE)?;
    }
    if !group_settled(group)? {
        signal_group(group, Signal::KILL)?;
        let _ = wait_group(child, group, KILL_GRACE)?;
    }
    if !group_settled(group)? {
        bail!("hook command process group {group} did not exit");
    }
    if !wait_child(child, KILL_GRACE)? {
        bail!("hook command process {group} did not exit");
    }
    Ok(())
}

impl Drop for Guard {
    fn drop(&mut self) {
        // The explicit path reaped before returning; this only catches missed error branches.
        if let Err(error) = self.cleanup() {
            tracing::error!(
                event = "hook_cleanup_failed",
                group = self.group,
                error = pp_core::logging::diagnostic_text(&format!("{error:#}"))
            );
        }
    }
}

pub(super) fn run(request: Request<'_>, cancellation: &CancellationToken) -> Result<Outcome> {
    let mut command = match request.args {
        Some(args) => {
            let mut command = Command::new(request.command);
            command.args(args);
            command
        }
        None => {
            let mut command = Command::new("bash");
            command.args(["--noprofile", "--norc", "-c", request.command]);
            command
        }
    };
    let spawned = command
        // The child runs only in the event cwd; host cwd/env are never touched.
        .current_dir(request.cwd)
        .env_remove("BASH_ENV")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        // Fixed context: the command text and arguments never reach errors, events or diagnostic logs.
        .context("starting hook command");
    let child = spawned?;
    let group = child.id();
    let mut guard = Guard {
        child: Some(child),
        group,
    };
    match pump(&mut guard, &request, cancellation) {
        Ok(outcome) => {
            guard.cleanup()?;
            Ok(outcome)
        }
        Err(error) => {
            // Failure path: reap first, then report the original error; a cleanup failure keeps the outer CleanupError and the original error stays on the same chain.
            match pp_core::cancellation::merge_cleanup_result(
                Err(error),
                guard.cleanup(),
                "stopping hook command",
            ) {
                Ok(()) => unreachable!("merging an Err result always returns Err"),
                Err(merged) => Err(merged),
            }
        }
    }
}

fn pump(
    guard: &mut Guard,
    request: &Request<'_>,
    cancellation: &CancellationToken,
) -> Result<Outcome> {
    let child = guard.child.as_mut().expect("guard owns the child");
    let mut stdin = Some(
        child
            .stdin
            .take()
            .context("hook command stdin pipe is unavailable")?,
    );
    let mut stdout = child
        .stdout
        .take()
        .context("hook command stdout pipe is unavailable")?;
    let mut stderr = child
        .stderr
        .take()
        .context("hook command stderr pipe is unavailable")?;
    nonblocking(stdin.as_ref().expect("stdin pipe present"))
        .context("configuring hook command stdin pipe")?;
    nonblocking(&stdout).context("configuring hook command stdout pipe")?;
    nonblocking(&stderr).context("configuring hook command stderr pipe")?;
    let deadline = Instant::now()
        .checked_add(request.timeout)
        .context("hook timeout is too large")?;
    let mut pending = request.input;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut out_eof = false;
    let mut err_eof = false;
    let mut status: Option<ExitStatus> = None;
    let mut drain_until: Option<Instant> = None;
    let mut buffer = [0_u8; READ_CHUNK];
    loop {
        if cancellation.is_cancelled() {
            return Err(Cancelled.into());
        }
        let now = Instant::now();
        if now >= deadline {
            bail!(
                "hook command timed out after {}s",
                request.timeout.as_secs()
            );
        }
        if let Some(limit) = drain_until
            && now >= limit
        {
            // root has exited but descendants still hold the pipes: hand over to process-group reaping instead of waiting for EOF.
            break;
        }
        let mut progressed = false;
        let mut close_stdin = false;
        if let Some(pipe) = stdin.as_mut() {
            if pending.is_empty() {
                close_stdin = true;
            } else {
                match pipe.write(pending) {
                    Ok(0) => close_stdin = true,
                    Ok(count) => {
                        pending = &pending[count..];
                        progressed = true;
                        close_stdin = pending.is_empty();
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    // Script does not read stdin: close the write end at once but keep reading its output.
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => close_stdin = true,
                    Err(error) => return Err(error).context("writing hook command stdin"),
                }
            }
        }
        if close_stdin {
            // Only closing the write end lets the script see EOF; merely setting a flag would wait until the timeout.
            stdin = None;
        }
        if !out_eof {
            match stdout.read(&mut buffer) {
                Ok(0) => out_eof = true,
                Ok(count) => {
                    progressed = true;
                    if out.len() + count > STDOUT_LIMIT {
                        bail!("hook command stdout exceeded {STDOUT_LIMIT} bytes");
                    }
                    out.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("reading hook command stdout"),
            }
        }
        if !err_eof {
            match stderr.read(&mut buffer) {
                Ok(0) => err_eof = true,
                Ok(count) => {
                    progressed = true;
                    push_tail(&mut err, &buffer[..count]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("reading hook command stderr"),
            }
        }
        if status.is_none()
            && let Some(exit) = child.try_wait().context("checking hook command")?
        {
            status = Some(exit);
            drain_until = Some(Instant::now() + DRAIN_GRACE);
        }
        if status.is_some() && out_eof && err_eof && stdin.is_none() {
            break;
        }
        if !progressed {
            // Wait only when there is no progress, so steady output is not slowed by the poll interval.
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(now)));
        }
    }
    Ok(Outcome {
        code: status.and_then(|status| status.code()),
        stdout: out,
        stderr: err,
    })
}

/// Only the tail of stderr is kept: excess is dropped from the front, O(excess bytes) and constant total.
fn push_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    tail.extend_from_slice(bytes);
    if tail.len() > STDERR_LIMIT {
        let excess = tail.len() - STDERR_LIMIT;
        tail.drain(..excess);
    }
}

fn nonblocking(pipe: &impl std::os::fd::AsFd) -> Result<()> {
    let flags = fcntl_getfl(pipe)?;
    fcntl_setfl(pipe, flags | OFlags::NONBLOCK)?;
    Ok(())
}

fn wait_child(child: &mut Child, window: Duration) -> Result<bool> {
    let deadline = Instant::now() + window;
    loop {
        if child.try_wait().context("checking hook command")?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

fn wait_group(child: &mut Child, group: u32, window: Duration) -> Result<bool> {
    let deadline = Instant::now() + window;
    loop {
        let _ = child.try_wait().context("checking hook command")?;
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
        // macOS returns EPERM for a process group that only holds zombies; a leader that has exited counts as done.
        Err(Errno::PERM) if cfg!(target_os = "macos") => {
            if group_leader_gone(group)? {
                Ok(())
            } else {
                bail!("cannot signal the hook command process group {id}");
            }
        }
        Err(error) => Err(error).context("terminating hook command process group"),
    }
}

fn group_present(id: u32) -> Result<bool> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(false);
    };
    match test_kill_process_group(group) {
        Ok(()) => Ok(true),
        Err(Errno::SRCH) => Ok(false),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(true),
        Err(error) => Err(error).context("checking hook command process group"),
    }
}

fn group_settled(id: u32) -> Result<bool> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(true);
    };
    match test_kill_process_group(group) {
        Ok(()) => Ok(false),
        Err(Errno::SRCH) => Ok(true),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(true),
        Err(error) => Err(error).context("checking hook command process group"),
    }
}

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
