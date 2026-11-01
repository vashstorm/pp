//! 单个语言服务器的受管理客户端：初始化握手、文档状态、诊断缓存与操作串行化。

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{Mode, OFlags};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use crate::cancellation::CancellationToken;
use crate::lsp::ReportStatus;
use crate::lsp::config::ServerDefinition;
use crate::lsp::transport::{Failure, ProcessSlot, ServerRequest, Transport, UNAUTHORIZED_EDIT};
use crate::lsp::types::{
    Diagnostic, Position, PublishDiagnosticsParams, SaveKind, ServerCapabilities, SyncKind,
};

const POLL: Duration = Duration::from_millis(20);
/// 未版本化报告的稳定窗口。
const SETTLE: Duration = Duration::from_millis(250);
/// 冷启动/重开后的未版本化空报告在这个窗口内不算干净。
const COLD_WINDOW: Duration = Duration::from_secs(12);
const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_READY_POLL: Duration = Duration::from_millis(100);
const DEFAULT_READY_SETTLE: Duration = Duration::from_secs(2);
const DEFAULT_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
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

/// 一次诊断等待的结果；`diagnostics: None` 表示未验证，绝不能读作干净。
#[derive(Clone, Debug)]
pub(crate) struct DiagnosticOutcome {
    pub(crate) diagnostics: Option<Vec<Diagnostic>>,
    pub(crate) note: Option<String>,
    /// 有版本证明或来自本次拉取。
    /// 供上层直接生成 `ServerReport`，不必解析英文 note。
    pub(crate) status: ReportStatus,
}

impl DiagnosticOutcome {
    /// 有效报告：空诊断是 clean，非空是 issues；调用方不得改写这个判定。
    fn reported(
        diagnostics: Vec<Diagnostic>,
        _version: Option<u64>,
        note: Option<String>,
        _version_proven: bool,
    ) -> Self {
        let status = if diagnostics.is_empty() {
            ReportStatus::ReportedClean
        } else {
            ReportStatus::ReportedIssues
        };
        Self {
            diagnostics: Some(diagnostics),
            note,
            status,
        }
    }

    /// 没有拿到足够新的报告（超时、等待未完成、无缓存 unchanged 等）。
    fn unverified(note: String) -> Self {
        Self {
            diagnostics: None,
            note: Some(note),
            status: ReportStatus::Unreported,
        }
    }

    /// 请求或报告本身失败；不能描述成“没有诊断”。
    fn failed(note: String) -> Self {
        Self {
            diagnostics: None,
            note: Some(note),
            status: ReportStatus::Failed,
        }
    }

    /// 服务器没有可用的诊断能力。
    fn unsupported(note: String) -> Self {
        Self {
            diagnostics: None,
            note: Some(note),
            status: ReportStatus::Unsupported,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Published {
    pub(crate) diagnostics: Vec<Diagnostic>,
    pub(crate) version: Option<u64>,
    pub(crate) seq: u64,
    pub(crate) epoch: u64,
}

/// 诊断缓存：push 报告按 URI 保存，带接收序号与缓存代次。
#[derive(Default)]
pub(crate) struct Diagnostics {
    entries: Mutex<HashMap<String, Published>>,
    seq: AtomicU64,
    epoch: AtomicU64,
    result_ids: Mutex<HashMap<String, String>>,
}

impl Diagnostics {
    pub(crate) fn publish(&self, uri: &str, version: Option<u64>, diagnostics: Vec<Diagnostic>) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let epoch = self.epoch.load(Ordering::SeqCst);
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                uri.to_owned(),
                Published {
                    diagnostics,
                    version,
                    seq,
                    epoch,
                },
            );
    }

    /// 标记某个 URI 的缓存过期（刚提交内容变更时调用）。
    pub(crate) fn invalidate(&self, uri: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = entries.get_mut(uri) {
            entry.seq = 0;
        }
    }

    pub(crate) fn snapshot(&self, uri: &str) -> Option<Published> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)
            .cloned()
    }

    pub(crate) fn seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    /// `workspace/diagnostic/refresh`：使旧代次的报告不再算新鲜。
    pub(crate) fn refresh(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// `unchanged` 报告只有缓存版本与当前文档版本一致、且代次未被刷新时才可信；
    /// 没有可信缓存时必须按未验证处理，绝不能读作干净。
    pub(crate) fn unchanged_report(&self, uri: &str, expected: u64) -> Option<Vec<Diagnostic>> {
        let entry = self.snapshot(uri)?;
        // seq == 0 由 `invalidate` 标记：内容已变，这个报告不再能证明当前版本干净。
        (entry.seq > 0
            && entry.version == Some(expected)
            && entry.epoch == self.epoch.load(Ordering::SeqCst))
        .then_some(entry.diagnostics)
    }

    fn set_result_id(&self, uri: &str, result_id: &str) {
        self.result_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(uri.to_owned(), result_id.to_owned());
    }

    fn result_id(&self, uri: &str) -> Option<String> {
        self.result_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(uri)
            .cloned()
    }
}

