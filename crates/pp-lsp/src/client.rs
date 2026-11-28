//! Managed client for one language server: initialization handshake, document state, diagnostic cache and operation serialization.

mod diagnostics;
mod document;
mod handshake;
mod lifecycle;
mod operation;
mod save;
mod workspace;

use std::time::Duration;

pub(crate) use document::read_document_text;
pub(crate) use lifecycle::Client;
pub(crate) use operation::Op;

pub(super) const POLL: Duration = Duration::from_millis(20);
/// Settle window for unversioned reports.
pub(super) const SETTLE: Duration = Duration::from_millis(250);
/// After a cold start/reopen, an unversioned empty report is not considered clean within this window.
pub(super) const COLD_WINDOW: Duration = Duration::from_secs(12);
pub(super) const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const DEFAULT_READY_POLL: Duration = Duration::from_millis(100);
pub(super) const DEFAULT_READY_SETTLE: Duration = Duration::from_secs(2);
pub(super) const DEFAULT_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const PROJECT_LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound for waiting on the normal shutdown RPC.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Grace period after shutdown for the process to exit on its own.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// Grace period for reaping the process after a failed initialize.
const HANDSHAKE_GRACE: Duration = Duration::from_millis(500);
/// Budget for the best-effort notification when abandoning a pending-write overlay; decoupled from caller cancellation and an expired deadline.
#[cfg(test)]
const ABORT_BUDGET: Duration = Duration::from_secs(2);

#[cfg(test)]
mod fixtures {
    use super::*;
    use crate::config;
    use crate::transport::{ProcessSlot, ServerRequest};
    use anyhow::Result;
    use pp_core::cancellation::CancellationToken;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    use super::handshake::unauthorized_edit;

    /// Start an isolated client on the controlled stdio fixture; it reads no real HOME and bypasses the registry.
    pub(super) fn fixture_client(pull: &[&str]) -> (TempDir, Arc<Client>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".pp")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.path().join("requests.jsonl");
        let mut args = vec![
            script.to_string_lossy().into_owned(),
            "--log".to_owned(),
            log.to_string_lossy().into_owned(),
        ];
        args.extend(pull.iter().map(|value| (*value).to_owned()));
        std::fs::write(
            dir.path().join(".pp/lsp.json"),
            json!({"servers": {"fixture": {
                "command": "python3",
                "args": args,
                "fileTypes": [".fixture"],
                "rootMarkers": [".pp"],
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
        (dir, client)
    }

    pub(super) fn logged(dir: &TempDir) -> Vec<Value> {
        std::fs::read_to_string(dir.path().join("requests.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// The server logs into its own process; poll with a bound until the target event has actually been handled.
    pub(super) fn wait_logged(
        dir: &TempDir,
        deadline: Instant,
        mut ready: impl FnMut(&[Value]) -> bool,
    ) -> Vec<Value> {
        loop {
            let events = logged(dir);
            if ready(&events) || Instant::now() >= deadline {
                return events;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn with_client<T>(
        client: &Arc<Client>,
        body: impl FnOnce(&mut Op<'_>) -> Result<T>,
    ) -> T {
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        client
            .operation(
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
                &mut service,
                body,
            )
            .unwrap()
    }
}
