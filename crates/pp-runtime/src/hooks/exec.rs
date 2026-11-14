//! 有界 command hook 执行器：stdin 一次写入 JSON、stdout 有上限、stderr 只留尾部。
//!
//! 与 Bash 工具（内嵌 brush）和 MCP/LSP（长期协议）都不同，这里是一次性
//! 短命进程：单线程交替写 stdin、读两个管道并轮询退出，所有等待都受
//! deadline 与取消约束，任何失败路径都先回收整个进程组再返回。

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

/// stdout 上限：超过即按执行故障终止，绝不把无限输出读进内存。
pub(super) const STDOUT_LIMIT: usize = 1024 * 1024;
/// stderr 只保留末尾，用于诊断与 exit 2 的理由。
pub(super) const STDERR_LIMIT: usize = 64 * 1024;

const POLL: Duration = Duration::from_millis(10);
const TERM_GRACE: Duration = Duration::from_millis(1000);
const KILL_GRACE: Duration = Duration::from_millis(1000);
const READ_CHUNK: usize = 8 * 1024;
/// root 退出后留给管道 EOF 的窗口；后代可能仍握着管道不放。
const DRAIN_GRACE: Duration = Duration::from_millis(1000);

pub(super) struct Request<'a> {
    pub(super) command: &'a str,
    /// `Some` 时直接 exec，不经过 shell；`None` 用 `bash -c`。
    pub(super) args: Option<&'a [String]>,
    pub(super) timeout: Duration,
    pub(super) cwd: &'a Path,
    pub(super) input: &'a [u8],
}

#[derive(Debug)]
pub(super) struct Outcome {
    /// 被信号终止时为 `None`。
    pub(super) code: Option<i32>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

struct Guard {
    child: Option<Child>,
    group: u32,
}

impl Guard {
    /// 探测 → TERM → 等待 → KILL → 等待 → 回收 root。
    /// 回收阶段的任何失败（探测、发信号、等待退出）都意味着无法确认进程组
    /// 已消失，统一包装为唯一的 `CleanupError`，调用方必须停止任务；否则
    /// `onFailure: continue` 会把未回收的进程当成普通 hook 故障放行。
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

/// 回收阶段的全部动作；错误由 `Guard::cleanup` 升级为 `CleanupError`。
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
        // 显式路径已在返回前回收；这里只兜住遗漏的错误分支。
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
        // 子进程只在事件 cwd 中运行；宿主 cwd/env 一律不动。
        .current_dir(request.cwd)
        .env_remove("BASH_ENV")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        // 固定上下文：命令原文与参数不进入错误、事件或诊断日志。
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
            // 失败路径：先回收再报告原错误；回收失败不能被原错误掩盖。
            let cleanup = guard.cleanup();
            match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup.context(format!("{error:#}"))),
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
            // root 已退出但后代仍握着管道：交给进程组回收，不再等待 EOF。
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
                    // 脚本不读 stdin：立即关闭写端，但继续读它的输出。
                    Err(error) if error.kind() == io::ErrorKind::BrokenPipe => close_stdin = true,
                    Err(error) => return Err(error).context("writing hook command stdin"),
                }
            }
        }
        if close_stdin {
            // 关闭写端才会让脚本看到 EOF；只置标志会一直等到超时。
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
            // 无进展时才等，避免持续输出被轮询间隔拖慢。
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(now)));
        }
    }
    Ok(Outcome {
        code: status.and_then(|status| status.code()),
        stdout: out,
        stderr: err,
    })
}

/// stderr 只保留末尾：超限时从头丢弃，O(超限字节) 且总量恒定。
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
        // macOS 对只剩僵尸的进程组返回 EPERM；leader 已退出即视为完成。
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
