//! Non-interactive structured output: `--mode json` (a single final summary JSON) and
//! `--mode stream-json` (a JSONL event stream).

use anyhow::{Context, Result};
use pp_core::cancellation;
use pp_core::context::TokenUsage;
use pp_core::events::{AgentEvent, OutputStream};
use pp_runtime::agent;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{self, Write},
    time::Instant,
};

use super::mcp_status;

/// CLI non-interactive output mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    Text,
    Json,
    StreamJson,
}

impl OutputMode {
    pub fn is_structured(self) -> bool {
        !matches!(self, Self::Text)
    }
}

pub(crate) fn run_json_task(
    agent: &mut agent::Agent,
    prompt: Option<&str>,
    options: &super::execution::NonInteractiveOptions<'_>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    result_written: &mut bool,
) -> Result<String> {
    let mode = options.mode;
    let resumed = options.resumed;
    let guard = options.guard;
    let started = Instant::now();
    // Structured mode waits for startup MCP connects to finish but prints no human
    // progress banner; configuration warnings still go to stderr.
    let mut sink = io::sink();
    mcp_status::wait(
        agent,
        &mut sink,
        guard.map(super::non_interactive::RunGuard::token),
    );
    for warning in agent.mcp_view().warnings() {
        writeln!(stderr, "{warning}")?;
    }

    let mut out_broken = false;
    let mut first_error = None;
    let stream_events = mode == OutputMode::StreamJson;

    if stream_events {
        let init = build_init_event(agent, resumed);
        write_json_line_checked(stdout, &init, &mut out_broken, &mut first_error);
    }

    let mut turns = 0u32;
    let mut final_text = String::new();
    let mut current_delta_text = String::new();
    let mut total_usage: Option<TokenUsage> = None;
    let mut tool_names: HashMap<String, String> = HashMap::new();

    let mut emit = |event: AgentEvent| {
        if let Some(record) = process_event(
            event,
            &mut turns,
            &mut final_text,
            &mut current_delta_text,
            &mut total_usage,
            &mut tool_names,
        ) && stream_events
        {
            write_json_line_checked(stdout, &record, &mut out_broken, &mut first_error);
        }
    };

    let fallback_token = cancellation::CancellationToken::new();
    let cancellation = guard.map_or(&fallback_token, super::non_interactive::RunGuard::token);
    let outcome = match prompt {
        Some(prompt) => agent.run(prompt, cancellation, None, &mut emit),
        None => agent.continue_session(cancellation, None, &mut emit),
    };
    let mut task_result = match outcome {
        Ok(cancellation::RunOutcome::Cancelled) => Err(match guard {
            Some(guard) => guard.cancellation_error(),
            None => cancellation::Cancelled.into(),
        }),
        Ok(_) => Ok(()),
        Err(error) => Err(match guard {
            Some(guard) => guard.remap_error(error),
            None => error,
        }),
    };
    if task_result.is_ok()
        && let Some(path) = options.output_file
        && let Err(error) = super::non_interactive::write_output_file(path, &final_text)
    {
        task_result = Err(error);
    }

    let _ = agent.take_session_notice();
    let error_text = task_result
        .as_ref()
        .err()
        .map(|error| pp_core::logging::diagnostic_text(&format!("{error:#}")));
    let result_event = build_result_event(
        task_result.is_ok(),
        agent.session_id(),
        Some(agent.current_model().reference.as_str()),
        Some(agent.think_level().as_str()),
        &final_text,
        turns,
        started.elapsed().as_millis() as u64,
        total_usage,
        error_text.as_deref(),
    );
    write_json_line_checked(stdout, &result_event, &mut out_broken, &mut first_error);
    *result_written = first_error.is_none();

    task_result?;
    if let Some(error) = first_error {
        return Err(error).context("writing JSON output");
    }
    Ok(final_text)
}