/// 进度与项目加载信号。
#[derive(Default)]
pub(crate) struct Signals {
    /// 已 begin 未 end 的进度 token；`report` 不算活动进度。
    active: Mutex<HashSet<String>>,
    /// 已追加工作区根、还没看到由这次追加触发的 `begin` 的加载数。
    pending_loads: AtomicU64,
    pub(crate) closed: AtomicBool,
}

impl Signals {
    fn apply_progress(&self, params: &Value) {
        let Some(token) = params.get("token") else {
            return;
        };
        let key = token.to_string();
        let Some(value) = params.get("value") else {
            return;
        };
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("report")
            .to_owned();
        match kind.as_str() {
            "begin" => {
                self.consume_pending_load();
                self.active
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key);
            }
            "end" => {
                self.active
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&key);
            }
            _ => {}
        }
    }

    fn busy(&self) -> bool {
        !self
            .active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// 追加工作区根之前登记一次等待加载：只有随后出现的 `begin` 才会消费它。
    fn begin_pending_load(&self) {
        self.pending_loads.fetch_add(1, Ordering::SeqCst);
    }

    /// 追加失败时撤销这次登记。
    fn cancel_pending_load(&self) {
        let _ = self
            .pending_loads
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            });
    }

    fn consume_pending_load(&self) {
        let _ = self
            .pending_loads
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            });
    }

    /// 等待所有进度结束且没有未开始的追加；超时、取消或连接关闭返回 false（未验证）。
    ///
    /// 在追加之前开始的进度 token 仍算活动：它稍后的 `end` 不会完成这次等待，
    /// 否则旧项目的结束会被误认为新根已加载。
    fn wait_project_load(&self, deadline: Instant, cancellation: &CancellationToken) -> bool {
        loop {
            if !self.busy() && self.pending_loads.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if self.closed.load(Ordering::SeqCst)
                || cancellation.is_cancelled()
                || Instant::now() >= deadline
            {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }
}

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
                    return Err(crate::cancellation::Cancelled.into());
                }
                match self.request_raw(method, params) {
                    Err(Failure::Cancelled) => Err(crate::cancellation::Cancelled.into()),
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

fn convert_unit(result: Result<(), Failure>, what: &str) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(failure) => Err(failure_to_error(failure, what)),
    }
}

