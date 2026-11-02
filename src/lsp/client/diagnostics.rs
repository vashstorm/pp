//! 诊断缓存（push/pull）、进度与项目加载信号，以及按文档采集诊断的客户端方法。

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::cancellation::CancellationToken;
use crate::lsp::ReportStatus;
use crate::lsp::transport::{Failure, ServerRequest};
use crate::lsp::types::Diagnostic;

use super::handshake::unauthorized_edit;
use super::{COLD_WINDOW, Client, Op, POLL, SETTLE};

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
    pub(super) fn apply_progress(&self, params: &Value) {
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
    pub(super) fn begin_pending_load(&self) {
        self.pending_loads.fetch_add(1, Ordering::SeqCst);
    }

    /// 追加失败时撤销这次登记。
    pub(super) fn cancel_pending_load(&self) {
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
    pub(super) fn wait_project_load(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> bool {
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

impl Client {
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
}

/// `full` 报告的 `items` 必须显式存在且严格可解析；缺失或畸形都是失败。
/// 错误信息不回显服务器给的任何值。
pub(super) fn full_items(report: &Value) -> Result<Vec<Diagnostic>, String> {
    let Some(items) = report.get("items") else {
        return Err("the full report has no items".to_owned());
    };
    serde_json::from_value::<Vec<Diagnostic>>(items.clone())
        .map_err(|_| "the full report's items are not a valid diagnostic array".to_owned())
}

/// `relatedDocuments` 中单个 URI 畸形只跳过该 URI 并记录 warning，不影响目标报告。
pub(super) fn related_reports(report: &Value) -> (Vec<(String, Vec<Diagnostic>)>, Vec<String>) {
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

pub(super) fn join_notes(notes: Vec<String>) -> Option<String> {
    if notes.is_empty() {
        None
    } else {
        Some(notes.join(" "))
    }
}
