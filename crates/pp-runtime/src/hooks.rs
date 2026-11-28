//! Lifecycle command hooks: config compilation, event matching, serial execution and decision merging.
//!
//! Configuration comes from the user-level `~/.pp/agent/hooks.yml` and the startup directory's `.pp/hooks.yml`.
//! Only command handlers exist; a script receives a JSON envelope on stdin and expresses its decision
//! through stdout JSON and the exit code. Any process whose reaping cannot be confirmed is escalated to
//! `CleanupError`, and the caller must stop the task.

mod exec;
pub(crate) mod protocol;

#[cfg(test)]
#[path = "hooks/tests.rs"]
mod integration;

use anyhow::{Context as _, Result, bail};
use pp_config::config::{HookEvent, HookFailurePolicy, HooksSettings};
use pp_core::cancellation::{CancellationToken, Cancelled, CleanupError};
use pp_core::events::AgentEvent;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

use protocol::Output;

/// Merge of all handler results within one event.
#[derive(Debug, Default)]
pub(crate) struct HookEffect {
    /// Event-level denial for UserPromptSubmit/PreToolUse; the continuation request for Stop.
    pub(crate) deny_reason: Option<String>,
    /// One-off forced approval reason for PreToolUse (stronger than allow, cannot skip the original approval).
    pub(crate) ask_reason: Option<String>,
    pub(crate) updated_input: Option<Value>,
    pub(crate) updated_output: Option<Value>,
    pub(crate) additional_context: Vec<String>,
    /// `continue:false`: stop the current task without disguising it as a user cancellation.
    pub(crate) stop_reason: Option<String>,
}

struct Handler {
    command: String,
    args: Option<Vec<String>>,
    timeout: Duration,
    on_failure: Option<HookFailurePolicy>,
}

#[derive(Debug)]
enum Matcher {
    All,
    Names(Vec<String>),
    Pattern(regex::Regex),
}

impl Matcher {
    fn compile(event: HookEvent, matcher: Option<&str>, group: usize) -> Result<Self> {
        let matcher = matcher.map(str::trim).unwrap_or_default();
        if matcher.is_empty() || matcher == "*" {
            return Ok(Self::All);
        }
        if !event.is_tool_event() {
            bail!(
                "hooks.{}[{group}].matcher must be empty or \"*\" for this event",
                event.as_str()
            );
        }
        // A matcher made up only of alphanumerics, `_`, `-`, spaces, commas and `|` is a name list;
        // everything else is treated as a Rust regex (JS look-around and backreferences are unsupported).
        if matcher
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | ' ' | ',' | '|'))
        {
            let names: Vec<String> = matcher
                .split([',', '|'])
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
                .collect();
            return Ok(Self::Names(names));
        }
        let pattern = regex::Regex::new(matcher).map_err(|_| {
            anyhow::anyhow!(
                "hooks.{}[{group}].matcher is not a supported regular expression (Rust regex syntax; look-around and backreferences are unavailable)",
                event.as_str()
            )
        })?;
        Ok(Self::Pattern(pattern))
    }

    fn matches(&self, tool: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Names(names) => tool.is_some_and(|tool| names.iter().any(|name| name == tool)),
            Self::Pattern(pattern) => pattern.is_match(tool.unwrap_or_default()),
        }
    }
}

struct Group {
    matcher: Matcher,
    handlers: Vec<Handler>,
}

pub(crate) struct Runner {
    events: [Vec<Group>; HookEvent::ALL.len()],
}

impl Runner {
    pub(crate) fn empty() -> Self {
        Self {
            events: std::array::from_fn(|_| Vec::new()),
        }
    }

    /// Compiles after reading the user-level and project-level hooks files; project-level groups are appended after user-level ones.
    pub(crate) fn load(user: &Path, project: &Path) -> Result<Self> {
        Self::compile(&pp_config::config::load_hooks(user, project)?)
    }

