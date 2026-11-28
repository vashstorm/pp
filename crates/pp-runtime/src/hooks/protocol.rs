//! Hook stdin input envelope and stdout decision protocol.
//!
//! Field names and exit-code semantics are Claude Code compatible, but tool names, tool
//! arguments and results keep pp's native format. Unknown control fields are always treated
//! as protocol errors, never silently allowed.

use anyhow::{Result, bail};
use pp_config::config::HookEvent;
use serde_json::{Map, Value};

/// Limit of a single text injected into model context (Unicode chars).
pub(super) const TEXT_LIMIT: usize = 10_000;
const TRUNCATED: &str = "…[truncated]";

/// Common envelope of one hook invocation.
pub(crate) struct Context<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) transcript_path: &'a std::path::Path,
    pub(crate) cwd: &'a std::path::Path,
    /// `bypassPermissions`/`auto_review`/`default`/`read-only`, mapped from the effective approval tier.
    pub(crate) permission_mode: &'a str,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) agent_type: Option<&'a str>,
}

/// Event-specific payload; serialization writes only the fields allowed for that event.
pub(crate) enum Payload<'a> {
    UserPrompt {
        prompt: &'a str,
    },
    PreTool {
        name: &'a str,
        input: Option<&'a Value>,
        call_id: &'a str,
    },
    PostTool {
        name: &'a str,
        input: Option<&'a Value>,
        call_id: &'a str,
        response: &'a Value,
    },
    PostToolFailure {
        name: &'a str,
        input: Option<&'a Value>,
        call_id: &'a str,
        error: &'a str,
    },
    Stop {
        stop_hook_active: bool,
        last_assistant_message: &'a str,
    },
}

pub(crate) fn input(event: HookEvent, context: &Context<'_>, payload: &Payload<'_>) -> Value {
    let mut object = Map::new();
    object.insert("session_id".into(), Value::from(context.session_id));
    object.insert(
        "transcript_path".into(),
        Value::from(context.transcript_path.to_string_lossy().into_owned()),
    );
    object.insert(
        "cwd".into(),
        Value::from(context.cwd.to_string_lossy().into_owned()),
    );
    object.insert(
        "permission_mode".into(),
        Value::from(context.permission_mode),
    );
    object.insert("hook_event_name".into(), Value::from(event.as_str()));
    match payload {
        Payload::UserPrompt { prompt } => {
            object.insert("prompt".into(), Value::from(*prompt));
        }
        Payload::PreTool {
            name,
            input,
            call_id,
        } => {
            tool_fields(&mut object, name, *input, call_id);
        }
        Payload::PostTool {
            name,
            input,
            call_id,
            response,
        } => {
            tool_fields(&mut object, name, *input, call_id);
            object.insert("tool_response".into(), (*response).clone());
        }
        Payload::PostToolFailure {
            name,
            input,
            call_id,
            error,
        } => {
            tool_fields(&mut object, name, *input, call_id);
            object.insert("error".into(), Value::from(*error));
            object.insert("is_interrupt".into(), Value::from(false));
        }
        Payload::Stop {
            stop_hook_active,
            last_assistant_message,
        } => {
            object.insert("stop_hook_active".into(), Value::from(*stop_hook_active));
            object.insert(
                "last_assistant_message".into(),
                Value::from(*last_assistant_message),
            );
        }
    }
    // Tool subevents additionally carry the real identity; the root agent never fakes agent_id.
    if event.is_tool_event() {
        if let Some(agent_id) = context.agent_id {
            object.insert("agent_id".into(), Value::from(agent_id));
        }
        if let Some(agent_type) = context.agent_type {
            object.insert("agent_type".into(), Value::from(agent_type));
        }
    }
    Value::Object(object)
}

fn tool_fields(object: &mut Map<String, Value>, name: &str, input: Option<&Value>, call_id: &str) {
    object.insert("tool_name".into(), Value::from(name));
    object.insert("tool_input".into(), input.cloned().unwrap_or(Value::Null));
    object.insert("tool_use_id".into(), Value::from(call_id));
}