/// Writes a final error JSON when the task loop fails before it starts (for example
/// argument, configuration or resume errors).
pub fn write_error_result(
    stdout: &mut impl Write,
    error: &anyhow::Error,
    session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    elapsed_ms: u64,
) -> io::Result<()> {
    let error_text = pp_core::logging::diagnostic_text(&format!("{error:#}"));
    let payload = build_result_event(
        false,
        session_id,
        model,
        effort,
        "",
        0,
        elapsed_ms,
        None,
        Some(&error_text),
    );
    write_json_line(stdout, &payload)
}

fn build_init_event(agent: &agent::Agent, resumed: bool) -> Value {
    json!({
        "type": "init",
        "session_id": agent.session_id(),
        "resumed": resumed,
        "cwd": agent.cwd().display().to_string(),
        "model": agent.current_model().reference,
        "effort": agent.think_level().as_str(),
        "tools": agent.active_tool_names(),
        "permission_mode": agent.permission_mode().as_str(),
        "sandbox": agent.sandbox_status().effective.as_str(),
    })
}

#[allow(clippy::too_many_arguments)]
fn build_result_event(
    ok: bool,
    session_id: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    text: &str,
    turns: u32,
    elapsed_ms: u64,
    usage: Option<TokenUsage>,
    error: Option<&str>,
) -> Value {
    let mut obj = json!({
        "type": "result",
        "status": if ok { "ok" } else { "error" },
        "session_id": session_id,
        "model": model,
        "effort": effort,
        "text": text,
        "turns": turns,
        "elapsed_ms": elapsed_ms,
        "usage": usage,
    });
    if let Some(error) = error {
        obj["error"] = Value::String(error.to_owned());
    }
    obj
}

