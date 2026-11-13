//! 行模式执行单元：正常 Agent 任务与 dev 构建的内嵌 mock 场景。

use anyhow::{Context, Result};
use std::io::Write;

use super::banner::cli_lsp_report;
use super::events::{cli_context_event, cli_output_result, cli_output_text, cli_write};
use pp_core::cancellation;
use pp_core::events;
use pp_runtime::agent;
#[cfg(debug_assertions)]
use pp_runtime::mock_ai;

/// 行模式下单个执行单元：正常 Agent 任务，或 dev 构建的内嵌 mock 场景。
enum CliExecution<'a> {
    Agent {
        agent: &'a mut agent::Agent,
        prompt: Option<&'a str>,
    },
    #[cfg(debug_assertions)]
    Mock {
        test: mock_ai::MockTest,
        endpoint: mock_ai::MockEndpoint,
    },
}

pub fn run_cli_task(
    agent: &mut agent::Agent,
    prompt: Option<&str>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    run_cli_execution(CliExecution::Agent { agent, prompt }, stdout, stderr)
}

/// dev/test 专用入口：用内嵌 mock server 运行真实 Agent 场景，不触碰正常会话。
#[cfg(debug_assertions)]
pub fn run_cli_mock_test(
    test: mock_ai::MockTest,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    let endpoint = mock_ai::current_endpoint()?;
    run_cli_execution(CliExecution::Mock { test, endpoint }, stdout, stderr)
}

