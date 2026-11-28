//! Client types, startup handshake, and controlled/idle shutdown.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant};

use crate::config::ServerDefinition;
use crate::transport::{ProcessSlot, ServerRequest, Transport};
use crate::types::{PublishDiagnosticsParams, ServerCapabilities};
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
    /// Workspace generation at client creation; the registry uses it to drop stale clients.
    pub(crate) _generation: u64,
    pub(crate) _started: Instant,
    pub(crate) transport: Transport,
    pub(super) documents: Mutex<HashMap<PathBuf, DocumentState>>,
    pub(super) op_lock: Mutex<()>,
    /// Start/end time of the most recent operation; idle reaping computes from it, not from creation time.
    pub(super) last_activity: Mutex<Instant>,
    pub(super) folders: Mutex<Vec<PathBuf>>,
    /// A one-time ContentModified retry is allowed only after a workspace root was appended.
    pub(super) roots_added: AtomicBool,
}

/// Tracks the transport's active operations; applyEdit is always rejected in the transport layer.
/// The count is guaranteed to drop back even if the operation body panics.
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
    /// Spawn the process and complete the initialize handshake; the client is ready on return.
    ///
    /// `identity` is computed and stored by the registry, and later reload/concurrent starts use it to decide reuse;
    /// when the handshake fails the process is reaped first, and a reap failure must be returned together with the initialization error, never masked as success.
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
        // `workspace/configuration` and `workspace/workspaceFolders` may arrive during initialize or
        // immediately after `initialized`; installing the real configuration and initial workspace root before the handshake
        // keeps the server from seeing null/an empty list first. This only assigns shared state and sends no RPC.
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

    /// Controlled shutdown: shutdown RPC → exit → process-group reap.
    pub(crate) fn shutdown(&self) -> Result<()> {
        self.touch();
        // Serialize with active operations where possible, but reaping must not be abandoned because the lock is unavailable.
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let guard = self.acquire(deadline, &CancellationToken::new()).ok();
        let outcome = self.shutdown_transport();
        drop(guard);
        outcome
    }

    /// Idle reaping: acquire the operation lock non-blocking and close only once existing operations have really finished.
    ///
    /// Clients holding an active operation (including initialization) are always skipped; after the lock is taken, shutdown
    /// is completed internally without recursively acquiring the same operation lock. Returns whether the client was actually closed.
    pub(crate) fn shutdown_if_idle(&self, timeout: Duration) -> Result<bool> {
        let _guard = match self.op_lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return Ok(false),
        };
        if self.idle_for() < timeout {
            return Ok(false);
        }
        // Do not refresh last_activity: when closing fails, the next scan should retry immediately instead of waiting another idle window.
        self.shutdown_transport()?;
        Ok(true)
    }

    /// Shutdown path when the operation lock is already held.
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

    /// Full capabilities returned by the server's `initialize` (verbatim, including extension fields and
    /// `executeCommandProvider.commands`). Only explicit capability queries need the full JSON.
    pub(crate) fn capabilities_json(&self) -> Value {
        self.capabilities.raw.clone()
    }
}

#[cfg(test)]
mod integration {
    use super::super::fixtures::*;
    use super::*;
    use crate::config;
    use crate::uri::path_to_uri;
    use serde_json::json;
    use std::sync::mpsc;

    #[test]
    fn lsp_client_capabilities_top_level_and_raw_extensions() {
        let (dir, client) = fixture_client(&[]);
        // A real server's navigation/command capabilities live at the capabilities top level.
        assert!(client.capabilities.hover);
        assert!(client.capabilities.definition);
        assert!(client.capabilities.references);
        assert!(client.capabilities.rename);
        assert!(client.capabilities.workspace_symbol);
        assert!(client.capabilities.execute_command);
        assert!(client.capabilities.diagnostic_provider.is_some());
        assert!(client.capabilities.workspace_folders_change_notifications);
        assert!(client.capabilities.document_formatting);

        // Raw capabilities preserve the server's extensions and command list verbatim without rebuilding a subset.
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

        // Consumer path: the top-level hover declaration corresponds to a real usable request.
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
