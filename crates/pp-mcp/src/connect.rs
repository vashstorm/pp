//! 配置加载、按 access 建连、工具发现与取消后的惰性重建。

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
    // 首次需要时加载配置；失败保持未加载，下一次调用重试。
    pub(super) fn ensure_loaded(&self) -> Result<()> {
        let Some(path) = self.config.clone() else {
            // 内存实例已就绪：绝不读取 HOME 或写外部配置。
            self.lock_state().loaded = true;
            return Ok(());
        };
        if self.lock_state().loaded {
            return Ok(());
        }
        // 解析（生产实例还会读 HOME 下的外部来源）在 state 锁外完成。
        let directory = config::Directory::load(&path, self.discover_user_sources)?;
        let mut state = self.lock_state();
        state.directory = directory;
        state.loaded = true;
        Ok(())
    }

    pub fn connect_enabled(&self, token: &CancellationToken, access: Access) -> Result<()> {
        // 启动期建连可能仍在进行：先等它结束，再补齐当前 access 缺的服务。
        self.await_connect(token)?;
        let _control = self.lock_control();
        self.ensure_loaded()?;
        self.connect_enabled_locked(token, access)
    }

    /// 启动期入口：登记后台建连并立即返回。重复调用只在上一轮结束后重新开始。
    pub fn start_connect(self: &Arc<Self>, access: Access) {
        // 无配置文件的内存实例（测试与 mock）没有可连的服务，也不留空线程。
        if self.config.is_none() {
            return;
        }
        let token = CancellationToken::new();
        {
            let mut state = self.lock_state();
            if state.closing || !state.connect.is_finished() {
                return;
            }
            // 先登记本轮进行中，再起线程：屏障与快照都不能看到“未开始”的空窗。
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
                // 上一轮已 join 或在 stop_tracked 中取走；覆盖不会泄漏线程。
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

    /// 启动期建连的只读快照；UI 与 CLI 只读这个，不在绘制或事件循环里触网。
    pub fn connect_snapshot(&self) -> ConnectSnapshot {
        self.lock_state().connect.snapshot()
    }

    /// 行模式一次性取走建连总状态：同一进程的后续任务不重复输出。
    pub fn take_connect_report(&self) -> Option<String> {
        self.lock_state().connect.take_report()
    }

    /// 行模式一次性领取尚未输出的已完成服务（按配置顺序连续）；重复调用与取消后重试
    /// 都不会重复已输出过的服务。
    pub fn take_connect_services(&self) -> Vec<ServerConnect> {
        self.lock_state().connect.take_claimed()
    }

    /// 任务入口屏障：等启动期建连结束。未启动过的实例（测试、mock）立即返回。
    pub fn await_connect(&self, cancellation: &CancellationToken) -> Result<()> {
        loop {
            cancellation::check(cancellation)?;
            if self.lock_state().connect.is_finished() {
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
    }

    // 后台线程主体：全程持 control，与 `/mcp` 变更和退出停止串行。
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
                // 配置不可用不是启动失败：提示行给出脱敏原因，首个任务照常进行。
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
            // 普通连接故障只落到条目状态；取消与回收失败结束本轮并记诊断。
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
        // 取消或退出时未建连的目标恢复为可重试状态，`/mcp reconnect` 不会误判。
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

    // 调用方必须已持有 control。
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

    // 单服务连接：普通故障只落到条目状态，取消与清理失败才向上传播。
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

    // 连接可被调用方的 token 取消：每次 spawn 前和 discovery 全程检查；
    // 取消不自动重试，由调用方决定是否再次 connect。
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

    // 取消后的惰性重建：只有新任务真正调用该服务工具时执行一次。
    // 在 server 锁内完成 spawn 与 discovery，用调用方在 state 锁下取得的快照
    // 做定义完全一致校验，不重新获取 state。
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
                    // 重建被取消：停止新进程并保留资格，下一次新任务可再尝试。
                    // 停止失败升级为 typed 清理错误，不被固定文案吞掉。
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
        // 与快照完整比较：增删或 exposed/原始名/描述/参数任一变化都拒绝，
        // 且校验通过前不发送新 tools/call、不更新全局工具表。
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
            // spawn 前已检查取消，fixture 进程从未启动。
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
            // 启动不等握手：起线程与登记进度立即返回，工具表由后台补齐。
            assert!(started.elapsed() < Duration::from_secs(1));
            manager.await_connect(&CancellationToken::new()).unwrap();
            let snapshot = manager.connect_snapshot();
            assert!(snapshot.finished);
            assert_eq!(snapshot.servers.len(), 1);
            assert_eq!(snapshot.servers[0].name, "local");
            assert_eq!(snapshot.servers[0].phase, ConnectPhase::Connected);
            assert!(snapshot.servers[0].elapsed.is_some());
            let tip = snapshot.tip().expect("已配置服务必须有总状态");
            assert!(
                tip.starts_with("Connected to MCP servers: 1 succeeded ("),
                "{tip}"
            );
            assert!(!tip.contains("failed"), "{tip}");
            assert!(tip.ends_with(')'), "{tip}");
            // 首个任务前的屏障保证模型请求能看到 MCP 工具。
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