/// Normalized result of one handler; merge order is handled by the caller per event rules.
#[derive(Debug, Default)]
pub(super) struct Output {
    /// Event-level denial reason: UserPromptSubmit/PreToolUse rejection, Stop requesting another turn.
    pub(super) deny: Option<String>,
    /// Forced single approval reason for PreToolUse.
    pub(super) ask: Option<String>,
    pub(super) updated_input: Option<Value>,
    pub(super) updated_output: Option<Value>,
    /// Context accumulated in handler order; empty strings are not included.
    pub(super) context: Vec<String>,
    /// Task stop reason for `continue:false`.
    pub(super) stop: Option<String>,
    pub(super) system_message: Option<String>,
}

/// Parses one handler's decision from the event, exit code and stdout/stderr.
///
/// `response` is used only for post events, to validate `ok`/`cancelled` of `updatedToolOutput`.
pub(super) fn parse(
    event: HookEvent,
    code: Option<i32>,
    stdout: &[u8],
    stderr: &[u8],
    response: Option<&Value>,
) -> Result<Output> {
    let stdout = std::str::from_utf8(stdout).map_err(|_| {
        anyhow::anyhow!(
            "{event} hook stdout is not valid UTF-8",
            event = event.as_str()
        )
    })?;
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    match code {
        Some(0) => parse_success(event, stdout, stderr, response),
        Some(2) => parse_blocking(event, stdout, stderr, response),
        Some(other) => bail!(
            "{} hook exited with status {other}; stderr: {}",
            event.as_str(),
            bound(&diagnostic(stderr))
        ),
        None => bail!("{} hook was terminated by a signal", event.as_str()),
    }
}

fn default_denial(event: HookEvent) -> &'static str {
    match event {
        HookEvent::UserPromptSubmit => "input rejected by a UserPromptSubmit hook.",
        HookEvent::PreToolUse => "tool call rejected by a PreToolUse hook.",
        HookEvent::PostToolUse => "PostToolUse hook reported a failure.",
        HookEvent::PostToolUseFailure => "PostToolUseFailure hook reported a failure.",
        HookEvent::Stop => "task must continue: a Stop hook requested another turn.",
    }
}

fn parse_success(
    event: HookEvent,
    stdout: &str,
    stderr: &str,
    response: Option<&Value>,
) -> Result<Output> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Ok(Output::default());
    }
    if !looks_like_json(trimmed) {
        // Plain text is meaningful only for UserPromptSubmit: it is injected as extra context.
        return Ok(match event {
            HookEvent::UserPromptSubmit => Output {
                context: vec![bound(trimmed)],
                ..Output::default()
            },
            _ => Output::default(),
        });
    }
    let value: Value = serde_json::from_str(trimmed)
        .map_err(|_| anyhow::anyhow!("{} hook stdout is not valid JSON", event.as_str()))?;
    let object = match value {
        Value::Object(object) => object,
        _ => bail!("{} hook stdout must be a JSON object", event.as_str()),
    };
    interpret(event, &object, stderr, response)
}

/// exit 2: denial semantics are fixed per event; JSON on stdout may still provide context and messages,
/// but it can neither cancel the denial nor turn a denial into an allow.
fn parse_blocking(
    event: HookEvent,
    stdout: &str,
    stderr: &str,
    response: Option<&Value>,
) -> Result<Output> {
    let reason = if stderr.is_empty() {
        bound(default_denial(event))
    } else {
        bound(&diagnostic(stderr))
    };
    let mut output = Output::default();
    let trimmed = stdout.trim();
    if !trimmed.is_empty()
        && looks_like_json(trimmed)
        && let Ok(Value::Object(object)) = serde_json::from_str::<Value>(trimmed)
        && let Ok(parsed) = interpret(event, &object, stderr, response)
    {
        output.context = parsed.context;
        output.system_message = parsed.system_message;
        output.stop = parsed.stop;
        output.updated_output = parsed.updated_output;
    }
    match event {
        HookEvent::UserPromptSubmit | HookEvent::PreToolUse | HookEvent::Stop => {
            output.deny = Some(reason);
        }
        // post cannot undo side effects that already happened: stderr is feedback context only.
        HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            output.context.insert(0, reason);
        }
    }
    Ok(output)
}

fn looks_like_json(text: &str) -> bool {
    matches!(
        text.as_bytes().first(),
        Some(b'{') | Some(b'[') | Some(b'"')
    )
}

