//! Config loading, connecting by access, tool discovery and lazy rebuild after cancellation.

use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use super::client::{Client, ConnectionSlot, SharedConnection};
#[cfg(test)]
use super::validate_config;
use super::{
    Access, ConnectSnapshot, McpManager, POLL, RemoteTool, ServerConfig, ServerConnect,
    ServerState, SharedServer, TOOL_LIMIT, VERSIONS, config, safe_name,
};
use std::time::Instant;

impl McpManager {
    // Load the config on first need; a failure leaves it unloaded so the next call retries.
    pub(super) fn ensure_loaded(&self) -> Result<()> {
        let Some(path) = self.config.clone() else {
            // The in-memory instance is ready: never read HOME or write external configuration.
            self.lock_state().loaded = true;
            return Ok(());
        };
        if self.lock_state().loaded {
            return Ok(());
        }
        // Parsing (production instances also read external sources under HOME) happens outside the state lock.
        let directory = config::Directory::load(&path, self.discover_user_sources)?;
        let mut state = self.lock_state();
        state.directory = directory;
        state.loaded = true;
        Ok(())
    }

    pub fn connect_enabled(&self, token: &CancellationToken, access: Access) -> Result<()> {
        // Startup connecting may still be running: wait for it to finish, then fill in whatever the current access still needs.
        self.await_connect(token)?;
        let _control = self.lock_control();
        self.ensure_loaded()?;
        self.connect_enabled_locked(token, access)
    }

    /// Startup entry point: registers the background connect and returns immediately. Repeated calls restart only after the previous round finished.
    pub fn start_connect(self: &Arc<Self>, access: Access) {
        // An in-memory instance without a config file (tests and mocks) has nothing to connect and leaves no idle thread.
        if self.config.is_none() {
            return;
        }
        let token = CancellationToken::new();
        {
            let mut state = self.lock_state();
            if state.closing || !state.connect.is_finished() {
                return;
            }
            // Register this round as in progress before spawning the thread: neither the barrier nor a snapshot may observe an "not started" gap.
            state.connect.reserve();
            state.connect_token = Some(token.clone());
        }
        let manager = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("pp-mcp-connect".into())
            .spawn(move || manager.run_connect(token, access));
        match spawned {
            Ok(thread) => {
                let mut slot = self
                    .connect_thread
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                // The previous round was either joined or taken by stop_tracked; overwriting leaks no thread.
                *slot = Some(thread);
            }
            Err(error) => {
                tracing::warn!(event = "mcp_connect_thread_failed", error = %error);
                self.lock_state()
                    .connect
                    .fail("failed to start the MCP connect worker");
            }
        }
    }

    /// Read-only snapshot of the startup connect; the UI and CLI read only this and never touch the network while drawing or inside the event loop.
    pub fn connect_snapshot(&self) -> ConnectSnapshot {
        self.lock_state().connect.snapshot()
    }

    /// Line mode takes the overall connect status once: later tasks in the same process do not print it again.
    pub fn take_connect_report(&self) -> Option<String> {
        self.lock_state().connect.take_report()
    }

    /// Line mode claims finished servers not yet printed (contiguous in config order); repeated calls and retries after
    /// cancellation never repeat servers already printed.
    pub fn take_connect_services(&self) -> Vec<ServerConnect> {
        self.lock_state().connect.take_claimed()
    }