fn convert(result: Result<Value, Failure>, what: &str) -> Result<Value> {
    match result {
        Ok(value) => Ok(value),
        Err(Failure::Cancelled) => Err(crate::cancellation::Cancelled.into()),
        Err(Failure::Timeout) => bail!("LSP {what} timed out"),
        Err(Failure::Rpc(code)) => bail!("LSP {what} failed with error code {code}"),
        Err(Failure::Closed) => {
            bail!("LSP {what} failed: the language server connection is closed")
        }
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
                        message = crate::logging::diagnostic_text(text)
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

    fn folders_json(&self) -> Vec<Value> {
        self.folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|path| folder_json(path))
            .collect()
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
                        return Err(crate::cancellation::Cancelled.into());
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

    /// 采集目标文件的诊断；`diagnostics: None` 表示未验证。
    ///
    /// `deadline` 是绝对期限：启动、等待 push 与 pull 共用这份预算。
    pub(crate) fn collect_diagnostics(
        &self,
        path: &Path,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<DiagnosticOutcome> {
        crate::cancellation::check(cancellation)?;
        if !self.supports_pull() && !self.syncs_documents() {
            return Ok(DiagnosticOutcome::unsupported(
                "The server advertises neither text document synchronization nor pull diagnostics."
                    .to_owned(),
            ));
        }
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        let mut outcome = None;
        self.operation(deadline, cancellation, &mut service, |op| {
            // 基线必须在 didOpen/didChange 之前保存：打开动作触发的 push 才算新鲜。
            let seq_before = self.diagnostics.seq();
            let state = self.open_or_refresh(op, path)?;
            let expected = state.version as u64;
            let uri = state.uri.clone();
            if self.supports_pull() {
                outcome = Some(self.pull(op, &uri, expected, seq_before)?);
            } else {
                outcome = Some(self.wait_push(
                    &uri,
                    expected,
                    seq_before,
                    state.opened_at,
                    deadline,
                    cancellation,
                )?);
            }
            Ok(())
        })?;
        Ok(outcome.unwrap_or_else(|| {
            DiagnosticOutcome::unverified("Diagnostics were not collected.".into())
        }))
    }

    fn supports_pull(&self) -> bool {
        self.capabilities.diagnostic_provider.is_some() || self.transport.diagnostics_registered()
    }

    fn pull(
        &self,
        op: &mut Op<'_>,
        uri: &str,
        expected: u64,
        seq_before: u64,
    ) -> Result<DiagnosticOutcome> {
        let mut params = json!({"textDocument": {"uri": uri}});
        if let Some(result_id) = self.diagnostics.result_id(uri) {
            params["previousResultId"] = json!(result_id);
        }
        let report = match op.request_raw("textDocument/diagnostic", params) {
            Ok(report) => report,
            Err(failure) => return self.pull_failure(failure, uri, expected, seq_before),
        };
        let mut merged = Vec::new();
        let mut note = None;
        match report.get("kind").and_then(Value::as_str) {
            Some("full") => {
                // `items` 必须显式存在且严格解析：畸形报告是失败，不是空报告。
                let items = match full_items(&report) {
                    Ok(items) => items,
                    Err(reason) => {
                        bail!("The server returned a malformed diagnostic report: {reason}.");
                    }
                };
                // 先解析成功，再更新 resultId 与缓存。
                let (related, warnings) = related_reports(&report);
                if let Some(result_id) = report.get("resultId").and_then(Value::as_str) {
                    self.diagnostics.set_result_id(uri, result_id);
                }
                self.diagnostics.publish(uri, Some(expected), items.clone());
                merged.extend(items);
                for (related_uri, items) in related {
                    self.diagnostics.publish(&related_uri, None, items);
                }
                note = join_notes(warnings);
            }
            Some("unchanged") => {
                // 只有版本匹配且代次未刷新的缓存才能证明当前版本没有诊断。
                match self.diagnostics.unchanged_report(uri, expected) {
                    Some(items) => {
                        if let Some(result_id) = report.get("resultId").and_then(Value::as_str) {
                            self.diagnostics.set_result_id(uri, result_id);
                        }
                        merged.extend(items);
                    }
                    None => {
                        return Ok(DiagnosticOutcome::unverified(
                            "The server reported unchanged diagnostics without a fresh cached \
                             report for this document version."
                                .to_owned(),
                        ));
                    }
                }
            }
            _ => {
                bail!("The server returned an unrecognized diagnostic report.");
            }
        }
        Ok(DiagnosticOutcome::reported(
            merged,
            Some(expected),
            note,
            true,
        ))
    }

    /// pull 失败时仍可使用本次基线之后到达、版本匹配的 push；没有则未验证。
    fn pull_failure(
        &self,
        failure: Failure,
        uri: &str,
        expected: u64,
        seq_before: u64,
    ) -> Result<DiagnosticOutcome> {
        match failure {
            Failure::Cancelled => Err(crate::cancellation::Cancelled.into()),
            Failure::Timeout => {
                Ok(self
                    .fresh_push(uri, expected, seq_before)
                    .unwrap_or_else(|| {
                        DiagnosticOutcome::unverified(
                            "The diagnostic request did not finish in time.".to_owned(),
                        )
                    }))
            }
            Failure::Rpc(code) if code == -32801 || code == -32802 => Ok(self
                .fresh_push(uri, expected, seq_before)
                .unwrap_or_else(|| {
                    DiagnosticOutcome::failed(
                        "The server was still computing diagnostics for the changed file."
                            .to_owned(),
                    )
                })),
            Failure::Rpc(code) => {
                Ok(self
                    .fresh_push(uri, expected, seq_before)
                    .unwrap_or_else(|| {
                        DiagnosticOutcome::failed(format!(
                            "The diagnostic request failed with error code {code}."
                        ))
                    }))
            }
            Failure::Closed => {
                Ok(self
                    .fresh_push(uri, expected, seq_before)
                    .unwrap_or_else(|| {
                        DiagnosticOutcome::failed(
                            "The language server connection closed before diagnostics arrived."
                                .to_owned(),
                        )
                    }))
            }
        }
    }

    /// 基线之后到达、版本匹配且代次仍有效的 push 报告。
    fn fresh_push(&self, uri: &str, expected: u64, seq_before: u64) -> Option<DiagnosticOutcome> {
        let entry = self.diagnostics.snapshot(uri)?;
        if entry.seq <= seq_before
            || entry.epoch != self.diagnostics.epoch.load(Ordering::SeqCst)
            || entry.version != Some(expected)
        {
            return None;
        }
        Some(DiagnosticOutcome::reported(
            entry.diagnostics,
            entry.version,
            None,
            true,
        ))
    }

    fn wait_push(
        &self,
        uri: &str,
        expected: u64,
        seq_before: u64,
        opened_at: Instant,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<DiagnosticOutcome> {
        let mut settled_at: Option<Instant> = None;
        let mut last_seq = seq_before;
        loop {
            // 等待期间也响应取消；不能在后台等满 deadline 才返回。
            crate::cancellation::check(cancellation)?;
            if let Some(entry) = self.diagnostics.snapshot(uri)
                && entry.seq > seq_before
                && entry.epoch == self.diagnostics.epoch.load(Ordering::SeqCst)
            {
                match entry.version {
                    Some(version) if version == expected => {
                        return Ok(DiagnosticOutcome::reported(
                            entry.diagnostics,
                            Some(version),
                            None,
                            true,
                        ));
                    }
                    Some(version) => {
                        settled_at = None;
                        last_seq = entry.seq;
                        tracing::debug!(event = "lsp_stale_diagnostics", version);
                    }
                    None => {
                        if entry.seq != last_seq {
                            last_seq = entry.seq;
                            settled_at = Some(Instant::now());
                        }
                        let settled = settled_at.is_some_and(|at| at.elapsed() >= SETTLE);
                        let cold_exception =
                            entry.diagnostics.is_empty() && opened_at.elapsed() < COLD_WINDOW;
                        if settled && !cold_exception {
                            return Ok(DiagnosticOutcome::reported(
                                entry.diagnostics,
                                None,
                                Some(
                                    "The server did not attach a document version to this report."
                                        .to_owned(),
                                ),
                                false,
                            ));
                        }
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(DiagnosticOutcome::unverified(
                    "No fresh diagnostics arrived before the deadline.".to_owned(),
                ));
            }
            std::thread::sleep(POLL);
        }
    }

    /// rust-analyzer 的 workspace-ready 轮询；其它服务器立即返回。
    pub(crate) fn wait_workspace_ready(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if !is_rust_analyzer(&self.command) {
            return Ok(());
        }
        let ready = self.definition.ready;
        let total = ready
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_READY_TIMEOUT);
        let poll = ready
            .poll_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_READY_POLL);
        let settle = ready
            .settle_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_READY_SETTLE);
        let status_timeout = ready
            .status_request_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STATUS_TIMEOUT);
        let started = Instant::now();
        let limit = deadline.min(started + total);
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        self.operation(limit, cancellation, &mut service, |op| {
            loop {
                let poll_deadline = Instant::now() + status_timeout;
                let probe = self.transport.request(
                    "rust-analyzer/analyzerStatus",
                    json!({}),
                    poll_deadline.min(limit),
                    cancellation,
                    op.service,
                );
                if let Ok(Value::String(status)) = probe
                    && !status.starts_with("No workspaces")
                    && started.elapsed() >= settle
                {
                    return Ok(());
                }
                if Instant::now() >= limit {
                    return Ok(());
                }
                std::thread::sleep(poll);
            }
        })
    }

    /// 打开 `path` 前把其 checkout 加入工作区。
    ///
    /// 仅在服务器声明 `workspaceFolders.changeNotifications`、文件位于另一个
    /// checkout，且该 checkout 不包含 HOME 时追加。调用方已经持有操作锁，因此
    /// 这里直接使用同一个 `Op`，不再进入另一个 operation；追加必须发生在首次
    /// didOpen 之前。
    pub(crate) fn ensure_workspace_folder(&self, op: &mut Op<'_>, path: &Path) -> Result<()> {
        let Some(checkout) = self.workspace_folder_to_add(path) else {
            return Ok(());
        };
        if self.folder_listed(&checkout) {
            return Ok(());
        }
        self.deliver_workspace_folder(op, &checkout)
    }

    pub(crate) fn roots_added(&self) -> bool {
        self.roots_added.load(Ordering::SeqCst)
    }

    fn folder_listed(&self, root: &Path) -> bool {
        self.folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|existing| existing == root)
    }

    fn workspace_folder_to_add(&self, path: &Path) -> Option<PathBuf> {
        checkout_to_add(
            &self.root,
            self.capabilities.workspace_folders_change_notifications,
            path,
            home_dir().as_deref(),
        )
    }

    /// 在已持有的操作里发送一次文件夹追加；失败撤销本次新增与加载登记。
    fn deliver_workspace_folder(&self, op: &mut Op<'_>, root: &Path) -> Result<()> {
        // 先登记待加载：旧项目的进度 end 不能完成这次新根的加载等待。
        self.signals.begin_pending_load();
        {
            let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
            folders.push(root.to_path_buf());
        }
        let notified = op.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({"event": {"added": [folder_json(root)], "removed": []}}),
        );
        if let Err(error) = notified {
            let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
            folders.retain(|existing| existing != root);
            self.signals.cancel_pending_load();
            return Err(error);
        }
        self.transport.set_folders(self.folders_json());
        self.roots_added.store(true, Ordering::SeqCst);
        Ok(())
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
            .map_err(|error| crate::cancellation::CleanupError(error).into())
    }

    /// 服务器 `initialize` 返回的完整 capabilities（原样，含扩展字段与
    /// `executeCommandProvider.commands`）。显式能力查询才需要完整 JSON。
    pub(crate) fn capabilities_json(&self) -> Value {
        self.capabilities.raw.clone()
    }
}