fn interpret(
    event: HookEvent,
    object: &Map<String, Value>,
    stderr: &str,
    response: Option<&Value>,
) -> Result<Output> {
    let mut output = Output::default();
    let mut specific: Option<&Map<String, Value>> = None;
    // `stopReason` is only the stop reason for `continue:false` and cannot trigger a stop on its own.
    let mut stop_reason: Option<String> = None;
    // `decision:"block"` must block execution even when reason is omitted; it is never silently allowed.
    let mut blocked = false;
    for (key, value) in object {
        match key.as_str() {
            // This field is handled by the branch outside r#continue#.
            "continue" => {}
            "stopReason" => {
                stop_reason = Some(bound(&as_text(event, key, value)?));
            }
            "systemMessage" => {
                let text = as_text(event, key, value)?;
                if !text.trim().is_empty() {
                    output.system_message = Some(bound(&text));
                }
            }
            "decision" => {
                supports_decision(event, key)?;
                match as_text(event, key, value)?.as_str() {
                    "block" => blocked = true,
                    other => bail!(
                        "{} hook returned unsupported decision {other:?}",
                        event.as_str()
                    ),
                }
            }
            "reason" => {
                supports_decision(event, key)?;
                let reason = bound(&as_text(event, key, value)?);
                match event {
                    HookEvent::PostToolUse => {
                        if !reason.trim().is_empty() {
                            output.context.push(reason);
                        }
                    }
                    _ => output.deny = Some(reason),
                }
            }
            "hookSpecificOutput" => {
                let Value::Object(nested) = value else {
                    bail!(
                        "{} hook returned a non-object hookSpecificOutput",
                        event.as_str()
                    );
                };
                specific = Some(nested);
            }
            other => bail!(
                "{} hook returned unsupported field {other:?}",
                event.as_str()
            ),
        }
    }
    if blocked {
        // post cannot undo side effects, so the default block reason is model feedback only; other events deny at event level.
        match event {
            HookEvent::PostToolUse => {
                if output.context.is_empty() {
                    output.context.push(bound(default_denial(event)));
                }
            }
            _ => {
                let reason = bound(default_denial(event));
                output.deny.get_or_insert(reason);
            }
        }
    }
    let continued = match object.get("continue") {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(_) => bail!("{} hook returned a non-boolean continue", event.as_str()),
        None => None,
    };
    if continued == Some(false) {
        // A blank reason equals the default reason: a stop must carry a readable failure cause.
        output.stop = Some(match stop_reason {
            Some(reason) if !reason.is_empty() => reason,
            _ => bound("A hook stopped this task."),
        });
        // continue:false is a task-level stop that absorbs other decisions.
        output.deny = None;
        output.ask = None;
        return Ok(output);
    }
    if let Some(nested) = specific {
        apply_specific(event, nested, stderr, response, &mut output)?;
    }
    Ok(output)
}

fn apply_specific(
    event: HookEvent,
    nested: &Map<String, Value>,
    stderr: &str,
    response: Option<&Value>,
    output: &mut Output,
) -> Result<()> {
    let mut declared: Option<String> = None;
    for (key, value) in nested {
        match key.as_str() {
            "hookEventName" => declared = Some(as_text(event, key, value)?),
            "permissionDecision" => {
                ensure_tool_event(event, key)?;
                match as_text(event, key, value)?.as_str() {
                    "deny" => {
                        output.deny = Some(bound(&fallback_reason(
                            nested.get("permissionDecisionReason"),
                            default_denial(event),
                        )));
                    }
                    "ask" => {
                        output.ask = Some(bound(&fallback_reason(
                            nested.get("permissionDecisionReason"),
                            "a PreToolUse hook requires permission for this tool call.",
                        )));
                    }
                    // `allow` only means the hook does not intercept; it never skips pp's existing permission checks.
                    "allow" => {}
                    other => bail!(
                        "{} hook returned unsupported permissionDecision {other:?}",
                        event.as_str()
                    ),
                }
            }
            "permissionDecisionReason" => {
                ensure_tool_event(event, key)?;
            }
            "updatedInput" => {
                if event != HookEvent::PreToolUse {
                    bail!("{} hook does not support updatedInput", event.as_str());
                }
                if !value.is_object() {
                    bail!("{} hook returned a non-object updatedInput", event.as_str());
                }
                output.updated_input = Some(value.clone());
            }
            "updatedToolOutput" => {
                // Post-failure accepts only additionalContext: a failed model result must not be rewritten.
                if event != HookEvent::PostToolUse {
                    bail!("{} hook does not support updatedToolOutput", event.as_str());
                }
                output.updated_output = Some(validated_tool_output(event, value, response)?);
            }
            "updatedMCPToolOutput" => bail!(
                "{} hook returned the unsupported legacy field \"updatedMCPToolOutput\"; use updatedToolOutput",
                event.as_str()
            ),
            "additionalContext" => {
                let text = as_text(event, key, value)?;
                if text.trim().is_empty() {
                    continue;
                }
                let text = bound(&text);
                match event {
                    HookEvent::Stop => output.deny = Some(text),
                    _ => output.context.push(text),
                }
            }
            other => bail!(
                "{} hook returned unsupported hookSpecificOutput field {other:?}",
                event.as_str()
            ),
        }
    }
    match declared {
        Some(name) if name == event.as_str() => Ok(()),
        Some(name) => bail!("{} hook reported hookEventName {name:?}", event.as_str()),
        None => bail!(
            "{} hook hookSpecificOutput is missing hookEventName",
            event.as_str()
        ),
    }
    .map(|()| {
        // The exit-2 reason takes priority over context order in JSON; the caller already handled stderr.
        let _ = stderr;
    })
}

