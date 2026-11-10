mod client;
mod config;
mod remote;
#[cfg(test)]
mod remote_tests;
pub use config::{EntryView, View};

use client::{Client, ConnectionSlot, SharedConnection, stop_locked};

use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, TryLockError},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Help,
    List,
    Enable(String),
    Disable(String),
    Reconnect(String),
    Reload,
}
pub const SUBCOMMANDS: &[(&str, &str, bool)] = &[
    ("list", "List all MCP servers with their status", false),
    ("enable", "Enable an MCP server", true),
    ("disable", "Disable an MCP server", true),
    ("reconnect", "Reconnect an MCP server", true),
    ("reload", "Reload config and MCP servers", false),
    ("help", "Show MCP command help", false),
];
pub fn help() -> String {
    SUBCOMMANDS
        .iter()
        .map(|(name, description, arg)| {
            format!(
                "/mcp {name}{}  {description}",
                if *arg { " <name>" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
impl View {
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = self.diagnostics.clone();
        warnings.extend(
            self.entries
                .iter()
                .filter(|e| e.status.starts_with("failed") || e.status.starts_with("unsupported"))
                .map(|e| format!("MCP {}: {}", pp_core::text::display_text(&e.name), e.status)),
        );
        warnings
    }
    pub fn text(&self) -> String {
        let mut text = String::from("Configured MCP Servers\n");
        for entry in &self.entries {
            text.push_str(&format!(
                "{} ({})  {} — {}\n",
                entry.source,
                entry.path.display(),
                pp_core::text::display_text(&entry.name),
                entry.status
            ));
        }
        if self.entries.is_empty() {
            text.push_str("No MCP servers configured.\n");
        }
        for diagnostic in &self.diagnostics {
            text.push_str(diagnostic);
            text.push('\n');
        }
        text
    }
}

const MESSAGE_LIMIT: usize = 1_048_576;
const TOOL_LIMIT: usize = 256;
const POLL: Duration = Duration::from_millis(5);
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

// 不实现 Debug，避免输出命令参数和环境变量中的凭证。
// PartialEq 只用于比较服务定义是否变化，不泄露字段内容。
#[derive(Clone, PartialEq)]
pub(crate) struct ServerConfig {
    transport: TransportConfig,
    timeout_seconds: u64,
    timeout_millis: Option<u64>,
    read_only_tools: BTreeSet<String>,
}

#[derive(Clone, PartialEq)]
enum TransportConfig {
    Stdio(StdioConfig),
    Http(RemoteConfig),
    Sse(RemoteConfig),
}

#[derive(Clone, PartialEq)]
struct StdioConfig {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
}

#[derive(Clone, PartialEq)]
struct RemoteConfig {
    url: reqwest::Url,
    headers: reqwest::header::HeaderMap,
}

impl ServerConfig {
    fn timeout(&self) -> Duration {
        self.timeout_millis
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(self.timeout_seconds))
    }
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
        if let TransportConfig::Stdio(config) = &config.transport {
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
        }
        ensure!(
            config.timeout().as_millis() > 0
                && Instant::now().checked_add(config.timeout()).is_some(),
            "invalid MCP timeout_seconds"
        );
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Full,
    ReadOnly,
}

impl Access {
    pub(crate) fn for_mode(mode: pp_core::plan::WorkMode) -> Self {
        match mode {
            pp_core::plan::WorkMode::Execute => Self::Full,
            pp_core::plan::WorkMode::Plan => Self::ReadOnly,
        }
    }

    fn allows_server(self, config: &ServerConfig) -> bool {
        self == Self::Full || !config.read_only_tools.is_empty()
    }
}

/// 进程级 MCP 管理器：根 `main` 构造唯一实例，经 `Arc` 注入 Agent、子 Agent 与 UI。
/// 只保存配置路径与工作目录，首次使用时加载配置；`empty()` 为测试与 mock 的内存实例。
pub struct McpManager {
    // `None` 表示无配置文件的内存实例：绝不读取 HOME 下的 Claude/Codex 来源。
    config: Option<PathBuf>,
    cwd: PathBuf,
    // 串行化配置加载、连接集合变更与退出停止；单次工具调用不经过它。
    control: Mutex<()>,
    state: Mutex<State>,
}

/// 内存态：临界区只做内存读写，绝不跨网络请求或进程 spawn 持锁。
#[derive(Default)]
pub(crate) struct State {
    directory: config::Directory,
    servers: BTreeMap<String, SharedServer>,
    tools: BTreeMap<String, RemoteTool>,
    // 全部已知连接槽，包含发现失败或已从 `servers` 摘除的槽；退出停止按槽执行。
    connections: Vec<SharedConnection>,
    loaded: bool,
    closing: bool,
}

#[derive(Clone, PartialEq)]
struct RemoteTool {
    server: String,
    original_name: String,
    definition: FunctionObject,
}

#[derive(Clone)]
pub(crate) struct SharedServer {
    state: Arc<Mutex<ServerState>>,
    connection: SharedConnection,
    read_only_tools: BTreeSet<String>,
}
impl SharedServer {
    fn new(state: ServerState) -> Self {
        Self {
            connection: state.client.connection.clone(),
            read_only_tools: state.config.read_only_tools.clone(),
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
// 与 `SharedServer` 同可见性：`Deref::Target` 属于 `pub(crate)` 接口，不能泄漏更私有的类型。
pub(crate) struct ServerState {
    config: ServerConfig,
    cwd: PathBuf,
    client: Client,
    // 仅因任务取消而停止的连接保留一次重建资格；普通故障不重连。
    restart_after_cancel: bool,
}

// 退出停止的工作集：槽用于 slot 级急停，服务句柄在停止后才释放，
// 避免 `Client::drop` 的优雅回收路径与显式停止互相干扰。
struct Tracked {
    slots: Vec<(String, SharedConnection)>,
    servers: Vec<SharedServer>,
}

// slot 级停止：只取连接槽锁，不等待 `ServerState`，因此能打断正在进行的请求。
fn stop_slots(slots: &[(String, SharedConnection)]) -> Result<()> {
    let mut failures = Vec::new();
    for (label, connection) in slots {
        let mut slot = connection.lock().unwrap_or_else(|error| error.into_inner());
        // 槽级关闭标记：清理完成后派发线程不能复活进程。
        slot.closing = true;
        if let Err(error) = stop_locked(&mut slot) {
            failures.push(format!("{label}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into())
    }
}

impl McpManager {
    /// 唯一生产构造入口：只保存配置路径与工作目录，首次使用时加载配置。
    pub fn new(config: PathBuf, cwd: PathBuf) -> Self {
        Self::with_optional_config(Some(config), cwd)
    }

    /// 无配置文件的内存实例：测试与 mock 专用，不读取 HOME 下的 Claude/Codex 来源。
    pub fn empty() -> Self {
        Self::with_optional_config(None, PathBuf::from("."))
    }

    fn with_optional_config(config: Option<PathBuf>, cwd: PathBuf) -> Self {
        // 无配置文件的实例已就绪；带路径的实例等首次使用时加载，失败后可重试。
        let loaded = config.is_none();
        Self {
            config,
            cwd,
            control: Mutex::new(()),
            state: Mutex::new(State {
                loaded,
                ..State::default()
            }),
        }
    }

    fn lock_control(&self) -> MutexGuard<'_, ()> {
        self.control
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn lock_state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// 退出路径：先让空闲服务优雅退出，再按连接槽急停剩余连接。
    /// 调用后管理器进入关闭状态，不再接受新连接；失败聚合成 typed `CleanupError`。
    pub fn shutdown(&self) -> Result<()> {
        self.stop_tracked()
    }

    fn stop_tracked(&self) -> Result<()> {
        let _control = self.lock_control();
        let tracked = self.take_tracked();
        // 先释放服务句柄：空闲的 stdio 服务在 `Client::drop` 中关闭 stdin 并得到
        // 有界宽限，服务端可自行收尾（退出后仍能观察到 fixture 的 `.closed`）。
        // 正被占用的服务仍由 worker 持有句柄，这里不会等待它。
        drop(tracked.servers);
        // 再按槽位急停剩余连接：不等待 `ServerState` 锁，能打断活动请求，
        // 并在槽级标记 closing，禁止清理完成后由 worker 复活进程。
        stop_slots(&tracked.slots)
    }

    // 在 state 下标记关闭并摘出全部槽与服务句柄；实际停止在锁外完成。
    fn take_tracked(&self) -> Tracked {
        let mut state = self.lock_state();
        state.closing = true;
        let servers = std::mem::take(&mut state.servers);
        state.tools.clear();
        let mut slots: Vec<(String, SharedConnection)> = servers
            .iter()
            .map(|(name, server)| (name.clone(), server.connection.clone()))
            .collect();
        for connection in std::mem::take(&mut state.connections) {
            if slots
                .iter()
                .any(|(_, existing)| Arc::ptr_eq(existing, &connection))
            {
                continue;
            }
            slots.push((format!("connection {}", slots.len() + 1), connection));
        }
        let names: Vec<String> = servers.keys().cloned().collect();
        for name in &names {
            if let Some(entry) = state.directory.entries.get_mut(name) {
                entry.status = "not connected";
            }
        }
        Tracked {
            slots,
            servers: servers.into_values().collect(),
        }
    }

    // 首次需要时加载配置；失败保持未加载，下一次调用重试。
    fn ensure_loaded(&self) -> Result<()> {
        let Some(path) = self.config.clone() else {
            // 内存实例已就绪：绝不读取 HOME 或写外部配置。
            self.lock_state().loaded = true;
            return Ok(());
        };
        if self.lock_state().loaded {
            return Ok(());
        }
        // 解析（可能读 HOME 下的外部来源）在 state 锁外完成。
        let directory = config::Directory::load(&path)?;
        let mut state = self.lock_state();
        state.directory = directory;
        state.loaded = true;
        Ok(())
    }

    pub(crate) fn connect_enabled(&self, token: &CancellationToken, access: Access) -> Result<()> {
        let _control = self.lock_control();
        self.ensure_loaded()?;
        self.connect_enabled_locked(token, access)
    }

    // 调用方必须已持有 control。
    fn connect_enabled_locked(&self, token: &CancellationToken, access: Access) -> Result<()> {
        let names: Vec<String> = {
            let state = self.lock_state();
            state
                .directory
                .entries
                .iter()
                .filter(|(_, entry)| {
                    Self::eligible_entry(entry, access) && entry.status == "not connected"
                })
                .map(|(name, _)| name.clone())
                .collect()
        };
        for name in names {
            cancellation::check(token)?;
            self.connect_one(&name, token)?;
        }
        Ok(())
    }

    fn eligible_entry(entry: &config::Entry, access: Access) -> bool {
        entry.enabled
            && entry
                .config
                .as_ref()
                .is_some_and(|config| access.allows_server(config))
    }

    fn eligible(&self, name: &str, access: Access) -> bool {
        self.lock_state()
            .directory
            .entries
            .get(name)
            .is_some_and(|entry| Self::eligible_entry(entry, access))
    }

    // 单服务连接：普通故障只落到条目状态，取消与清理失败才向上传播。
    fn connect_one(&self, name: &str, token: &CancellationToken) -> Result<()> {
        let Some(config) = self
            .lock_state()
            .directory
            .entries
            .get(name)
            .and_then(|entry| entry.config.clone())
        else {
            return Ok(());
        };
        let result = self.add_server(name, &config, token);
        let status = if result.is_ok() {
            "connected"
        } else if result.as_ref().is_err_and(|error| error.is::<Cancelled>()) {
            "not connected"
        } else if result.as_ref().err().is_some_and(|error| {
            error
                .chain()
                .any(|cause| cause.to_string().contains("timed out"))
        }) {
            "failed (timeout)"
        } else if result
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string() == "starting MCP process failed")
        {
            "failed (process start)"
        } else {
            "failed (initialization)"
        };
        if let Some(entry) = self.lock_state().directory.entries.get_mut(name) {
            entry.status = status;
        }
        match result {
            Err(error) if error.is::<CleanupError>() || error.is::<Cancelled>() => Err(error),
            _ => Ok(()),
        }
    }

    pub(crate) fn manage(
        &self,
        action: &Action,
        token: &CancellationToken,
        access: Access,
    ) -> Result<()> {
        let _control = self.lock_control();
        self.ensure_loaded()?;
        self.manage_locked(action, token, access)
    }

    // 调用方必须已持有 control。
    fn manage_locked(
        &self,
        action: &Action,
        token: &CancellationToken,
        access: Access,
    ) -> Result<()> {
        match action {
            Action::Help | Action::List => {}
            Action::Reload => {
                // 内存实例的 Reload 是空操作，绝不读取外部来源。
                let Some(path) = self.config.clone() else {
                    return Ok(());
                };
                // 解析失败保留旧连接与工具，直接返回错误。
                let directory = config::Directory::load(&path)?;
                // state 级停止：失败保留 servers/tools 供重试，不假装成功。
                self.stop_all_servers()?;
                {
                    let mut state = self.lock_state();
                    state.directory = directory;
                    state.loaded = true;
                }
                self.connect_enabled_locked(token, access)?;
            }
            Action::Enable(name) | Action::Disable(name) => {
                let enabled = matches!(action, Action::Enable(_));
                // 名称必须在已加载的目录里：内存实例因此报 unknown MCP server。
                let directory = {
                    let state = self.lock_state();
                    ensure!(
                        state.directory.entries.contains_key(name),
                        "unknown MCP server"
                    );
                    state.directory.clone()
                };
                // 文件锁与原子替换可能等待其他进程，绝不持 state。
                directory.save_enabled(name, enabled, token)?;
                if let Some(entry) = self.lock_state().directory.entries.get_mut(name) {
                    entry.enabled = enabled;
                }
                self.disconnect(name)?;
                if self.eligible(name, access) {
                    self.connect_one(name, token)?;
                }
            }
            Action::Reconnect(name) => {
                {
                    let state = self.lock_state();
                    let entry = state
                        .directory
                        .entries
                        .get(name)
                        .ok_or_else(|| anyhow::anyhow!("unknown MCP server"))?;
                    ensure!(entry.enabled, "MCP server is disabled; enable it first");
                    ensure!(
                        access == Access::Full || Self::eligible_entry(entry, access),
                        "MCP server has no authorized read-only tools for Plan/Review"
                    );
                }
                self.disconnect(name)?;
                self.connect_one(name, token)?;
            }
        }
        Ok(())
    }

    // 停止单个服务并摘除其工具：启停与重连前先断开旧连接。
    fn disconnect(&self, name: &str) -> Result<()> {
        let server = { self.lock_state().servers.get(name).cloned() };
        if let Some(server) = server {
            let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
            state.client.stop().map_err(CleanupError)?;
            // 永久关闭旧引用，旧子 Agent 快照不能重新启动它。
            state.restart_after_cancel = false;
        }
        // 旧句柄在 state 锁外释放；`_removed` 在函数结束时触发 `Client::drop`。
        let _removed = {
            let mut state = self.lock_state();
            let removed = state.servers.remove(name);
            if removed.is_some()
                && let Some(entry) = state.directory.entries.get_mut(name)
            {
                entry.status = "not connected";
            }
            state.tools.retain(|_, tool| tool.server != name);
            removed
        };
        self.prune_connections();
        Ok(())
    }

    fn prune_connections(&self) {
        let mut state = self.lock_state();
        state.connections.retain(|slot| {
            slot.lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_live()
        });
    }

    // 显式停止全部服务（`/mcp reload` 使用）：逐 server 调用 client.stop() 并检查
    // 结果；任何失败都保留 servers/tools 供重试，全部确认回收后才清空并清理残留槽。
    fn stop_all_servers(&self) -> Result<()> {
        let servers: Vec<(String, SharedServer)> = {
            let state = self.lock_state();
            state
                .servers
                .iter()
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect()
        };
        let mut failures = Vec::new();
        for (name, server) in &servers {
            let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
            if let Err(error) = state.client.stop() {
                failures.push(format!("{name}: {error:#}"));
            }
            // 清理后旧快照不能再重建进程。
            state.restart_after_cancel = false;
        }
        if !failures.is_empty() {
            return Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into());
        }
        // 发现失败的连接可能还未注册工具，但仍留在跟踪列表里。
        let connections = { self.lock_state().connections.clone() };
        for (index, connection) in connections.iter().enumerate() {
            let mut slot = connection.lock().unwrap_or_else(|error| error.into_inner());
            if slot.is_live() {
                slot.closing = true;
                if let Err(error) = stop_locked(&mut slot) {
                    failures.push(format!("connection {}: {error:#}", index + 1));
                }
            }
        }
        if !failures.is_empty() {
            return Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into());
        }
        let _dropped = {
            let mut state = self.lock_state();
            let names: Vec<String> = state.servers.keys().cloned().collect();
            for name in &names {
                if let Some(entry) = state.directory.entries.get_mut(name) {
                    entry.status = "not connected";
                }
            }
            let dropped: Vec<SharedServer> =
                std::mem::take(&mut state.servers).into_values().collect();
            state.tools.clear();
            state.connections.clear();
            dropped
        };
        Ok(())
    }

    // 连接可被调用方的 token 取消：每次 spawn 前和 discovery 全程检查；
    // 取消不自动重试，由调用方决定是否再次 connect。
    #[cfg(test)]
    pub(crate) fn connect(
        configs: &BTreeMap<String, ServerConfig>,
        cwd: &std::path::Path,
        cancellation: &CancellationToken,
    ) -> Result<Self> {
        validate_config(configs)?;
        let manager = Self::with_optional_config(None, cwd.to_owned());
        for (name, config) in configs {
            if let Err(error) = manager
                .add_server(name, config, cancellation)
                .with_context(|| format!("connecting MCP server {name}"))
            {
                return match manager.stop_tracked() {
                    Ok(()) => Err(error),
                    Err(cleanup) => {
                        Err(cleanup.context(format!("Connection also failed: {error:#}")))
                    }
                };
            }
        }
        Ok(manager)
    }

    fn add_server(
        &self,
        name: &str,
        config: &ServerConfig,
        token: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(token)?;
        let process: SharedConnection = Arc::new(Mutex::new(ConnectionSlot {
            child: None,
            remote: None,
            closing: false,
        }));
        {
            // 先登记槽再 spawn，退出停止才能覆盖尚未完成的连接。
            let mut state = self.lock_state();
            ensure!(!state.closing, "MCP manager is shutting down");
            state.connections.push(process.clone());
        }
        // spawn 与 discovery 都不持 state 锁。
        let mut client = Client::spawn(config, &self.cwd, process)?;
        let tools = match discover(&mut client, name, token) {
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
        // 连接入口由 control 串行化，校验与提交分两次短临界区完成。
        let validation = {
            let state = self.lock_state();
            (|| -> Result<()> {
                for tool_name in tools.keys() {
                    ensure!(
                        !state.tools.contains_key(tool_name),
                        "duplicate MCP tool name"
                    );
                }
                ensure!(
                    state.tools.len() + tools.len() <= TOOL_LIMIT,
                    "too many MCP tools (maximum 256)"
                );
                Ok(())
            })()
        };
        if let Err(error) = validation {
            client.stop().map_err(CleanupError)?;
            return Err(error);
        }
        {
            let mut state = self.lock_state();
            state.tools.extend(tools);
            state.servers.insert(
                name.to_owned(),
                SharedServer::new(ServerState {
                    config: config.clone(),
                    cwd: self.cwd.clone(),
                    client,
                    restart_after_cancel: false,
                }),
            );
        }
        Ok(())
    }

    pub(crate) fn is_read_only(&self, name: &str) -> bool {
        let state = self.lock_state();
        state
            .tools
            .get(name)
            .is_some_and(|tool| read_only_tool(&state, tool))
    }

    pub(crate) fn definitions(&self, access: Access) -> Vec<FunctionObject> {
        let state = self.lock_state();
        state
            .tools
            .values()
            .filter(|tool| {
                tool_available(&state, tool)
                    && (access == Access::Full || read_only_tool(&state, tool))
            })
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(crate) fn view(&self, access: Access) -> View {
        let state = self.lock_state();
        View {
            entries: state
                .directory
                .entries
                .iter()
                .map(|(name, entry)| {
                    let status = if !entry.enabled {
                        "disabled"
                    } else if entry.config.is_none() {
                        entry.status
                    } else if !entry
                        .config
                        .as_ref()
                        .is_some_and(|config| access.allows_server(config))
                    {
                        "suspended (Plan/Review)"
                    } else if let Some(server) = state.servers.get(name) {
                        // 并行调用正在使用服务时视为可用，不阻塞管理查询。
                        match server.try_lock() {
                            Ok(server) => {
                                if server.restart_after_cancel {
                                    "not connected (cancelled)"
                                } else if server.client.stopped {
                                    "failed (connection closed)"
                                } else {
                                    "connected"
                                }
                            }
                            Err(TryLockError::Poisoned(_)) | Err(TryLockError::WouldBlock) => {
                                "connected"
                            }
                        }
                    } else {
                        entry.status
                    };
                    EntryView {
                        name: name.clone(),
                        source: entry.source.clone(),
                        path: entry.path.clone(),
                        status,
                    }
                })
                .collect(),
            diagnostics: state
                .directory
                .diagnostics
                .iter()
                .cloned()
                .chain(state.servers.iter().filter_map(|(name, server)| {
                    let missing = server
                        .read_only_tools
                        .iter()
                        .filter(|original| {
                            state
                                .tools
                                .get(&exposed_name(name, original))
                                .is_none_or(|tool| {
                                    tool.server != *name || tool.original_name != **original
                                })
                        })
                        .count();
                    (missing > 0).then(|| {
                        format!(
                            "MCP {name}: {missing} configured read-only tools were not discovered"
                        )
                    })
                }))
                .collect(),
        }
    }

    pub(crate) fn proxy_call(
        self: &Arc<Self>,
        name: &str,
        arguments: &str,
        access: Access,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        cancellation::check(cancellation)?;
        // worker 持独立 Arc 与新 transport token：父连接不因子 Agent 取消而中断。
        let manager = Arc::clone(self);
        let request_token = cancellation.clone();
        let name = name.to_owned();
        let arguments = arguments.to_owned();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("pp-mcp-proxy".into())
            .spawn(move || {
                let outcome = manager.call_inner(&name, &arguments, access, &request_token, true);
                let _ = sender.send(outcome);
            })?;
        loop {
            cancellation::check(cancellation)?;
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => bail!("MCP worker exited"),
            }
        }
    }

    pub(crate) fn call(
        &self,
        name: &str,
        arguments: &str,
        access: Access,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        self.call_inner(name, arguments, access, cancellation, false)
    }

    fn call_inner(
        &self,
        name: &str,
        arguments: &str,
        access: Access,
        cancellation: &CancellationToken,
        proxy: bool,
    ) -> Result<Value> {
        // 短临界区：解析工具、校验 access、克隆服务句柄与该服务的定义快照；
        // 之后阻塞获取 server 锁，绝不在持 state 锁时等待网络或进程。
        let (server, server_name, original_name, expected) = {
            let state = self.lock_state();
            let tool = state.tools.get(name).context("unknown tool")?;
            ensure!(
                access == Access::Full || read_only_tool(&state, tool),
                "MCP tool is not authorized for read-only access"
            );
            let server = state
                .servers
                .get(&tool.server)
                .cloned()
                .context("MCP server is not connected")?;
            let expected: BTreeMap<String, RemoteTool> = state
                .tools
                .iter()
                .filter(|(_, other)| other.server == tool.server)
                .map(|(name, other)| (name.clone(), other.clone()))
                .collect();
            (
                server,
                tool.server.clone(),
                tool.original_name.clone(),
                expected,
            )
        };
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|_| anyhow::anyhow!("invalid MCP tool arguments"))?;
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be an object"
        );
        let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
        if state.restart_after_cancel {
            Self::restart(&server_name, &mut state, &expected, cancellation)?;
        }
        cancellation::check(cancellation)?;
        let transport_token = if proxy {
            CancellationToken::new()
        } else {
            cancellation.clone()
        };
        let result = match state.client.request(
            "tools/call",
            json!({"name":original_name,"arguments":arguments}),
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
    // 在 server 锁内完成 spawn 与 discovery，用调用方在 state 锁下取得的快照
    // 做定义完全一致校验，不重新获取 state。
    fn restart(
        server: &str,
        state: &mut ServerState,
        expected: &BTreeMap<String, RemoteTool>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        let process = state.client.connection.clone();
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
        // 与快照完整比较：增删或 exposed/原始名/描述/参数任一变化都拒绝，
        // 且校验通过前不发送新 tools/call、不更新全局工具表。
        let unchanged = discovered.len() == expected.len()
            && expected
                .iter()
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

    #[cfg(test)]
    pub(crate) fn test_state(&self) -> MutexGuard<'_, State> {
        self.lock_state()
    }

    #[cfg(test)]
    pub(crate) fn test_server(&self, name: &str) -> Option<SharedServer> {
        self.lock_state().servers.get(name).cloned()
    }

    #[cfg(test)]
    pub(crate) fn test_connection_count(&self) -> usize {
        self.lock_state().connections.len()
    }

    #[cfg(test)]
    pub(crate) fn test_stop_servers(&self) -> Result<()> {
        let _control = self.lock_control();
        self.stop_all_servers()
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        // panic 或提前返回的兜底：失败只记日志，退出码由根 main 的 `shutdown()` 决定。
        if let Err(error) = self.stop_tracked() {
            tracing::error!("MCP cleanup failed during drop: {error:#}");
        }
    }
}

// 服务可用性：`WouldBlock` 表示正被其他线程调用，视为可用；中毒沿用旧的可读语义。
fn server_available(server: &SharedServer) -> bool {
    match server.try_lock() {
        Ok(state) => !state.client.stopped || state.restart_after_cancel,
        Err(TryLockError::Poisoned(_)) | Err(TryLockError::WouldBlock) => true,
    }
}

fn tool_available(state: &State, tool: &RemoteTool) -> bool {
    state
        .servers
        .get(&tool.server)
        .is_some_and(server_available)
}

fn read_only_tool(state: &State, tool: &RemoteTool) -> bool {
    state
        .servers
        .get(&tool.server)
        .is_some_and(|server| server.read_only_tools.contains(&tool.original_name))
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use crate::mcp::client::stop_connection;
    use std::path::Path;
    #[test]
    fn mcp_manager_is_send_and_sync() {
        // 单例经 `Arc` 注入 Agent、子 Agent 与 UI worker 线程。
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<McpManager>();
    }
    #[test]
    fn mcp_spawn_refuses_after_cleanup_marks_slot_closing() {
        let process: SharedConnection = Arc::new(Mutex::new(ConnectionSlot {
            child: None,
            remote: None,
            closing: true,
        }));
        let config = ServerConfig {
            transport: TransportConfig::Stdio(StdioConfig {
                command: "bash".into(),
                args: vec!["-c".into(), "sleep 30".into()],
                env: BTreeMap::new(),
                cwd: None,
            }),
            timeout_seconds: 1,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
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
        let process: SharedConnection = Arc::new(Mutex::new(ConnectionSlot {
            child: Some(child),
            remote: None,
            closing: false,
        }));
        let config = ServerConfig {
            transport: TransportConfig::Stdio(StdioConfig {
                command: "bash".into(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            }),
            timeout_seconds: 1,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
        };
        let error = Client::spawn(&config, Path::new("."), process.clone())
            .err()
            .unwrap();
        assert_eq!(format!("{error:#}"), "MCP process is still running");
        stop_connection(&process).unwrap();
        assert!(process.lock().unwrap().child.is_none());
    }

    fn plan_server_config(args: &[&str]) -> ServerConfig {
        ServerConfig {
            transport: TransportConfig::Stdio(StdioConfig {
                command: "bash".into(),
                args: args.iter().map(|arg| (*arg).into()).collect(),
                env: BTreeMap::new(),
                cwd: None,
            }),
            timeout_seconds: 1,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
        }
    }

    fn plan_spawn_client(config: &ServerConfig) -> (Client, SharedConnection) {
        let process: SharedConnection = Arc::new(Mutex::new(ConnectionSlot {
            child: None,
            remote: None,
            closing: false,
        }));
        let client = Client::spawn(config, Path::new("."), process.clone()).unwrap();
        (client, process)
    }

    #[test]
    fn proxy_cancelled_while_queued_never_dispatches_or_closes_shared_connection() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            transport: TransportConfig::Stdio(StdioConfig {
                command: "bash".into(),
                args: vec![format!(
                    "{}/../../tests/fixtures/mcp_server.sh",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::from([("MCP_LOG".into(), "proxy-log".into())]),
                cwd: None,
            }),
            timeout_seconds: 1,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
        };
        let manager = Arc::new(
            McpManager::connect(
                &BTreeMap::from([("local".into(), config)]),
                dir.path(),
                &CancellationToken::new(),
            )
            .unwrap(),
        );
        let server = manager.test_server("local").expect("已连接的服务");
        // 先占住 server 锁，让代理调用停在 `call_inner` 的阻塞获取上。
        let guard = server.lock().unwrap();
        let proxy = manager.clone();
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            proxy.proxy_call(
                "mcp__local__echo",
                r#"{"text":"must-not-dispatch"}"#,
                Access::Full,
                &worker_token,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        // worker 在 state 下克隆服务句柄后 strong_count 才达到 3；
        // 此时取消才能证明“取到锁后才发现已取消”，而不是压根没进入调用。
        while Arc::strong_count(&server.state) < 3 {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        token.cancel();
        assert!(worker.join().unwrap().unwrap_err().is::<Cancelled>());
        drop(guard);
        while Arc::strong_count(&server.state) > 2 {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        // 取消不关闭共享连接：同一 manager 的后继调用照常成功。
        assert!(
            manager
                .call(
                    "mcp__local__echo",
                    r#"{"text":"survived"}"#,
                    Access::Full,
                    &CancellationToken::new()
                )
                .unwrap()["ok"]
                == true
        );
        let log = std::fs::read_to_string(dir.path().join("proxy-log")).unwrap();
        assert!(!log.contains("must-not-dispatch"));
    }

    fn plan_register(manager: &McpManager, name: &str, client: Client) {
        let mut state = manager.test_state();
        state.servers.insert(
            name.into(),
            SharedServer::new(ServerState {
                config: plan_server_config(&["-c", "true"]),
                cwd: PathBuf::from("."),
                client,
                restart_after_cancel: false,
            }),
        );
        state.tools.insert(
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
        let manager = McpManager::empty();
        let mut processes = Vec::new();
        for name in ["a", "b"] {
            let (client, process) = plan_spawn_client(&plan_server_config(&["-c", "sleep 30"]));
            plan_register(&manager, name, client);
            processes.push(process);
        }
        manager.test_stop_servers().unwrap();
        {
            let state = manager.test_state();
            assert!(state.servers.is_empty() && state.tools.is_empty());
        }
        assert_eq!(manager.test_connection_count(), 0);
        for process in &processes {
            // 全部进程组已被回收，slot 不再持有句柄。
            assert!(process.lock().unwrap().child.is_none());
        }
        // 清空后重复停止仍是成功，不误报。
        manager.test_stop_servers().unwrap();
    }

    #[test]
    fn plan_mcp_shutdown_failure_retains_registry_and_retry_does_not_fake_success() {
        let manager = McpManager::empty();
        let (healthy, healthy_process) =
            plan_spawn_client(&plan_server_config(&["-c", "sleep 30"]));
        plan_register(&manager, "a", healthy);
        let (faulty, faulty_process) = plan_spawn_client(&plan_server_config(&["-c", "true"]));
        plan_register(&manager, "b", faulty);
        // 登记槽，验证失败时连接跟踪不被丢弃。
        {
            let mut state = manager.test_state();
            state.connections.push(healthy_process.clone());
            state.connections.push(faulty_process.clone());
        }

        // 故障注入：外部先回收子进程，使后续 kill/wait 无法确认回收。
        let pid = faulty_process.lock().unwrap().child.as_ref().unwrap().id() as i32;
        waitid(
            WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
            WaitIdOptions::EXITED,
        )
        .unwrap();
        assert!(faulty_process.lock().unwrap().is_live());

        let error = manager.test_stop_servers().err().unwrap();
        // 聚合错误保持 typed CleanupError，且逐服务保留明细。
        assert!(error.downcast_ref::<CleanupError>().is_some());
        assert!(format!("{error:#}").contains("b: stopping MCP process failed"));
        // 任一失败保留整个 servers/tools 与跟踪槽。
        {
            let state = manager.test_state();
            assert_eq!(state.servers.len(), 2);
            assert_eq!(state.tools.len(), 2);
        }
        assert_eq!(manager.test_connection_count(), 2);
        // 重试不假成功：故障未清除前停止仍然失败。
        assert!(manager.test_stop_servers().is_err());
        {
            let state = manager.test_state();
            assert!(!state.servers.is_empty() && !state.tools.is_empty());
        }

        // 故障清除后（句柄回收完成）重试才真正成功并清空。
        faulty_process.lock().unwrap().child.take();
        manager.test_stop_servers().unwrap();
        {
            let state = manager.test_state();
            assert!(state.servers.is_empty() && state.tools.is_empty());
        }
        assert!(healthy_process.lock().unwrap().child.is_none());
    }

    #[test]
    fn plan_mcp_client_stop_stays_unstopped_until_reclaim_succeeds() {
        let (mut client, process) = plan_spawn_client(&plan_server_config(&["-c", "true"]));
        let pid = process.lock().unwrap().child.as_ref().unwrap().id() as i32;
        waitid(
            WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
            WaitIdOptions::EXITED,
        )
        .unwrap();
        assert!(client.stop().is_err());
        assert!(!client.stopped);
        // 停止失败保留未回收句柄，重试不假成功。
        assert!(process.lock().unwrap().is_live());
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
        let error = McpManager::connect(&configs, Path::new("."), &cancellation)
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
        let error = McpManager::connect(&configs, Path::new("."), &cancellation)
            .err()
            .unwrap();
        // discovery 取消且显式回收成功：错误保持 Cancelled，
        // 若停止失败会被升级为 CleanupError 而非 Cancelled。
        assert!(error.downcast_ref::<Cancelled>().is_some());
    }
}