/// `full` 报告的 `items` 必须显式存在且严格可解析；缺失或畸形都是失败。
/// 错误信息不回显服务器给的任何值。
fn full_items(report: &Value) -> Result<Vec<Diagnostic>, String> {
    let Some(items) = report.get("items") else {
        return Err("the full report has no items".to_owned());
    };
    serde_json::from_value::<Vec<Diagnostic>>(items.clone())
        .map_err(|_| "the full report's items are not a valid diagnostic array".to_owned())
}

/// `relatedDocuments` 中单个 URI 畸形只跳过该 URI 并记录 warning，不影响目标报告。
fn related_reports(report: &Value) -> (Vec<(String, Vec<Diagnostic>)>, Vec<String>) {
    let Some(related) = report.get("relatedDocuments") else {
        return (Vec::new(), Vec::new());
    };
    let Some(documents) = related.as_object() else {
        return (
            Vec::new(),
            vec!["relatedDocuments was not an object".to_owned()],
        );
    };
    let mut parsed = Vec::new();
    let mut warnings = Vec::new();
    for (uri, document) in documents {
        match document
            .get("items")
            .map(|items| serde_json::from_value::<Vec<Diagnostic>>(items.clone()))
        {
            Some(Ok(items)) => parsed.push((uri.clone(), items)),
            _ => warnings.push("Skipped a malformed related diagnostics report.".to_owned()),
        }
    }
    (parsed, warnings)
}

