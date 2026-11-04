//! Content-Length 帧、I/O 线程与受管理 stdio 进程。
//!
//! 每个语言服务器拥有一个专用 reader 线程（分发 response/通知/server
//! request）、一个 writer 线程与一个 stderr drain 线程。上层同步等待，不
//! 使用 async。reader 线程从不执行磁盘编辑，也不等待 UI：`workspace/applyEdit`
//! 在 reader 中立即回复拒绝，不等待模型或执行文件操作。

use anyhow::{Context, Result, bail};
use rustix::io::Errno;
use rustix::process::{
    Pid, Signal, WaitId, WaitIdOptions, kill_process_group, test_kill_process_group, waitid,
};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pp_core::cancellation::CancellationToken;

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

impl Transport {
    /// 启动语言服务器进程；stderr 只保留末尾 64 KiB。
    pub(crate) fn spawn(
        command: &Path,
        args: &[String],
        cwd: &Path,
        process: Arc<Mutex<ProcessSlot>>,
        sink: NotificationSink,
    ) -> Result<Self> {
        {
            let slot = process.lock().unwrap_or_else(|e| e.into_inner());
            anyhow::ensure!(!slot.closing, "LSP client is shutting down");
            anyhow::ensure!(
                slot.child.is_none(),
                "LSP server process has not been reaped"
            );
        }
        let child = Command::new(command)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting LSP server {}", command.display()))?;
        // 子进程一产生就登记：后续任何取管道/起线程失败都留下可重试的 slot，
        // 而不是把未回收的进程丢给调用方。
        {
            let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
            if slot.closing || slot.child.is_some() {
                drop(slot);
                // 未登记成功的进程由本线程自行回收，不进入 registry 的 slot。
                let orphan = Arc::new(Mutex::new(ProcessSlot {
                    child: Some(child),
                    closing: true,
                }));
                terminate_slot(&orphan, Duration::ZERO)
                    .map_err(pp_core::cancellation::CleanupError)?;
                bail!("LSP client is shutting down");
            }
            slot.child = Some(child);
        }
        let (stdin, stdout, stderr) = {
            let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
            let child = slot
                .child
                .as_mut()
                .context("LSP server process was not registered")?;
            (
                child.stdin.take().context("LSP server stdin unavailable")?,
                child
                    .stdout
                    .take()
                    .context("LSP server stdout unavailable")?,
                child
                    .stderr
                    .take()
                    .context("LSP server stderr unavailable")?,
            )
        };

        let (write_tx, write_rx) = sync_channel::<Vec<u8>>(WRITE_QUEUE);
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(write_tx)),
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

        let thread_shared = shared.clone();
        let threads = (move || -> Result<()> {
            let writer_shared = thread_shared.clone();
            std::thread::Builder::new()
                .name("pp-lsp-writer".into())
                .spawn(move || writer_loop(stdin, write_rx, writer_shared))
                .context("starting LSP writer thread")?;

            let reader_shared = thread_shared.clone();
            std::thread::Builder::new()
                .name("pp-lsp-reader".into())
                .spawn(move || reader_loop(stdout, reader_shared, sink))
                .context("starting LSP reader thread")?;

            let stderr_shared = thread_shared.clone();
            std::thread::Builder::new()
                .name("pp-lsp-stderr".into())
                .spawn(move || drain_stderr(stderr, stderr_shared))
                .context("starting LSP stderr thread")?;
            Ok(())
        })();
        if let Err(error) = threads {
            // 起线程失败时也要让 writer 看到队列断开，否则已启动的 writer
            // 线程会永远阻塞在 recv 上；进程仍保留在 slot 里供重试回收。
            *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
            let _ = terminate_slot(&process, Duration::ZERO);
            return Err(error);
        }

        // 起线程期间清理路径可能已设置 closing 并回收进程；此时不能返回一个
        // 声称可用的 transport。
        let closing = {
            let slot = process.lock().unwrap_or_else(|e| e.into_inner());
            slot.closing
        };
        if closing {
            *shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
            terminate_slot(&process, Duration::ZERO)
                .map_err(pp_core::cancellation::CleanupError)?;
            shared.note_close("LSP client is shutting down");
            bail!("LSP client is shutting down");
        }