    /// Task entry barrier: waits for the startup connect to finish. Instances that never started (tests, mocks) return immediately.
    pub fn await_connect(&self, cancellation: &CancellationToken) -> Result<()> {
        loop {
            cancellation::check(cancellation)?;
            if self.lock_state().connect.is_finished() {
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    // Background thread body: holds control throughout, serialized against `/mcp` changes and shutdown.
    fn run_connect(self: Arc<Self>, token: CancellationToken, access: Access) {
        let _control = self.lock_control();
        let prepared = (|| -> Result<Vec<String>> {
            self.ensure_loaded()?;
            let mut state = self.lock_state();
            let names: Vec<String> = state
                .directory
                .entries
                .iter()
                .filter(|(_, entry)| {
                    Self::eligible_entry(entry, access) && entry.status == "not connected"
                })
                .map(|(name, _)| name.clone())
                .collect();
            state.connect.begin(names.clone());
            for name in &names {
                if let Some(entry) = state.directory.entries.get_mut(name) {
                    entry.status = "connecting";
                }
            }
            Ok(names)
        })();
        let names = match prepared {
            Ok(names) => names,
            Err(error) => {
                // Unavailable configuration is not a startup failure: the tip line shows the redacted reason and the first task proceeds normally.
                let mut state = self.lock_state();
                state.connect.fail(&format!("{error:#}"));
                state.connect_token = None;
                return;
            }
        };
        for name in names {
            if token.is_cancelled() {
                break;
            }
            self.lock_state().connect.connecting(&name);
            let started = Instant::now();
            // Ordinary connection failures only affect the entry status; cancellation and reaping failures end the round and log diagnostics.
            match self.connect_one_outcome(&name, &token) {
                Ok(reason) => {
                    let connected = self
                        .lock_state()
                        .directory
                        .entries
                        .get(&name)
                        .is_some_and(|entry| entry.status == "connected");
                    self.lock_state().connect.complete(
                        &name,
                        connected,
                        started.elapsed(),
                        if connected { None } else { reason.as_deref() },
                    );
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    tracing::error!(
                        event = "mcp_connect_aborted",
                        server = %name,
                        kind = if error.is::<Cancelled>() { "cancelled" } else { "cleanup" },
                        error = %pp_core::text::display_text(&message)
                    );
                    self.lock_state().connect.complete(
                        &name,
                        false,
                        started.elapsed(),
                        Some(&message),
                    );
                    break;
                }
            }
        }
        let mut state = self.lock_state();
        // Targets not connected because of cancellation or shutdown return to a retryable state, so `/mcp reconnect` does not misjudge them.
        for name in state.connect.unfinished() {
            if let Some(entry) = state.directory.entries.get_mut(&name)
                && entry.status == "connecting"
            {
                entry.status = "not connected";
            }
        }
        state.connect.finish();
        state.connect_token = None;
    }

    // The caller must already hold control.
    pub(super) fn connect_enabled_locked(
        &self,
        token: &CancellationToken,
        access: Access,
    ) -> Result<()> {
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

    pub(super) fn eligible_entry(entry: &config::Entry, access: Access) -> bool {
        entry.enabled
            && entry
                .config
                .as_ref()
                .is_some_and(|config| access.allows_server(config))
    }

    pub(super) fn eligible(&self, name: &str, access: Access) -> bool {
        self.lock_state()
            .directory
            .entries
            .get(name)
            .is_some_and(|entry| Self::eligible_entry(entry, access))
    }

    // Single-server connect: ordinary failures only affect the entry status; only cancellation and cleanup failures propagate.
    pub(super) fn connect_one(&self, name: &str, token: &CancellationToken) -> Result<()> {
        self.connect_one_outcome(name, token).map(|_| ())
    }

    fn connect_one_outcome(&self, name: &str, token: &CancellationToken) -> Result<Option<String>> {
        let Some(config) = self
            .lock_state()
            .directory
            .entries
            .get(name)
            .and_then(|entry| entry.config.clone())
        else {
            return Ok(None);
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
            Ok(()) => Ok(None),
            Err(error) if error.is::<CleanupError>() || error.is::<Cancelled>() => Err(error),
            Err(error) => Ok(Some(format!("{error:#}"))),
        }
    }

    // The connection can be cancelled by the caller's token: checked before every spawn and throughout discovery;
    // cancellation does not retry automatically and the caller decides whether to connect again.
    #[cfg(test)]
    pub(crate) fn connect(
        configs: &BTreeMap<String, ServerConfig>,
        cwd: &std::path::Path,
        cancellation: &CancellationToken,
    ) -> Result<Self> {
        validate_config(configs)?;
        let manager = Self::with_optional_config(None, cwd.to_owned(), false);
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
            // Register the slot before spawning so shutdown stop can cover a connection that is not complete yet.
            let mut state = self.lock_state();
            ensure!(!state.closing, "MCP manager is shutting down");
            state.connections.push(process.clone());
        }
        // Neither spawn nor discovery holds the state lock.
        let mut client = Client::spawn(config, &self.cwd, process)?;
        let tools = match discover(&mut client, name, token) {
            Ok(tools) => tools,
            Err(error) => {
                // Initialization failure does not rely on Drop's best-effort reaping: stop explicitly; on stop failure merge
                // the original error; any stop failure is upgraded to a typed cleanup error.
                return Err(match client.stop() {
                    Ok(()) => error,
                    Err(stop_error) => CleanupError(
                        stop_error.context(format!("Initialization also failed: {error:#}")),
                    )
                    .into(),
                });
            }
        };
        // The connect entry point is serialized by control; validation and commit happen in two short critical sections.
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

    // Lazy rebuild after cancellation: runs once, only when a new task actually calls a tool of this server.
    // Spawn and discovery happen under the server lock, verifying that definitions match exactly the snapshot
    // the caller took under the state lock, without re-acquiring state.
    pub(super) fn restart(
        server: &str,
        state: &mut ServerState,
        expected: &BTreeMap<String, RemoteTool>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        let process = state.client.connection.clone();
        let mut client = match Client::spawn(&state.config, &state.cwd, process) {
            Ok(client) => client,
            Err(error) => {
                state.restart_after_cancel = false;
                if error.is::<CleanupError>() {
                    return Err(error);
                }
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
                    // The rebuild was cancelled: stop the new process and keep eligibility so the next new task can try again.
                    // A failed stop is upgraded to a typed cleanup error and not swallowed by fixed text.
                    if let Err(stop_error) = client.stop() {
                        state.restart_after_cancel = false;
                        return Err(CleanupError(stop_error).into());
                    }
                    return Err(error);
                }
                state.restart_after_cancel = false;
                if let Err(stop_error) = client.stop() {
                    return Err(CleanupError(stop_error).into());
                }
                bail!("MCP restart failed; restart pp to reconnect.");
            }
        };
        // Compare against the snapshot in full: any added/removed tool or any change to exposed/original name, description or parameters
        // is rejected, and no new tools/call is sent and the global tool table is not updated until validation passes.
        let unchanged = discovered.len() == expected.len()
            && expected
                .iter()
                .all(|(name, tool)| discovered.get(name) == Some(tool));
        if !unchanged {
            state.restart_after_cancel = false;
            if let Err(stop_error) = client.stop() {
                return Err(CleanupError(stop_error).into());
            }
            bail!("MCP tool definitions changed; restart pp to reconnect.");
        }
        // The old connection was stopped at cancellation, so Drop never touches the new process in the stable slot.
        state.client = client;
        state.restart_after_cancel = false;
        Ok(())
    }
}

// Handshake and tool discovery: the initial connection and the post-cancellation rebuild share this flow.
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
                // Short names are kept as-is; other names get a deterministic encoding so the model's tool name stays valid.
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
                    tool.get("description")
                        .is_none_or(|v| v.is_string() || v.is_null()),
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
                None | Some(Value::Null) => break,
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

pub(super) fn exposed_name(server: &str, tool: &str) -> String {
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
mod integration {
    use super::super::fixtures::plan_server_config;
    use super::*;
    use std::path::Path;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn plan_mcp_connect_cancellation_guards() {
        {
            let marker =
                std::env::temp_dir().join(format!("pp_mcp_no_spawn_{}", std::process::id()));
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
            // Cancellation was checked before spawn, so the fixture process never started.
            assert!(!marker.exists());
        }
        {
            let cancellation = CancellationToken::new();
            let canceller = cancellation.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(50));
                canceller.cancel();
            });
            let mut configs = BTreeMap::new();
            // The timeout is far larger than the cancellation delay, so the failure comes from cancellation rather than a request timeout.
            let mut config = plan_server_config(&["-c", "sleep 30"]);
            config.timeout_seconds = 30;
            configs.insert("slow".into(), config);
            let error = McpManager::connect(&configs, Path::new("."), &cancellation)
                .err()
                .unwrap();
            // Discovery was cancelled and explicit reaping succeeded: the error stays Cancelled;
            // had the stop failed it would be upgraded to CleanupError instead of Cancelled.
            assert!(error.downcast_ref::<Cancelled>().is_some());
        }
    }
}

#[cfg(test)]
mod background_tests {
    use super::*;
    use crate::{Access, ConnectPhase, McpManager};
    use std::time::Duration;