fn join_notes(notes: Vec<String>) -> Option<String> {
    if notes.is_empty() {
        None
    } else {
        Some(notes.join(" "))
    }
}

/// 单个工作区根的 `WorkspaceFolder` 形态；initialize 与后续追加共用同一形状。
fn folder_json(path: &Path) -> Value {
    json!({
        "uri": path_to_uri(path),
        "name": path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
    })
}

/// 打开 `file` 前应该追加的工作区根：解析 symlink 后文件所在 checkout，
/// 且不是客户端根所在的 checkout、也不是包含 HOME 的根（dotfiles 仓库会把
/// 整个主目录交给服务器索引）。只有声明 `changeNotifications` 的服务器才追加。
fn checkout_to_add(
    root: &Path,
    change_notifications: bool,
    path: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if !change_notifications {
        return None;
    }
    let file = real_path(path);
    let checkout = find_repo_root(file.parent()?)?;
    if find_repo_root(&real_path(root)).as_deref() == Some(checkout.as_path()) {
        return None;
    }
    if let Some(home) = home
        && path_inside(&checkout, &real_path(home))
    {
        return None;
    }
    Some(checkout)
}

/// 解析 symlink 的绝对路径；缺失的尾部保留，逐级解析已存在的父目录。
/// 解析失败时退回词法绝对路径，绝不因为权限/竞态而放弃判断。
fn real_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => canonical,
        Err(_) => match absolute.parent() {
            Some(parent) if parent != absolute.as_path() => match absolute.file_name() {
                Some(name) => real_path(parent).join(name),
                None => absolute,
            },
            _ => absolute,
        },
    }
}