        Ok(Self {
            process,
            shared,
            next_id: AtomicI64::new(1),
        })
    }

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

    /// 标记一次活动操作；不授予服务器任何文件修改权限。
    pub(crate) fn begin_operation(&self) {
        let _callbacks = self
            .shared
            .callbacks
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.active.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn end_operation(&self) {
        // 入队与操作结束共用锁，旧授权范围的请求不能留给下一次操作。
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

    /// 发送一条 JSON-RPC 通知（无响应）。
    pub(crate) fn notify(&self, method: &str, params: Value) -> Result<(), Failure> {
        self.write_message(json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    /// 发送请求并等待响应，期间处理 server 回调。
    pub(crate) fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancellation: &CancellationToken,
        service: &mut dyn FnMut(ServerRequest),
    ) -> Result<Value, Failure> {
        // 预检：已关闭/已取消/已过期都不必再占用 id、写帧或登记 pending。
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

    /// 放弃一个在途请求：先移除 pending（迟到的响应会被 reader 丢弃），再
    /// 通知服务器取消。两者都不能反过来——先拿到响应的 reader 不会再等待。
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

    /// 处理累积的 server 回调；回调失败不打断外层等待。
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
                // 写队列满说明 stdin 写入被阻塞：按协议失效处理，交给上层
                // 关闭连接并确认回收，而不是无限等待。
                self.shared
                    .note_close("LSP server stopped reading requests");
                Err(Failure::Closed)
            }
        }
    }

    /// 关闭 stdin 并等待进程/进程组退出；未确认回收时保留 slot 并报错。
    pub(crate) fn terminate(&self, grace: Duration) -> Result<()> {
        // 先断开写队列：writer 线程退出并关闭 stdin，服务器有机会自行退出。
        *self.shared.writer.lock().unwrap_or_else(|e| e.into_inner()) = None;
        terminate_slot(&self.process, grace)?;
        self.shared.note_close("LSP server was terminated");
        Ok(())
    }
}

/// 回收一个服务器进程 slot：root 退出不代表后代关闭了管道，必须确认整个
/// 进程组消失。registry 在初始化过程中取消/关闭时也用它，因此不依赖
/// `Transport`（shared 尚未可用）。
///
/// 失败时保留 `slot.child`，调用方可以重试；绝不把未确认的回收报告为成功。
pub(crate) fn terminate_slot(process: &Arc<Mutex<ProcessSlot>>, grace: Duration) -> Result<()> {
    let mut slot = process.lock().unwrap_or_else(|e| e.into_inner());
    slot.closing = true;
    let Some(child) = slot.child.as_mut() else {
        return Ok(());
    };
    let group = child.id();
    // 宽限期内先等 root 自己退出（stdin 已关闭时通常会成功）。
    let _ = wait_child(child, grace)?;
    // root 退出不代表后代关闭了管道：只要进程组仍在就发信号回收。
    // 先探测“还在”（EPERM 也算还在）再决定是否发信号，避免对已消失的
    // pgid 发送信号（pgid 可能已被复用）。
    if group_present(group)? {
        signal_group(group, Signal::TERM)?;
        let _ = wait_group(child, group, TERM_GRACE)?;
    }
    if !group_settled(group)? {
        signal_group(group, Signal::KILL)?;
        let _ = wait_group(child, group, KILL_GRACE)?;
    }
    if !group_settled(group)? {
        bail!("LSP server process group {group} did not exit");
    }
    // 组已消失/只剩僵尸意味着 root 也已退出；最后确认并回收，避免留下
    // 僵尸进程。
    if !wait_child(child, KILL_GRACE)? {
        bail!("LSP server process {group} did not exit");
    }
    slot.child = None;
    Ok(())
}

