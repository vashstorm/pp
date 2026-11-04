//! stdio JSON-RPC 客户端、消息读写与进程组回收。

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use serde_json::{Value, json};
use std::{
    io::{self, Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::CleanupError;

use super::{MESSAGE_LIMIT, POLL, ServerConfig};

pub(super) struct ProcessSlot {
    pub(super) child: Option<Child>,
    pub(super) closing: bool,
}

pub(super) type SharedProcess = Arc<Mutex<ProcessSlot>>;

pub(super) struct Client {
    pub(super) process: SharedProcess,
    input: Option<ChildStdin>,
    output: ChildStdout,
    pending: Vec<u8>,
    timeout: Duration,
    next_id: u64,
    pub(super) stopped: bool,
}

impl Client {
    // 稳定 slot 保证退出清理能覆盖重建后的新进程；closing 拒绝清理完成后的复活。
    pub(super) fn spawn(config: &ServerConfig, cwd: &Path, process: SharedProcess) -> Result<Self> {
        let (input, output) = {
            let mut slot = process.lock().unwrap_or_else(|error| error.into_inner());
            let spawned = (|| -> Result<(ChildStdin, ChildStdout)> {
                ensure!(!slot.closing, "MCP is shutting down");
                ensure!(slot.child.is_none(), "MCP process is still running");
                let child = Command::new(&config.command)
                    .args(&config.args)
                    .envs(&config.env)
                    .current_dir(config.cwd.as_deref().unwrap_or(cwd))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .spawn()
                    .map_err(|_| anyhow::anyhow!("starting MCP process failed"))?;
                slot.child = Some(child);
                let child = slot.child.as_mut().expect("MCP child installed");
                let input = child.stdin.take().context("MCP stdin unavailable")?;
                let output = child.stdout.take().context("MCP stdout unavailable")?;
                Ok((input, output))
            })();
            match spawned {
                Ok(pipes) => pipes,
                Err(error) => {
                    if slot.child.is_some() {
                        stop_locked(&mut slot).map_err(CleanupError)?;
                    }
                    return Err(error);
                }
            }
        };
        let mut client = Self {
            process,
            input: Some(input),
            output,
            pending: Vec::new(),
            timeout: config.timeout(),
            next_id: 0,
            stopped: false,
        };
        if let Err(error) = nonblocking(client.input.as_ref().expect("MCP stdin captured"))
            .and_then(|_| nonblocking(&client.output))
        {
            // 尚未注册的进程也通过同一 slot 回收，不泄漏。
            client.stop().map_err(CleanupError)?;
            return Err(error);
        }
        Ok(client)
    }

    pub(super) fn request(
        &mut self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        ensure!(!self.stopped, "MCP connection is closed");
        self.next_id += 1;
        let id = self.next_id;
        let deadline = Instant::now() + self.timeout;
        let response = (|| {
            self.send(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                deadline,
                cancellation,
            )?;
            loop {
                let mut message = self.receive(deadline, cancellation)?;
                ensure!(message["jsonrpc"] == "2.0", "invalid MCP JSON-RPC message");
                if let Some(method) = message.get("method") {
                    ensure!(
                        method.is_string()
                            && message.get("result").is_none()
                            && message.get("error").is_none(),
                        "invalid MCP request"
                    );
                    if let Some(request_id) = message.get("id") {
                        ensure!(
                            request_id.is_string() || request_id.is_i64() || request_id.is_u64(),
                            "invalid MCP request id"
                        );
                        let reply = if method == "ping" {
                            json!({"jsonrpc":"2.0","id":request_id,"result":{}})
                        } else {
                            json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Method not supported"}})
                        };
                        self.send(reply, deadline, cancellation)?;
                    }
                    continue;
                }
                ensure!(
                    message.get("id") == Some(&json!(id)),
                    "unexpected MCP response id"
                );
                ensure!(
                    message.get("result").is_some() != message.get("error").is_some(),
                    "invalid MCP response"
                );
                if message.get("error").is_some() {
                    // 服务端错误体可能包含凭证，禁止传递原文。
                    return Ok(Err(anyhow::anyhow!("MCP request failed")));
                }
                ensure!(message["result"].is_object(), "invalid MCP result");
                return Ok(Ok(message["result"].take()));
            }
        })();
        match response {
            Ok(result) => result,
            Err(error) => {
                // 任意传输故障都必须确认进程已回收；失败不能降为普通工具错误。
                match self.stop() {
                    Ok(()) => Err(error),
                    Err(stop_error) => Err(CleanupError(stop_error).into()),
                }
            }
        }
    }

    pub(super) fn notify(
        &mut self,
        method: &str,
        params: Value,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        self.send(
            json!({"jsonrpc":"2.0","method":method,"params":params}),
            Instant::now() + self.timeout,
            cancellation,
        )
    }

    fn send(
        &mut self,
        message: Value,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut bytes = message.to_string().into_bytes();
        ensure!(
            bytes.len() < MESSAGE_LIMIT,
            "MCP message exceeds size limit"
        );
        bytes.push(b'\n');
        let input = self.input.as_mut().context("MCP connection is closed")?;
        let mut offset = 0;
        while offset < bytes.len() {
            cancellation::check(cancellation)?;
            ensure!(Instant::now() < deadline, "MCP request timed out");
            match input.write(&bytes[offset..]) {
                Ok(0) => bail!("MCP connection closed while writing"),
                Ok(count) => offset += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => bail!("writing MCP message failed"),
            }
        }
        Ok(())
    }

    fn receive(&mut self, deadline: Instant, cancellation: &CancellationToken) -> Result<Value> {
        loop {
            cancellation::check(cancellation)?;
            ensure!(Instant::now() < deadline, "MCP request timed out");
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let message = serde_json::from_slice(&self.pending[..end])
                    .map_err(|_| anyhow::anyhow!("invalid MCP JSON message"))?;
                self.pending.drain(..=end);
                return Ok(message);
            }
            ensure!(
                self.pending.len() < MESSAGE_LIMIT,
                "MCP message exceeds size limit"
            );
            let mut chunk = [0; 8192];
            let capacity = chunk.len().min(MESSAGE_LIMIT - self.pending.len());
            match self.output.read(&mut chunk[..capacity]) {
                Ok(0) => bail!("MCP connection closed while reading"),
                Ok(count) => self.pending.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(POLL),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => bail!("reading MCP message failed"),
            }
        }
    }

    pub(super) fn stop(&mut self) -> Result<()> {
        if !self.stopped {
            self.input.take();
            // 仅在确认回收成功后置 stopped；失败保留 slot 供重试，
            // 避免 shutdown 重试或退出清理误报成功。
            let result = stop_process(&self.process);
            if result.is_ok() {
                self.stopped = true;
            }
            result
        } else {
            Ok(())
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.stopped {
            self.input.take();
            // 正常退出先关闭 stdin，再给服务短暂的清理机会；故障路径直接终止。
            {
                let slot = self
                    .process
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if let Some(group) = slot
                    .child
                    .as_ref()
                    .and_then(|child| Pid::from_raw(child.id() as i32))
                {
                    'shutdown: for signal in [None, Some(Signal::TERM)] {
                        if let Some(signal) = signal {
                            let _ = kill_process_group(group, signal);
                        }
                        let deadline = Instant::now() + Duration::from_millis(100);
                        while Instant::now() < deadline {
                            // 保留子进程等待状态，清理整个组前避免进程号被复用。
                            match waitid(
                                WaitId::Pid(group),
                                WaitIdOptions::EXITED
                                    | WaitIdOptions::NOHANG
                                    | WaitIdOptions::NOWAIT,
                            ) {
                                Ok(Some(_)) => break 'shutdown,
                                Err(error) if error != rustix::io::Errno::INTR => break 'shutdown,
                                _ => thread::sleep(POLL),
                            }
                        }
                    }
                }
            }
        }
        // Drop 仅做 best-effort 收尾，显式忽略停止失败。
        let _ = self.stop();
    }
}

pub(super) fn stop_process(process: &SharedProcess) -> Result<()> {
    let mut slot = process.lock().unwrap_or_else(|error| error.into_inner());
    stop_locked(&mut slot)
}

// 仅在已持有 slot 锁时调用；锁内禁止再调用会重取锁的 stop_process。
pub(super) fn stop_locked(slot: &mut ProcessSlot) -> Result<()> {
    let Some(mut child) = slot.child.take() else {
        return Ok(());
    };
    let stopped = (|| -> Result<()> {
        if let Some(group) = Pid::from_raw(child.id() as i32) {
            signal_group(group, Signal::KILL).context("stopping MCP process group failed")?;
        }
        child.kill().context("stopping MCP process failed")?;
        child.wait().context("reaping MCP process failed")?;
        Ok(())
    })();
    if let Err(error) = stopped {
        // 未确认终止时保留未回收句柄，避免进程号被复用后误杀新进程组。
        slot.child = Some(child);
        return Err(error);
    }
    Ok(())
}

fn signal_group(group: Pid, signal: Signal) -> io::Result<()> {
    match kill_process_group(group, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        // macOS 对仅剩僵尸进程的组返回 EPERM，确认直接子进程已退出后视为清理完成。
        Err(rustix::io::Errno::PERM) if cfg!(target_os = "macos") && group_exited(group)? => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn group_exited(group: Pid) -> io::Result<bool> {
    loop {
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

fn nonblocking(pipe: &impl AsFd) -> Result<()> {
    fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK).context("configuring MCP pipe")
}