    pub(crate) fn compile(settings: &HooksSettings) -> Result<Self> {
        let mut events: [Vec<Group>; HookEvent::ALL.len()] = std::array::from_fn(|_| Vec::new());
        for (index, event) in HookEvent::ALL.iter().enumerate() {
            for (group_index, group) in settings.groups(*event).iter().enumerate() {
                if group.hooks.is_empty() {
                    continue;
                }
                let matcher = Matcher::compile(*event, group.matcher.as_deref(), group_index)?;
                let handlers = group
                    .hooks
                    .iter()
                    .map(|hook| Handler {
                        command: hook.command.clone(),
                        args: hook.args.clone(),
                        timeout: Duration::from_secs(hook.timeout),
                        on_failure: hook.on_failure,
                    })
                    .collect();
                events[index].push(Group { matcher, handlers });
            }
        }
        Ok(Self { events })
    }

    pub(crate) fn has_handlers(&self, event: HookEvent) -> bool {
        !self.events[event_index(event)].is_empty()
    }

    /// Runs every matching handler for the event serially and merges decisions by event rules.
    ///
    /// An `Err` return means the event must fail the task (control points default to fail-closed,
    /// `onFailure: block`, or a `CleanupError` whose reaping could not be confirmed); cancellation keeps
    /// the `Cancelled` type.
    #[cfg(test)]
    pub(crate) fn run(
        &self,
        event: HookEvent,
        input: &Value,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<HookEffect> {
        self.run_with_stats(event, input, None, cancellation, emit)
    }

    pub(crate) fn run_with_stats(
        &self,
        event: HookEvent,
        input: &Value,
        stats: Option<&pp_stats::Database>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<HookEffect> {
        let mut effect = HookEffect::default();
        let tool = input.get("tool_name").and_then(Value::as_str);
        let session_file = input
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(Path::new);
        let cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));
        // Each handler sees the envelope as rewritten by its predecessors: `updatedToolOutput` is passed on to later handlers.
        let mut envelope = input.clone();
        let mut encoded: Option<Vec<u8>> = None;
        for group in &self.events[event_index(event)] {
            if !group.matcher.matches(tool) {
                continue;
            }
            for (index, handler) in group.handlers.iter().enumerate() {
                pp_core::cancellation::check(cancellation)?;
                let bytes = match &encoded {
                    Some(bytes) => bytes,
                    None => {
                        let bytes = serde_json::to_vec(&envelope)
                            .context("serializing hook input as JSON")?;
                        encoded = Some(bytes);
                        encoded.as_ref().expect("just inserted")
                    }
                };
                let started = Instant::now();
                let outcome = exec::run(
                    exec::Request {
                        command: &handler.command,
                        args: handler.args.as_deref(),
                        timeout: handler.timeout,
                        cwd,
                        input: bytes,
                    },
                    cancellation,
                )
                .and_then(|outcome| {
                    protocol::parse(
                        event,
                        outcome.code,
                        &outcome.stdout,
                        &outcome.stderr,
                        envelope.get("tool_response"),
                    )
                });
                let duration = started.elapsed();
                let elapsed_ms = duration.as_millis() as u64;
                let duration_ms = duration.as_secs_f64() * 1000.0;
                let output = match outcome {
                    Ok(output) => output,
                    Err(error) => {
                        if error
                            .chain()
                            .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>())
                        {
                            return Err(error);
                        }
                        record_stat(
                            stats,
                            session_file,
                            cwd,
                            event,
                            tool,
                            pp_stats::HookStatus::Error,
                            false,
                            duration_ms,
                        );
                        let policy = handler.on_failure.unwrap_or(if event.is_control_point() {
                            HookFailurePolicy::Block
                        } else {
                            HookFailurePolicy::Continue
                        });
                        let message = protocol::bound(&format!(
                            "{event} hook handler [{index}] failed: {error:#}",
                            event = event.as_str()
                        ));
                        tracing::error!(
                            event = "hook_handler_failed",
                            hook = event.as_str(),
                            handler = index,
                            elapsed_ms,
                            policy = ?policy,
                            error = pp_core::logging::diagnostic_text(&format!("{error:#}"))
                        );
                        emit(AgentEvent::HookDiagnostic {
                            event: event.as_str().to_owned(),
                            message,
                        });
                        if policy == HookFailurePolicy::Block {
                            bail!(
                                "{} hook handler failed: {}",
                                event.as_str(),
                                protocol::bound(&format!("{error:#}"))
                            );
                        }
                        continue;
                    }
                };
                let blocked = output.deny.is_some() || output.stop.is_some();
                let modified = !blocked
                    && (output.updated_input.is_some()
                        || output.updated_output.is_some()
                        || !output.context.is_empty());
                record_stat(
                    stats,
                    session_file,
                    cwd,
                    event,
                    tool,
                    if blocked {
                        pp_stats::HookStatus::Blocked
                    } else {
                        pp_stats::HookStatus::Ok
                    },
                    modified,
                    duration_ms,
                );
                tracing::info!(
                    event = "hook_handler_finished",
                    hook = event.as_str(),
                    handler = index,
                    status = if blocked { "blocked" } else { "ok" },
                    elapsed_ms
                );
                if let Some(message) = output.system_message.clone() {
                    emit(AgentEvent::HookDiagnostic {
                        event: event.as_str().to_owned(),
                        message,
                    });
                }
                if let Some(updated) = output.updated_output.clone()
                    && let Value::Object(object) = &mut envelope
                {
                    object.insert("tool_response".to_owned(), updated);
                    encoded = None;
                }
                merge(event, &mut effect, output);
                if effect.stop_reason.is_some() {
                    // Task-level stop wins: run no further handlers and execute no tools.
                    return Ok(effect);
                }
                if effect.deny_reason.is_some() {
                    return Ok(effect);
                }
            }
        }
        Ok(effect)
    }
}