/// 在窗口内等待 root 退出；返回是否已回收。
fn wait_child(child: &mut Child, window: Duration) -> Result<bool> {
    let deadline = Instant::now() + window;
    loop {
        if child
            .try_wait()
            .context("checking LSP server process")?
            .is_some()
        {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

/// 在窗口内等待整个进程组落定，同时持续回收 root。
fn wait_group(child: &mut Child, group: u32, window: Duration) -> Result<bool> {
    let deadline = Instant::now() + window;
    loop {
        let _ = child.try_wait().context("checking LSP server process")?;
        if group_settled(group)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(POLL);
    }
}

fn signal_group(id: u32, signal: Signal) -> Result<()> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(());
    };
    match kill_process_group(group, signal) {
        Ok(()) | Err(Errno::SRCH) => Ok(()),
        // macOS 对仅剩僵尸的组返回 EPERM；确认 leader 已退出/已回收后视为
        // 清理完成（僵尸成员不可运行，也无法再响应信号）。
        Err(Errno::PERM) if cfg!(target_os = "macos") => {
            if group_leader_gone(group)? {
                Ok(())
            } else {
                bail!("cannot signal the LSP server process group {id}");
            }
        }
        Err(error) => Err(error).context("terminating LSP process group"),
    }
}

/// 进程组是否仍在：EPERM 也视为仍在，必须先发信号再判定落定，避免把
/// “只剩僵尸 + 仍有活后代”误判为空。
fn group_present(id: u32) -> Result<bool> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(false);
    };
    match test_kill_process_group(group) {
        Ok(()) => Ok(true),
        Err(Errno::SRCH) => Ok(false),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(true),
        Err(error) => Err(error).context("checking LSP server process group"),
    }
}

/// 进程组是否已没有可运行成员：ESRCH，或 macOS 上信号被拒（只剩僵尸）。
/// 这不等于“干净成功”——调用方仍需确认 root 已被回收。
fn group_settled(id: u32) -> Result<bool> {
    let Some(group) = Pid::from_raw(id as i32) else {
        return Ok(true);
    };
    match test_kill_process_group(group) {
        Ok(()) => Ok(false),
        Err(Errno::SRCH) => Ok(true),
        Err(Errno::PERM) if cfg!(target_os = "macos") => Ok(true),
        Err(error) => Err(error).context("checking LSP server process group"),
    }
}

/// leader 已退出（尚未回收）或已被回收；ECHILD 说明它已经被 wait 掉了。
fn group_leader_gone(group: Pid) -> io::Result<bool> {
    loop {
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(Errno::INTR) => continue,
            Err(Errno::CHILD) => return Ok(true),
            Err(error) => return Err(error.into()),
        }
    }
}

fn writer_loop(
    mut stdin: impl Write,
    frames: std::sync::mpsc::Receiver<Vec<u8>>,
    shared: Arc<Shared>,
) {
    for frame in frames {
        if let Err(error) = stdin.write_all(&frame).and_then(|()| stdin.flush()) {
            shared.note_close(&format!("LSP stdin write failed: {error}"));
            return;
        }
    }
}

fn drain_stderr(mut stderr: impl Read, shared: Arc<Shared>) {
    let mut buffer = [0_u8; 4096];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(count) => {
                let mut tail = shared.stderr.lock().unwrap_or_else(|e| e.into_inner());
                tail.extend_from_slice(&buffer[..count]);
                if tail.len() > STDERR_LIMIT {
                    let excess = tail.len() - STDERR_LIMIT;
                    tail.drain(..excess);
                }
            }
        }
    }
}

/// 读取并分发服务器消息。reader 线程只做解析、内存状态更新与回帧：从不执行
/// 磁盘编辑、不等待 UI；applyEdit 在这里立即拒绝。
fn reader_loop(stdout: impl Read, shared: Arc<Shared>, sink: NotificationSink) {
    let mut reader = BufReader::new(stdout);
    loop {
        let body = match read_frame(&mut reader) {
            Ok(body) => body,
            Err(reason) => {
                shared.note_close(&reason);
                return;
            }
        };
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            shared.note_close("LSP server sent malformed JSON");
            return;
        };
        let Some(object) = message.as_object() else {
            shared.note_close("LSP server sent a non-object message");
            return;
        };
        let method = object.get("method").and_then(Value::as_str);
        let id = object.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                handle_server_request(&shared, &sink, method, id, params);
            }
            (Some(method), None) => {
                let params = object.get("params").cloned().unwrap_or(Value::Null);
                handle_notification(&shared, &sink, method, params);
            }
            (None, Some(id)) => {
                let Some(key) = id.as_i64() else {
                    continue;
                };
                let outcome = if let Some(error) = object.get("error") {
                    Err(RpcError {
                        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                        message: error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("LSP request failed")
                            .to_owned(),
                    })
                } else {
                    Ok(object.get("result").cloned().unwrap_or(Value::Null))
                };
                let sender = shared
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
                if let Some(sender) = sender {
                    let _ = sender.send(outcome);
                }
            }
            (None, None) => {
                shared.note_close("LSP server sent an incomplete message");
                return;
            }
        }
    }
}

