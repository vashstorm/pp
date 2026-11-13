//! 客户端类型、启动握手与受控／空闲退出。

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::lsp::config::ServerDefinition;
use crate::lsp::transport::{ProcessSlot, ServerRequest, Transport};
use crate::lsp::types::{PublishDiagnosticsParams, ServerCapabilities};
use pp_core::cancellation::CancellationToken;

use super::diagnostics::{Diagnostics, Signals};
use super::document::DocumentState;
use super::handshake::{initialize_handshake, startup_failure, unauthorized_edit};
use super::workspace::folder_json;
use super::{EXIT_GRACE, HANDSHAKE_GRACE, SHUTDOWN_TIMEOUT};

pub(crate) struct Client {
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) identity: String,
    pub(crate) command: PathBuf,
    pub(crate) definition: ServerDefinition,
    pub(crate) capabilities: ServerCapabilities,
    pub(crate) diagnostics: Arc<Diagnostics>,
    pub(crate) signals: Arc<Signals>,
    /// 客户端创建时的工作区代数；registry 用它丢弃过期客户端。
    pub(crate) _generation: u64,
    pub(crate) _started: Instant,
    pub(super) transport: Transport,
    pub(super) documents: Mutex<HashMap<PathBuf, DocumentState>>,
    pub(super) op_lock: Mutex<()>,
    /// 最近一次操作开始/结束的时间；空闲回收按它计算而不是按创建时间。
    pub(super) last_activity: Mutex<Instant>,
    pub(super) folders: Mutex<Vec<PathBuf>>,
    /// 追加过工作区根之后才允许 ContentModified 单次重试。
    pub(super) roots_added: AtomicBool,
}

/// 跟踪 transport 的活动操作；applyEdit 始终在传输层拒绝。
/// 即使操作体 panic 也保证计数回落。
pub(super) struct ActiveOperation<'a> {
    transport: &'a Transport,
}

impl<'a> ActiveOperation<'a> {
    pub(super) fn new(transport: &'a Transport) -> Self {
        transport.begin_operation();
        Self { transport }
    }
}

impl Drop for ActiveOperation<'_> {
    fn drop(&mut self) {
        self.transport.end_operation();
    }
}

impl Client {
    /// 启动进程并完成 initialize 握手；返回时客户端已 ready。
    ///
    /// `identity` 由 registry 计算并保存，之后 reload/并发启动都以它判断能否复用；
    /// 握手失败时先回收进程，回收失败必须随初始化错误一起返回，不能伪装成功。
    pub(crate) fn start(
        definition: &ServerDefinition,
        cwd: &Path,
        identity: String,
        generation: u64,
        deadline: Instant,
        cancellation: &CancellationToken,
        process: Arc<Mutex<ProcessSlot>>,
    ) -> Result<Arc<Self>> {
        let command = definition
            .resolved_command
            .clone()
            .context("language server executable was not resolved")?;
        let diagnostics = Arc::new(Diagnostics::default());
        let signals = Arc::new(Signals::default());
        let sink = {
            let diagnostics = diagnostics.clone();
            let signals = signals.clone();
            Arc::new(move |method: &str, params: Value| match method {
                "textDocument/publishDiagnostics" => {
                    if let Ok(parsed) = serde_json::from_value::<PublishDiagnosticsParams>(params) {
                        let version = parsed.version.flatten();
                        diagnostics.publish(&parsed.uri, version, parsed.diagnostics);
                    }
                }
                "$/progress" => signals.apply_progress(&params),
                "workspace/diagnostic/refresh" => diagnostics.refresh(),
                "window/logMessage" | "window/showMessage" => {
                    let text = params
                        .get("message")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    tracing::debug!(
                        event = "lsp_server_message",
                        message = pp_core::logging::diagnostic_text(text)
                    );
                }
                _ => {}
            })
        };
        let transport = Transport::spawn(&command, &definition.spawn_args(), cwd, process, sink)?;
        // `workspace/configuration` 与 `workspace/workspaceFolders` 可能在 initialize
        // 期间或 `initialized` 之后立刻到达；握手之前就放入真实配置与初始工作区根，
        // 服务器不会先看到 null/空列表。这里只是共享状态赋值，不发任何 RPC。
        transport.set_settings(definition.settings.clone());
        transport.set_folders(vec![folder_json(cwd)]);
        let capabilities =
            match initialize_handshake(&transport, definition, cwd, deadline, cancellation) {
                Ok(capabilities) => capabilities,
                Err(error) => {
                    return Err(startup_failure(error, transport.terminate(HANDSHAKE_GRACE)));
                }
            };
        let now = Instant::now();
        let client = Arc::new(Self {
            name: definition.name.clone(),
            root: cwd.to_path_buf(),
            identity,
            command,
            definition: definition.clone(),
            capabilities,
            diagnostics,
            signals,
            _generation: generation,
            _started: now,
            transport,
            documents: Mutex::new(HashMap::new()),
            op_lock: Mutex::new(()),
            last_activity: Mutex::new(now),
            folders: Mutex::new(vec![cwd.to_path_buf()]),
            roots_added: AtomicBool::new(false),
        });
        client.transport.set_folders(client.folders_json());
        Ok(client)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.transport.is_closed()
    }

