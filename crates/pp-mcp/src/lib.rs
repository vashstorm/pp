mod call;
mod client;
mod command;
mod config;
mod connect;
mod connect_status;
mod lifecycle;
mod manage;
mod remote;
#[cfg(test)]
mod remote_tests;
mod view;
pub use command::{Action, SUBCOMMANDS, help};
pub use config::{EntryView, View};
pub use connect_status::{ConnectPhase, ConnectSnapshot, ServerConnect};
pub use view::ToolSummary;

use anyhow::{Result, ensure};
use async_openai::types::chat::FunctionObject;
use client::{Client, SharedConnection};
use pp_core::cancellation::CancellationToken;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

#[cfg(test)]
use connect::exposed_name;
#[cfg(test)]
use pp_core::cancellation::Cancelled;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::thread;

const MESSAGE_LIMIT: usize = 1_048_576;
const TOOL_LIMIT: usize = 256;
const POLL: Duration = Duration::from_millis(5);
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

// No Debug impl, so credentials in command arguments and environment variables are never printed.
// PartialEq only compares whether the server definition changed and never leaks field contents.
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
pub enum Access {
    Full,
    ReadOnly,
}

impl Access {
    fn allows_server(self, config: &ServerConfig) -> bool {
        self == Self::Full || !config.read_only_tools.is_empty()
    }
}

/// Process-level MCP manager: root `main` constructs the single instance and injects it into Agent, subagents and the UI via `Arc`.
/// Stores only the config path and working directory and loads the config on first use; `empty()` is the in-memory instance for tests and mocks.
// A process-level singleton must be `Send + Sync`: enforced by the `Arc` handed across threads (Agent, subagents, UI worker).
pub struct McpManager {
    // `None` means the in-memory instance without a config file: never reads Claude/Codex sources under HOME.
    config: Option<PathBuf>,
    // Whether to discover user-level Claude/Codex sources under HOME; isolated instances read only the given pp config.
    discover_user_sources: bool,
    cwd: PathBuf,
    // Serializes config loading, connection set changes and shutdown; a single tool call does not take it.
    control: Mutex<()>,
    state: Mutex<State>,
    // Startup background connect thread: cancel it before reaping on exit so services are not started again after shutdown.
    connect_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// In-memory state: critical sections only touch memory and never hold a lock across network requests or process spawns.
#[derive(Default)]
pub(crate) struct State {
    directory: config::Directory,
    servers: BTreeMap<String, SharedServer>,
    tools: BTreeMap<String, RemoteTool>,
    // Every known connection slot, including slots that failed discovery or were removed from `servers`; shutdown stops each slot.
    connections: Vec<SharedConnection>,
    loaded: bool,
    closing: bool,
    // Startup background connect progress; snapshot reads never touch the network and `await_connect` only polls `finished`.
    connect: connect_status::Progress,
    // Cancellation handle of the current background connect: shutdown and test teardown use it to end the wait promptly.
    connect_token: Option<CancellationToken>,
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
// Same visibility as `SharedServer`: `Deref::Target` is part of the `pub(crate)` interface and must not leak a more private type.
pub(crate) struct ServerState {
    config: ServerConfig,
    cwd: PathBuf,
    client: Client,
    // A connection stopped only because of task cancellation keeps one rebuild chance; ordinary failures do not reconnect.
    restart_after_cancel: bool,
}

impl McpManager {
    /// The only production constructor: stores just the config path and working directory and loads the config on first use.
    pub fn new(config: PathBuf, cwd: PathBuf) -> Self {
        Self::with_optional_config(Some(config), cwd, true)
    }

    /// Isolated instance: reads only the given pp config and does not discover Claude/Codex sources under HOME.
    ///
    /// For unit tests and embedded scenarios only: `cfg(test)` is not set when the crate under test is compiled as a dependency,
    /// so the test state cannot be detected in the config layer anymore and the caller must disable external source discovery explicitly.
    pub fn new_isolated(config: PathBuf, cwd: PathBuf) -> Self {
        Self::with_optional_config(Some(config), cwd, false)
    }

    /// In-memory instance without a config file: for tests and mocks only; does not read Claude/Codex sources under HOME.
    pub fn empty() -> Self {
        Self::with_optional_config(None, PathBuf::from("."), false)
    }

    fn with_optional_config(
        config: Option<PathBuf>,
        cwd: PathBuf,
        discover_user_sources: bool,
    ) -> Self {
        // An instance without a config file is ready immediately; an instance with a path loads on first use and can retry after a failure.
        let loaded = config.is_none();
        Self {
            config,
            discover_user_sources,
            cwd,
            control: Mutex::new(()),
            state: Mutex::new(State {
                loaded,
                ..State::default()
            }),
            connect_thread: Mutex::new(None),
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
        // Fallback for panics or early returns: failures are only logged, and the exit code is decided by root main's `shutdown()`.
        if let Err(error) = self.stop_tracked() {
            tracing::error!("MCP cleanup failed during drop: {error:#}");
        }
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use crate::client::ConnectionSlot;
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
mod integration {
    use super::*;
    use crate::client::{ConnectionSlot, stop_connection};
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    use super::fixtures::{plan_server_config, plan_spawn_client};

    #[test]
    fn mcp_client_spawn_guards_and_stop_reclaim() {
        {
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
        {
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
        {
            let (mut client, process) = plan_spawn_client(&plan_server_config(&["-c", "true"]));
            let pid = process.lock().unwrap().child.as_ref().unwrap().id() as i32;
            waitid(
                WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
                WaitIdOptions::EXITED,
            )
            .unwrap();
            assert!(client.stop().is_err());
            assert!(!client.stopped);
            // A failed stop keeps the unreaped handle, so a retry cannot report success falsely.
            assert!(process.lock().unwrap().is_live());
            assert!(client.stop().is_err());

            // `stopped` is set only after reaping succeeds, so repeated stops are idempotent successes.
            process.lock().unwrap().child.take();
            client.stop().unwrap();
            assert!(client.stopped);
            assert!(client.stop().is_ok());
        }
    }
}
