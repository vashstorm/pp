//! 任务与模型请求收尾的结构化诊断日志。
use super::*;

pub(in crate::agent) fn log_task_finished(
    operation: &str,
    started: std::time::Instant,
    result: &Result<RunOutcome>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let pid = std::process::id();
    match result {
        Ok(RunOutcome::Completed) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "ok",
            elapsed_ms
        ),
        Ok(RunOutcome::AwaitingPlanReview) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "awaiting_plan_review",
            elapsed_ms
        ),
        Ok(RunOutcome::Cancelled) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "cancelled",
            elapsed_ms
        ),
        Err(error) => tracing::error!(
            event = "task_finished",
            pid,
            operation,
            error = pp_core::logging::diagnostic_text(&format!("{error:#}")),
            status = "error",
            elapsed_ms
        ),
    }
}

pub(in crate::agent) fn log_model_request_finished<T>(
    protocol: &str,
    started: std::time::Instant,
    outcome: &anyhow::Result<T>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let pid = std::process::id();
    match outcome {
        Ok(_) => tracing::info!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "ok",
            elapsed_ms
        ),
        Err(error) if error.is::<Cancelled>() => tracing::info!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "cancelled",
            elapsed_ms
        ),
        Err(error) => tracing::error!(
            event = "model_request_finished",
            pid,
            protocol,
            error = pp_core::logging::diagnostic_text(&format!("{error:#}")),
            status = "error",
            elapsed_ms
        ),
    }
}