/// 从 `start` 向上找最近的含 `.git`（文件或目录）的目录。
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = real_path(start);
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

/// `path` 是否等于或位于 `workspace` 之内（按路径组件比较）。
fn path_inside(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace).is_ok()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// initialize 握手：协商 UTF-16、保存 initialized 与初始配置推送。
fn initialize_handshake(
    transport: &Transport,
    definition: &ServerDefinition,
    cwd: &Path,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ServerCapabilities> {
    let mut deny = |request: ServerRequest| request.respond(unauthorized_edit());
    let mut service: &mut dyn FnMut(ServerRequest) = &mut deny;
    let result = transport
        .request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "clientInfo": {"name": "pp", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": path_to_uri(cwd),
                "rootPath": cwd.display().to_string(),
                "workspaceFolders": [{
                    "uri": path_to_uri(cwd),
                    "name": cwd.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| cwd.display().to_string()),
                }],
                "capabilities": client_capabilities(),
                "initializationOptions": definition.init_options,
                "trace": "off",
            }),
            deadline,
            cancellation,
            &mut service,
        )
        .map_err(|failure| failure_to_error(failure, "initialize"))?;
    let raw_capabilities = result.get("capabilities").cloned().unwrap_or(Value::Null);
    // 顶层解析（含 `diagnosticProvider`、`executeCommandProvider` 与
    // `workspace.workspaceFolders.changeNotifications`）都在 ServerCapabilities::parse 里，
    // 这里不再用 raw JSON 打补丁，避免两套能力约定。
    let capabilities = ServerCapabilities::parse(raw_capabilities);
    if let Some(encoding) = capabilities.position_encoding.as_deref()
        && !encoding.eq_ignore_ascii_case("utf-16")
    {
        bail!(
            "language server {} selected unsupported position encoding `{}`",
            definition.name,
            encoding
        );
    }
    convert_unit(transport.notify("initialized", json!({})), "initialized")?;
    convert_unit(
        transport.notify(
            "workspace/didChangeConfiguration",
            json!({"settings": definition.settings}),
        ),
        "workspace/didChangeConfiguration",
    )?;
    Ok(capabilities)
}

