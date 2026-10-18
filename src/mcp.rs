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
    path::Path,
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

#[derive(Default)]
pub(crate) struct Registry {
    servers: BTreeMap<String, Mutex<Client>>,
    tools: BTreeMap<String, RemoteTool>,
}

struct RemoteTool {
    server: String,
    original_name: String,
    definition: FunctionObject,
}

type SharedProcess = Arc<Mutex<Option<Child>>>;

pub(crate) struct Cleanup {
    processes: Vec<SharedProcess>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for process in &self.processes {
            stop_process(process);
        }
    }
}

impl Registry {
    pub(crate) fn cleanup(&self) -> Result<Cleanup> {
        let processes = self
            .servers
            .values()
            .map(|client| {
                client
                    .lock()
                    .map(|client| client.process.clone())
                    .map_err(|_| anyhow::anyhow!("MCP server unavailable"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Cleanup { processes })
    }

    pub(crate) fn connect(configs: &BTreeMap<String, ServerConfig>, cwd: &Path) -> Result<Self> {
        validate_config(configs)?;
        let mut registry = Self::default();
        for (name, config) in configs {
            registry
                .add_server(name, config, cwd)
                .with_context(|| format!("connecting MCP server {name}"))?;
        }
        Ok(registry)
    }

    fn add_server(&mut self, server: &str, config: &ServerConfig, cwd: &Path) -> Result<()> {
        let mut client = Client::spawn(config, cwd)?;
        let initialized = client.request(
            "initialize",
            json!({
                "protocolVersion":VERSIONS[0], "capabilities":{},
                "clientInfo":{"name":"pp", "version":env!("CARGO_PKG_VERSION")}
            }),
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
        client.notify("notifications/initialized", json!({}))?;
        if initialized["capabilities"].get("tools").is_some() {
            let mut cursor = None;
            let mut cursors = BTreeSet::new();
            for page in 0..TOOL_LIMIT {
                let params = cursor
                    .as_ref()
                    .map_or_else(|| json!({}), |value| json!({"cursor":value}));
                let result = client.request("tools/list", params)?;
                let tools = result["tools"]
                    .as_array()
                    .context("invalid MCP tools/list result")?;
                for tool in tools {
                    ensure!(
                        self.tools.len() < TOOL_LIMIT,
                        "too many MCP tools (maximum 256)"
                    );
                    let original = tool["name"].as_str().context("invalid MCP tool name")?;
                    // 短名称原样保留，其他名称使用确定性编码，保证模型工具名合法。
                    let exposed = exposed_name(server, original);
                    ensure!(
                        !original.is_empty() && original.len() <= 256,
                        "invalid MCP tool name"
                    );
                    ensure!(
                        !self.tools.contains_key(&exposed),
                        "duplicate MCP tool name"
                    );
                    let schema = &tool["inputSchema"];
                    ensure!(
                        schema.is_object() && schema["type"] == "object",
                        "invalid MCP tool input schema"
                    );
                    ensure!(
                        tool.get("description").is_none_or(Value::is_string),
                        "invalid MCP tool description"
                    );
                    self.tools.insert(
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
        self.servers.insert(server.into(), Mutex::new(client));
        Ok(())
    }

    pub(crate) fn definitions(&self) -> Vec<FunctionObject> {
        self.tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(crate) fn call(&self, name: &str, arguments: &str) -> Result<Value> {
        let tool = self.tools.get(name).context("unknown tool")?;
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|_| anyhow::anyhow!("invalid MCP tool arguments"))?;
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be an object"
        );
        let mut client = self.servers[&tool.server]
            .lock()
            .map_err(|_| anyhow::anyhow!("MCP server unavailable"))?;
        let result = client.request(
            "tools/call",
            json!({"name":tool.original_name,"arguments":arguments}),
        )?;
        ensure!(
            result["content"].is_array() && result.get("isError").is_none_or(Value::is_boolean),
            "invalid MCP tools/call result"
        );
        // 保留 MCP 内容块和结构化结果，工具级失败仍作为正常协议响应处理。
        Ok(json!({"ok":result["isError"] != true, "result":result}))
    }
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
    fn spawn(config: &ServerConfig, cwd: &Path) -> Result<Self> {
        let mut child = Command::new(&config.command)
            .args(&config.args)
            .envs(&config.env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|_| anyhow::anyhow!("starting MCP process failed"))?;
        let input = child.stdin.take().context("MCP stdin unavailable")?;
        let output = child.stdout.take().context("MCP stdout unavailable")?;
        let client = Self {
            process: Arc::new(Mutex::new(Some(child))),
            input: Some(input),
            output,
            pending: Vec::new(),
            timeout: Duration::from_secs(config.timeout_seconds),
            next_id: 0,
            stopped: false,
        };
        nonblocking(client.input.as_ref().expect("MCP stdin captured"))?;
        nonblocking(&client.output)?;
        Ok(client)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        ensure!(!self.stopped, "MCP connection is closed");
        self.next_id += 1;
        let id = self.next_id;
        let deadline = Instant::now() + self.timeout;
        let response = (|| {
            self.send(
                json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
                deadline,
            )?;
            loop {
                let message = self.receive(deadline)?;
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
                        self.send(reply, deadline)?;
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
                return Ok(Ok(message["result"].clone()));
            }
        })();
        match response {
            Ok(result) => result,
            Err(error) => {
                self.stop();
                Err(error)
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(
            json!({"jsonrpc":"2.0","method":method,"params":params}),
            Instant::now() + self.timeout,
        )
    }

    fn send(&mut self, message: Value, deadline: Instant) -> Result<()> {
        let mut bytes = message.to_string().into_bytes();
        ensure!(
            bytes.len() < MESSAGE_LIMIT,
            "MCP message exceeds size limit"
        );
        bytes.push(b'\n');
        let input = self.input.as_mut().context("MCP connection is closed")?;
        let mut offset = 0;
        while offset < bytes.len() {
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

    fn receive(&mut self, deadline: Instant) -> Result<Value> {
        loop {
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

    fn stop(&mut self) {
        if !self.stopped {
            self.input.take();
            stop_process(&self.process);
            self.stopped = true;
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if !self.stopped {
            self.input.take();
            // 正常退出先关闭 stdin，再给服务短暂的清理机会；故障路径直接终止。
            let process = self
                .process
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let Some(group) = process
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
                            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                        ) {
                            Ok(Some(_)) => break 'shutdown,
                            Err(error) if error != rustix::io::Errno::INTR => break 'shutdown,
                            _ => thread::sleep(POLL),
                        }
                    }
                }
            }
        }
        self.stop();
    }
}

fn stop_process(process: &SharedProcess) {
    let mut process = process.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(mut child) = process.take() {
        if let Some(group) = Pid::from_raw(child.id() as i32) {
            let _ = kill_process_group(group, Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn nonblocking(pipe: &impl AsFd) -> Result<()> {
    fcntl_setfl(pipe, fcntl_getfl(pipe)? | OFlags::NONBLOCK).context("configuring MCP pipe")
}
