//! Diagnostic cache (push/pull), progress and project-load signals, and the client methods that collect per-document diagnostics.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use crate::transport::{Failure, ServerRequest};
use crate::types::Diagnostic;
use pp_core::cancellation::CancellationToken;
use pp_core::lsp_report::ReportStatus;

use super::handshake::unauthorized_edit;
use super::{COLD_WINDOW, Client, Op, POLL, SETTLE};

/// Result of one diagnostic wait; `diagnostics: None` means unverified and must never be read as clean.
#[derive(Clone, Debug)]
pub(crate) struct DiagnosticOutcome {
    pub(crate) diagnostics: Option<Vec<Diagnostic>>,
    pub(crate) note: Option<String>,
    /// Either backed by a version proof or produced by this pull.
    /// Lets upper layers build a `ServerReport` directly without parsing the English note.
    pub(crate) status: ReportStatus,
}

impl DiagnosticOutcome {
    /// Valid report: empty diagnostics are clean, non-empty are issues; callers must not override this decision.
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

    /// No sufficiently fresh report was obtained (timeout, incomplete wait, cached unchanged, etc.).
    fn unverified(note: String) -> Self {
        Self {
            diagnostics: None,
            note: Some(note),
            status: ReportStatus::Unreported,
        }
    }

    /// The request or the report itself failed; it cannot be described as "no diagnostics".
    fn failed(note: String) -> Self {
        Self {
            diagnostics: None,
            note: Some(note),
            status: ReportStatus::Failed,
        }
    }

    /// The server has no usable diagnostics capability.
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

/// Diagnostic cache: push reports are stored per URI with a receive sequence number and a cache epoch.
#[derive(Default)]
pub(crate) struct Diagnostics {
    entries: Mutex<HashMap<String, Published>>,
    seq: AtomicU64,
    epoch: AtomicU64,
    result_ids: Mutex<HashMap<String, String>>,
}

impl Diagnostics {
    pub(crate) fn publish(&self, uri: &str, version: Option<u64>, diagnostics: Vec<Diagnostic>) {
        // Allocate `seq` and read `epoch` while holding the `entries` lock, avoiding out-of-order overwrites with `invalidate()` and concurrent `pull()`.
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        let epoch = self.epoch.load(Ordering::SeqCst);
        entries.insert(
            uri.to_owned(),
            Published {
                diagnostics,
                version,
                seq,
                epoch,
            },
        );
    }

    /// Mark the cache for a URI stale (called when content was just committed).
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

    /// Unified freshness decision: `seq > 0` (not marked stale by [`Diagnostics::invalidate`]) and
    /// `epoch` is still the current generation (not advanced by [`Diagnostics::refresh`]).
    ///
    /// Whether the version matches is appended separately per caller semantics; but these two baselines are decided only here,
    /// and push reuse, `unchanged` reports and quick-fix context must not each implement their own.
    pub(crate) fn fresh_snapshot(&self, uri: &str) -> Option<Published> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        let entry = self.snapshot(uri)?;
        (entry.seq > 0 && entry.epoch == epoch).then_some(entry)
    }

