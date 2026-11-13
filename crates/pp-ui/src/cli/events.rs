//! 行模式事件输出：stdout/stderr 增量、shell 运行状态与上下文压缩结果。

use anyhow::{Context, Result};
use std::io::{self, Write};

use pp_core::cancellation;
use pp_core::context;
use pp_core::events;
use pp_runtime::agent;
use pp_runtime::tools;

// Display failures must not interrupt recording results of tools that already ran.
pub(crate) fn cli_write(
    writer: &mut impl Write,
    text: &str,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    if *broken {
        return;
    }
    if let Err(error) = writer
        .write_all(text.as_bytes())
        .and_then(|()| writer.flush())
    {
        *broken = true;
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

// 头尾窗口滚动后停止追加，结束时仅补显示最终尾窗，避免反复打印头部。
pub(crate) fn cli_output_delta(output: &str, previous: &mut String, finished: bool) -> String {
    if let Some(delta) = output.strip_prefix(previous.as_str()) {
        let delta = delta.to_owned();
        *previous = output.to_owned();
        delta
    } else if finished {
        let tail = output
            .find("[OUTPUT TRUNCATED:")
            .map_or(output, |offset| &output[offset..]);
        format!("\n[Final output preview]\n{tail}")
    } else {
        String::new()
    }
}

pub(crate) fn cli_output_text(
    output: &str,
    previous: &mut String,
    finished: bool,
    label: &str,
) -> String {
    let delta = cli_output_delta(output, previous, finished);
    if delta.is_empty() {
        if finished && !previous.is_empty() && !previous.ends_with('\n') {
            "\n".into()
        } else {
            String::new()
        }
    } else {
        format!(
            "[{label} output] {delta}{}",
            if finished && !delta.ends_with('\n') {
                "\n"
            } else {
                ""
            }
        )
    }
}

pub(crate) fn cli_output_result(
    value: &serde_json::Value,
    previous: &mut String,
    label: &str,
) -> String {
    let mut text = cli_output_text(
        value["output"].as_str().unwrap_or_default(),
        previous,
        true,
        label,
    );
    let notice = tools::output::notice(value);
    if !notice.is_empty() {
        text.push_str(&notice);
        text.push('\n');
    }
    // 阶段正文不进入主输出；这里只追加有界状态与补读 URI。
    if let Some(status) = tools::output::pipes_status(&value["pipes"]) {
        text.push_str(&status.notice());
        text.push('\n');
    }
    text
}

pub(crate) fn run_cli_shell(
    agent: &mut agent::Agent,
    command: &str,
    stderr: &mut impl Write,
) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    let mut broken = false;
    let mut first_error = None;
    let mut newline = true;
    let mut streamed_output = String::new();
    let result = agent.run_shell(
        command,
        "local-shell-cli",
        &cancellation::CancellationToken::new(),
        &mut |event| match event {
            AgentEvent::ToolStarted { .. } => {
                cli_write(stderr, "shell: running\n", &mut broken, &mut first_error)
            }
            AgentEvent::ToolPipelineSnapshot { .. } | AgentEvent::ToolShellSnapshot { .. } => {}
            AgentEvent::ToolOutputSnapshot { output, .. } => {
                let delta = cli_output_text(&output, &mut streamed_output, false, "shell");
                cli_write(stderr, &delta, &mut broken, &mut first_error);
                newline = delta.ends_with('\n');
            }
            AgentEvent::ToolOutput { stream, text, .. } => {
                let label = match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                };
                cli_write(
                    stderr,
                    &format!("[shell {label}] {text}"),
                    &mut broken,
                    &mut first_error,
                );
                newline = text.ends_with('\n');
            }
            AgentEvent::ToolFinished { result, .. } => {
                if result["output"].is_string() {
                    cli_write(
                        stderr,
                        &cli_output_result(&result, &mut streamed_output, "shell"),
                        &mut broken,
                        &mut first_error,
                    );
                }
                if !newline {
                    cli_write(stderr, "\n", &mut broken, &mut first_error);
                }
                let status = if result["cancelled"] == true {
                    "shell: cancelled".to_owned()
                } else if result["timed_out"] == true {
                    "shell: timed out".to_owned()
                } else if let Some(code) = result["exit_code"].as_i64() {
                    format!("shell: exit {code}")
                } else {
                    "shell: failed".to_owned()
                };
                cli_write(
                    stderr,
                    &format!("{status}\n"),
                    &mut broken,
                    &mut first_error,
                );
                for key in ["error", "warning"] {
                    if let Some(message) = result[key].as_str() {
                        cli_write(
                            stderr,
                            &format!("shell: {message}\n"),
                            &mut broken,
                            &mut first_error,
                        );
                    }
                }
                if let Some(warnings) = result["warnings"].as_array() {
                    for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
                        cli_write(
                            stderr,
                            &format!("shell: {warning}\n"),
                            &mut broken,
                            &mut first_error,
                        );
                    }
                }
                if result["stdout_truncated"] == true
                    || result["stderr_truncated"] == true
                    || result["truncated"] == true
                {
                    cli_write(
                        stderr,
                        "shell: output truncated\n",
                        &mut broken,
                        &mut first_error,
                    );
                }
            }
            _ => {}
        },
    );
    if result.as_ref().is_err_and(|error| {
        error
            .chain()
            .any(|cause| cause.is::<cancellation::CleanupError>())
    }) {
        return result;
    }
    if let Some(error) = first_error {
        return Err(error).context("writing shell output");
    }
    result
}

pub(crate) fn cli_context_event(
    event: events::AgentEvent,
    usage: &mut Option<context::ContextUsage>,
    stderr: &mut impl Write,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    match event {
        events::AgentEvent::ContextUpdated { usage: current } => *usage = Some(current),
        events::AgentEvent::CompactionStarted { .. } => {
            cli_write(stderr, "Compacting context...\n", broken, first_error)
        }
        events::AgentEvent::CompactionFinished {
            before_tokens,
            after_tokens,
        } => cli_write(
            stderr,
            &format!("Context compacted: ~{before_tokens} → ~{after_tokens} tokens (estimated)\n"),
            broken,
            first_error,
        ),
        _ => {}
    }
}

pub(crate) fn run_cli_compaction(agent: &mut agent::Agent, stderr: &mut impl Write) -> Result<()> {
    use agent::context::CompactionOutcome;
    let mut usage = None;
    let mut broken = false;
    let mut first_error = None;
    let result = agent.compact(&cancellation::CancellationToken::new(), &mut |event| {
        cli_context_event(event, &mut usage, stderr, &mut broken, &mut first_error);
    });
    match &result {
        Ok(CompactionOutcome::Unchanged) => cli_write(
            stderr,
            "Nothing to compact.\n",
            &mut broken,
            &mut first_error,
        ),
        Ok(CompactionOutcome::Cancelled) => cli_write(
            stderr,
            "Context compaction cancelled.\n",
            &mut broken,
            &mut first_error,
        ),
        _ => {}
    }
    if let Some(id) = agent.take_session_notice() {
        cli_write(
            stderr,
            &format!("Session: {id}\n"),
            &mut broken,
            &mut first_error,
        );
    }
    let usage = match usage {
        Some(usage) => usage,
        None => agent.context_usage()?,
    };
    cli_write(
        stderr,
        &usage.cli_status(&agent.current_model().reference),
        &mut broken,
        &mut first_error,
    );
    if let Some(error) = first_error {
        return Err(error).context("writing context output");
    }
    result.map(|_| ())
}