fn handle_notification(
    _shared: &Arc<Shared>,
    sink: &NotificationSink,
    method: &str,
    params: Value,
) {
    sink(method, params);
}

fn handle_server_request(
    shared: &Arc<Shared>,
    sink: &NotificationSink,
    method: &str,
    id: Value,
    params: Value,
) {
    let reply_id = id.clone();
    let reply = move |result: Value| {
        let _ = reply_message(shared, &reply_id, result);
    };
    match method {
        "workspace/applyEdit" => {
            reply(json!({"applied": false, "failureReason": UNAUTHORIZED_EDIT}));
        }
        "workspace/configuration" => {
            let settings = shared
                .settings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let items = params
                .get("items")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let answers: Vec<Value> = items
                .iter()
                .map(|item| {
                    let Some(section) = item.get("section").and_then(Value::as_str) else {
                        return Value::Null;
                    };
                    let mut current = &settings;
                    for part in section.split('.') {
                        match current.get(part) {
                            Some(next) => current = next,
                            None => return Value::Null,
                        }
                    }
                    current.clone()
                })
                .collect();
            reply(Value::Array(answers));
        }
        "workspace/workspaceFolders" => {
            let folders = shared
                .folders
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            reply(if folders.is_empty() {
                Value::Null
            } else {
                Value::Array(folders)
            });
        }
        "client/registerCapability" => {
            if let Some(registrations) = params.get("registrations").and_then(Value::as_array) {
                shared.register_capabilities(registrations);
            }
            reply(Value::Null);
        }
        "client/unregisterCapability" => {
            // LSP 规范的字段名是 `unregisterations`；部分实现写成
            // `unregistrations`，与上游一致地两种都接受。
            let entries = params
                .get("unregisterations")
                .or_else(|| params.get("unregistrations"))
                .and_then(Value::as_array);
            if let Some(entries) = entries {
                shared.unregister_capabilities(entries);
            }
            reply(Value::Null);
        }
        "window/showMessageRequest" => reply(Value::Null),
        "window/showDocument" => reply(json!({"success": false})),
        "window/workDoneProgress/create" => reply(Value::Null),
        "workspace/diagnostic/refresh"
        | "workspace/codeLens/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/inlineValue/refresh" => {
            sink(method, params);
            reply(Value::Null);
        }
        _ => {
            let _ = reply_error(shared, &id, -32601, "method not supported");
        }
    }
}

fn reply_message(shared: &Arc<Shared>, id: &Value, result: Value) -> Result<(), Failure> {
    send_raw(
        shared,
        json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

fn reply_error(shared: &Arc<Shared>, id: &Value, code: i64, message: &str) -> Result<(), Failure> {
    send_raw(
        shared,
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    )
}

/// reader 线程直接回复 server request 时复用的写路径；写队列由 writer 线程消费，
/// 队列满即视为协议失效（与调用线程的判定一致）。
fn send_raw(shared: &Arc<Shared>, message: Value) -> Result<(), Failure> {
    let body = serde_json::to_vec(&message).map_err(|_| Failure::Closed)?;
    if body.len() > BODY_LIMIT {
        return Err(Failure::Closed);
    }
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    let sender = {
        let guard = shared.writer.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone()
    };
    let Some(sender) = sender else {
        return Err(Failure::Closed);
    };
    match sender.try_send(frame) {
        Ok(()) => Ok(()),
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(Failure::Closed),
        Err(std::sync::mpsc::TrySendError::Full(_)) => {
            shared.note_close("LSP server stopped reading requests");
            Err(Failure::Closed)
        }
    }
}

fn read_frame(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let headers = read_headers(reader)?;
    let text = String::from_utf8_lossy(&headers);
    let mut length: Option<usize> = None;
    for line in text.split('\n') {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("content-length") {
            continue;
        }
        // 重复 Content-Length 直接判定连接失效：不同值可能是请求走私的雏形，
        // 相同值也没有合法理由出现。
        if length.is_some() {
            return Err("LSP message repeats the Content-Length header".to_owned());
        }
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("LSP message has an invalid Content-Length header".to_owned());
        }
        let parsed = value
            .parse::<usize>()
            .map_err(|_| "LSP message has an invalid Content-Length header".to_owned())?;
        length = Some(parsed);
    }
    let Some(length) = length else {
        return Err("LSP message is missing a Content-Length header".to_owned());
    };
    if length > BODY_LIMIT {
        return Err("LSP message body exceeds the size limit".to_owned());
    }
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("LSP body read failed: {error}"))?;
    Ok(body)
}

