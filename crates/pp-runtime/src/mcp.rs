mod call;
mod client;
mod command;
mod config;
mod connect;
mod lifecycle;
mod manage;
mod remote;
#[cfg(test)]
mod remote_tests;
mod view;
pub use command::{Action, SUBCOMMANDS, help};
pub use config::{EntryView, View};

use anyhow::{Result, ensure};
use async_openai::types::chat::FunctionObject;
use client::{Client, SharedConnection};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

#[cfg(test)]
use connect::exposed_name;
#[cfg(test)]
use pp_core::cancellation::{CancellationToken, Cancelled};
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::thread;

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

#[cfg(test)]
mod fixtures {
    use super::*;
    use crate::mcp::client::ConnectionSlot;
    use std::path::Path;

    pub(super) fn plan_server_config(args: &[&str]) -> ServerConfig {
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

    pub(super) fn plan_spawn_client(config: &ServerConfig) -> (Client, SharedConnection) {
        let process: SharedConnection = Arc::new(Mutex::new(ConnectionSlot {
            child: None,
            remote: None,
            closing: false,
        }));
        let client = Client::spawn(config, Path::new("."), process.clone()).unwrap();
        (client, process)
    }

    pub(super) fn plan_register(manager: &McpManager, name: &str, client: Client) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client::{ConnectionSlot, stop_connection};
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    use super::fixtures::{plan_server_config, plan_spawn_client};

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
}