fn unauthorized_edit() -> Value {
    json!({"applied": false, "failureReason": UNAUTHORIZED_EDIT})
}

fn failure_to_error(failure: Failure, what: &str) -> anyhow::Error {
    match failure {
        Failure::Cancelled => crate::cancellation::Cancelled.into(),
        Failure::Timeout => anyhow::anyhow!("LSP {what} timed out"),
        Failure::Rpc(code) => anyhow::anyhow!("LSP {what} failed with error code {code}"),
        Failure::Closed => anyhow::anyhow!("LSP {what} failed: the connection is closed"),
    }
}

/// 初始化失败时的收尾：回收成功才返回原始错误；回收失败时保留
/// `CleanupError`（registry 据此不写负缓存并可重试 slot），同时保留初始化原因文本。
fn startup_failure(startup: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => startup,
        Err(cleanup) => {
            let cleanup: anyhow::Error = crate::cancellation::CleanupError(cleanup).into();
            cleanup.context(format!("language server failed to start: {startup:#}"))
        }
    }
}

pub(crate) fn is_rust_analyzer(command: &Path) -> bool {
    command
        .file_name()
        .map(|name| {
            let name = name.to_string_lossy().to_ascii_lowercase();
            name == "rust-analyzer" || name == "rust-analyzer.exe"
        })
        .unwrap_or(false)
}

fn client_capabilities() -> Value {
    json!({
        "general": {
            "positionEncodings": ["utf-16"],
            "markdown": {"parser": "none"},
        },
        "textDocument": {
            "synchronization": {
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            },
            "definition": {"dynamicRegistration": false, "linkSupport": true},
            "typeDefinition": {"dynamicRegistration": false, "linkSupport": true},
            "implementation": {"dynamicRegistration": false, "linkSupport": true},
            "references": {"dynamicRegistration": false},
            "hover": {"dynamicRegistration": false, "contentFormat": ["markdown", "plaintext"]},
            "documentSymbol": {
                "dynamicRegistration": false,
                "hierarchicalDocumentSymbolSupport": true,
            },
            "rename": {"dynamicRegistration": false, "prepareSupport": true},
            "codeAction": {
                "dynamicRegistration": false,
                "codeActionLiteralSupport": {
                    "codeActionKind": {
                        "valueSet": [
                            "quickfix",
                            "refactor",
                            "refactor.extract",
                            "refactor.inline",
                            "refactor.rewrite",
                            "source",
                            "source.organizeImports",
                            "source.fixAll",
                        ],
                    },
                },
                "resolveSupport": {"properties": ["edit"]},
            },
            "publishDiagnostics": {
                "relatedInformation": true,
                "versionSupport": true,
                "tagSupport": {"valueSet": [1, 2]},
                "codeDescriptionSupport": true,
                "dataSupport": true,
            },
            "diagnostic": {"dynamicRegistration": true},
        },
        "window": {"workDoneProgress": true},
        "workspace": {
            "applyEdit": false,
            "workspaceEdit": {
                "documentChanges": true,
                "resourceOperations": ["create", "rename", "delete"],
                            },
            "configuration": true,
            "workspaceFolders": true,
            "symbol": {"dynamicRegistration": false},
            "fileOperations": {
                "dynamicRegistration": false,
                "willCreate": false,
                "didCreate": false,
                "willRename": true,
                "didRename": false,
                "willDelete": false,
                "didDelete": false,
            },
        },
    })
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

/// 全量替换用的范围：覆盖旧内容的最后一行末列。
fn whole_range(previous: &str) -> Value {
    let mut line = 0u32;
    let mut character = 0u32;
    for ch in previous.chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }
    json!({
        "start": {"line": 0, "character": 0},
        "end": {"line": line, "character": character},
    })
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

/// 路径 → `file:` URI，按字节百分号编码。
///
/// 先做纯词法绝对化：`..` 弹出上一段，而不是被丢弃——`/a/../b` 必须是 `/b`。
/// 不做 canonicalize：新文件可能还不存在，语义导航也允许工作区外路径；符号
/// 链接保持调用方给出的字面路径。
pub(crate) fn path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    let mut encoded = String::from("file://");
    for component in normalized.components() {
        match component {
            Component::RootDir => encoded.push('/'),
            Component::Normal(part) => {
                encoded.push_str(&encode_segment(&part.to_string_lossy()));
                encoded.push('/');
            }
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {}
        }
    }
    if encoded.len() > "file:///".len() && encoded.ends_with('/') {
        encoded.pop();
    }
    encoded
}

