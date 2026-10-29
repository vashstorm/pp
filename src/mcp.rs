use crate::cancellation::{self, CancellationToken, Cancelled, CleanupError};
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const MESSAGE_LIMIT: usize = 1_048_576;
const TOOL_LIMIT: usize = 256;
const POLL: Duration = Duration::from_millis(5);
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

// 不实现 Debug，避免输出命令参数和环境变量中的凭证。
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    30
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}

pub(crate) fn validate_config(servers: &BTreeMap<String, ServerConfig>) -> Result<()> {
    ensure!(servers.len() <= 32, "too many MCP servers (maximum 32)");
    for (name, config) in servers {
        ensure!(
            safe_name(name) && name.len() <= 24 && !name.contains("__"),
            "invalid MCP server name"
        );
        ensure!(
            !config.command.trim().is_empty() && !config.command.contains('\0'),
            "invalid MCP command"
        );
        ensure!(
            config.args.iter().all(|arg| !arg.contains('\0')),
            "invalid MCP arguments"
        );
        ensure!(
            config.env.iter().all(|(key, value)| !key.is_empty()
                && !key.contains(['=', '\0'])
                && !value.contains('\0')),
            "invalid MCP environment"
        );
        ensure!(
            config.timeout_seconds > 0
                && Instant::now()
                    .checked_add(Duration::from_secs(config.timeout_seconds))
                    .is_some(),
            "invalid MCP timeout_seconds"
        );
    }
    Ok(())
}

#[derive(Clone, Default)]
pub(crate) struct Registry {
    servers: BTreeMap<String, SharedServer>,
    tools: BTreeMap<String, RemoteTool>,
}

#[derive(Clone, PartialEq)]
struct RemoteTool {
    server: String,
    original_name: String,
    definition: FunctionObject,
}

#[derive(Clone)]
struct SharedServer {
    state: Arc<Mutex<ServerState>>,
    process: SharedProcess,
}
impl SharedServer {
    fn new(state: ServerState) -> Self {
        Self {
            process: state.client.process.clone(),
            state: Arc::new(Mutex::new(state)),
        }
    }
}
impl std::ops::Deref for SharedServer {
    type Target = Mutex<ServerState>;
    fn deref(&self) -> &Self::Target {
        &self.state
    }
}
struct ServerState {
    config: ServerConfig,
    cwd: PathBuf,
    client: Client,
    // 仅因任务取消而停止的连接保留一次重建资格；普通故障不重连。
    restart_after_cancel: bool,
}

struct ProcessSlot {
    child: Option<Child>,
    closing: bool,
}

type SharedProcess = Arc<Mutex<ProcessSlot>>;

pub(crate) struct Cleanup {
    processes: Vec<SharedProcess>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for process in &self.processes {
            // 退出清理在 slot 锁内标记 closing，拒绝清理完成后由 worker 复活进程。
            let mut slot = process.lock().unwrap_or_else(|error| error.into_inner());
            slot.closing = true;
            let _ = stop_locked(&mut slot);
        }
    }
}

