//! Request table, cancellation/timeout, notifications and the write queue.

use anyhow::Result;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Instant;

use pp_core::cancellation::CancellationToken;

use super::{
    BODY_LIMIT, Failure, POLL, ServerRequest, Transport, UNAUTHORIZED_EDIT, decrement_active,
};

impl Transport {
    pub(crate) fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    pub(crate) fn close_reason(&self) -> String {
        self.shared.close_reason()
    }

    pub(crate) fn set_settings(&self, settings: Value) {
        *self
            .shared
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = settings;
    }

    pub(crate) fn set_folders(&self, folders: Vec<Value>) {
        *self
            .shared
            .folders
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = folders;
    }

    pub(crate) fn diagnostics_registered(&self) -> bool {
        self.shared.diagnostics_registered()
    }

    /// Mark one active operation; grants the server no file modification permission whatsoever.
    pub(crate) fn begin_operation(&self) {
        let _callbacks = self
            .shared
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.active.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn end_operation(&self) {
        // Enqueueing and ending an operation share the lock, so requests from an old authorization scope cannot be left for the next operation.
        let mut callbacks = self
            .shared
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if decrement_active(&self.shared.active) == Some(1) {
            for request in callbacks.drain(..) {
                request.respond(json!({"applied": false, "failureReason": UNAUTHORIZED_EDIT}));
            }
        }
    }

    /// Send a JSON-RPC notification (no response).
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), Failure> {
        self.write_message(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// Send a request and wait for its response, handling server callbacks meanwhile.
    pub(crate) fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: &CancellationToken,
        service: &mut dyn FnMut(ServerRequest),
    ) -> Result<Value, Failure> {
        // Precheck: closed/cancelled/expired all skip consuming an id, writing a frame or registering a pending entry.
        if self.is_closed() {
            return Err(Failure::Closed);
        }
        if cancellation.is_cancelled() {
            return Err(Failure::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(Failure::Timeout);
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = sync_channel(1);
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        if let Err(failure) = self.write_message(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })) {
            self.shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(failure);
        }
        loop {
            match rx.recv_timeout(POLL) {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(error)) => {
                    return if error.code == -32800 && self.is_closed() {
                        Err(Failure::Closed)
                    } else {
                        Err(Failure::Rpc(error.code))
                    };
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.shared
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    return Err(Failure::Closed);
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.service_callbacks(service);
            if cancellation.is_cancelled() {
                self.abandon(id);
                return Err(Failure::Cancelled);
            }
            if Instant::now() >= deadline {
                self.abandon(id);
                return Err(Failure::Timeout);
            }
            if self.is_closed() {
                self.shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                return Err(Failure::Closed);
            }
        }
    }

    /// Abandon an in-flight request: remove the pending entry first (a late response is dropped by the reader), then
    /// notify the server of the cancellation. Neither order may be reversed — a reader that already got the response no longer waits.
    fn abandon(&self, id: i64) {
        self.shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        let _ = self.write_message(json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": {"id": id},
        }));
    }

    /// Handle accumulated server callbacks; a callback failure does not interrupt the outer wait.
    pub(crate) fn service_callbacks(&self, service: &mut dyn FnMut(ServerRequest)) {
        loop {
            let next = {
                let mut queue = self
                    .shared
                    .callbacks
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                queue.pop_front()
            };
            let Some(request) = next else {
                return;
            };
            service(request);
        }
    }

    fn write_message(&self, message: Value) -> Result<(), Failure> {
        if self.is_closed() {
            return Err(Failure::Closed);
        }
        let body = serde_json::to_vec(&message).map_err(|_| Failure::Closed)?;
        if body.len() > BODY_LIMIT {
            return Err(Failure::Closed);
        }
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        let sender = {
            let guard = self.shared.writer.lock().unwrap_or_else(|e| e.into_inner());
            guard.clone()
        };
        let Some(sender) = sender else {
            return Err(Failure::Closed);
        };
        match sender.try_send(frame) {
            Ok(()) => Ok(()),
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(Failure::Closed),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                // A full write queue means stdin writes are blocked: treat it as a protocol failure, leaving it to the upper layer
                // to close the connection and confirm reaping instead of waiting forever.
                self.shared
                    .note_close("LSP server stopped reading requests");
                Err(Failure::Closed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    #[test]
    fn lsp_transport_request_preflight_skips_cancelled_and_expired_work() {
        let (shared, frames) = test_shared();
        let transport = transport_for(shared.clone(), empty_slot());
        let mut service = |_: ServerRequest| {};

        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            transport.request(
                "test/never",
                json!({}),
                Instant::now() + Duration::from_secs(5),
                &token,
                &mut service,
            ),
            Err(Failure::Cancelled)
        );
        assert!(shared.pending.lock().unwrap().is_empty());
        assert!(frames.try_recv().is_err());

        let token = CancellationToken::new();
        assert_eq!(
            transport.request(
                "test/never",
                json!({}),
                Instant::now() - Duration::from_millis(1),
                &token,
                &mut service,
            ),
            Err(Failure::Timeout)
        );
        assert!(shared.pending.lock().unwrap().is_empty());
        assert!(frames.try_recv().is_err());
        // A failed precheck does not consume a request id.
        assert_eq!(transport.next_id.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn lsp_transport_request_timeout_clears_pending_and_sends_cancel_request() {
        let (shared, frames) = test_shared();
        let transport = transport_for(shared.clone(), empty_slot());
        let token = CancellationToken::new();
        let mut service = |_: ServerRequest| {};
        let failure = transport
            .request(
                "test/slow",
                json!({"a": 1}),
                Instant::now() + Duration::from_millis(30),
                &token,
                &mut service,
            )
            .expect_err("request must time out");
        assert_eq!(failure, Failure::Timeout);
        assert!(shared.pending.lock().unwrap().is_empty());
        let request = read_body(&frames.recv().expect("request frame"));
        assert_eq!(request["method"], json!("test/slow"));
        assert_eq!(request["id"], json!(1));
        let cancel = read_body(&frames.recv().expect("cancel frame"));
        assert_eq!(cancel["method"], json!("$/cancelRequest"));
        assert_eq!(cancel["params"]["id"], json!(1));
    }

    #[test]
    fn lsp_transport_request_cancellation_clears_pending_and_sends_cancel_request() {
        let (shared, frames) = test_shared();
        let transport = transport_for(shared.clone(), empty_slot());
        let token = CancellationToken::new();
        let cancel = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            cancel.cancel();
        });
        let mut service = |_: ServerRequest| {};
        let failure = transport
            .request(
                "test/slow",
                json!({}),
                Instant::now() + Duration::from_secs(5),
                &token,
                &mut service,
            )
            .expect_err("request must be cancelled");
        canceller.join().expect("canceller thread");
        assert_eq!(failure, Failure::Cancelled);
        assert!(shared.pending.lock().unwrap().is_empty());
        let request = read_body(&frames.recv().expect("request frame"));
        assert_eq!(request["id"], json!(1));
        let cancel = read_body(&frames.recv().expect("cancel frame"));
        assert_eq!(cancel["method"], json!("$/cancelRequest"));
        assert_eq!(cancel["params"]["id"], json!(1));
    }

    #[test]
    fn lsp_transport_end_operation_saturates_at_zero() {
        let (shared, _frames) = test_shared();
        let transport = transport_for(shared.clone(), empty_slot());
        transport.begin_operation();
        transport.begin_operation();
        transport.end_operation();
        transport.end_operation();
        transport.end_operation();
        assert_eq!(shared.active.load(Ordering::SeqCst), 0);
    }
}
