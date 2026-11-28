//! Multi-transport JSON-RPC client, message read/write and connection reaping.

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

pub(super) struct ConnectionSlot {
    pub(super) child: Option<Child>,
    pub(super) remote: Option<super::remote::Worker>,
    pub(super) closing: bool,
}

pub(super) type SharedConnection = Arc<Mutex<ConnectionSlot>>;

impl ConnectionSlot {
    pub(super) fn is_live(&self) -> bool {
        self.child.is_some() || self.remote.is_some()
    }
}

enum Transport {
    Stdio(StdioClient),
    Remote(super::remote::RemoteClient),
}

pub(super) struct Client {
    pub(super) connection: SharedConnection,
    transport: Transport,
    timeout: Duration,
    next_id: u64,
    pub(super) stopped: bool,
}

impl Client {
    pub(super) fn spawn(
        config: &ServerConfig,
        cwd: &Path,
        connection: SharedConnection,
    ) -> Result<Self> {
        let transport = match &config.transport {
            super::TransportConfig::Stdio(_) => {
                Transport::Stdio(StdioClient::spawn(config, cwd, connection.clone())?)
            }
            super::TransportConfig::Http(remote) | super::TransportConfig::Sse(remote) => {
                let mut slot = connection.lock().unwrap_or_else(|e| e.into_inner());
                ensure!(!slot.closing, "MCP is shutting down");
                ensure!(!slot.is_live(), "MCP connection is still running");
                let (client, worker) = super::remote::RemoteClient::spawn(
                    remote.clone(),
                    matches!(config.transport, super::TransportConfig::Sse(_)),
                )?;
                slot.remote = Some(worker);
                Transport::Remote(client)
            }
        };
        Ok(Self {
            connection,
            transport,
            timeout: config.timeout(),
            next_id: 0,
            stopped: false,
        })
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
                    // A server error body may contain credentials, so never forward the raw text.
                    return Ok(Err(anyhow::anyhow!("MCP request failed")));
                }
                ensure!(message["result"].is_object(), "invalid MCP result");
                if method == "initialize" {
                    let version = message["result"]["protocolVersion"]
                        .as_str()
                        .filter(|version| super::VERSIONS.contains(version))
                        .context("unsupported MCP protocol version")?;
                    self.initialized(version, deadline, cancellation)?;
                }
                return Ok(Ok(message["result"].take()));
            }
        })();
        match response {
            Ok(result) => result,
            Err(error) => {
                // Any transport failure must confirm the process is reaped; the failure must not degrade into an ordinary tool error.
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

    fn send(&mut self, message: Value, deadline: Instant, token: &CancellationToken) -> Result<()> {
        match &mut self.transport {
            Transport::Stdio(client) => client.send(message, deadline, token),
            Transport::Remote(client) => client.send(message, deadline, token),
        }
    }

    fn receive(&mut self, deadline: Instant, token: &CancellationToken) -> Result<Value> {
        match &mut self.transport {
            Transport::Stdio(client) => client.receive(deadline, token),
            Transport::Remote(client) => client.receive(deadline, token),
        }
    }

    fn initialized(
        &mut self,
        version: &str,
        deadline: Instant,
        token: &CancellationToken,
    ) -> Result<()> {
        if let Transport::Remote(client) = &mut self.transport {
            client.set_version(version, deadline, token)?;
        }
        Ok(())
    }

    pub(super) fn stop(&mut self) -> Result<()> {
        if !self.stopped {
            match &mut self.transport {
                Transport::Stdio(client) => client.stop()?,
                Transport::Remote(_) => stop_connection(&self.connection)?,
            }
            self.stopped = true;
        }
        Ok(())
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // The stdio backend keeps the existing graceful exit that closes stdin first.
        if matches!(self.transport, Transport::Remote(_)) {
            let _ = self.stop();
        }
    }
}

pub(super) struct StdioClient {
    pub(super) connection: SharedConnection,
    input: Option<ChildStdin>,
    output: ChildStdout,
    pending: Vec<u8>,
    pub(super) stopped: bool,
}

impl StdioClient {
    // A stable slot guarantees shutdown cleanup covers the rebuilt process; `closing` refuses resurrection after cleanup finishes.
    pub(super) fn spawn(
        config: &ServerConfig,
        cwd: &Path,
        process: SharedConnection,
    ) -> Result<Self> {
        let super::TransportConfig::Stdio(config) = &config.transport else {
            bail!("invalid MCP transport")
        };
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
            connection: process,
            input: Some(input),
            output,
            pending: Vec::new(),
            stopped: false,
        };
        if let Err(error) = nonblocking(client.input.as_ref().expect("MCP stdin captured"))
            .and_then(|_| nonblocking(&client.output))
        {
            // A not-yet-registered process is reaped through the same slot, so it never leaks.
            client.stop().map_err(CleanupError)?;
            return Err(error);
        }
        Ok(client)
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
            // `stopped` is set only after reaping is confirmed; a failure keeps the slot for retry so a shutdown retry or exit cleanup
            // cannot report success falsely.
            let result = stop_connection(&self.connection);
            if result.is_ok() {
                self.stopped = true;
            }
            result
        } else {
            Ok(())
        }
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        if !self.stopped {
            self.input.take();
            // Normal exit closes stdin first and gives the service a brief chance to clean up; failure paths terminate directly.
            {
                let slot = self
                    .connection
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
                            // Keep the child's wait status so the pid is not reused before the whole group is cleaned up.
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
        // Drop only does best-effort teardown and explicitly ignores stop failures.
        let _ = self.stop();
    }
}

pub(super) fn stop_connection(process: &SharedConnection) -> Result<()> {
    let mut slot = process.lock().unwrap_or_else(|error| error.into_inner());
    stop_locked(&mut slot)
}

// Only call while already holding the slot lock; inside the lock, never call stop_connection, which re-takes it.
pub(super) fn stop_locked(slot: &mut ConnectionSlot) -> Result<()> {
    if let Some(mut remote) = slot.remote.take() {
        remote.stop()?;
    }
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
        // Keep the unreaped handle when termination is unconfirmed, so a reused pid cannot make us kill a new process group.
        slot.child = Some(child);
        return Err(error);
    }
    Ok(())
}

fn signal_group(group: Pid, signal: Signal) -> io::Result<()> {
    match kill_process_group(group, signal) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        // macOS returns EPERM for a group of only zombies; once the direct child has exited this counts as cleaned up.
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