impl Registry {
    pub(crate) fn abort_connections(&self) -> Result<()> {
        for server in self.servers.values() {
            let mut process = server.process.lock().unwrap_or_else(|e| e.into_inner());
            process.closing = true;
            stop_locked(&mut process)?;
        }
        Ok(())
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.servers
            .values()
            .any(|server| Arc::strong_count(&server.state) > 1)
    }
    pub(crate) fn cleanup(&self) -> Result<Cleanup> {
        let processes = self
            .servers
            .values()
            .map(|state| {
                state
                    .lock()
                    .map(|state| state.client.process.clone())
                    .map_err(|_| anyhow::anyhow!("MCP server unavailable"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Cleanup { processes })
    }

    // 连接可被调用方的 token 取消：每次 spawn 前和 discovery 全程检查；
    // 取消不自动重试，由调用方决定是否再次 connect。
    pub(crate) fn connect(
        configs: &BTreeMap<String, ServerConfig>,
        cwd: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self> {
        validate_config(configs)?;
        let mut registry = Self::default();
        for (name, config) in configs {
            if let Err(error) = registry
                .add_server(name, config, cwd, cancellation)
                .with_context(|| format!("connecting MCP server {name}"))
            {
                return match registry.shutdown() {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(cleanup.context(format!("Connection also failed: {error:#}")))
                    }
                };
            }
        }
        Ok(registry)
    }

    // 显式停止全部服务：逐一 stop 并检查结果，任何失败都保留 registry/slot
    // 并返回聚合的 typed 清理错误；全部确认回收后才清空 registry，
    // 保证 Cleanup 快照始终覆盖当前真实进程。
    pub(crate) fn shutdown(&mut self) -> Result<()> {
        let mut failures = 0usize;
        let mut detail = String::new();
        for (name, state) in &self.servers {
            let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
            if let Err(error) = state.client.stop() {
                failures += 1;
                if !detail.is_empty() {
                    detail.push_str("; ");
                }
                detail.push_str(name);
                detail.push_str(": ");
                detail.push_str(&format!("{error:#}"));
            }
        }
        if failures == 0 {
            self.servers.clear();
            self.tools.clear();
            return Ok(());
        }
        Err(CleanupError(anyhow::anyhow!("{detail}")).into())
    }

    fn add_server(
        &mut self,
        server: &str,
        config: &ServerConfig,
        cwd: &Path,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        let process: SharedProcess = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: false,
        }));
        let mut client = Client::spawn(config, cwd, process)?;
        let tools = match discover(&mut client, server, cancellation) {
            Ok(tools) => tools,
            Err(error) => {
                // 初始化失败不依赖 Drop 的尽力回收：显式停止；停止失败时合并
                // 原始错误；任何停止失败均升级为 typed 清理错误。
                return Err(match client.stop() {
                    Ok(()) => error,
                    Err(stop_error) => CleanupError(
                        stop_error.context(format!("Initialization also failed: {error:#}")),
                    )
                    .into(),
                });
            }
        };
        for name in tools.keys() {
            ensure!(!self.tools.contains_key(name), "duplicate MCP tool name");
        }
        ensure!(
            self.tools.len() + tools.len() <= TOOL_LIMIT,
            "too many MCP tools (maximum 256)"
        );
        self.tools.extend(tools);
        self.servers.insert(
            server.into(),
            SharedServer::new(ServerState {
                config: config.clone(),
                cwd: cwd.to_owned(),
                client,
                restart_after_cancel: false,
            }),
        );
        Ok(())
    }