fn read_headers(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut headers = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        match reader.read_exact(&mut byte) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err("LSP server closed its output".to_owned());
            }
            Err(error) => return Err(format!("LSP header read failed: {error}")),
        }
        headers.push(byte[0]);
        if headers.len() > HEADER_LIMIT {
            return Err("LSP message header exceeds the size limit".to_owned());
        }
        if headers.ends_with(b"\r\n\r\n") || headers.ends_with(b"\n\n") {
            return Ok(headers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    fn no_sink() -> NotificationSink {
        Arc::new(|_, _| {})
    }

    fn frame(body: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(body).expect("serialize frame body");
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        frame
    }

    fn read_body(frame: &[u8]) -> Value {
        let split = frame
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("frame separator")
            + 4;
        serde_json::from_slice(&frame[split..]).expect("frame body")
    }

    fn test_shared() -> (Arc<Shared>, Receiver<Vec<u8>>) {
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

    fn empty_slot() -> Arc<Mutex<ProcessSlot>> {
        Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: false,
        }))
    }

    fn transport_for(shared: Arc<Shared>, process: Arc<Mutex<ProcessSlot>>) -> Transport {
        Transport {
            process,
            shared,
            next_id: AtomicI64::new(1),
        }
    }

    fn child_in_own_group(script: &str) -> Child {
        Command::new("/bin/sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn shell")
    }

    /// 用 WNOWAIT 观察 leader 退出但不回收，模拟“root 已退出、slot 还持有句柄”。
    fn wait_leader_exit(group: u32) {
        let pid = Pid::from_raw(group as i32).expect("valid group id");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(_)) => return,
                Ok(None) => {}
                Err(Errno::INTR) => {}
                Err(error) => panic!("waitid failed: {error}"),
            }
            assert!(Instant::now() < deadline, "shell did not exit in time");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lsp_transport_frame_parser_rejects_duplicate_content_length() {
        // 不同值的重复头是请求走私的典型形态，相同值也没有合法理由。
        let mut input = Cursor::new(b"Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}x".to_vec());
        let error = read_frame(&mut input).expect_err("duplicate header must fail");
        assert!(error.contains("repeats the Content-Length"), "{error}");

        let mut identical =
            Cursor::new(b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec());
        let error = read_frame(&mut identical).expect_err("duplicate header must fail");
        assert!(error.contains("repeats the Content-Length"), "{error}");
    }

    #[test]
    fn lsp_transport_frame_parser_bounds_and_validates_frames() {
        let body = br#"{"jsonrpc":"2.0"}"#;
        let mut valid = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        valid.extend_from_slice(body);
        assert_eq!(read_frame(&mut Cursor::new(valid)).unwrap(), body.to_vec());

        let mut invalid = Cursor::new(b"Content-Length: abc\r\n\r\n".to_vec());
        let error = read_frame(&mut invalid).expect_err("invalid length must fail");
        assert!(error.contains("invalid Content-Length"), "{error}");

        let mut missing = Cursor::new(b"Content-Type: application/json\r\n\r\n".to_vec());
        let error = read_frame(&mut missing).expect_err("missing length must fail");
        assert!(error.contains("missing a Content-Length"), "{error}");

        let mut oversized =
            Cursor::new(format!("Content-Length: {}\r\n\r\n", BODY_LIMIT + 1).into_bytes());
        let error = read_frame(&mut oversized).expect_err("oversized body must fail");
        assert!(error.contains("exceeds the size limit"), "{error}");

        let mut header_bomb = Cursor::new(vec![b'A'; HEADER_LIMIT + 1]);
        let error = read_frame(&mut header_bomb).expect_err("oversized header must fail");
        assert!(error.contains("header exceeds the size limit"), "{error}");
    }

    #[test]
    fn lsp_transport_dynamic_registration_survives_unregister_of_other_ids() {
        let (shared, frames) = test_shared();
        let sink = no_sink();
        handle_server_request(
            &shared,
            &sink,
            "client/registerCapability",
            json!(1),
            json!({"registrations": [
                {"id": "diag", "method": "textDocument/diagnostic"},
                {"id": "watch", "method": "workspace/didChangeWatchedFiles"},
            ]}),
        );
        assert!(shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/unregisterCapability",
            json!(2),
            json!({"unregisterations": [
                {"id": "watch", "method": "workspace/didChangeWatchedFiles"},
            ]}),
        );
        // 一个 unregister 不能清掉同 method 的其他注册。
        assert!(shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/unregisterCapability",
            json!(3),
            json!({"unregistrations": [
                {"id": "diag", "method": "textDocument/diagnostic"},
            ]}),
        );
        assert!(!shared.diagnostics_registered());
        for _ in 0..3 {
            assert_eq!(
                read_body(&frames.recv().expect("reply"))["result"],
                Value::Null
            );
        }
    }

    #[test]
    fn lsp_transport_dynamic_registration_replaces_method_for_same_id() {
        let (shared, _frames) = test_shared();
        let sink = no_sink();
        handle_server_request(
            &shared,
            &sink,
            "client/registerCapability",
            json!(1),
            json!({"registrations": [{"id": "diag", "method": "textDocument/diagnostic"}]}),
        );
        assert!(shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/registerCapability",
            json!(2),
            json!({"registrations": [{"id": "diag", "method": "workspace/didChangeWatchedFiles"}]}),
        );
        // 同 id 重新注册是替换，不是叠加。
        assert!(!shared.diagnostics_registered());
        handle_server_request(
            &shared,
            &sink,
            "client/unregisterCapability",
            json!(3),
            json!({"unregisterations": [{"id": "diag", "method": "workspace/didChangeWatchedFiles"}]}),
        );
        assert!(shared.registrations.lock().unwrap().is_empty());
    }

    #[test]
    fn lsp_transport_apply_edit_is_rejected_when_no_operation_waits() {
        let (shared, frames) = test_shared();
        let sink = no_sink();
        handle_server_request(
            &shared,
            &sink,
            "workspace/applyEdit",
            json!(7),
            json!({"edit": {}}),
        );
        let reply = read_body(&frames.recv().expect("reply"));
        assert_eq!(reply["id"], json!(7));
        assert_eq!(reply["result"]["applied"], json!(false));
        assert_eq!(reply["result"]["failureReason"], json!(UNAUTHORIZED_EDIT));
        assert!(shared.callbacks.lock().unwrap().is_empty());
    }

    #[test]
    fn lsp_transport_apply_edit_is_refused_during_active_operation() {
        let (shared, frames) = test_shared();
        shared.active.store(1, Ordering::SeqCst);
        handle_server_request(
            &shared,
            &no_sink(),
            "workspace/applyEdit",
            json!(11),
            json!({"edit":{}}),
        );
        let reply = read_body(&frames.recv().unwrap());
        assert_eq!(reply["result"]["applied"], false);
        assert!(shared.callbacks.lock().unwrap().is_empty());
    }

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
        // 预检失败不消耗请求 id。
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

    #[test]
    fn lsp_transport_reader_dispatches_messages_without_blocking() {
        let (shared, frames) = test_shared();
        let (tx, rx) = sync_channel(1);
        shared.pending.lock().unwrap().insert(5, tx);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = seen.clone();
        let sink: NotificationSink = Arc::new(move |method, _params| {
            recorded.lock().unwrap().push(method.to_owned());
        });
        let mut input = Vec::new();
        input.extend(frame(
            &json!({"jsonrpc": "2.0", "id": 5, "result": {"ok": true}}),
        ));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {},
        })));
        input.extend(frame(&json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "window/showMessageRequest",
            "params": {},
        })));
        reader_loop(Cursor::new(input), shared.clone(), sink);
        assert_eq!(rx.recv().expect("response").unwrap(), json!({"ok": true}));
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["textDocument/publishDiagnostics".to_owned()]
        );
        let reply = read_body(&frames.recv().expect("reply"));
        assert_eq!(reply["id"], json!(9));
        assert_eq!(reply["result"], Value::Null);
        assert!(shared.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn lsp_transport_reader_ignores_responses_for_abandoned_requests() {
        let (shared, _frames) = test_shared();
        let sink = no_sink();
        let input = frame(&json!({"jsonrpc": "2.0", "id": 42, "result": true}));
        reader_loop(Cursor::new(input), shared.clone(), sink);
        // 迟到响应没有等待方：reader 丢弃它并正常结束。
        assert!(shared.pending.lock().unwrap().is_empty());
        assert!(shared.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn lsp_transport_reader_closes_transport_on_malformed_json() {
        let (shared, _frames) = test_shared();
        let (tx, rx) = sync_channel(1);
        shared.pending.lock().unwrap().insert(1, tx);
        let sink = no_sink();
        let input = b"Content-Length: 3\r\n\r\nnot".to_vec();
        reader_loop(Cursor::new(input), shared.clone(), sink);
        assert!(shared.closed.load(Ordering::SeqCst));
        let error = rx.recv().expect("closed pending").unwrap_err();
        assert_eq!(error.code, -32800);
        assert!(shared.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn lsp_transport_terminate_slot_marks_closing_and_is_idempotent() {
        let process = empty_slot();
        terminate_slot(&process, Duration::ZERO).expect("first terminate");
        assert!(process.lock().unwrap().closing);
        terminate_slot(&process, Duration::ZERO).expect("second terminate");
        assert!(process.lock().unwrap().child.is_none());
    }

    #[test]
    fn lsp_transport_terminate_slot_reclaims_descendants_after_root_exit() {
        let child = child_in_own_group("sleep 60 & exit 0");
        let group = child.id();
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: Some(child),
            closing: false,
        }));
        wait_leader_exit(group);
        // root 已退出但仍是僵尸；后台 sleep 仍持有同一个进程组。
        terminate_slot(&process, Duration::ZERO).expect("terminate");
        assert!(process.lock().unwrap().child.is_none());
        assert!(group_settled(group).expect("probe group"));
    }

    #[test]
    fn lsp_transport_terminate_slot_kills_group_after_root_was_reaped() {
        let mut child = child_in_own_group("sleep 60 & exit 0");
        let group = child.id();
        wait_leader_exit(group);
        // 调用方先回收 root：Child 仍保留 id 与缓存的退出状态。
        assert!(child.try_wait().expect("reap root").is_some());
        assert!(group_present(group).expect("probe group"));
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: Some(child),
            closing: false,
        }));
        terminate_slot(&process, Duration::ZERO).expect("terminate");
        assert!(process.lock().unwrap().child.is_none());
        assert!(group_settled(group).expect("probe group"));
    }

    #[test]
    fn lsp_transport_spawn_refuses_after_cleanup_marks_slot_closing() {
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: true,
        }));
        let error = match Transport::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 60".to_owned()],
            Path::new("/"),
            process,
            no_sink(),
        ) {
            Ok(_) => panic!("spawn must be refused while closing"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("shutting down"), "{error}");
    }

    #[test]
    fn lsp_transport_terminate_reclaims_the_whole_group() {
        let process = empty_slot();
        let transport = Transport::spawn(
            Path::new("/bin/sh"),
            &["-c".to_owned(), "sleep 60".to_owned()],
            Path::new("/"),
            process.clone(),
            no_sink(),
        )
        .expect("spawn server");
        let group = process
            .lock()
            .unwrap()
            .child
            .as_ref()
            .expect("registered child")
            .id();
        transport
            .terminate(Duration::ZERO)
            .expect("terminate server");
        assert!(process.lock().unwrap().child.is_none());
        assert!(transport.is_closed());
        assert!(group_settled(group).expect("probe group"));
    }
}
