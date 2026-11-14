//! 生命周期 command hooks：配置编译、事件匹配、串行执行与决定合并。
//!
//! 配置来自用户级 `~/.pp/agent/hooks.yml` 与启动目录 `.pp/hooks.yml`。
//! 只有 command handler；脚本通过 stdin 收到 JSON 信封，用 stdout JSON 与
//! 退出码表达决定。任何无法确认回收的进程都升级为 `CleanupError`，调用方
//! 必须停止任务。

mod exec;
pub(crate) mod protocol;

#[cfg(test)]
mod tests;

use anyhow::{Context as _, Result, bail};
use pp_config::config::{HookEvent, HookFailurePolicy, HooksSettings};
use pp_core::cancellation::{CancellationToken, Cancelled, CleanupError};
use pp_core::events::AgentEvent;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

use protocol::Output;

/// 同一个事件内所有 handler 的结果合并。
#[derive(Debug, Default)]
pub(crate) struct HookEffect {
    /// UserPromptSubmit／PreToolUse 的事件级拒绝；Stop 的续跑请求。
    pub(crate) deny_reason: Option<String>,
    /// PreToolUse 的单次强制审批理由（强于 allow，不能跳过原审批）。
    pub(crate) ask_reason: Option<String>,
    pub(crate) updated_input: Option<Value>,
    pub(crate) updated_output: Option<Value>,
    pub(crate) additional_context: Vec<String>,
    /// `continue:false`：停止当前任务，不伪装成用户取消。
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
        // 仅由字母数字、`_`、`-`、空格、逗号、`|` 组成的 matcher 是名称列表；
        // 其余一律当作 Rust regex（不支持 JS 的 look-around 与反向引用）。
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
    /// 读取用户级与项目级 hooks 文件后编译；项目级组追加在用户级之后。
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

    /// 串行执行该事件的全部匹配 handler，并按事件规则合并决定。
    ///
    /// 返回 `Err` 表示该事件必须让任务失败（控制点默认 fail-closed、
    /// `onFailure: block`，或无法确认回收的 `CleanupError`）；被取消时保留
    /// `Cancelled` 类型。
    pub(crate) fn run(
        &self,
        event: HookEvent,
        input: &Value,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<HookEffect> {
        let mut effect = HookEffect::default();
        let tool = input.get("tool_name").and_then(Value::as_str);
        let cwd = input
            .get("cwd")
            .and_then(Value::as_str)
            .map(Path::new)
            .unwrap_or_else(|| Path::new("."));
        // 每个 handler 看到的是前序改写后的信封：`updatedToolOutput` 要传给后续 handler。
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
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let output = match outcome {
                    Ok(output) => output,
                    Err(error) => {
                        if error.is::<Cancelled>() || error.is::<CleanupError>() {
                            return Err(error);
                        }
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
                tracing::info!(
                    event = "hook_handler_finished",
                    hook = event.as_str(),
                    handler = index,
                    status = if output.deny.is_some() || output.stop.is_some() {
                        "blocked"
                    } else {
                        "ok"
                    },
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
                    // 任务级停止优先：不再运行后续 handler，也不执行任何工具。
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
        // Stop 的 deny／additionalContext 都是“再跑一轮”的请求。
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

fn event_index(event: HookEvent) -> usize {
    match event {
        HookEvent::UserPromptSubmit => 0,
        HookEvent::PreToolUse => 1,
        HookEvent::PostToolUse => 2,
        HookEvent::PostToolUseFailure => 3,
        HookEvent::Stop => 4,
    }
}