    pub(crate) fn definitions(&self) -> Vec<FunctionObject> {
        self.tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(crate) fn proxy_call(
        &self,
        name: &str,
        arguments: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        cancellation::check(cancellation)?;
        let registry = self.clone();
        let request_token = cancellation.clone();
        let name = name.to_owned();
        let arguments = arguments.to_owned();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("pp-mcp-proxy".into())
            .spawn(move || {
                let outcome = registry.call_inner(&name, &arguments, &request_token, true);
                let _ = sender.send(outcome);
            })?;
        loop {
            cancellation::check(cancellation)?;
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => bail!("MCP 代理已退出"),
            }
        }
    }
    pub(crate) fn call(
        &self,
        name: &str,
        arguments: &str,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        self.call_inner(name, arguments, cancellation, false)
    }
    fn call_inner(
        &self,
        name: &str,
        arguments: &str,
        cancellation: &CancellationToken,
        proxy: bool,
    ) -> Result<Value> {
        let tool = self.tools.get(name).context("unknown tool")?;
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|_| anyhow::anyhow!("invalid MCP tool arguments"))?;
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be an object"
        );
        let mut state = loop {
            cancellation::check(cancellation)?;
            match self.servers[&tool.server].try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => thread::sleep(POLL),
                Err(_) => bail!("MCP server unavailable"),
            }
        };
        if state.restart_after_cancel {
            self.restart(&tool.server, &mut state, cancellation)?;
        }
        cancellation::check(cancellation)?;
        let transport_token = if proxy {
            CancellationToken::new()
        } else {
            cancellation.clone()
        };
        let result = match state.client.request(
            "tools/call",
            json!({"name":tool.original_name,"arguments":arguments}),
            &transport_token,
        ) {
            Ok(result) => result,
            Err(error) => {
                // 仅任务取消赋予下一次实际调用时的重建资格；普通故障保持不可用。
                if error.downcast_ref::<Cancelled>().is_some() {
                    state.restart_after_cancel = true;
                }
                return Err(error);
            }
        };
        ensure!(
            result["content"].is_array() && result.get("isError").is_none_or(Value::is_boolean),
            "invalid MCP tools/call result"
        );
        // 保留 MCP 内容块和结构化结果，工具级失败仍作为正常协议响应处理。
        Ok(json!({"ok":result["isError"] != true, "result":result}))
    }

    // 取消后的惰性重建：只有新任务真正调用该服务工具时执行一次。
    fn restart(
        &self,
        server: &str,
        state: &mut ServerState,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        let process = state.client.process.clone();
        let mut client = match Client::spawn(&state.config, &state.cwd, process) {
            Ok(client) => client,
            Err(_) => {
                state.restart_after_cancel = false;
                bail!("MCP restart failed; restart pp to reconnect.");
            }
        };
        let discovered = match discover(&mut client, server, cancellation) {
            Ok(tools) => tools,
            Err(error) => {
                if error.is::<CleanupError>() {
                    state.restart_after_cancel = false;
                    return Err(error);
                }
                if error.is::<Cancelled>() {
                    // 重建被取消：停止新进程并保留资格，下一次新任务可再尝试。
                    // 停止失败升级为 typed 清理错误，不被固定文案吞掉。
                    if let Err(stop_error) = client.stop() {
                        state.restart_after_cancel = false;
                        return Err(CleanupError(stop_error).into());
                    }
                    return Err(error);
                }
                state.restart_after_cancel = false;
                let _ = client.stop();
                bail!("MCP restart failed; restart pp to reconnect.");
            }
        };
        // 与旧集合完整比较：增删或 exposed/原始名/描述/参数任一变化都拒绝，
        // 且校验通过前不发送新 tools/call、不更新全局工具表。
        let previous = self
            .tools
            .values()
            .filter(|tool| tool.server == server)
            .count();
        let unchanged = discovered.len() == previous
            && self
                .tools
                .iter()
                .filter(|(_, tool)| tool.server == server)
                .all(|(name, tool)| discovered.get(name) == Some(tool));
        if !unchanged {
            state.restart_after_cancel = false;
            let _ = client.stop();
            bail!("MCP tool definitions changed; restart pp to reconnect.");
        }
        // 旧连接在取消时已停止，Drop 不会触碰稳定 slot 中的新进程。
        state.client = client;
        state.restart_after_cancel = false;
        Ok(())
    }
}

// 握手与工具发现：初始连接与取消后的重建共用同一流程。
fn discover(
    client: &mut Client,
    server: &str,
    cancellation: &CancellationToken,
) -> Result<BTreeMap<String, RemoteTool>> {
    let initialized = client.request(
        "initialize",
        json!({
            "protocolVersion":VERSIONS[0], "capabilities":{},
            "clientInfo":{"name":"pp", "version":env!("CARGO_PKG_VERSION")}
        }),
        cancellation,
    )?;
    ensure!(
        initialized["protocolVersion"]
            .as_str()
            .is_some_and(|version| VERSIONS.contains(&version)),
        "unsupported MCP protocol version"
    );
    ensure!(
        initialized["capabilities"].is_object()
            && initialized["serverInfo"]["name"].is_string()
            && initialized["serverInfo"]["version"].is_string()
            && initialized["capabilities"]
                .get("tools")
                .is_none_or(Value::is_object),
        "invalid MCP initialize result"
    );
    client.notify("notifications/initialized", json!({}), cancellation)?;
    let mut tools = BTreeMap::new();
    if initialized["capabilities"].get("tools").is_some() {
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        for page in 0..TOOL_LIMIT {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |value| json!({"cursor":value}));
            let result = client.request("tools/list", params, cancellation)?;
            let tool_list = result["tools"]
                .as_array()
                .context("invalid MCP tools/list result")?;
            for tool in tool_list {
                ensure!(tools.len() < TOOL_LIMIT, "too many MCP tools (maximum 256)");
                let original = tool["name"].as_str().context("invalid MCP tool name")?;
                // 短名称原样保留，其他名称使用确定性编码，保证模型工具名合法。
                let exposed = exposed_name(server, original);
                ensure!(
                    !original.is_empty() && original.len() <= 256,
                    "invalid MCP tool name"
                );
                ensure!(!tools.contains_key(&exposed), "duplicate MCP tool name");
                let schema = &tool["inputSchema"];
                ensure!(
                    schema.is_object() && schema["type"] == "object",
                    "invalid MCP tool input schema"
                );
                ensure!(
                    tool.get("description").is_none_or(Value::is_string),
                    "invalid MCP tool description"
                );
                tools.insert(
                    exposed.clone(),
                    RemoteTool {
                        server: server.into(),
                        original_name: original.into(),
                        definition: FunctionObject {
                            name: exposed,
                            description: tool["description"].as_str().map(str::to_owned),
                            parameters: Some(schema.clone()),
                            strict: Some(false),
                        },
                    },
                );
            }
            match result.get("nextCursor") {
                None => break,
                Some(Value::String(value)) if !value.is_empty() => {
                    ensure!(
                        cursors.insert(value.clone()) && page + 1 < TOOL_LIMIT,
                        "invalid MCP pagination"
                    );
                    cursor = Some(value.clone());
                }
                _ => bail!("invalid MCP pagination"),
            }
        }
    }
    Ok(tools)
}

