//! 诊断动作：目标解析、逐文件采集与结构化报告。

use crate::lsp::Registry;
use crate::lsp::diagnostics;
use crate::tools::Tools;
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::time::Instant;

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use pp_core::lsp_report::ReportStatus;

use super::{Access, Action, Params, done, owner, with_diagnostics};

pub(super) fn diagnostics_action(
    tools: &Tools,
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
    // 未显式给 timeout 时诊断等待沿用 requestTimeout；显式 timeout 已经缩短
    // 了 call deadline，两者共用同一个绝对期限。
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
            owner(tools),
            false,
            diagnostic_deadline,
            cancellation,
        )?;
        // 全部服务器失败才不成功；未验证或没有能力都显式出现在正文里。
        success &= report
            .servers
            .iter()
            .any(|server| server.status != ReportStatus::Failed);
        sections.push(diagnostics::render(&report, None));
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
