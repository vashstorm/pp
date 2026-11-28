//! Diagnostic actions: target resolution, per-file collection and structured reports.

use crate::Registry;
use crate::diagnostics;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::time::Instant;

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use pp_core::lsp_report::ReportStatus;

use super::{Access, Action, LspToolHost, Params, done, owner, with_diagnostics};

pub(super) fn diagnostics_action(
    host: &dyn LspToolHost,
    registry: &Registry,
    params: &Params,
    _access: Access,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Diagnostics;
    let file = params.file()?;
    ensure!(
        file != "*",
        "Workspace diagnostics are not run by LSP. Use bash for cargo check, tsc, go build or other project checks."
    );
    // Without an explicit timeout the diagnostics wait follows requestTimeout; an explicit timeout has already shortened
    // the call deadline, and both share the same absolute deadline.
    let diagnostic_deadline = match params.timeout {
        Some(_) => deadline,
        None => deadline.min(Instant::now() + registry.settings().request_budget()),
    };
    let targets = diagnostics::resolve_targets(registry.cwd(), file, cancellation)?;
    if targets.paths.is_empty() {
        return Ok(done(
            action,
            false,
            format!("No files matched pattern: {file}"),
        ));
    }
    let mut sections = Vec::new();
    if targets.truncated {
        sections.push(format!(
            "Pattern matched more than {} files; showing the first {} in sorted order.",
            diagnostics::MAX_TARGETS,
            diagnostics::MAX_TARGETS
        ));
    }
    let mut success = true;
    let mut reports = Vec::with_capacity(targets.paths.len());
    for path in &targets.paths {
        check(cancellation)?;
        let report = diagnostics::collect(
            registry,
            path,
            "lsp",
            owner(host),
            false,
            diagnostic_deadline,
            cancellation,
        )?;
        // Only when every server failed is the call unsuccessful; unverified or missing capability always shows up explicitly in the body.
        success &= report
            .servers
            .iter()
            .any(|server| server.status != ReportStatus::Failed);
        sections.push(diagnostics::render(
            &report,
            Some(diagnostics::EXPLICIT_MESSAGE_LIMIT),
        ));
        reports.push(report);
    }
    let content = sections.join("\n\n");
    let value = done(action, success, content);
    let structured = if reports.len() == 1 {
        json!(reports.pop())
    } else {
        Value::Array(reports.into_iter().map(|report| json!(report)).collect())
    };
    Ok(with_diagnostics(value, structured))
}