fn exposed_name(server: &str, tool: &str) -> String {
    let plain = format!("mcp__{server}__{tool}");
    if safe_name(tool) && plain.len() <= 64 {
        plain
    } else {
        let hash = tool.bytes().fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
        format!("mcp__{server}__tool_{hash:016x}")
    }
}

struct Client {
    process: SharedProcess,
    input: Option<ChildStdin>,
    output: ChildStdout,
    pending: Vec<u8>,
    timeout: Duration,
    next_id: u64,
    stopped: bool,
}

impl Client {
    // 稳定 slot 保证退出清理能覆盖重建后的新进程；closing 拒绝清理完成后的复活。
    fn spawn(config: &ServerConfig, cwd: &Path, process: SharedProcess) -> Result<Self> {
        let (input, output) = {
            let mut slot = process.lock().unwrap_or_else(|error| error.into_inner());
            let spawned = (|| -> Result<(ChildStdin, ChildStdout)> {
                ensure!(!slot.closing, "MCP is shutting down");
                ensure!(slot.child.is_none(), "MCP process is still running");
                let child = Command::new(&config.command)
                    .args(&config.args)
                    .envs(&config.env)
                    .current_dir(cwd)
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
                    let _ = stop_locked(&mut slot);
                    return Err(error);
                }
            }
        };
        let mut client = Self {
            process,
            input: Some(input),
            output,
            pending: Vec::new(),
            timeout: Duration::from_secs(config.timeout_seconds),
            next_id: 0,
            stopped: false,
        };
        if let Err(error) = nonblocking(client.input.as_ref().expect("MCP stdin captured"))
            .and_then(|_| nonblocking(&client.output))
        {
            // 尚未注册的进程也通过同一 slot 回收，不泄漏。
            let _ = client.stop();
            return Err(error);
        }
        Ok(client)
    }

