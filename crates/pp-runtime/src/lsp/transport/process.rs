//! 受管理 stdio 进程的启动、stderr 汇总与进程组回收。

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
    /// 启动语言服务器进程；stderr 只保留末尾 64 KiB。
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
        // 子进程一产生就登记：后续任何取管道/起线程失败都留下可重试的 slot，
        // 而不是把未回收的进程丢给调用方。
        {
            let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
            if slot.closing || slot.child.is_some() {
                drop(slot);
                // 未登记成功的进程由本线程自行回收，不进入 registry 的 slot。
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
            // 起线程失败时也要让 writer 看到队列断开，否则已启动的 writer
            // 线程会永远阻塞在 recv 上；进程仍保留在 slot 里供重试回收。
            *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = terminate_slot(&process, Duration::ZERO);
            return Err(error);
        }

        // 起线程期间清理路径可能已设置 closing 并回收进程；此时不能返回一个
        // 声称可用的 transport。
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

    /// 关闭 stdin 并等待进程/进程组退出；未确认回收时保留 slot 并报错。
    pub(crate) fn terminate(&self, grace: Duration) -> Result<()> {
        // 先断开写队列：writer 线程退出并关闭 stdin，服务器有机会自行退出。
        *self.shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
        terminate_slot(&self.process, grace)?;
        self.shared.note_close("LSP server was terminated");
        Ok(())
    }
}

/// 回收一个服务器进程 slot：root 退出不代表后代关闭了管道，必须确认整个
/// 进程组消失。registry 在初始化过程中取消/关闭时也用它，因此不依赖
/// `Transport`（shared 尚未可用）。
///
/// 失败时保留 `slot.child`，调用方可以重试；绝不把未确认的回收报告为成功。
pub(crate) fn terminate_slot(process: &Arc<Mutex<ProcessSlot>>, grace: Duration) -> Result<()> {
    let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
    slot.closing = true;
    let Some(child) = slot.child.as_mut() else {
        return Ok(());
    };
    let group = child.id();
    // 宽限期内先等 root 自己退出（stdin 已关闭时通常会成功）。
    let _ = wait_child(child, grace)?;
    // root 退出不代表后代关闭了管道：只要进程组仍在就发信号回收。
    // 先探测“还在”（EPERM 也算还在）再决定是否发信号，避免对已消失的
    // pgid 发送信号（pgid 可能已被复用）。
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
    // 组已消失/只剩僵尸意味着 root 也已退出；最后确认并回收，避免留下
    // 僵尸进程。
    if !wait_child(child, KILL_GRACE)? {
        bail!("LSP server process {group} did not exit");
    }
    slot.child = None;
    Ok(())
}

/// 在窗口内等待 root 退出；返回是否已回收。
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

/// 在窗口内等待整个进程组落定，同时持续回收 root。
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
        // macOS 对仅剩僵尸的组返回 EPERM；确认 leader 已退出/已回收后视为
        // 清理完成（僵尸成员不可运行，也无法再响应信号）。
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

/// 进程组是否仍在：EPERM 也视为仍在，必须先发信号再判定落定，避免把
/// “只剩僵尸 + 仍有活后代”误判为空。
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

/// 进程组是否已没有可运行成员：ESRCH，或 macOS 上信号被拒（只剩僵尸）。
/// 这不等于“干净成功”——调用方仍需确认 root 已被回收。
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

/// leader 已退出（尚未回收）或已被回收；ECHILD 说明它已经被 wait 掉了。
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
mod tests {
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

    /// 用 WNOWAIT 观察 leader 退出但不回收，模拟“root 已退出、slot 还持有句柄”。
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
        // root 已退出但仍是僵尸；后台 sleep 仍持有同一个进程组。
        terminate_slot(&process, Duration::ZERO).expect("terminate");
        assert!(process.lock().unwrap().child.is_none());
        assert!(group_settled(group).expect("probe group"));
    }

    #[test]
    fn lsp_transport_terminate_slot_kills_group_after_root_was_reaped() {
        let mut child = child_in_own_group("sleep 60 & exit 0");
        let group = child.id();
        wait_leader_exit(group);
        // 调用方先回收 root：Child 仍保留 id 与缓存的退出状态。
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
