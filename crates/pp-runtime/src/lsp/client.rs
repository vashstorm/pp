//! 单个语言服务器的受管理客户端：初始化握手、文档状态、诊断缓存与操作串行化。

mod diagnostics;
mod handshake;
mod workspace;

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::lsp::config::ServerDefinition;
use crate::lsp::position::whole_range;
use crate::lsp::transport::{Failure, ProcessSlot, ServerRequest, Transport};
use crate::lsp::types::{PublishDiagnosticsParams, SaveKind, ServerCapabilities, SyncKind};
use crate::lsp::uri::path_to_uri;
use diagnostics::{Diagnostics, Signals};
use handshake::{convert, convert_unit, initialize_handshake, startup_failure, unauthorized_edit};
use pp_core::cancellation::CancellationToken;
use workspace::folder_json;

pub(super) const POLL: Duration = Duration::from_millis(20);
/// 未版本化报告的稳定窗口。
pub(super) const SETTLE: Duration = Duration::from_millis(250);
/// 冷启动/重开后的未版本化空报告在这个窗口内不算干净。
pub(super) const COLD_WINDOW: Duration = Duration::from_secs(12);
pub(super) const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const DEFAULT_READY_POLL: Duration = Duration::from_millis(100);
pub(super) const DEFAULT_READY_SETTLE: Duration = Duration::from_secs(2);
pub(super) const DEFAULT_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const PROJECT_LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// 正常 shutdown RPC 的等待上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// shutdown 之后等待进程自己退出的宽限。
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// initialize 失败后回收进程的宽限。
const HANDSHAKE_GRACE: Duration = Duration::from_millis(500);
/// 放弃待写 overlay 时 best-effort 通知的预算；与调用方取消/已过期 deadline 解耦。
#[cfg(test)]
const ABORT_BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub(crate) struct DocumentState {
    pub(crate) uri: String,
    pub(crate) language_id: String,
    pub(crate) version: i64,
    pub(crate) text: String,
    pub(crate) opened: bool,
    /// 最近一次 didOpen 的时间；冷启动空报告的窗口按文档计算。
    pub(crate) opened_at: Instant,
    pub(crate) last_used: Instant,
    pub(crate) pending_write: bool,
}

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
    transport: Transport,
    documents: Mutex<HashMap<PathBuf, DocumentState>>,
    op_lock: Mutex<()>,
    /// 最近一次操作开始/结束的时间；空闲回收按它计算而不是按创建时间。
    last_activity: Mutex<Instant>,
    folders: Mutex<Vec<PathBuf>>,
    /// 追加过工作区根之后才允许 ContentModified 单次重试。
    roots_added: AtomicBool,
}

/// 一次高层操作的上下文：绝对 deadline、取消令牌与 server 回调处理。
pub(crate) struct Op<'a> {
    client: &'a Client,
    deadline: Instant,
    cancellation: &'a CancellationToken,
    service: &'a mut dyn FnMut(ServerRequest),
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

/// 跟踪 transport 的活动操作；applyEdit 始终在传输层拒绝。
/// 即使操作体 panic 也保证计数回落。
struct ActiveOperation<'a> {
    transport: &'a Transport,
}