fn process_event(
    event: AgentEvent,
    turns: &mut u32,
    final_text: &mut String,
    current_delta_text: &mut String,
    total_usage: &mut Option<TokenUsage>,
    tool_names: &mut HashMap<String, String>,
) -> Option<Value> {
    match event {
        AgentEvent::AssistantStarted => {
            current_delta_text.clear();
            Some(json!({"type": "assistant_started"}))
        }
        AgentEvent::ThinkingDelta { text } => Some(json!({"type": "thinking_delta", "text": text})),
        AgentEvent::ReasoningSummaryDelta { text } => {
            Some(json!({"type": "reasoning_summary_delta", "text": text}))
        }
        AgentEvent::AssistantDelta { text, refusal } => {
            if !refusal {
                current_delta_text.push_str(&text);
            }
            Some(json!({"type": "assistant_delta", "text": text, "refusal": refusal}))
        }
        AgentEvent::AssistantFinished { text } => {
            if !text.is_empty() {
                *final_text = text.clone();
            } else if !current_delta_text.is_empty() {
                *final_text = std::mem::take(current_delta_text);
            }
            current_delta_text.clear();
            Some(json!({"type": "assistant_finished", "text": text}))
        }
        AgentEvent::ModelAttemptFailed {
            attempt,
            model,
            error,
        } => {
            current_delta_text.clear();
            Some(json!({
                "type": "model_attempt_failed",
                "attempt": attempt,
                "model": model,
                "error": error,
            }))
        }
        AgentEvent::ModelRequestFinished {
            elapsed_ms,
            usage,
            success,
        } => {
            *turns += 1;
            if success && let Some(turn_usage) = usage {
                accumulate_usage(total_usage, turn_usage);
            }
            Some(json!({
                "type": "model_request_finished",
                "turn": *turns,
                "elapsed_ms": elapsed_ms,
                "success": success,
                "usage": usage,
            }))
        }
        AgentEvent::ModelRouteChanged {
            model,
            effort,
            source,
            temporary,
        } => Some(json!({
            "type": "model_route_changed",
            "model": model,
            "effort": effort.as_str(),
            "source": source,
            "temporary": temporary,
        })),
        AgentEvent::RequestRetrying {
            reason,
            attempt,
            wait_ms,
        } => Some(json!({
            "type": "request_retrying",
            "reason": reason,
            "attempt": attempt,
            "wait_ms": wait_ms,
        })),
        AgentEvent::ToolStarted {
            id,
            name,
            arguments,
        } => {
            tool_names.insert(id.clone(), name.clone());
            let parsed_args =
                serde_json::from_str::<Value>(&arguments).unwrap_or(Value::String(arguments));
            Some(json!({
                "type": "tool_started",
                "id": id,
                "name": name,
                "arguments": parsed_args,
            }))
        }
        AgentEvent::ToolOutput { id, stream, text } => {
            let stream_name = match stream {
                OutputStream::Stdout => "stdout",
                OutputStream::Stderr => "stderr",
            };
            Some(json!({
                "type": "tool_output",
                "id": id,
                "stream": stream_name,
                "text": text,
            }))
        }
        AgentEvent::FileChanged { id, path, diff } => Some(json!({
            "type": "file_changed",
            "id": id,
            "path": path,
            "diff": diff,
        })),
        AgentEvent::ToolFinished { id, result } => {
            let name = tool_names.remove(&id);
            let ok = result["ok"] == true;
            Some(json!({
                "type": "tool_finished",
                "id": id,
                "name": name,
                "ok": ok,
                "result": result,
            }))
        }
        AgentEvent::ContextUpdated { usage } => Some(json!({
            "type": "context_updated",
            "usage": usage,
        })),
        AgentEvent::CompactionStarted { automatic } => Some(json!({
            "type": "compaction_started",
            "automatic": automatic,
        })),
        AgentEvent::CompactionFinished {
            before_tokens,
            after_tokens,
        } => Some(json!({
            "type": "compaction_finished",
            "before_tokens": before_tokens,
            "after_tokens": after_tokens,
        })),
        AgentEvent::LspDiagnostics { id, report } => Some(json!({
            "type": "lsp_diagnostics",
            "id": id,
            "report": report,
        })),
        AgentEvent::SubagentResultsInjected { run_ids } => Some(json!({
            "type": "subagent_results_injected",
            "run_ids": run_ids,
        })),
        AgentEvent::HookDiagnostic { event, message } => Some(json!({
            "type": "hook_diagnostic",
            "event": event,
            "message": message,
        })),
        AgentEvent::TodoReminder {
            todos,
            attempt,
            max_attempts,
        } => Some(json!({
            "type": "todo_reminder",
            "attempt": attempt,
            "max_attempts": max_attempts,
            "todos": todos,
        })),
        AgentEvent::AutoReviewStarted { tool } => Some(json!({
            "type": "auto_review_started",
            "tool": tool,
        })),
        AgentEvent::AutoReviewFinished {
            tool,
            passed,
            reason,
        } => Some(json!({
            "type": "auto_review_finished",
            "tool": tool,
            "passed": passed,
            "reason": reason,
        })),
        AgentEvent::ModelRequestStarted
        | AgentEvent::ToolArgumentsDelta { .. }
        | AgentEvent::ModelOutputProgress { .. }
        | AgentEvent::ToolOutputSnapshot { .. }
        | AgentEvent::ToolShellSnapshot { .. }
        | AgentEvent::ProjectInstructionsLoaded { .. }
        | AgentEvent::PlanProgressUpdated { .. }
        | AgentEvent::TodoStateUpdated { .. }
        | AgentEvent::TodoSnapshot { .. }
        | AgentEvent::UserInputAccepted { .. }
        | AgentEvent::UserInputRejected { .. } => None,
    }
}