fn run_cli_execution(
    mut execution: CliExecution<'_>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    match &execution {
        CliExecution::Agent { agent, .. } => {
            for warning in agent.mcp_view().warnings() {
                writeln!(stderr, "{warning}")?;
            }
        }
        #[cfg(debug_assertions)]
        CliExecution::Mock { .. } => {}
    }
    let mut first_error = None;
    let (mut out_broken, mut err_broken) = (false, false);
    let (mut visible, mut content, mut newline) = (false, false, true);
    let mut current_tool = None;
    let mut thinking_newline = true;
    let mut context_usage = None;
    let mut streamed_output = String::new();
    let mut emit = |event: AgentEvent| {
        // 运行时指标事件只服务 TUI，CLI 不输出，也不得触发思考换行。
        if matches!(
            &event,
            AgentEvent::ModelRequestStarted
                | AgentEvent::ToolArgumentsDelta { .. }
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. }
                | AgentEvent::UserInputAccepted { .. }
                | AgentEvent::PlanProgressUpdated { .. }
                | AgentEvent::ProjectInstructionsLoaded { .. }
        ) {
            return;
        }
        if !matches!(
            &event,
            AgentEvent::ThinkingDelta { .. } | AgentEvent::ReasoningSummaryDelta { .. }
        ) && !thinking_newline
        {
            cli_write(stderr, "\n", &mut err_broken, &mut first_error);
            thinking_newline = true;
        }
        match event {
            event @ (AgentEvent::ContextUpdated { .. }
            | AgentEvent::CompactionStarted { .. }
            | AgentEvent::CompactionFinished { .. }) => cli_context_event(
                event,
                &mut context_usage,
                stderr,
                &mut err_broken,
                &mut first_error,
            ),
            AgentEvent::ModelAttemptFailed {
                attempt,
                model,
                error,
            } => {
                if visible && !newline {
                    cli_write(stdout, "\n", &mut out_broken, &mut first_error);
                }
                cli_write(
                    stderr,
                    &format!(
                        "Model attempt {attempt} failed; partial output is excluded from history.\nModel: {model}\nReason: {error}\n"
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
                visible = false;
                newline = true;
            }
            AgentEvent::ModelRouteChanged { model, source, .. } => {
                cli_write(
                    stderr,
                    &format!("{}: {model}\n", source.describe()),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::AssistantStarted => {
                visible = false;
                content = false;
                newline = true;
            }
            AgentEvent::ThinkingDelta { text } | AgentEvent::ReasoningSummaryDelta { text } => {
                if !text.is_empty() {
                    thinking_newline = text.ends_with('\n');
                }
                cli_write(stderr, &text, &mut err_broken, &mut first_error);
            }
            AgentEvent::AssistantDelta {
                text,
                refusal: false,
            } => {
                if !text.is_empty() {
                    cli_write(stdout, &text, &mut out_broken, &mut first_error);
                    visible = true;
                    content |= !text.trim().is_empty();
                    newline = text.ends_with('\n');
                }
            }
            AgentEvent::AssistantDelta { refusal: true, .. } => {}
            AgentEvent::AssistantFinished { text } => {
                if !content && !text.is_empty() {
                    cli_write(stdout, &text, &mut out_broken, &mut first_error);
                    visible = true;
                    newline = text.ends_with('\n');
                }
                if visible && !newline {
                    cli_write(stdout, "\n", &mut out_broken, &mut first_error);
                }
                visible = false;
                newline = true;
            }
            AgentEvent::ToolStarted { id, name, .. } => {
                cli_write(
                    stderr,
                    &format!("tool {name}: running\n"),
                    &mut err_broken,
                    &mut first_error,
                );
                streamed_output.clear();
                current_tool = Some((id, name));
            }
            AgentEvent::FileChanged { .. } => {}
            AgentEvent::ToolOutputSnapshot { id, output } => {
                if !current_tool
                    .as_ref()
                    .is_some_and(|(current_id, name)| current_id == &id && name == "wait")
                {
                    let delta = cli_output_text(&output, &mut streamed_output, false, "bash");
                    cli_write(stderr, &delta, &mut err_broken, &mut first_error);
                }
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                let name = current_tool
                    .as_ref()
                    .filter(|(current_id, _)| current_id == &id)
                    .map_or("tool", |(_, name)| name.as_str());
                let label = match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                };
                cli_write(
                    stderr,
                    &format!("[{name} {label}] {text}"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::ToolFinished { id, result } => {
                if current_tool
                    .as_ref()
                    .is_some_and(|(current_id, name)| current_id == &id && name == "wait")
                    && let Some(jobs) = result["jobs"].as_array()
                {
                    let running = jobs
                        .iter()
                        .filter(|job| matches!(job["status"].as_str(), Some("queued" | "running")))
                        .count();
                    let suffix = if result["interrupted"] == true {
                        "; interrupted by steering"
                    } else if result["timed_out"] == true {
                        "; timed out"
                    } else {
                        ""
                    };
                    cli_write(
                        stderr,
                        &format!(
                            "wait: {} settled, {running} running{suffix}\n",
                            jobs.len() - running
                        ),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if result["output"].is_string() {
                    cli_write(
                        stderr,
                        &cli_output_result(&result, &mut streamed_output, "bash"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if current_tool.as_ref().is_some_and(|(current_id, name)| {
                    current_id == &id
                        && (matches!(name.as_str(), "rg" | "glob" | "todo")
                            || name == "read"
                                && result["kind"] != "image"
                                && result.get("pipe").is_none())
                }) && let Some(content) = result["content"]
                    .as_str()
                    .or_else(|| result["error"].as_str())
                {
                    let content = if current_tool
                        .as_ref()
                        .is_some_and(|(_, name)| matches!(name.as_str(), "read" | "todo"))
                    {
                        pp_core::text::display_text(content)
                    } else {
                        std::borrow::Cow::Borrowed(content)
                    };
                    cli_write(
                        stderr,
                        &format!("{content}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                    if let Some(warnings) = result["warnings"].as_array() {
                        for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
                            cli_write(
                                stderr,
                                &format!("Warning: {warning}\n"),
                                &mut err_broken,
                                &mut first_error,
                            );
                        }
                    }
                }
                if result["kind"] == "image"
                    && let Some(content) = result["content"].as_str()
                {
                    cli_write(
                        stderr,
                        &format!("{content}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if let (Some(task_id), Some(status)) =
                    (result["task_id"].as_str(), result["status"].as_str())
                {
                    cli_write(
                        stderr,
                        &format!("task {task_id}: {status}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                // 内联 LspDiagnostics 只来自 write/edit；lsp 工具正文在这里展示。
                let lsp_content = current_tool
                    .as_ref()
                    .is_some_and(|(current_id, name)| current_id == &id && name.as_str() == "lsp");
                if lsp_content && let Some(content) = result["content"].as_str() {
                    cli_write(
                        stderr,
                        &format!("\n{}\n", pp_core::text::display_text(content)),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if let Some((current_id, name)) = current_tool.take()
                    && current_id == id
                {
                    let status = if result["ok"] == true { "ok" } else { "failed" };
                    cli_write(
                        stderr,
                        &format!("\ntool {name}: {status}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
            }
            AgentEvent::TodoSnapshot {
                revision,
                phases,
                status,
                ..
            } => {
                let preview = if matches!(
                    status,
                    pp_core::plan::PlanStatus::Draft | pp_core::plan::PlanStatus::Ready
                ) {
                    " · Preview (not executing)"
                } else {
                    ""
                };
                cli_write(
                    stderr,
                    &format!(
                        "Plan tasks · {status:?} · r{revision}{preview}\n{}\n",
                        pp_core::text::display_text(&pp_runtime::tools::todo::format_summary(
                            &phases,
                            &[],
                            true
                        ))
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::TodoReminder {
                todos,
                attempt,
                max_attempts,
            } => {
                // stderr 提醒与 open 项；stdout 仍只保留助手文本。
                let open: Vec<&pp_core::todo::TodoItem> = todos
                    .iter()
                    .filter(|item| {
                        !matches!(
                            item.status,
                            pp_core::todo::TodoStatus::Completed
                                | pp_core::todo::TodoStatus::Abandoned
                        )
                    })
                    .collect();
                cli_write(
                    stderr,
                    &format!(
                        "{} incomplete todos - reminder {attempt}/{max_attempts}\n",
                        open.len()
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
                for item in &open {
                    cli_write(
                        stderr,
                        &format!("  - {}\n", pp_core::text::display_text(&item.content)),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
            }
            AgentEvent::RequestRetrying {
                reason,
                attempt,
                wait_ms,
            } => {
                cli_write(
                    stderr,
                    &format!(
                        "{reason}; retrying in {} (attempt {attempt})\n",
                        crate::tui::format_wait(wait_ms)
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::LspDiagnostics { id, report } => {
                cli_lsp_report(
                    &report,
                    id.is_none(),
                    stderr,
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::ModelRequestStarted
            | AgentEvent::ToolArgumentsDelta { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. }
            | AgentEvent::UserInputAccepted { .. }
            | AgentEvent::ProjectInstructionsLoaded { .. }
            | AgentEvent::PlanProgressUpdated { .. }
            | AgentEvent::ToolPipelineSnapshot { .. }
            | AgentEvent::ToolShellSnapshot { .. }
            | AgentEvent::TodoStateUpdated { .. } => {}
        }
    };
    let cancellation = cancellation::CancellationToken::new();
    let result = match &mut execution {
        CliExecution::Agent { agent, prompt } => match prompt {
            Some(prompt) => agent.run(prompt, &cancellation, None, &mut emit),
            None => agent.continue_session(&cancellation, None, &mut emit),
        }
        .map(|_| ()),
        #[cfg(debug_assertions)]
        CliExecution::Mock { test, endpoint } => {
            mock_ai::run_test(*test, endpoint.clone(), None, &cancellation, &mut |event| {
                if let mock_ai::MockTestEvent::Progress(event) = event {
                    emit(event);
                }
            })
            .map(|_| ())
        }
    };
    if !thinking_newline {
        cli_write(stderr, "\n", &mut err_broken, &mut first_error);
    }
    if visible && !newline {
        cli_write(stdout, "\n", &mut out_broken, &mut first_error);
    }
    match &mut execution {
        CliExecution::Agent { agent, .. } => {
            // Report a newly created session even when the task itself failed;
            // stdout stays reserved for model answers.
            if let Some(id) = agent.take_session_notice() {
                cli_write(
                    stderr,
                    &format!("Session: {id}\n"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            if let Some(usage) = context_usage {
                cli_write(
                    stderr,
                    &usage.cli_status(&agent.current_model().reference),
                    &mut err_broken,
                    &mut first_error,
                );
            }
        }
        #[cfg(debug_assertions)]
        CliExecution::Mock { .. } => {
            if let Some(usage) = context_usage {
                cli_write(
                    stderr,
                    &usage.cli_status("mock-root/primary"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error).context("writing task output");
    }
    result
}
