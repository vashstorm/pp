//! Content-Length 帧、I/O 线程与受管理 stdio 进程。
//!
//! 每个语言服务器拥有一个专用 reader 线程（分发 response/通知/server
//! request）、一个 writer 线程与一个 stderr drain 线程。上层同步等待，不
//! 使用 async。reader 线程从不执行磁盘编辑，也不等待 UI：`workspace/applyEdit`
//! 在 reader 中立即回复拒绝，不等待模型或执行文件操作。

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

/// header 上限 8 KiB、body 上限 16 MiB：超限直接判定连接失效，不尝试按行容错。
pub(crate) const HEADER_LIMIT: usize = 8 * 1024;
pub(crate) const BODY_LIMIT: usize = 16 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;
const WRITE_QUEUE: usize = 64;
const POLL: Duration = Duration::from_millis(20);
/// 组内进程响应 TERM 的窗口；超时后升级为 KILL。
const TERM_GRACE: Duration = Duration::from_millis(1000);
/// KILL 后等待进程组彻底消失（含被 init/launchd 回收僵尸）的窗口。
const KILL_GRACE: Duration = Duration::from_millis(1000);
/// 未授权 server 自主编辑时的统一拒绝文案。
pub(crate) const UNAUTHORIZED_EDIT: &str =
    "LSP is an information source; server edits are never applied. Use edit/write/bash.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    Cancelled,
    /// 等待超出绝对 deadline。
    Timeout,
    /// 服务器响应了 JSON-RPC error。
    Rpc(i64),
    /// 传输已关闭、写阻塞或协议失效。
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

/// 操作服务接口使用的响应句柄。`workspace/applyEdit` 在 reader 中立即拒绝，
/// 不会经由该接口执行文件修改或等待模型决策。
pub(crate) struct ServerRequest {
    id: Value,
    shared: Arc<Shared>,
}

impl ServerRequest {
    pub(crate) fn respond(self, value: Value) {
        // 连接已关闭时写入必然是 best-effort；这里不把错误升级为工具失败。
        let _ = reply_message(&self.shared, &self.id, value);
    }
}

type NotificationSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

type Pending = Arc<Mutex<HashMap<i64, SyncSender<Result<Value, RpcError>>>>>;

struct Shared {
    /// 唯一的写入队列；writer 线程消费，reader 线程回复 server request 也用它。
    writer: Mutex<Option<SyncSender<Vec<u8>>>>,
    pending: Pending,
    callbacks: Mutex<VecDeque<ServerRequest>>,
    /// 当前等待 RPC 的操作数；applyEdit 不依赖此值，始终拒绝。
    active: AtomicUsize,
    closed: AtomicBool,
    /// 配置值，供 `workspace/configuration` 的 section 查询。
    settings: Mutex<Value>,
    folders: Mutex<Vec<Value>>,
    /// 动态注册的能力：registration id → method。按 id 记录，单个 unregister
    /// 只移除它自己的注册，不影响其他注册（包括同 method 的注册）。
    registrations: Mutex<HashMap<String, String>>,
    stderr: Mutex<Vec<u8>>,
    /// 传输关闭原因（脱敏），供错误信息使用。
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
        // 先取出再发送：发送失败可能再次进入 note_close，不能持锁重入。
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

/// 原子递减并返回递减前的值；计数已为 0 时不写入并返回 `None`。
/// `try_update` 超出 MSRV 1.88，`fetch_update` 已在其后更名弃用，
/// 所以用 CAS 循环手工完成这次“有值才减”的更新。
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