fn accumulate_usage(total: &mut Option<TokenUsage>, turn: TokenUsage) {
    match total {
        Some(acc) => {
            acc.input_tokens = acc.input_tokens.saturating_add(turn.input_tokens);
            acc.output_tokens = acc.output_tokens.saturating_add(turn.output_tokens);
            acc.cached_tokens = match (acc.cached_tokens, turn.cached_tokens) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            acc.cache_write_tokens = match (acc.cache_write_tokens, turn.cache_write_tokens) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            acc.reasoning_tokens = match (acc.reasoning_tokens, turn.reasoning_tokens) {
                (Some(a), Some(b)) => Some(a.saturating_add(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
        }
        None => *total = Some(turn),
    }
}

fn write_json_line_checked(
    writer: &mut impl Write,
    value: &Value,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    if *broken {
        return;
    }
    if let Err(error) = write_json_line(writer, value) {
        *broken = true;
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

fn write_json_line(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_mapper_tracks_turns_usage_tools_and_resets_failed_partial_text() {
        let mut turns = 0;
        let mut final_text = String::new();
        let mut delta = String::new();
        let mut total_usage = None;
        let mut tool_names = HashMap::new();

        let started = process_event(
            AgentEvent::ToolStarted {
                id: "call-1".into(),
                name: "read".into(),
                arguments: r#"{"path":"README.md"}"#.into(),
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        )
        .unwrap();
        assert_eq!(started["type"], "tool_started");
        assert_eq!(started["arguments"]["path"], "README.md");

        let finished = process_event(
            AgentEvent::ToolFinished {
                id: "call-1".into(),
                result: json!({"ok": true, "content": "hi"}),
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        )
        .unwrap();
        assert_eq!(finished["name"], "read");
        assert_eq!(finished["ok"], true);

        let _ = process_event(
            AgentEvent::AssistantDelta {
                text: "partial".into(),
                refusal: false,
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        );
        let _ = process_event(
            AgentEvent::ModelAttemptFailed {
                attempt: 1,
                model: "m".into(),
                error: "err".into(),
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        );
        assert!(delta.is_empty());

        let _ = process_event(
            AgentEvent::AssistantFinished {
                text: "final answer".into(),
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        );
        assert_eq!(final_text, "final answer");

        let req_1 = process_event(
            AgentEvent::ModelRequestFinished {
                elapsed_ms: 10,
                usage: Some(TokenUsage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cached_tokens: Some(40),
                    cache_write_tokens: None,
                    reasoning_tokens: Some(12),
                }),
                success: true,
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        )
        .unwrap();
        assert_eq!(req_1["turn"], 1);
        let _ = process_event(
            AgentEvent::ModelRequestFinished {
                elapsed_ms: 12,
                usage: Some(TokenUsage {
                    input_tokens: 150,
                    output_tokens: 30,
                    cached_tokens: Some(10),
                    cache_write_tokens: Some(5),
                    reasoning_tokens: Some(8),
                }),
                success: true,
            },
            &mut turns,
            &mut final_text,
            &mut delta,
            &mut total_usage,
            &mut tool_names,
        );
        assert_eq!(turns, 2);
        assert_eq!(
            total_usage,
            Some(TokenUsage {
                input_tokens: 250,
                output_tokens: 50,
                cached_tokens: Some(50),
                cache_write_tokens: Some(5),
                reasoning_tokens: Some(20),
            })
        );
    }

    #[test]
    fn write_error_result_redacts_secrets_and_outputs_single_line_json() {
        let mut buf = Vec::new();
        let err = anyhow::anyhow!(r#"request failed: Authorization: Bearer sk-secret-token-123"#);
        write_error_result(&mut buf, &err, Some("sess-1"), None, None, 42).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert_eq!(line.lines().count(), 1);
        let value: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(value["type"], "result");
        assert_eq!(value["status"], "error");
        assert_eq!(value["session_id"], "sess-1");
        assert_eq!(value["elapsed_ms"], 42);
        assert!(
            !value["error"]
                .as_str()
                .unwrap()
                .contains("sk-secret-token-123")
        );
        assert!(value["error"].as_str().unwrap().contains("[REDACTED]"));
    }
}