fn encode_segment(segment: &str) -> String {
    const SAFE: &[u8] = b"-._~!$&'()*+,;=:@";
    let mut encoded = String::new();
    for byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || SAFE.contains(byte) {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// `file:` URI → 路径；非 file scheme、远端主机与非法编码都返回 `None`。
///
/// 未转义的 `?`/`#` 是 URI 的 query/fragment，不属于文件名，必须拒绝；而
/// `%3F`/`%23` 解码出的 `?`/`#` 是合法的文件名字符。解码后的 NUL 永远拒绝。
pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    if rest.contains('?') || rest.contains('#') {
        return None;
    }
    let rest = match rest.strip_prefix("localhost/") {
        Some(rest) => format!("/{rest}"),
        None => rest.to_owned(),
    };
    let rest = if rest.starts_with('/') {
        rest
    } else {
        return None;
    };
    let mut bytes = Vec::new();
    let raw = rest.as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%' {
            let hex = raw.get(index + 1..index + 3)?;
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            bytes.push(value);
            index += 3;
        } else {
            bytes.push(raw[index]);
            index += 1;
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    Some(PathBuf::from(text))
}

/// 位置换算：按 UTF-16 列从行首切分。
///
/// 返回 `None` 表示行不存在、列超出该行，或列落在代理对中间：调用方必须
/// 拒绝这个位置，不能猜测相邻位置。CRLF 的 `\r` 不属于行内容。
pub(crate) fn position_offset(text: &str, position: Position) -> Option<usize> {
    let mut line_start = 0;
    for _ in 0..position.line {
        let index = text[line_start..].find('\n')? + line_start;
        line_start = index + 1;
    }
    let mut line_end = text[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len());
    if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    let mut units = 0u32;
    for (offset, ch) in text[line_start..line_end].char_indices() {
        if units == position.character {
            return Some(line_start + offset);
        }
        units = units.checked_add(ch.len_utf16() as u32)?;
        if units > position.character {
            return None;
        }
    }
    (units == position.character).then_some(line_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::config;
    use std::sync::mpsc;
    use tempfile::TempDir;

    /// 用受控 stdio fixture 启动一个独立客户端；不读取真实 HOME，也不经 registry。
    fn fixture_client(pull: &[&str]) -> (TempDir, Arc<Client>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".pp")).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp_server.py");
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
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp_server.py");
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