    pub(crate) fn close_reason(&self) -> String {
        self.transport.close_reason()
    }

    pub(crate) fn roots_added(&self) -> bool {
        self.roots_added.load(Ordering::SeqCst)
    }

    /// 受控退出：shutdown RPC → exit → 进程组回收。
    pub(crate) fn shutdown(&self) -> Result<()> {
        self.touch();
        // 尽量与活动操作串行，但回收不能因为等不到锁而放弃。
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let guard = self.acquire(deadline, &CancellationToken::new()).ok();
        let outcome = self.shutdown_transport();
        drop(guard);
        outcome
    }

    /// 空闲回收：非阻塞地取得操作锁，确认已有操作真正结束后才关闭。
    ///
    /// 持有活动操作（包括初始化）的客户端一律跳过；取到锁之后也在内部
    /// 完成 shutdown，不再递归获取同一把操作锁。返回是否真的关闭了客户端。
    pub(crate) fn shutdown_if_idle(&self, timeout: Duration) -> Result<bool> {
        let _guard = match self.op_lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(false),
        };
        if self.idle_for() < timeout {
            return Ok(false);
        }
        // 不刷新 last_activity：关闭失败时下一次扫描应立即重试，而不是再等一个空闲窗口。
        self.shutdown_transport()?;
        Ok(true)
    }

    /// 已经持有操作锁时的关闭路径。
    fn shutdown_transport(&self) -> Result<()> {
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let cancellation = CancellationToken::new();
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        let _ = self.transport.request(
            "shutdown",
            Value::Null,
            deadline,
            &cancellation,
            &mut service,
        );
        let _ = self.transport.notify("exit", Value::Null);
        self.transport
            .terminate(EXIT_GRACE)
            .map_err(|error| pp_core::cancellation::CleanupError(error).into())
    }

    /// 服务器 `initialize` 返回的完整 capabilities（原样，含扩展字段与
    /// `executeCommandProvider.commands`）。显式能力查询才需要完整 JSON。
    pub(crate) fn capabilities_json(&self) -> Value {
        self.capabilities.raw.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::lsp::config;
    use crate::lsp::uri::path_to_uri;
    use serde_json::json;
    use std::sync::mpsc;

    #[test]
    fn lsp_client_capabilities_top_level_and_raw_extensions() {
        let (dir, client) = fixture_client(&[]);
        // 真实服务器的导航/命令能力都在 capabilities 顶层。
        assert!(client.capabilities.hover);
        assert!(client.capabilities.definition);
        assert!(client.capabilities.references);
        assert!(client.capabilities.rename);
        assert!(client.capabilities.workspace_symbol);
        assert!(client.capabilities.execute_command);
        assert!(client.capabilities.diagnostic_provider.is_some());
        assert!(client.capabilities.workspace_folders_change_notifications);
        assert!(client.capabilities.document_formatting);

        // 原始 capabilities 原样保留服务器扩展与命令列表，不重建子集。
        let raw = client.capabilities_json();
        assert_eq!(raw["experimental"]["fixtureExtension"]["value"], json!(42));
        assert_eq!(
            raw["executeCommandProvider"]["commands"],
            json!(["fixture.reloadProjects"])
        );
        assert_eq!(raw["hoverProvider"], json!(true));
        assert!(
            raw.get("textDocument").is_none(),
            "raw capabilities must be the server's own object"
        );

        // 消费者路径：顶层 hover 声明对应真实可用的请求。
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "value\n").unwrap();
        let hover = with_client(&client, |op| {
            op.request(
                "textDocument/hover",
                json!({
                    "textDocument": {"uri": path_to_uri(&path)},
                    "position": {"line": 0, "character": 0},
                }),
            )
        });
        assert_eq!(hover["contents"]["value"], "fixture hover");
        let definition = with_client(&client, |op| {
            op.request(
                "textDocument/definition",
                json!({
                    "textDocument": {"uri": path_to_uri(&path)},
                    "position": {"line": 0, "character": 0},
                }),
            )
        });
        assert_eq!(definition[0]["uri"], json!(path_to_uri(&path)));
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_idle_skips_active_operation() {
        let (_dir, client) = fixture_client(&[]);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let worker = {
            let client = client.clone();
            std::thread::spawn(move || {
                let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
                client
                    .operation(
                        Instant::now() + Duration::from_secs(5),
                        &CancellationToken::new(),
                        &mut service,
                        |_op| {
                            entered_tx.send(()).unwrap();
                            release_rx.recv().unwrap();
                            Ok(())
                        },
                    )
                    .unwrap();
            })
        };
        entered_rx.recv().unwrap();
        assert!(
            !client.shutdown_if_idle(Duration::ZERO).unwrap(),
            "idle reaping must skip a client with an active operation"
        );
        assert!(!client.is_closed());
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(client.shutdown_if_idle(Duration::from_nanos(1)).unwrap());
        assert!(client.is_closed());
    }

    #[test]
    fn lsp_client_initialized_probe_sees_settings_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".pp")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.path().join("requests.jsonl");
        std::fs::write(
            dir.path().join(".pp/lsp.json"),
            json!({"servers": {"fixture": {
                "command": "python3",
                "args": [
                    script.to_string_lossy().into_owned(),
                    "--log".to_owned(),
                    log.to_string_lossy().into_owned(),
                    "--probe-initialized".to_owned(),
                ],
                "fileTypes": [".fixture"],
                "rootMarkers": [".pp"],
                "settings": {"fixture": {"nested": 7}},
            }}})
            .to_string(),
        )
        .unwrap();
        let config = config::Config::load_with(dir.path(), None).unwrap();
        let definition = config.server("fixture").unwrap().clone();
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: false,
        }));
        let client = Client::start(
            &definition,
            dir.path(),
            "fixture".to_owned(),
            1,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            process,
        )
        .unwrap();
        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            ["probe-configuration", "probe-folders"].iter().all(|id| {
                events
                    .iter()
                    .any(|event| event["id"] == json!(id) && event.get("result").is_some())
            })
        });
        let configuration = events
            .iter()
            .find(|event| {
                event["id"] == json!("probe-configuration") && event.get("result").is_some()
            })
            .unwrap();
        assert_eq!(
            configuration["result"],
            json!([{"nested": 7}, 7, null]),
            "workspace/configuration must see the real definition settings"
        );
        let folders = events
            .iter()
            .find(|event| event["id"] == json!("probe-folders") && event.get("result").is_some())
            .unwrap();
        let folders = folders["result"].as_array().unwrap();
        assert_eq!(folders.len(), 1, "the initial cwd folder must be visible");
        assert_eq!(
            folders[0]["uri"],
            json!(path_to_uri(dir.path())),
            "workspace/workspaceFolders must see the initial root"
        );
        client.shutdown().unwrap();
    }
}
