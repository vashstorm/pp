//! 单次高层操作的上下文与操作锁。

use anyhow::{Result, bail};
use serde_json::Value;
use std::sync::{MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::lsp::transport::{Failure, ServerRequest};
use pp_core::cancellation::CancellationToken;

use super::handshake::{convert, convert_unit};
use super::lifecycle::ActiveOperation;
use super::{Client, POLL, PROJECT_LOAD_TIMEOUT};

/// 一次高层操作的上下文：绝对 deadline、取消令牌与 server 回调处理。
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

    /// 原始请求：保留失败分类，供 method-not-found 之类的分支使用。
    pub(crate) fn request_raw(&mut self, method: &str, params: Value) -> Result<Value, Failure> {
        self.client.transport.request(
            method,
            params,
            self.deadline,
            self.cancellation,
            self.service,
        )
    }

    /// 常规请求：把失败分类转换为工具错误。追加过工作区根后，服务器可能用
    /// ContentModified 取消正在重新索引的请求；此时等待项目加载后只重试一次，
    /// 且不越过原 deadline。
    pub(crate) fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        match self.request_raw(method, params.clone()) {
            Err(Failure::Rpc(-32801)) if self.client.roots_added() => {
                let limit = self.deadline.min(Instant::now() + PROJECT_LOAD_TIMEOUT);
                let _ = self
                    .client
                    .signals
                    .wait_project_load(limit, self.cancellation);
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
    /// 串行执行一次高层操作；等待期间处理 server request。
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

    /// 记录一次活动；空闲回收只依据操作真正开始/结束的时间。
    pub(super) fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// 最近一次活动距今的时间。
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