/// Only input, post-success and Stop use top-level `decision`/`reason`;
/// tool pre-use uses `permissionDecision`, and post-failure accepts only `additionalContext`.
fn supports_decision(event: HookEvent, key: &str) -> Result<()> {
    if matches!(
        event,
        HookEvent::UserPromptSubmit | HookEvent::PostToolUse | HookEvent::Stop
    ) {
        return Ok(());
    }
    bail!("{} hook does not support {key}", event.as_str())
}

fn ensure_tool_event(event: HookEvent, key: &str) -> Result<()> {
    if event != HookEvent::PreToolUse {
        bail!("{} hook does not support {key}", event.as_str());
    }
    Ok(())
}

fn fallback_reason(value: Option<&Value>, default: &str) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| default.to_owned())
}

/// `updatedToolOutput` may change only what the model sees: `ok`/`cancelled` must keep their original values,
/// and status fields absent from the original result must not be invented.
fn validated_tool_output(
    event: HookEvent,
    value: &Value,
    response: Option<&Value>,
) -> Result<Value> {
    let Value::Object(mut object) = value.clone() else {
        bail!(
            "{} hook returned a non-object updatedToolOutput",
            event.as_str()
        );
    };
    if let Some(Value::Object(original)) = response {
        for key in ["ok", "cancelled"] {
            // Status fields must match the real result: they can be neither changed nor invented.
            match (original.get(key), object.get(key)) {
                (Some(expected), Some(actual)) if actual == expected => {}
                (Some(_), Some(_)) => bail!(
                    "{} hook attempted to change the tool result field {key:?}",
                    event.as_str()
                ),
                (None, Some(_)) => bail!(
                    "{} hook attempted to add the tool result field {key:?}",
                    event.as_str()
                ),
                (Some(expected), None) => {
                    object.insert(key.to_owned(), expected.clone());
                }
                (None, None) => {}
            }
        }
        // `kind:"image"` links an image result to the real attachment: the body may be rewritten,
        // but it must not be dropped or changed, or cross-protocol history conversion rejects an attachment that still exists.
        if original.get("kind").and_then(Value::as_str) == Some("image") {
            match object.get("kind") {
                Some(Value::String(kind)) if kind == "image" => {}
                Some(_) => bail!(
                    "{} hook attempted to change the tool result field \"kind\"",
                    event.as_str()
                ),
                None => {
                    object.insert("kind".to_owned(), Value::String("image".to_owned()));
                }
            }
        }
    }
    Ok(Value::Object(object))
}

fn as_text(event: HookEvent, key: &str, value: &Value) -> Result<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        _ => bail!("{} hook returned a non-string {key}", event.as_str()),
    }
}

/// Diagnostic text is first redacted for credentials, then sanitized of control characters and terminal escapes.
fn diagnostic(text: &str) -> String {
    let redacted = pp_core::logging::diagnostic_text(text);
    pp_core::text::display_text(&redacted).into_owned()
}

/// A single injected text is truncated on Unicode char boundaries and keeps an explicit marker.
pub(crate) fn bound(text: &str) -> String {
    let text = diagnostic(text.trim());
    if text.chars().count() <= TEXT_LIMIT {
        return text;
    }
    let mut truncated: String = text.chars().take(TEXT_LIMIT).collect();
    truncated.push_str(TRUNCATED);
    truncated
}