fn merge(event: HookEvent, effect: &mut HookEffect, output: Output) {
    if let Some(reason) = output.stop {
        effect.stop_reason.get_or_insert(reason);
        return;
    }
    if event == HookEvent::Stop {
        // Both deny and additionalContext on Stop are "run another turn" requests.
        if let Some(reason) = output.deny {
            effect.deny_reason.get_or_insert(reason);
            return;
        }
    } else if let Some(reason) = output.deny {
        effect.deny_reason.get_or_insert(reason);
        return;
    }
    if let Some(reason) = output.ask {
        effect.ask_reason.get_or_insert(reason);
    }
    if let Some(input) = output.updated_input {
        effect.updated_input = Some(input);
    }
    if let Some(output) = output.updated_output {
        effect.updated_output = Some(output);
    }
    effect.additional_context.extend(output.context);
}

#[expect(clippy::too_many_arguments)]
fn record_stat(
    stats: Option<&pp_stats::Database>,
    session_file: Option<&Path>,
    cwd: &Path,
    event: HookEvent,
    tool: Option<&str>,
    status: pp_stats::HookStatus,
    modified: bool,
    duration_ms: f64,
) {
    let Some(database) = stats else {
        return;
    };
    let Some(session_file) = session_file.filter(|path| !path.as_os_str().is_empty()) else {
        return;
    };
    pp_stats::hook(
        database,
        &pp_stats::HookRun {
            session_file,
            cwd,
            event,
            tool,
            status,
            modified,
            timestamp: chrono::Utc::now().timestamp_millis(),
            duration_ms,
        },
    );
}

fn event_index(event: HookEvent) -> usize {
    match event {
        HookEvent::UserPromptSubmit => 0,
        HookEvent::PreToolUse => 1,
        HookEvent::PostToolUse => 2,
        HookEvent::PostToolUseFailure => 3,
        HookEvent::Stop => 4,
    }
}