impl<'a> ActiveOperation<'a> {
    fn new(transport: &'a Transport) -> Self {
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
    fn touch(&self) {
        *self.last_activity.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    /// 最近一次活动距今的时间。
    fn idle_for(&self) -> Duration {
        self.last_activity
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .elapsed()
    }

    fn acquire(
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

    pub(crate) fn document(&self, path: &Path) -> Option<DocumentState> {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned()
    }

    /// 打开或刷新目标文档：读取当前盘内容；真正首次打开时只发 didOpen。
    ///
    /// 文档可能因为 5 分钟空闲被 didClose，再次打开时必须重新 didOpen，
    /// 不能在未打开的文档上先发 didChange。
    pub(crate) fn open_or_refresh(&self, op: &mut Op<'_>, path: &Path) -> Result<DocumentState> {
        let uri = path_to_uri(path);
        let previous = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned();
        let language_id = language_id(&self.definition, path);
        if let Some(previous) = previous.as_ref()
            && previous.pending_write
        {
            // 写入流程自己负责同步；此时不得用盘上旧内容覆盖 overlay。
            return Ok(previous.clone());
        }
        let text = read_document_text(path)?;
        let now = Instant::now();
        match previous {
            None => {
                self.ensure_workspace_folder(op, path)?;
                let state = DocumentState {
                    uri: uri.clone(),
                    language_id: language_id.clone(),
                    version: 1,
                    text: text.clone(),
                    opened: true,
                    opened_at: now,
                    last_used: now,
                    pending_write: false,
                };
                if self.syncs_documents() {
                    self.notify_did_open(op, &state)?;
                }
                self.diagnostics.invalidate(&uri);
                self.store_document(path, &state);
                Ok(state)
            }
            Some(mut state) => {
                state.last_used = now;
                if !state.opened {
                    // 空闲关闭后重开：先用完整内容 didOpen，绝不再发一次 didChange。
                    self.ensure_workspace_folder(op, path)?;
                    state.version += 1;
                    state.text = text;
                    state.opened = true;
                    state.opened_at = now;
                    self.diagnostics.invalidate(&uri);
                    if self.syncs_documents() {
                        self.notify_did_open(op, &state)?;
                    }
                    self.store_document(path, &state);
                    return Ok(state);
                }
                if state.text == text {
                    self.store_document(path, &state);
                    return Ok(state);
                }
                let previous_text = std::mem::replace(&mut state.text, text);
                state.version += 1;
                self.diagnostics.invalidate(&uri);
                self.notify_did_change(op, &state, &previous_text)?;
                self.store_document(path, &state);
                Ok(state)
            }
        }
    }

    /// 服务器是否需要 didOpen（只有 openClose 或文本同步能力时才有意义）。
    fn syncs_documents(&self) -> bool {
        self.capabilities.open_close || self.capabilities.sync != SyncKind::None
    }

    fn notify_did_open(&self, op: &mut Op<'_>, state: &DocumentState) -> Result<()> {
        op.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": state.uri,
                    "languageId": state.language_id,
                    "version": state.version,
                    "text": state.text,
                }
            }),
        )
    }

    fn notify_did_change(
        &self,
        op: &mut Op<'_>,
        state: &DocumentState,
        previous_text: &str,
    ) -> Result<()> {
        if self.capabilities.sync == SyncKind::None {
            return Ok(());
        }
        let change = match self.capabilities.sync {
            SyncKind::Full => json!({"text": state.text}),
            SyncKind::Incremental => {
                json!({"range": whole_range(previous_text), "text": state.text})
            }
            SyncKind::None => json!({"text": state.text}),
        };
        op.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": state.uri, "version": state.version},
                "contentChanges": [change],
            }),
        )
    }

    fn store_document(&self, path: &Path, state: &DocumentState) {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf(), state.clone());
    }

    /// 把一份尚未落盘的内容放进服务器文档视图（临时 overlay）。
    ///
    /// 调用方必须已经持有本客户端的 operation（`op` 来自 [`Client::operation`]），
    /// 这里既不重新加锁，也不再次进入 operation。内容从不读盘：新文件即使还
    /// 不存在也能先以 `didOpen` 建立临时文档；`pending_write=true` 期间
    /// [`Client::open_or_refresh`] 不会用盘上旧内容覆盖它。
    ///
    /// 已打开的文档内容变化发 `didChange`，空闲关闭或从未打开的文档发
    /// `didOpen`。诊断缓存按 URI 失效，旧版本的报告不会被当成新鲜结果。
    #[cfg(test)]
    pub(crate) fn set_overlay(
        &self,
        op: &mut Op<'_>,
        path: &Path,
        content: &str,
        pending_write: bool,
    ) -> Result<DocumentState> {
        let uri = path_to_uri(path);
        let previous = self.document(path);
        let now = Instant::now();
        let state = match previous {
            None => {
                self.ensure_workspace_folder(op, path)?;
                let state = DocumentState {
                    uri: uri.clone(),
                    language_id: language_id(&self.definition, path),
                    version: 1,
                    text: content.to_owned(),
                    opened: true,
                    opened_at: now,
                    last_used: now,
                    pending_write,
                };
                if self.syncs_documents() {
                    self.notify_did_open(op, &state)?;
                }
                state
            }
            Some(mut state) => {
                state.last_used = now;
                state.pending_write = pending_write;
                if !state.opened {
                    // 空闲关闭后重开：只能用完整内容 didOpen，不能先发 didChange。
                    self.ensure_workspace_folder(op, path)?;
                    state.version += 1;
                    state.text = content.to_owned();
                    state.opened = true;
                    state.opened_at = now;
                    if self.syncs_documents() {
                        self.notify_did_open(op, &state)?;
                    }
                } else if state.text != content {
                    let previous_text = std::mem::replace(&mut state.text, content.to_owned());
                    state.version += 1;
                    self.notify_did_change(op, &state, &previous_text)?;
                }
                state
            }
        };
        self.diagnostics.invalidate(&uri);
        self.store_document(path, &state);
        Ok(state)
    }

    /// 在已持有的 operation 里同步刚提交到磁盘的路径，不重新进入 operation。
    ///
    /// 调用方给出的是它已经按 Registry 覆盖关系筛选过的路径；这里不再用本客户端的
    /// `fileTypes` 二次过滤（共享/完整 identity 的别名可能比单一定义更宽）。
    /// 存在的普通文件：刷新文档文本、清除 `pending_write`，已打开的文档补发
    /// `didChange`/`didSave`；缺失或不再是普通文件：关闭已打开的文档、丢弃
    /// 文档状态与诊断。两种情况都发出对应的 watched-files 变更。
    ///
    /// 磁盘读取失败返回错误且不改写 overlay：调用方可以把它记成警告，但已提交
    /// 的事实不会因此被改写。
    #[cfg(test)]
    pub(crate) fn sync_committed_in_operation(
        &self,
        op: &mut Op<'_>,
        paths: &[PathBuf],
    ) -> Result<()> {
        self.sync_committed_locked(op, paths.iter().map(|path| (path.as_path(), 2)))
    }

    /// 在**已经持有本客户端 operation** 的 server 回调里同步刚提交的路径。
    ///
    /// 不变量：调用方必须位于 [`Client::operation`] 的 body 或 service 回调中，
    /// 且该 operation 仍持有 `op_lock`。因此本方法绝不重新取得 operation 锁、
    /// 也绝不进入另一个 operation（那会等待自己并永久阻塞）；它只写 transport
    /// 队列、更新文档缓存与诊断缓存。
    ///
    /// 不在 operation 内的普通保存后同步必须使用
    /// [`Client::sync_committed_in_operation`] 或 `edits::sync_committed`。
    #[cfg(test)]
    pub(crate) fn notify_committed_from_current_operation(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
        paths: &[PathBuf],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut deny = |request: ServerRequest| request.respond(unauthorized_edit());
        let service: &mut dyn FnMut(ServerRequest) = &mut deny;
        let mut op = Op {
            client: self,
            deadline,
            cancellation,
            service,
        };
        self.touch();
        self.sync_committed_locked(&mut op, paths.iter().map(|path| (path.as_path(), 2)))
    }

    pub(crate) fn sync_file_events_in_operation(
        &self,
        op: &mut Op<'_>,
        events: &[(PathBuf, u8)],
    ) -> Result<()> {
        self.sync_committed_locked(
            op,
            events.iter().map(|(path, kind)| (path.as_path(), *kind)),
        )
    }

    fn sync_committed_locked<'a>(
        &self,
        op: &mut Op<'_>,
        events: impl IntoIterator<Item = (&'a Path, u8)>,
    ) -> Result<()> {
        let mut watched = Vec::new();
        for (path, change_kind) in events {
            let uri = path_to_uri(path);
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() => {
                    let text = match read_document_text(path) {
                        Ok(text) => text,
                        Err(error) => {
                            // 已提交但无法表示为 UTF-8 文本：不清除 pending_write
                            // 会让后续 reconcile 永远被挡住，所以先清除标记再报错；
                            // 调用方仍会看到错误并保留“已提交”的事实。
                            if let Some(mut state) = self.take_document(path) {
                                state.pending_write = false;
                                self.store_document(path, &state);
                            }
                            return Err(error);
                        }
                    };
                    let state = self.take_document(path);
                    let Some(mut state) = state else {
                        // 没有打开过的文档：只通知内容变化，不替调用方打开新文档。
                        self.diagnostics.invalidate(&uri);
                        watched.push(json!({"uri": uri, "type": change_kind}));
                        continue;
                    };
                    state.pending_write = false;
                    state.last_used = Instant::now();
                    if state.text != text {
                        let previous_text = std::mem::replace(&mut state.text, text);
                        state.version += 1;
                        if state.opened {
                            self.notify_did_change(op, &state, &previous_text)?;
                        }
                    }
                    if state.opened && self.capabilities.save != SaveKind::Never {
                        let mut params = json!({"textDocument": {"uri": uri}});
                        if self.capabilities.save == SaveKind::IncludeText {
                            params["text"] = json!(&state.text);
                        }
                        op.notify("textDocument/didSave", params)?;
                    }
                    self.diagnostics.invalidate(&uri);
                    self.store_document(path, &state);
                    watched.push(json!({"uri": uri, "type": change_kind}));
                }
                Ok(_) => {
                    // 目录/FIFO/设备：不是可同步的文档，按删除处理。
                    self.close_document(op, path)?;
                    watched.push(json!({"uri": uri, "type": 3}));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.close_document(op, path)?;
                    watched.push(json!({"uri": uri, "type": 3}));
                }
                Err(error) => {
                    return Err(anyhow::Error::from(error).context(format!(
                        "inspecting {} while syncing {}",
                        path.display(),
                        self.name
                    )));
                }
            }
        }
        if !watched.is_empty() {
            op.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes": watched}),
            )?;
        }
        Ok(())
    }

    /// 放弃一次尚未落盘的临时内容：恢复盘上内容；文件仍不存在时关闭临时文档。
    ///
    /// 本地失效是无条件且先发生的：即使调用方令牌已取消、`deadline` 已过期，
    /// 或随后的通知失败，`pending_write` 也会被清除，新文件的状态会被移除。
    /// 这样下一次 [`Client::open_or_refresh`] 只会读盘，绝不会继续使用失败的候选
    /// 内容。磁盘内容读取用非阻塞 fd 完成（FIFO 不会挂住），通知本身使用与调用
    /// 方取消解耦的有界预算；失败只作为警告返回，已完成的清除不会回退。
    #[cfg(test)]
    pub(crate) fn abort_pending_write(
        &self,
        path: &Path,
        _deadline: Instant,
        _cancellation: &CancellationToken,
    ) -> Result<()> {
        let uri = path_to_uri(path);
        let Some(mut state) = self.take_document(path) else {
            return Ok(());
        };
        let previous_text = state.text.clone();
        state.pending_write = false;
        state.last_used = Instant::now();
        let restored = read_document_text(path).ok();
        let mut changed = false;
        if let Some(text) = restored.as_ref()
            && state.text != *text
        {
            state.version += 1;
            state.text = text.clone();
            changed = true;
        }
        if restored.is_some() {
            // 盘上内容可读：缓存回到真实内容，但已有内容不一致时仍需通知服务器。
            self.store_document(path, &state);
        }
        self.diagnostics.invalidate(&uri);

        let notify_close = restored.is_none() && state.opened && self.capabilities.open_close;
        let notify_change = restored.is_some() && changed && state.opened && self.syncs_documents();
        if !notify_close && !notify_change {
            return Ok(());
        }
        // 有界、与调用方取消解耦的 best-effort 通知。
        let mut deny = |request: ServerRequest| request.respond(unauthorized_edit());
        let service: &mut dyn FnMut(ServerRequest) = &mut deny;
        let limit = Instant::now() + ABORT_BUDGET;
        let token = CancellationToken::new();
        let notified = self.operation(limit, &token, service, |op| {
            if notify_close {
                op.notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                )?;
            } else {
                self.notify_did_change(op, &state, &previous_text)?;
            }
            Ok(())
        });
        match notified {
            Ok(()) => Ok(()),
            Err(error) => {
                // 通知没送达时缓存不能留下可能与会话不一致的文档：下一次打开
                // 会以盘上内容重新 didOpen。清除先于这个错误，错误只是警告。
                let _ = self.take_document(path);
                self.diagnostics.invalidate(&uri);
                Err(error)
            }
        }
    }

    fn take_document(&self, path: &Path) -> Option<DocumentState> {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path)
    }

    /// 关闭并丢弃文档状态；`didClose` 只在服务器声明 openClose 时发送。
    fn close_document(&self, op: &mut Op<'_>, path: &Path) -> Result<()> {
        match self.take_document(path) {
            Some(state) => self.close_document_state(op, path, state),
            None => Ok(()),
        }
    }

    fn close_document_state(
        &self,
        op: &mut Op<'_>,
        path: &Path,
        state: DocumentState,
    ) -> Result<()> {
        let uri = path_to_uri(path);
        self.diagnostics.invalidate(&uri);
        if state.opened && self.capabilities.open_close {
            op.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri}}),
            )?;
        }
        Ok(())
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

