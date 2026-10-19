use super::{StreamText, drain_pipe, emit_output};
use crate::cancellation::{CancellationCleanupError, CancellationToken};
use crate::events::{AgentEvent, OutputStream};
use anyhow::{Context, Result};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    os::{fd::AsFd, unix::process::CommandExt},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(5);
const GRACE: Duration = Duration::from_secs(1);
const OUTPUT_LINE_LIMIT: usize = 500;

/// Distinguishes a real exit from the two supervised stop reasons; a single
/// bool cannot keep timeout and cancellation apart when both race.
enum Termination {
    Exited,
    TimedOut,
    Cancelled,
}

fn retain_lines<'a>(bytes: &'a [u8], at_line_start: &mut bool, remaining: &mut usize) -> &'a [u8] {
    for (index, byte) in bytes.iter().enumerate() {
        if *at_line_start {
            if *remaining == 0 {
                return &bytes[..index];
            }
            *remaining -= 1;
        }
        *at_line_start = *byte == b'\n';
    }
    bytes
}

struct ProcessGroup {
    child: Child,
    group: Pid,
    reaped: bool,
}
impl ProcessGroup {
    fn signal(&self, signal: Signal) -> io::Result<()> {
        match kill_process_group(self.group, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            // macOS 对仅剩僵尸进程的组返回 EPERM，确认直接子进程已退出后视为清理完成。
            Err(rustix::io::Errno::PERM) if cfg!(target_os = "macos") && self.exited()? => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn exited(&self) -> io::Result<bool> {
        loop {
            match waitid(
                WaitId::Pid(self.group),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(status) => return Ok(status.is_some()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn supervise(
        mut self,
        timeout: Duration,
        stop: &AtomicBool,
        readers: &AtomicUsize,
        failed: &AtomicBool,
        cancellation: &CancellationToken,
    ) -> Result<(ExitStatus, Termination)> {
        // 无论监督线程如何退出，都唤醒非阻塞管道读取线程。
        struct Stop<'a>(&'a AtomicBool);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let _stop = Stop(stop);
        let started = Instant::now();
        loop {
            // 清理整个组之前保留直接子进程的等待状态，防止 PID/进程组号被复用。
            let exited = self.exited().context("checking command status")?;
            if exited && readers.load(Ordering::Acquire) == 2 {
                return Ok((self.finish()?, Termination::Exited));
            }
            if failed.load(Ordering::Acquire) {
                return Ok((self.finish()?, Termination::Exited));
            }
            // 同一轮内取消优先于 deadline；超时一旦选定并发出 TERM，
            // 后续 grace 期间的取消不再改判为取消。
            if cancellation.is_cancelled() {
                let status = self
                    .stop_cancelled()
                    .map_err(|error| anyhow::Error::new(CancellationCleanupError(error)))?;
                return Ok((status, Termination::Cancelled));
            }
            if started.elapsed() >= timeout {
                self.signal(Signal::TERM)
                    .context("terminating timed out command process group")?;
                thread::sleep(GRACE);
                return Ok((self.finish()?, Termination::TimedOut));
            }
            thread::sleep(POLL);
        }
    }

    // 取消收尾的 TERM/grace/KILL/wait 失败都包装为 typed
    // CancellationCleanupError，不能被当作普通工具失败吞掉。
    fn stop_cancelled(&mut self) -> Result<ExitStatus> {
        self.signal(Signal::TERM)
            .context("terminating cancelled command process group")?;
        thread::sleep(GRACE);
        self.finish()
    }

    fn finish(&mut self) -> Result<ExitStatus> {
        self.signal(Signal::KILL)
            .context("cleaning up command process group")?;
        let status = self.child.wait().context("reaping command")?;
        self.reaped = true;
        Ok(status)
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // 正常结束也清理留在同一进程组的后台任务；异常路径负责回收直接子进程。
        if !self.reaped {
            let _ = self.signal(Signal::KILL);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn nonblocking(pipe: &impl AsFd) -> Result<()> {
    fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK)
        .context("configuring nonblocking command pipe")
}

fn reader(
    pipe: impl Read,
    stream: OutputStream,
    sender: SyncSender<(OutputStream, Vec<u8>)>,
    stop: &AtomicBool,
    done: &AtomicUsize,
    failed: &AtomicBool,
) -> io::Result<bool> {
    struct Done<'a>(&'a AtomicUsize);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }
    let _done = Done(done);
    let result = drain_pipe(pipe, stream, sender, stop);
    if result.is_err() {
        failed.store(true, Ordering::Release);
    }
    result
}

pub(super) fn run(
    cwd: &Path,
    cache: &Path,
    command: &str,
    timeout_seconds: u64,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<Value> {
    let mut process = Command::new("bash");
    process.arg("-c").arg(command);
    run_command(
        cwd,
        cache,
        process,
        timeout_seconds,
        call_id,
        cancellation,
        emit,
    )
    .context("running bash")
}

pub(super) fn run_command(
    cwd: &Path,
    cache: &Path,
    mut command: Command,
    timeout_seconds: u64,
    call_id: &str,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<Value> {
    let scratch = tempfile::Builder::new()
        .prefix(".pp-tmp-")
        .tempdir_in(cwd)
        .context("creating command temporary directory")?;
    let child = command
        .current_dir(cwd)
        .env("TMPDIR", scratch.path())
        .env("TMP", scratch.path())
        .env("TEMP", scratch.path())
        .env("XDG_CACHE_HOME", cache)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .context("starting command")?;
    let group = Pid::from_raw(child.id() as i32).expect("a spawned child has a positive pid");
    let mut process = ProcessGroup {
        child,
        group,
        reaped: false,
    };
    let stdout = process
        .child
        .stdout
        .take()
        .context("opening command stdout")?;
    let stderr = process
        .child
        .stderr
        .take()
        .context("opening command stderr")?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let stop = AtomicBool::new(false);
    let done = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let mut stdout_text = StreamText::default();
    let mut stderr_text = StreamText::default();
    let mut remaining_lines = OUTPUT_LINE_LIMIT;
    let mut stdout_line_start = true;
    let mut stderr_line_start = true;
    let mut stdout_line_truncated = false;
    let mut stderr_line_truncated = false;
    let (status, termination, stdout_truncated, stderr_truncated) =
        thread::scope(|scope| -> Result<_> {
            let (sender, receiver) = sync_channel(16);
            let err_sender = sender.clone();
            let out =
                scope.spawn(|| reader(stdout, OutputStream::Stdout, sender, &stop, &done, &failed));
            let err = scope.spawn(|| {
                reader(
                    stderr,
                    OutputStream::Stderr,
                    err_sender,
                    &stop,
                    &done,
                    &failed,
                )
            });
            let monitor = scope.spawn(|| {
                process.supervise(
                    Duration::from_secs(timeout_seconds),
                    &stop,
                    &done,
                    &failed,
                    cancellation,
                )
            });
            for (stream, bytes) in receiver {
                let (at_line_start, truncated) = match stream {
                    OutputStream::Stdout => (&mut stdout_line_start, &mut stdout_line_truncated),
                    OutputStream::Stderr => (&mut stderr_line_start, &mut stderr_line_truncated),
                };
                let retained = retain_lines(&bytes, at_line_start, &mut remaining_lines);
                *truncated |= retained.len() < bytes.len();
                let text = match stream {
                    OutputStream::Stdout => stdout_text.append(retained),
                    OutputStream::Stderr => stderr_text.append(retained),
                };
                emit_output(call_id, stream, text, emit);
            }
            emit_output(call_id, OutputStream::Stdout, stdout_text.finish(), emit);
            emit_output(call_id, OutputStream::Stderr, stderr_text.finish(), emit);
            let (status, termination) = monitor
                .join()
                .map_err(|_| anyhow::anyhow!("command supervisor thread failed"))??;
            let out = out
                .join()
                .map_err(|_| anyhow::anyhow!("command stdout reader thread failed"))??;
            let err = err
                .join()
                .map_err(|_| anyhow::anyhow!("command stderr reader thread failed"))??;
            Ok((
                status,
                termination,
                out || stdout_line_truncated,
                err || stderr_line_truncated,
            ))
        })?;
    let (ok, timed_out, cancelled) = match termination {
        Termination::Exited => (status.success(), false, false),
        Termination::TimedOut => (false, true, false),
        Termination::Cancelled => (false, false, true),
    };
    let mut result = json!({
        "ok":ok, "exit_code":status.code(), "timed_out":timed_out, "cancelled":cancelled,
        "stdout":stdout_text.text, "stderr":stderr_text.text,
        "stdout_truncated":stdout_truncated, "stderr_truncated":stderr_truncated,
    });
    if cancelled {
        result["error"] = json!("Command cancelled; side effects were not rolled back.");
    }
    if let Err(error) = scratch.close() {
        result["warning"] = json!(format!(
            "command finished, but temporary directory cleanup failed: {error}"
        ));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_limit_handles_chunk_boundaries_and_unterminated_lines() {
        for line in ["中😀\n", "\r\n", "\n"] {
            for last_newline in [false, true] {
                let mut exact = line.repeat(OUTPUT_LINE_LIMIT);
                if !last_newline {
                    exact.pop();
                }
                for excess in ["", "extra", "extra\n"] {
                    let input = format!("{exact}{excess}");
                    let expected = if last_newline { &exact } else { &input };
                    for chunk_size in [1, 7, 4096] {
                        let mut remaining = OUTPUT_LINE_LIMIT;
                        let mut at_line_start = true;
                        let mut output = Vec::new();
                        let mut truncated = false;
                        for bytes in input.as_bytes().chunks(chunk_size) {
                            let retained = retain_lines(bytes, &mut at_line_start, &mut remaining);
                            truncated |= retained.len() < bytes.len();
                            output.extend_from_slice(retained);
                        }
                        assert_eq!(output, expected.as_bytes());
                        assert_eq!(truncated, last_newline && !excess.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn shared_line_budget_keeps_an_already_started_line() {
        let mut remaining = 1;
        let mut stdout_line_start = true;
        let mut stderr_line_start = true;
        assert_eq!(
            retain_lines(b"out", &mut stdout_line_start, &mut remaining),
            b"out"
        );
        assert!(retain_lines(b"err\n", &mut stderr_line_start, &mut remaining).is_empty());
        assert_eq!(
            retain_lines(b"-end\nextra", &mut stdout_line_start, &mut remaining),
            b"-end\n"
        );
        assert!(retain_lines(b"extra", &mut stdout_line_start, &mut remaining).is_empty());
    }
}
