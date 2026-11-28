//! Content-Length framing, I/O threads and managed stdio processes.
//!
//! Each language server owns a dedicated reader thread (dispatching responses/notifications/server
//! requests), a writer thread and a stderr drain thread. Upper layers wait synchronously and do not
//! use async. The reader thread never performs disk edits and never waits on the UI: `workspace/applyEdit`
//! is rejected immediately in the reader, without waiting for the model or performing any file operation.

mod dispatch;
mod io;
mod process;
mod rpc;

use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use self::io::reply_message;

pub(crate) use self::process::terminate_slot;

/// header limit 8 KiB, body limit 16 MiB: exceeding either fails the connection outright; no line-by-line tolerance.
pub(crate) const HEADER_LIMIT: usize = 8 * 1024;
pub(crate) const BODY_LIMIT: usize = 16 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const WRITE_QUEUE: usize = 64;
const POLL: Duration = Duration::from_millis(20);
/// Window for processes in the group to answer TERM; escalates to KILL on timeout.
const TERM_GRACE: Duration = Duration::from_millis(1000);
/// Window after KILL to wait until the process group is completely gone (including zombies reaped by init/launchd).
const KILL_GRACE: Duration = Duration::from_millis(1000);
/// Uniform rejection text for unauthorized server-initiated edits.
pub(crate) const UNAUTHORIZED_EDIT: &str =
    "LSP is an information source; server edits are never applied. Use edit/write/bash.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Cancelled,
    /// The wait exceeded the absolute deadline.
    Timeout,
    /// The server answered with a JSON-RPC error.
    Rpc(i64),
    /// The transport is closed, writes are blocked, or the protocol failed.
    Closed,
}

#[derive(Debug)]
pub(crate) struct RpcError {
    pub(crate) code: i64,
    pub(crate) message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "LSP error {}: {}",
            self.code,
            pp_core::logging::diagnostic_text(&self.message)
        )
    }
}

/// Response handle used by the operation service interface. `workspace/applyEdit` is rejected immediately in the reader
/// and never performs file modifications or waits for model decisions through this interface.
pub(crate) struct ServerRequest {
    id: Value,
    shared: Arc<Shared>,
}

impl ServerRequest {
    pub(crate) fn respond(self, value: Value) {
        // Writes on a closed connection are necessarily best-effort; the error is not escalated to a tool failure here.
        let _ = reply_message(&self.shared, &self.id, value);
    }
}

type NotificationSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

type Pending = Arc<Mutex<HashMap<i64, SyncSender<Result<Value, RpcError>>>>>;

struct Shared {
    /// The single write queue; consumed by the writer thread, and also used by the reader thread to answer server requests.
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    pending: Pending,
    callbacks: Mutex<VecDeque<ServerRequest>>,
    /// Number of operations currently waiting on an RPC; applyEdit does not depend on it and is always rejected.
    active: AtomicUsize,
    closed: AtomicBool,
    /// Configuration values, for `workspace/configuration` section queries.
    settings: Mutex<Value>,
    folders: Mutex<Vec<Value>>,
    /// Dynamically registered capabilities: registration id → method. Keyed by id, so a single unregister
    /// removes only its own registration and leaves others (including ones with the same method) untouched.
    registrations: Mutex<HashMap<String, String>>,
    stderr: Mutex<Vec<u8>>,
    /// Redacted transport close reason, used in error messages.
    reason: Mutex<Option<String>>,
}

impl Shared {
    fn note_close(&self, reason: &str) {
        {
            let mut slot = self.reason.lock().unwrap_or_else(|e| e.into_inner());
            if slot.is_none() {
                *slot = Some(reason.to_owned());
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        // Take the senders out before sending: a failed send may re-enter note_close, so the lock must not be re-entered.
        let pending: Vec<SyncSender<Result<Value, RpcError>>> = {
            let mut guard = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain().map(|(_, sender)| sender).collect()
        };
        for sender in pending {
            let _ = sender.send(Err(RpcError {
                code: -32800,
                message: reason.to_owned(),
            }));
        }
        let callbacks: Vec<ServerRequest> = {
            let mut guard = self.callbacks.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain(..).collect()
        };
        for request in callbacks {
            request.respond(json!({"applied": false, "failureReason": UNAUTHORIZED_EDIT}));
        }
    }

    fn close_reason(&self) -> String {
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| "LSP transport was closed".to_owned())
    }

    fn register_capabilities(&self, registrations: &[Value]) {
        let mut stored = self.registrations.lock().unwrap_or_else(|e| e.into_inner());
        for registration in registrations {
            let (Some(id), Some(method)) = (
                registration.get("id").and_then(Value::as_str),
                registration.get("method").and_then(Value::as_str),
            ) else {
                continue;
            };
            stored.insert(id.to_owned(), method.to_owned());
        }
    }

    fn unregister_capabilities(&self, unregistrations: &[Value]) {
        let mut stored = self.registrations.lock().unwrap_or_else(|e| e.into_inner());
        for registration in unregistrations {
            let Some(id) = registration.get("id").and_then(Value::as_str) else {
                continue;
            };
            stored.remove(id);
        }
    }

    fn diagnostics_registered(&self) -> bool {
        self.registrations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|method| {
                matches!(
                    method.as_str(),
                    "textDocument/diagnostic" | "workspace/diagnostic"
                )
            })
    }
}

pub(crate) struct ProcessSlot {
    pub(crate) child: Option<Child>,
    pub(crate) closing: bool,
}

pub(crate) struct Transport {
    pub(crate) process: Arc<Mutex<ProcessSlot>>,
    shared: Arc<Shared>,
    next_id: AtomicI64,
}

/// Atomically decrement and return the pre-decrement value; when the count is already 0 nothing is written and `None` is returned.
/// `try_update` is beyond MSRV 1.88 and `fetch_update` was renamed and deprecated after it,
/// so a CAS loop performs this "decrement only if present" update by hand.
fn decrement_active(active: &AtomicUsize) -> Option<usize> {
    let mut expected = active.load(Ordering::SeqCst);
    loop {
        let next = expected.checked_sub(1)?;
        match active.compare_exchange_weak(expected, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Some(expected),
            Err(observed) => expected = observed,
        }
    }
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize};
    use std::sync::mpsc::{Receiver, sync_channel};

    pub(super) fn no_sink() -> NotificationSink {
        Arc::new(|_, _| {})
    }

    pub(super) fn frame(body: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(body).expect("serialize frame body");
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        frame
    }

    pub(super) fn read_body(frame: &[u8]) -> Value {
        let split = frame
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("frame separator")
            + 4;
        serde_json::from_slice(&frame[split..]).expect("frame body")
    }

    pub(super) fn test_shared() -> (Arc<Shared>, Receiver<Vec<u8>>) {
        let (tx, rx) = sync_channel(WRITE_QUEUE);
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(tx)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            callbacks: Mutex::new(VecDeque::new()),
            active: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            settings: Mutex::new(Value::Null),
            folders: Mutex::new(Vec::new()),
            registrations: Mutex::new(HashMap::new()),
            stderr: Mutex::new(Vec::new()),
            reason: Mutex::new(None),
        });
        (shared, rx)
    }

    pub(super) fn empty_slot() -> Arc<Mutex<ProcessSlot>> {
        Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: false,
        }))
    }

    pub(super) fn transport_for(
        shared: Arc<Shared>,
        process: Arc<Mutex<ProcessSlot>>,
    ) -> Transport {
        Transport {
            process,
            shared,
            next_id: AtomicI64::new(1),
        }
    }
}