    fn write_config(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = dir.join("mcp.json");
        let log = dir.join("server-log");
        let script = format!(
            "{}/../../tests/fixtures/mcp_server.sh",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::write(
            &path,
            serde_json::json!({
                "mcpServers": {name: {"command": "bash", "args": [script], "env": {"MCP_LOG": log.to_str().unwrap()}}}
            })
            .to_string(),
        )
        .unwrap();
        path
    }

    #[test]
    fn background_connect_lifecycle() {
        {
            let dir = tempfile::tempdir().unwrap();
            let path = write_config(dir.path(), "local");
            let manager = Arc::new(McpManager::new_isolated(path, dir.path().to_path_buf()));
            let started = Instant::now();
            manager.start_connect(Access::Full);
            // Startup does not wait for the handshake: spawning the thread and registering progress return immediately and the background fills in the tool table.
            assert!(started.elapsed() < Duration::from_secs(1));
            manager.await_connect(&CancellationToken::new()).unwrap();
            let snapshot = manager.connect_snapshot();
            assert!(snapshot.finished);
            assert_eq!(snapshot.servers.len(), 1);
            assert_eq!(snapshot.servers[0].name, "local");
            assert_eq!(snapshot.servers[0].phase, ConnectPhase::Connected);
            assert!(snapshot.servers[0].elapsed.is_some());
            let tip = snapshot
                .tip()
                .expect("a configured server must have an overall status");
            assert!(
                tip.starts_with("Connected to MCP servers: 1 succeeded ("),
                "{tip}"
            );
            assert!(!tip.contains("failed"), "{tip}");
            assert!(tip.ends_with(')'), "{tip}");
            // The barrier before the first task guarantees the model request can see the MCP tools.
            assert!(
                !manager.definitions(Access::Full).is_empty(),
                "{}",
                manager.view(Access::Full).text()
            );
            manager.shutdown().unwrap();
        }
        {
            let manager = McpManager::empty();
            let started = Instant::now();
            manager.await_connect(&CancellationToken::new()).unwrap();
            assert!(started.elapsed() < Duration::from_secs(1));
            assert!(manager.connect_snapshot().tip().is_none());
        }
    }
}
