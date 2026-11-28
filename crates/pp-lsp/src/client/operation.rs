//! Context and operation lock for a single high-level operation.

use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::{MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::transport::{Failure, ServerRequest};
use pp_core::cancellation::CancellationToken;

use super::handshake::{convert, convert_unit};
use super::lifecycle::ActiveOperation;
use super::{Client, POLL, PROJECT_LOAD_TIMEOUT};

/// Context of one high-level operation: absolute deadline, cancellation token and server-request handling.
pub(crate) struct Op<'a> {
    pub(super) client: &'a Client,
    pub(super) deadline: Instant,
    pub(super) cancellation: &'a CancellationToken,
    pub(super) service: &'a mut dyn FnMut(ServerRequest),
}

impl<'a> Op<'a> {
    #[cfg(test)]
    pub(crate) fn deadline(&self) -> Instant {
        self.deadline
    }

    #[cfg(test)]
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        self.cancellation
    }

    /// Raw request: keeps the failure classification for branches such as method-not-found.
    pub(crate) fn request_raw(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        self.client.transport.request(
            method,
            params,
            self.deadline,
            self.cancellation,
            self.service,
        )
    }

    /// Regular request: converts the failure classification into a tool error. After a workspace root was appended, the server may
    /// cancel a re-indexing request with ContentModified; then wait for the project load and retry exactly once,
    /// without exceeding the original deadline.
    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        match self.request_raw(method, params.clone()) {
            Err(Failure::Rpc(-32801)) if self.client.roots_added() => {
                let limit = self.deadline.min(Instant::now() + PROJECT_LOAD_TIMEOUT);
                let _ =
                    self.client
                        .signals
                        .wait_project_load_with(limit, self.cancellation, || {
                            self.client.is_closed()
                        });
                if self.cancellation.is_cancelled() {
                    return Err(pp_core::cancellation::Cancelled.into());
                }
                match self.request_raw(method, params) {
                    Err(Failure::Cancelled) => Err(pp_core::cancellation::Cancelled.into()),
                    other => convert(other, method),
                }
            }
            other => convert(other, method),
        }
    }

    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<()> {
        convert_unit(self.client.transport.notify(method, params), method)
    }
}

impl Client {
    /// Run one high-level operation serially; server requests are handled while waiting.
    pub(crate) fn operation<T>(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
        service: &mut dyn FnMut(ServerRequest),
        body: impl FnOnce(&mut Op<'_>) -> Result<T>,
    ) -> Result<T> {
        let guard = self.acquire(deadline, cancellation)?;
        let _guard = guard;
        self.touch();
        let outcome = {
            let _active = ActiveOperation::new(&self.transport);
            let mut op = Op {
                client: self,
                deadline,
                cancellation,
                service,
            };
            body(&mut op)
        };
        self.touch();
        outcome
    }

    /// Record one activity; idle reaping relies only on when operations really start/end.
    pub(super) fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// Time since the most recent activity.
    pub(super) fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    pub(super) fn acquire(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<MutexGuard<'_, ()>> {
        loop {
            match self.op_lock.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    if cancellation.is_cancelled() {
                        return Err(pp_core::cancellation::Cancelled.into());
                    }
                    if Instant::now() >= deadline {
                        bail!(
                            "LSP operation on {} timed out waiting for the previous operation",
                            self.name
                        );
                    }
                    std::thread::sleep(POLL);
                }
            }
        }
    }
}