    /// `workspace/diagnostic/refresh`: reports from older generations no longer count as fresh.
    pub(crate) fn refresh(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// An `unchanged` report is trustworthy only when the cached version matches the current document version and the generation was not refreshed;
    /// without a trustworthy cache it must be treated as unverified, never read as clean.
    pub(crate) fn unchanged_report(&self, uri: &str, expected: u64) -> Option<Vec<Diagnostic>> {
        // Freshness is decided uniformly by `fresh_snapshot`; only "version must match the current document" is added here.
        let entry = self.fresh_snapshot(uri)?;
        (entry.version == Some(expected)).then_some(entry.diagnostics)
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

/// Progress and project-load signals.
#[derive(Default)]
pub(crate) struct Signals {
    /// Progress tokens that began but have not ended; `report` does not count as active progress.
    active: Mutex<HashSet<String>>,
    /// Number of loads whose workspace root was appended but whose triggering `begin` has not been seen yet.
    pending_loads: AtomicU64,
    pub(crate) closed: AtomicBool,
}

/// Atomic saturating decrement: `try_update` is beyond MSRV 1.88 and `fetch_update` was renamed and deprecated after it,
/// so a CAS loop expresses the "never underflow" semantics.
fn saturating_decrement(counter: &AtomicU64) {
    let mut expected = counter.load(Ordering::SeqCst);
    loop {
        let Some(next) = expected.checked_sub(1) else {
            return;
        };
        match counter.compare_exchange_weak(expected, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return,
            Err(observed) => expected = observed,
        }
    }
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

    /// Register one pending load before appending a workspace root: only a subsequently observed `begin` consumes it.
    pub(super) fn begin_pending_load(&self) {
        self.pending_loads.fetch_add(1, Ordering::SeqCst);
    }

    /// Undo that registration when the append fails.
    pub(super) fn cancel_pending_load(&self) {
        saturating_decrement(&self.pending_loads);
    }

    fn consume_pending_load(&self) {
        saturating_decrement(&self.pending_loads);
    }

    /// Wait until all progress ends and no append is still pending; timeout, cancellation or a closed connection returns false (unverified).
    ///
    /// A progress token that started before the append still counts as active: its later `end` does not complete this wait,
    /// otherwise the end of the old project would be mistaken for the new root being loaded.
    pub(super) fn wait_project_load_with(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
        is_closed: impl Fn() -> bool,
    ) -> bool {
        loop {
            if !self.busy() && self.pending_loads.load(Ordering::SeqCst) == 0 {
                return true;
            }
            if self.closed.load(Ordering::SeqCst)
                || is_closed()
                || cancellation.is_cancelled()
                || Instant::now() >= deadline
            {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }
}

struct PushTarget<'a> {
    uri: &'a str,
    expected: u64,
    seq_before: u64,
    synced: bool,
    opened_at: Instant,
}

impl Client {
    /// Collect diagnostics for the target file; `diagnostics: None` means unverified.
    ///
    /// `deadline` is an absolute deadline: startup, waiting for pushes and the pull all share this budget.
    pub(crate) fn collect_diagnostics(
        &self,
        path: &Path,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<DiagnosticOutcome> {
        pp_core::cancellation::check(cancellation)?;
        if !self.supports_pull() && !self.syncs_documents() {
            return Ok(DiagnosticOutcome::unsupported(
                "The server advertises neither text document synchronization nor pull diagnostics."
                    .to_owned(),
            ));
        }
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        let mut outcome = None;
        self.operation(deadline, cancellation, &mut service, |op| {
            // The baseline must be recorded before didOpen/didChange so pushes triggered by the open count as fresh.
            let seq_before = self.diagnostics.seq();
            let (state, synced) = self.open_or_refresh_with_sync(op, path)?;
            let expected = state.version as u64;
            let uri = state.uri.clone();
            if self.supports_pull() {
                outcome = Some(self.pull(op, &uri, expected, seq_before)?);
            } else {
                outcome = Some(self.wait_push(
                    PushTarget {
                        uri: &uri,
                        expected,
                        seq_before,
                        synced,
                        opened_at: state.opened_at,
                    },
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
                // `items` must be present explicitly and parse strictly: a malformed report is a failure, not an empty report.
                let items = match full_items(&report) {
                    Ok(items) => items,
                    Err(reason) => {
                        bail!("The server returned a malformed diagnostic report: {reason}.");
                    }
                };
                // Parse successfully first, then update resultId and the cache.
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
                // Only a cache whose version matches and whose generation was not refreshed can prove the current version has no diagnostics.
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

    /// When the pull fails, a version-matching push that arrived after this baseline is still usable; otherwise unverified.
    fn pull_failure(
        &self,
        failure: Failure,
        uri: &str,
        expected: u64,
        seq_before: u64,
    ) -> Result<DiagnosticOutcome> {
        match failure {
            Failure::Cancelled => Err(pp_core::cancellation::Cancelled.into()),
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

    /// A push report that arrived after the baseline, matches the version and is still in a valid generation.
    fn fresh_push(&self, uri: &str, expected: u64, seq_before: u64) -> Option<DiagnosticOutcome> {
        // A report that arrived before the baseline cannot prove the state after this request, hence also compare `seq_before`.
        let entry = self.diagnostics.fresh_snapshot(uri)?;
        if entry.seq <= seq_before || entry.version != Some(expected) {
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
        target: PushTarget<'_>,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<DiagnosticOutcome> {
        let PushTarget {
            uri,
            expected,
            seq_before,
            synced,
            opened_at,
        } = target;
        // When this call sent no didOpen/didChange and a fresh cache exists for the current version and epoch, reuse it directly,
        // avoiding a redundant query of an unchanged file on push-only servers or waiting until timeout when save linkage returned before collection.
        if !synced && let Some(entry) = self.diagnostics.fresh_snapshot(uri) {
            match entry.version {
                Some(version) if version == expected => {
                    return Ok(DiagnosticOutcome::reported(
                        entry.diagnostics,
                        Some(version),
                        None,
                        true,
                    ));
                }
                None if !(entry.diagnostics.is_empty() && opened_at.elapsed() < COLD_WINDOW) => {
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
                _ => {}
            }
        }
        let mut settled_at: Option<Instant> = None;
        let mut last_seq = seq_before;
        loop {
            // Cancellation is honored while waiting too; the call must not sit until the deadline and then return.
            pp_core::cancellation::check(cancellation)?;
            if let Some(entry) = self.diagnostics.fresh_snapshot(uri)
                && entry.seq > seq_before
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
            if self.is_closed() {
                return Ok(DiagnosticOutcome::failed(
                    "The language server connection closed before diagnostics arrived.".to_owned(),
                ));
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

/// `items` of a `full` report must be present explicitly and strictly parseable; missing or malformed is a failure.
/// Error messages never echo any value the server supplied.
pub(super) fn full_items(report: &Value) -> Result<Vec<Diagnostic>, String> {
    let Some(items) = report.get("items") else {
        return Err("the full report has no items".to_owned());
    };
    serde_json::from_value::<Vec<Diagnostic>>(items.clone())
        .map_err(|_| "the full report's items are not a valid diagnostic array".to_owned())
}

/// A malformed single URI in `relatedDocuments` only skips that URI and records a warning; the target report is unaffected.
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

#[cfg(test)]
mod integration {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::lsp_report::ReportStatus;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    #[test]
    fn lsp_client_pull_malformed_unchanged_and_issues_transitions() {
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

        {
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
            assert_eq!(outcome.status, ReportStatus::ReportedIssues);
            assert!(outcome.diagnostics.is_some_and(|items| !items.is_empty()));
            client.shutdown().unwrap();
        }

        {
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
    }

    #[test]
    fn lsp_client_diagnostics_cache_progress_and_related_docs() {
        {
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

        {
            let signals = Signals::default();
            let token = CancellationToken::new();
            let progress =
                |value: Value| signals.apply_progress(&json!({"token": "t", "value": value}));
            progress(json!({"kind": "begin"}));
            signals.begin_pending_load();
            progress(json!({"kind": "end"}));
            assert!(
                !signals.wait_project_load_with(
                    Instant::now() + Duration::from_millis(80),
                    &token,
                    || false,
                ),
                "a progress end that began before the add completed the new root's load"
            );
            progress(json!({"kind": "begin"}));
            progress(json!({"kind": "end"}));
            assert!(signals.wait_project_load_with(
                Instant::now() + Duration::from_secs(1),
                &token,
                || false,
            ));
        }

        {
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
    }

    #[test]
    fn lsp_client_wait_push_reuses_fresh_cached_report_when_not_resynced() {
        let (_dir, client) = fixture_client(&[]);
        let uri = "file:///cached.fixture";
        let diag: Diagnostic = serde_json::from_value(json!({
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
            "message": "cached error"
        }))
        .unwrap();
        client.diagnostics.publish(uri, Some(2), vec![diag]);
        let seq_before = client.diagnostics.seq();

        let outcome = client
            .wait_push(
                PushTarget {
                    uri,
                    expected: 2,
                    seq_before,
                    synced: false,
                    opened_at: Instant::now(),
                },
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(outcome.status, ReportStatus::ReportedIssues);
        assert_eq!(outcome.diagnostics.as_ref().map(Vec::len), Some(1));

        // When a new sync happened, a newer push must be awaited even if a stale cache with the same version exists; it must not be misused.
        let stale_outcome = client
            .wait_push(
                PushTarget {
                    uri,
                    expected: 2,
                    seq_before,
                    synced: true,
                    opened_at: Instant::now(),
                },
                Instant::now() + Duration::from_millis(20),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(stale_outcome.status, ReportStatus::Unreported);
        client.shutdown().unwrap();
    }
}