    fn request(
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
                // 取消必须先真正终止并回收服务进程组，之后才返回；
                // 取消路径的停止失败升级为 typed 清理错误，普通错误保持原文。
                let cancelled = error.downcast_ref::<Cancelled>().is_some();
                match self.stop() {
                    Ok(()) => Err(error),
                    Err(stop_error) if cancelled => Err(CleanupError(stop_error).into()),
                    Err(_) => Err(error),
                }
            }
        }
    }

    fn notify(
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

    fn stop(&mut self) -> Result<()> {
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

fn stop_process(process: &SharedProcess) -> Result<()> {
    let mut slot = process.lock().unwrap_or_else(|error| error.into_inner());
    stop_locked(&mut slot)
}

// 仅在已持有 slot 锁时调用；锁内禁止再调用会重取锁的 stop_process。
fn stop_locked(slot: &mut ProcessSlot) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_spawn_refuses_after_cleanup_marks_slot_closing() {
        let process: SharedProcess = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: true,
        }));
        let config = ServerConfig {
            command: "bash".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            env: BTreeMap::new(),
            timeout_seconds: 1,
        };
        let error = Client::spawn(&config, Path::new("."), process)
            .err()
            .unwrap();
        assert_eq!(format!("{error:#}"), "MCP is shutting down");
    }

    #[test]
    fn mcp_spawn_refuses_to_replace_unreaped_process() {
        let child = Command::new("bash")
            .args(["-c", "sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let process: SharedProcess = Arc::new(Mutex::new(ProcessSlot {
            child: Some(child),
            closing: false,
        }));
        let config = ServerConfig {
            command: "bash".into(),
            args: vec![],
            env: BTreeMap::new(),
            timeout_seconds: 1,
        };
        let error = Client::spawn(&config, Path::new("."), process.clone())
            .err()
            .unwrap();
        assert_eq!(format!("{error:#}"), "MCP process is still running");
        stop_process(&process).unwrap();
        assert!(process.lock().unwrap().child.is_none());
    }

    fn plan_server_config(args: &[&str]) -> ServerConfig {
        ServerConfig {
            command: "bash".into(),
            args: args.iter().map(|arg| (*arg).into()).collect(),
            env: BTreeMap::new(),
            timeout_seconds: 1,
        }
    }

    fn plan_spawn_registry_client(config: &ServerConfig) -> (Client, SharedProcess) {
        let process: SharedProcess = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: false,
        }));
        let client = Client::spawn(config, Path::new("."), process.clone()).unwrap();
        (client, process)
    }

    #[test]
    fn proxy_cancelled_while_queued_never_dispatches_or_closes_shared_connection() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            command: "bash".into(),
            args: vec![format!(
                "{}/tests/fixtures/mcp_server.sh",
                env!("CARGO_MANIFEST_DIR")
            )],
            env: BTreeMap::from([("MCP_LOG".into(), "proxy-log".into())]),
            timeout_seconds: 1,
        };
        let registry = Registry::connect(
            &BTreeMap::from([("local".into(), config)]),
            dir.path(),
            &CancellationToken::new(),
        )
        .unwrap();
        let guard = registry.servers["local"].lock().unwrap();
        let proxy = registry.clone();
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            proxy.proxy_call(
                "mcp__local__echo",
                r#"{"text":"must-not-dispatch"}"#,
                &worker_token,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Arc::strong_count(&registry.servers["local"].state) < 3 {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        token.cancel();
        assert!(worker.join().unwrap().unwrap_err().is::<Cancelled>());
        drop(guard);
        while Arc::strong_count(&registry.servers["local"].state) > 1 {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        assert!(
            registry
                .call(
                    "mcp__local__echo",
                    r#"{"text":"survived"}"#,
                    &CancellationToken::new()
                )
                .unwrap()["ok"]
                == true
        );
        let log = std::fs::read_to_string(dir.path().join("proxy-log")).unwrap();
        assert!(!log.contains("must-not-dispatch"));
    }

    fn plan_register(registry: &mut Registry, name: &str, client: Client) {
        registry.servers.insert(
            name.into(),
            SharedServer::new(ServerState {
                config: plan_server_config(&["-c", "true"]),
                cwd: PathBuf::from("."),
                client,
                restart_after_cancel: false,
            }),
        );
        registry.tools.insert(
            format!("mcp__{name}__tool"),
            RemoteTool {
                server: name.into(),
                original_name: "tool".into(),
                definition: FunctionObject {
                    name: format!("mcp__{name}__tool"),
                    description: None,
                    parameters: None,
                    strict: Some(false),
                },
            },
        );
    }

    #[test]
    fn plan_mcp_shutdown_stops_all_clients_and_clears_registry() {
        let mut registry = Registry::default();
        let mut processes = Vec::new();
        for name in ["a", "b"] {
            let (client, process) =
                plan_spawn_registry_client(&plan_server_config(&["-c", "sleep 30"]));
            plan_register(&mut registry, name, client);
            processes.push(process);
        }
        registry.shutdown().unwrap();
        assert!(registry.servers.is_empty() && registry.tools.is_empty());
        for process in &processes {
            // 全部进程组已被回收，slot 不再持有句柄。
            assert!(process.lock().unwrap().child.is_none());
        }
        // 清空后重复 shutdown 仍是成功，不误报。
        registry.shutdown().unwrap();
    }

    #[test]
    fn plan_mcp_shutdown_failure_retains_registry_and_retry_does_not_fake_success() {
        let mut registry = Registry::default();
        let (healthy, healthy_process) =
            plan_spawn_registry_client(&plan_server_config(&["-c", "sleep 30"]));
        plan_register(&mut registry, "a", healthy);
        let (faulty, faulty_process) =
            plan_spawn_registry_client(&plan_server_config(&["-c", "true"]));
        plan_register(&mut registry, "b", faulty);

        // 故障注入：外部先回收子进程，使后续 kill/wait 无法确认回收。
        let pid = faulty_process.lock().unwrap().child.as_ref().unwrap().id() as i32;
        waitid(
            WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
            WaitIdOptions::EXITED,
        )
        .unwrap();
        assert!(faulty_process.lock().unwrap().child.is_some());

        let error = registry.shutdown().err().unwrap();
        // 聚合错误保持 typed CleanupError，且逐服务保留明细。
        assert!(error.downcast_ref::<CleanupError>().is_some());
        assert!(format!("{error:#}").contains("b: stopping MCP process failed"));
        // 任一失败保留整个 registry/slot。
        assert_eq!(registry.servers.len(), 2);
        assert_eq!(registry.tools.len(), 2);
        assert_eq!(registry.cleanup().unwrap().processes.len(), 2);
        // 重试不假成功：故障未清除前 shutdown 仍失败。
        assert!(registry.shutdown().is_err());
        assert!(!registry.servers.is_empty() && !registry.tools.is_empty());

        // 故障清除后（句柄回收完成）重试才真正成功并清空 registry。
        faulty_process.lock().unwrap().child.take();
        registry.shutdown().unwrap();
        assert!(registry.servers.is_empty() && registry.tools.is_empty());
        assert!(healthy_process.lock().unwrap().child.is_none());
    }

    #[test]
    fn plan_mcp_client_stop_stays_unstopped_until_reclaim_succeeds() {
        let (mut client, process) =
            plan_spawn_registry_client(&plan_server_config(&["-c", "true"]));
        let pid = process.lock().unwrap().child.as_ref().unwrap().id() as i32;
        waitid(
            WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
            WaitIdOptions::EXITED,
        )
        .unwrap();
        assert!(client.stop().is_err());
        assert!(!client.stopped);
        // 停止失败保留未回收句柄，重试不假成功。
        assert!(process.lock().unwrap().child.is_some());
        assert!(client.stop().is_err());

        // 回收成功后 stopped 才置位，重复 stop 幂等成功。
        process.lock().unwrap().child.take();
        client.stop().unwrap();
        assert!(client.stopped);
        assert!(client.stop().is_ok());
    }

    #[test]
    fn plan_mcp_connect_cancelled_before_spawn_does_not_spawn() {
        let marker = std::env::temp_dir().join(format!("pp_mcp_no_spawn_{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let config =
            plan_server_config(&["-c", &format!("touch {} && sleep 30", marker.display())]);
        let mut configs = BTreeMap::new();
        configs.insert("never".into(), config);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = Registry::connect(&configs, Path::new("."), &cancellation)
            .err()
            .unwrap();
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert!(format!("{error:#}").contains("connecting MCP server never"));
        // spawn 前已检查取消，fixture 进程从未启动。
        assert!(!marker.exists());
    }

    #[test]
    fn plan_mcp_connect_cancelled_during_discovery_reclaims_process() {
        let cancellation = CancellationToken::new();
        let canceller = cancellation.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            canceller.cancel();
        });
        let mut configs = BTreeMap::new();
        // 超时远大于取消时机，确保失败来自取消而非请求超时。
        let mut config = plan_server_config(&["-c", "sleep 30"]);
        config.timeout_seconds = 30;
        configs.insert("slow".into(), config);
        let error = Registry::connect(&configs, Path::new("."), &cancellation)
            .err()
            .unwrap();
        // discovery 取消且显式回收成功：错误保持 Cancelled，
        // 若停止失败会被升级为 CleanupError 而非 Cancelled。
        assert!(error.downcast_ref::<Cancelled>().is_some());
    }
}