/// 目标文件的文本；缺失、非普通文件与非法 UTF-8 都是错误。
///
/// 先以 `O_NONBLOCK` 打开 fd，再检查它确实是普通文件：这样 stat 与 read
/// 之间即使路径被换成 FIFO，也不会在 `read` 上永久挂住。符号链接仍按
/// 只读导航的需要正常跟随（工作区外文件也允许读取）。
pub(crate) fn read_document_text(path: &Path) -> Result<String> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("reading {} for the language server", path.display()))?;
    let mut file = File::from(fd);
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(text)
}

/// 语言标识：配置优先，否则按扩展名推断。
pub(crate) fn language_id(definition: &ServerDefinition, path: &Path) -> String {
    if let Some(language_id) = &definition.language_id {
        return language_id.clone();
    }
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "dockerfile" {
        return "dockerfile".to_owned();
    }
    match extension.as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "json" => "json",
        "jsonc" => "jsonc",
        "css" => "css",
        "go" => "go",
        "py" | "pyi" => "python",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "cs" => "csharp",
        "php" => "php",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" | "htm" => "html",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "swift" => "swift",
        "c" => "c",
        "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "zig" => "zig",
        "tla" | "tlaplus" => "tlaplus",
        "tex" => "latex",
        "md" => "markdown",
        "dart" => "dart",
        "erl" | "hrl" => "erlang",
        "ex" | "exs" => "elixir",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "sol" => "solidity",
        "tf" | "tfvars" => "terraform",
        "graphql" | "gql" => "graphql",
        "prisma" => "prisma",
        "vim" => "vim",
        "nix" => "nix",
        "cmake" => "cmake",
        _ => "plaintext",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::diagnostics::{full_items, join_notes, related_reports};
    use super::workspace::{checkout_to_add, real_path};
    use super::*;
    use crate::lsp::config;
    use crate::lsp::position::position_offset;
    use crate::lsp::types::Position;
    use crate::lsp::uri::uri_to_path;
    use pp_core::lsp_report::ReportStatus;
    use std::sync::mpsc;
    use tempfile::TempDir;

    /// 用受控 stdio fixture 启动一个独立客户端；不读取真实 HOME，也不经 registry。
    fn fixture_client(pull: &[&str]) -> (TempDir, Arc<Client>) {
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

    fn logged(dir: &TempDir) -> Vec<Value> {
        std::fs::read_to_string(dir.path().join("requests.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// 服务器在自己的进程里写日志；有界轮询等待目标事件真正被处理。
    fn wait_logged(
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

    fn with_client<T>(client: &Arc<Client>, body: impl FnOnce(&mut Op<'_>) -> Result<T>) -> T {
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

    #[test]
    fn lsp_client_malformed_pull_is_failed_not_clean() {
        for kind in ["malformed", "missing"] {
            let (dir, client) = fixture_client(&["--pull", kind]);
            let path = dir.path().join("sample.fixture");
            std::fs::write(&path, "wrong\n").unwrap();
            let error = match client.collect_diagnostics(
                &path,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            ) {
                Ok(outcome) => panic!(
                    "{kind} pull was reported as {:?} instead of failed: {:?}",
                    outcome.status, outcome.note
                ),
                Err(error) => error,
            };
            assert!(
                format!("{error:#}").contains("malformed diagnostic report"),
                "{kind}: {error:#}"
            );
            client.shutdown().unwrap();
        }
    }

    #[test]
    fn lsp_client_unchanged_report_requires_matching_fresh_cache() {
        let diagnostics = Diagnostics::default();
        assert!(diagnostics.unchanged_report("file:///a", 1).is_none());
        diagnostics.publish("file:///a", Some(1), Vec::new());
        assert!(
            diagnostics.unchanged_report("file:///a", 2).is_none(),
            "a cache for another document version is not unchanged proof"
        );
        assert_eq!(
            diagnostics
                .unchanged_report("file:///a", 1)
                .map(|items| items.len()),
            Some(0)
        );
        diagnostics.invalidate("file:///a");
        assert!(
            diagnostics.unchanged_report("file:///a", 1).is_none(),
            "an invalidated cache entry is not unchanged proof"
        );
        diagnostics.publish("file:///a", Some(1), Vec::new());
        assert!(diagnostics.unchanged_report("file:///a", 1).is_some());
        diagnostics.refresh();
        assert!(
            diagnostics.unchanged_report("file:///a", 1).is_none(),
            "a refreshed epoch invalidates the unchanged proof"
        );
    }

    #[test]
    fn lsp_client_unchanged_uses_fresh_matching_push() {
        let (dir, client) = fixture_client(&["--pull", "unchanged"]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "wrong\n").unwrap();
        let outcome = client
            .collect_diagnostics(
                &path,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        // didOpen 的 push 已给出当前版本的真实报告；unchanged 复用它而不是凭空干净。
        assert_eq!(outcome.status, ReportStatus::ReportedIssues);
        assert!(outcome.diagnostics.is_some_and(|items| !items.is_empty()));
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_pull_status_is_issues_then_clean() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "wrong\n").unwrap();
        let issues = client
            .collect_diagnostics(
                &path,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(issues.status, ReportStatus::ReportedIssues);
        assert!(issues.diagnostics.is_some_and(|items| !items.is_empty()));
        std::fs::write(&path, "fine\n").unwrap();
        let clean = client
            .collect_diagnostics(
                &path,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(clean.status, ReportStatus::ReportedClean);
        assert_eq!(clean.diagnostics.as_ref().map(Vec::len), Some(0));
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_reopen_sends_did_open_without_did_change() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "first\n").unwrap();
        let _ = with_client(&client, |op| client.open_or_refresh(op, &path));
        client
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&path)
            .unwrap()
            .opened = false;
        std::fs::write(&path, "second\n").unwrap();
        let _ = with_client(&client, |op| client.open_or_refresh(op, &path));

        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events
                .iter()
                .filter(|event| event["method"] == "textDocument/didOpen")
                .count()
                >= 2
        });
        let opens: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didOpen")
            .collect();
        let changes = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didChange")
            .count();
        assert_eq!(opens.len(), 2, "reopen must send didOpen again");
        assert_eq!(
            changes, 0,
            "an unopened document must not receive didChange"
        );
        assert_eq!(
            opens[1]["params"]["textDocument"]["text"], "second\n",
            "reopen must send the full current text"
        );
        client.shutdown().unwrap();
    }

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
    fn lsp_client_pending_overlay_survives_disk_reconcile() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "disk-old\n").unwrap();
        let state = with_client(&client, |op| {
            client.set_overlay(op, &path, "pending   \n\n", true)
        });
        assert!(state.pending_write);
        assert_eq!(state.text, "pending   \n\n");

        // 盘上内容在这期间变化，但 pending_write 的 overlay 不能被读盘覆盖。
        std::fs::write(&path, "disk-new\n").unwrap();
        let refreshed = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(refreshed.text, "pending   \n\n");
        assert!(refreshed.pending_write);

        // 提交后同步：overlay 采用最终盘上内容并清除 pending_write。
        std::fs::write(&path, "committed\n").unwrap();
        with_client(&client, |op| {
            client.sync_committed_in_operation(op, std::slice::from_ref(&path))
        });
        let synced = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(synced.text, "committed\n");
        assert!(!synced.pending_write);

        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events.iter().any(|event| {
                event["method"] == "textDocument/didChange"
                    && event["params"]["contentChanges"][0]["text"] == "committed\n"
            })
        });
        let open = events
            .iter()
            .find(|event| event["method"] == "textDocument/didOpen")
            .unwrap();
        assert_eq!(open["params"]["textDocument"]["text"], "pending   \n\n");
        let changes: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didChange")
            .collect();
        let last = changes.last().expect("committed didChange");
        assert_eq!(
            last["params"]["contentChanges"][0]["text"], "committed\n",
            "committed content must replace the pending overlay"
        );
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_abort_pending_write_restores_disk_and_closes_new_file() {
        let (dir, client) = fixture_client(&[]);
        let existing = dir.path().join("existing.fixture");
        std::fs::write(&existing, "disk\n").unwrap();
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &existing, "pending\n", true)
        });
        client
            .abort_pending_write(
                &existing,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        let state = client.document(&existing).unwrap();
        assert_eq!(state.text, "disk\n");
        assert!(!state.pending_write);

        let new_file = dir.path().join("new.fixture");
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &new_file, "pending\n", true)
        });
        client
            .abort_pending_write(
                &new_file,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(client.document(&new_file).is_none());
        assert!(!new_file.exists(), "aborting must not create the file");

        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events
                .iter()
                .any(|event| event["method"] == "textDocument/didClose")
        });
        let closed: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didClose")
            .collect();
        assert_eq!(closed.len(), 1, "only the temporary new document closed");
        let uri = closed[0]["params"]["textDocument"]["uri"].as_str().unwrap();
        assert!(uri.ends_with("new.fixture"), "{uri}");
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_current_operation_sync_does_not_reenter_lock() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "committed\n").unwrap();
        // 在同一个 operation 里先放 overlay 再同步提交内容：helper 不能自己
        // 再取 operation 锁，否则会永久阻塞。
        with_client(&client, |op| {
            client.set_overlay(op, &path, "pending\n", true)?;
            client.notify_committed_from_current_operation(
                op.deadline(),
                op.cancellation(),
                std::slice::from_ref(&path),
            )
        });
        let state = client.document(&path).unwrap();
        assert_eq!(state.text, "committed\n");
        assert!(!state.pending_write);
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
    fn lsp_client_reads_regular_files_through_nonblocking_fd() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok.fixture");
        std::fs::write(&file, "ok\n").unwrap();
        assert_eq!(read_document_text(&file).unwrap(), "ok\n");
        assert!(
            read_document_text(dir.path()).is_err(),
            "directories are not documents"
        );

        let fifo = dir.path().join("pipe.fixture");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let error = read_document_text(&fifo).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );

        let invalid = dir.path().join("bad.fixture");
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        let error = read_document_text(&invalid).unwrap_err();
        assert!(
            format!("{error:#}").contains("not valid UTF-8"),
            "{error:#}"
        );
    }

    #[test]
    fn lsp_client_position_offsets_are_strict_utf16() {
        let text = "a\u{1f600}b\r\nxy\n";
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            Some(0)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 1
                }
            ),
            Some(1)
        );
        // 列落在代理对中间：拒绝，不能夹到相邻字符。
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            None
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            Some(5)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            Some(6)
        );
        // CRLF 的 \r 不属于行内容。
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 5
                }
            ),
            None
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 1,
                    character: 2
                }
            ),
            Some(10)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 2,
                    character: 0
                }
            ),
            Some(11)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 3,
                    character: 0
                }
            ),
            None
        );
    }

    #[test]
    fn lsp_client_abort_pending_write_clears_with_cancelled_token() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "disk\n").unwrap();
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &path, "failed candidate\n", true)
        });
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        // 已取消的令牌 + 已过期的 deadline：仍必须立刻清除 pending_write。
        let _ = client.abort_pending_write(
            &path,
            Instant::now() - Duration::from_secs(1),
            &cancellation,
        );
        // 下一次语义查询必须读盘，不能继续读失败的候选内容。
        let state = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(state.text, "disk\n");
        assert!(!state.pending_write);

        // 新文件的失败候选直接移除，且不会创建文件。
        let new_file = dir.path().join("new.fixture");
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &new_file, "failed candidate\n", true)
        });
        let _ = client.abort_pending_write(
            &new_file,
            Instant::now() - Duration::from_secs(1),
            &cancellation,
        );
        assert!(client.document(&new_file).is_none());
        assert!(!new_file.exists());
        let reopened = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(reopened.text, "disk\n");
        client.shutdown().unwrap();
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

    #[test]
    fn lsp_client_uri_paths_roundtrip_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        // `..` 必须词法弹出上一段，不能被丢弃成 `/a/b`。
        let dotted = dir.path().join("a").join("..").join("b");
        assert_eq!(
            path_to_uri(&dotted),
            format!("file://{}", dir.path().join("b").display()),
            "`..` must pop the previous segment"
        );
        // 新文件不需要存在，也不做 canonicalize。
        let missing = dir.path().join("new file?.rs");
        let uri = path_to_uri(&missing);
        assert!(uri.ends_with("new%20file%3F.rs"), "{uri}");
        assert_eq!(uri_to_path(&uri).as_deref(), Some(missing.as_path()));

        // 百分号编码的 `?`/`#` 是合法文件名字符。
        assert_eq!(
            uri_to_path("file:///tmp/a%3Fb%23c"),
            Some(PathBuf::from("/tmp/a?b#c"))
        );
        // 未转义的 query/fragment、NUL 与畸形百分号都拒绝。
        assert!(uri_to_path("file:///tmp/a?x=1").is_none());
        assert!(uri_to_path("file:///tmp/a#frag").is_none());
        assert!(uri_to_path("file:///tmp/a%00b").is_none());
        assert!(uri_to_path("file:///tmp/a%2").is_none());
        assert!(uri_to_path("http:///tmp/a").is_none());
        assert!(uri_to_path("file://remote/tmp/a").is_none());
    }

    #[test]
    fn lsp_client_project_load_ignores_stale_progress_end() {
        let signals = Signals::default();
        let token = CancellationToken::new();
        let progress =
            |value: Value| signals.apply_progress(&json!({"token": "t", "value": value}));
        progress(json!({"kind": "begin"}));
        signals.begin_pending_load();
        progress(json!({"kind": "end"}));
        assert!(
            !signals.wait_project_load(Instant::now() + Duration::from_millis(80), &token),
            "a progress end that began before the add completed the new root's load"
        );
        progress(json!({"kind": "begin"}));
        progress(json!({"kind": "end"}));
        assert!(signals.wait_project_load(Instant::now() + Duration::from_secs(1), &token));
    }

    #[test]
    fn lsp_client_malformed_related_documents_only_warn() {
        let report = json!({
            "kind": "full",
            "items": [],
            "relatedDocuments": {
                "file:///ok": {"kind": "full", "items": []},
                "file:///bad": {"kind": "full", "items": "invalid"},
                "file:///missing": {"kind": "full"}
            }
        });
        let (related, warnings) = related_reports(&report);
        assert_eq!(related.len(), 1, "only the valid related report survives");
        assert_eq!(related[0].0, "file:///ok");
        assert_eq!(warnings.len(), 2);
        assert!(join_notes(warnings).is_some());
        assert!(full_items(&report).unwrap().is_empty());
        assert!(full_items(&json!({"kind": "full"})).is_err());
        assert!(full_items(&json!({"kind": "full", "items": "invalid"})).is_err());
    }

    #[test]
    fn lsp_client_checkout_rules_follow_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let other = dir.path().join("other");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(other.join(".git")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(other.join("b.rs"), "").unwrap();

        // 服务器不支持 changeNotifications：永不追加。
        assert_eq!(
            checkout_to_add(&root, false, &root.join("src/a.rs"), None),
            None
        );
        // 文件与客户端根在同一个 checkout：服务器已经知道它。
        assert_eq!(
            checkout_to_add(&root, true, &root.join("src/a.rs"), None),
            None
        );
        // 另一个 checkout：追加解析 symlink 后的根。
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), None),
            Some(real_path(&other))
        );
        // 包含 HOME 的 checkout 不追加。
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), Some(&other)),
            None
        );
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), Some(&root)),
            Some(real_path(&other))
        );
        // 最近的 .git 优先。
        std::fs::create_dir_all(other.join("nested/.git")).unwrap();
        std::fs::write(other.join("nested/c.rs"), "").unwrap();
        assert_eq!(
            checkout_to_add(&root, true, &other.join("nested/c.rs"), None),
            Some(real_path(&other.join("nested")))
        );
        // 路径中的 symlink 先解析，再判断 checkout。
        std::os::unix::fs::symlink(&other, dir.path().join("link")).unwrap();
        assert_eq!(
            checkout_to_add(&root, true, &dir.path().join("link/b.rs"), None),
            Some(real_path(&other))
        );
    }
}
