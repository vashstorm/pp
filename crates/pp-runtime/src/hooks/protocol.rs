//! hook 的 stdin 输入信封与 stdout 决策协议。
//!
//! 兼容 Claude Code 的字段名与退出码语义，但工具名、工具参数与结果保持 pp
//! 原生格式。未知控制字段一律按协议错误处理，不静默放行。

use anyhow::{Result, bail};
use pp_config::config::HookEvent;
use serde_json::{Map, Value};

/// 注入模型上下文的单条文本上限（Unicode 字符）。
pub(super) const TEXT_LIMIT: usize = 10_000;
const TRUNCATED: &str = "…[truncated]";

/// 一次 hook 调用的公共信封。
pub(crate) struct Context<'a> {
    pub(crate) session_id: &'a str,
    pub(crate) transcript_path: &'a std::path::Path,
    pub(crate) cwd: &'a std::path::Path,
    /// `bypassPermissions`／`acceptEdits`／`default`，由生效审批档位映射而来。
    pub(crate) permission_mode: &'a str,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) agent_type: Option<&'a str>,
}

/// 事件专属载荷；序列化时只写入该事件允许的字段。
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
    // 子工具事件额外带真实身份；根 Agent 不伪造 agent_id。
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

/// 一个 handler 的规范化结果；合并顺序由调用方按事件规则处理。
#[derive(Debug, Default)]
pub(super) struct Output {
    /// 事件级否决理由：UserPromptSubmit／PreToolUse 拒绝，Stop 请求续跑。
    pub(super) deny: Option<String>,
    /// PreToolUse 的单次强制审批理由。
    pub(super) ask: Option<String>,
    pub(super) updated_input: Option<Value>,
    pub(super) updated_output: Option<Value>,
    /// 按 handler 顺序累计的上下文；空串不进入。
    pub(super) context: Vec<String>,
    /// `continue:false` 的任务停止理由。
    pub(super) stop: Option<String>,
    pub(super) system_message: Option<String>,
}

/// 按事件、退出码与 stdout/stderr 解析一个 handler 的决定。
///
/// `response` 只在 post 事件里用于校验 `updatedToolOutput` 的 `ok`／`cancelled`。
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
        // 普通文本只对 UserPromptSubmit 有意义：作为额外上下文注入。
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

/// exit 2：拒绝语义按事件固定，stdout 的 JSON 仍可提供上下文与消息，
/// 但不能抵消拒绝，也不能把拒绝变成允许。
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
        // post 无法撤销已发生的副作用：stderr 只作为反馈上下文。
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
    // `stopReason` 只是 `continue:false` 的停止理由，不能独立触发停止。
    let mut stop_reason: Option<String> = None;
    // `decision:"block"` 即使省略 reason 也必须阻止执行，不能静默放行。
    let mut blocked = false;
    for (key, value) in object {
        match key.as_str() {
            // 这些字段由 r#continue# 之外的分支处理。
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
        // post 无法撤销副作用，block 的默认理由只作为模型反馈；其它事件是事件级否决。
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
        // 空白理由等同于缺省理由：停止必须带可读的失败原因。
        output.stop = Some(match stop_reason {
            Some(reason) if !reason.is_empty() => reason,
            _ => bound("A hook stopped this task."),
        });
        // continue:false 是任务级停止，吸收其它决定。
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
                            "a PreToolUse hook requires approval for this tool call.",
                        )));
                    }
                    // `allow` 只表示 hook 不拦截；不跳过 pp 原有审批。
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
                // 后置失败只接受 additionalContext：失败的模型结果不能被改写。
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
        // exit 2 的理由优先于 JSON 里的上下文顺序；stderr 已由调用方处理。
        let _ = stderr;
    })
}

/// 只有输入、后置成功与 Stop 使用顶层 `decision`／`reason`；
/// 工具前置用 `permissionDecision`，后置失败只接受 `additionalContext`。
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

/// `updatedToolOutput` 只能改模型看到的内容：`ok`／`cancelled` 必须保留原值，
/// 原结果没有的状态字段也不能凭空新增。
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
            // 状态字段必须与真实结果一致：既不能改值，也不能凭空新增。
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
        // `kind:"image"` 是图片结果与真实附件的关联标识：正文可以被改写，
        // 但它不能丢失或被改值，否则跨协议历史转换会拒绝仍然存在的附件。
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

/// 诊断文本先遮蔽凭据，再净化控制字符与终端转义。
fn diagnostic(text: &str) -> String {
    let redacted = pp_core::logging::diagnostic_text(text);
    pp_core::text::display_text(&redacted).into_owned()
}

/// 单条注入文本按 Unicode 字符边界截断，并保留明确标记。
pub(crate) fn bound(text: &str) -> String {
    let text = diagnostic(text.trim());
    if text.chars().count() <= TEXT_LIMIT {
        return text;
    }
    let mut truncated: String = text.chars().take(TEXT_LIMIT).collect();
    truncated.push_str(TRUNCATED);
    truncated
}
