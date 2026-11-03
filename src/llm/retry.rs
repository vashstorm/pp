//! 模型失败分类、错误脱敏与重试预算。

use anyhow::Result;
use async_openai::error::OpenAIError;
use std::time::{Duration, Instant};

use crate::cancellation::Cancelled;
use crate::events::AgentEvent;

use super::{OpenAiClient, elapsed_millis};

/// 模型边界错误与本地事务错误分开，防止重试磁盘写入或工具副作用。
#[derive(Debug)]
pub(super) struct ModelFailure {
    pub(super) message: String,
    pub(super) reason: crate::events::RequestRetryReason,
    pub(super) retry_after: Option<u64>,
}
impl std::fmt::Display for ModelFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ModelFailure {}

pub(crate) fn model_request_failed(error: &anyhow::Error) -> bool {
    error.is::<ModelFailure>()
}

pub(super) fn provider_failure(error: OpenAIError, api: &str) -> anyhow::Error {
    let reason =
        request_retry_reason(&error).unwrap_or(crate::events::RequestRetryReason::ModelError);
    let retry_after = retry_after_seconds(&error);
    let error = redacted_error(error, api);
    let message = error.to_string();
    ModelFailure {
        message,
        reason,
        retry_after,
    }
    .into()
}

pub(super) struct RequestAttempts {
    pub(super) attempt: u32,
    pub(super) waited: Duration,
    pub(super) budget: Duration,
    pub(super) delay: Option<Duration>,
    pub(super) started: Instant,
    pub(super) request_context: Option<crate::stats::RequestSnapshot>,
    pub(super) pending: Option<crate::stats::Observation>,
    pub(super) pending_open: bool,
    pub(super) partial: bool,
    pub(super) summary: bool,
    pub(super) accounting_id: u64,
}
impl RequestAttempts {
    pub(super) fn new(budget: Duration) -> Self {
        Self {
            attempt: 0,
            waited: Duration::ZERO,
            budget,
            delay: None,
            started: Instant::now(),
            request_context: None,
            pending: None,
            pending_open: false,
            partial: false,
            summary: false,
            accounting_id: 0,
        }
    }
    pub(super) fn finish<T>(&mut self, result: &Result<T>) {
        self.delay = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ModelFailure>())
            .and_then(|error| {
                if self.attempt >= REQUEST_MAX_ATTEMPTS || self.waited >= self.budget {
                    return None;
                }
                let delay = error
                    .retry_after
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| Duration::from_secs(1 << (self.attempt - 1)));
                Some(delay.min(self.budget - self.waited))
            });
    }
}

impl OpenAiClient {
    pub(super) fn finish_backoff(
        &mut self,
        attempts: &mut RequestAttempts,
        model: &str,
        protocol: &str,
        emit: &mut dyn FnMut(AgentEvent),
    ) {
        if !attempts.pending_open {
            return;
        }
        attempts.pending_open = false;
        self.record_model_usage();
        self.settle_task_request(attempts.accounting_id, None);
        if let Some(mut observation) = attempts.pending.take() {
            observation.request_context = attempts.request_context.take();
            if let Some(context) = &self.stats_context {
                let cancelled: Result<()> = Err(Cancelled.into());
                self.stats_key = crate::stats::save(
                    context,
                    model,
                    protocol,
                    observation,
                    attempts.started,
                    &cancelled,
                );
            }
        }
        emit(AgentEvent::ModelRequestFinished {
            elapsed_ms: elapsed_millis(attempts.started),
            usage: None,
            success: false,
        });
    }
}

pub(super) fn redacted_error(error: OpenAIError, api: &str) -> anyhow::Error {
    // 先保存 SDK 仍持有的原始解析现场，再生成面向终端的脱敏消息。
    tracing::error!(
        event = "ai_transport_error",
        pid = std::process::id(),
        api,
        detail = crate::logging::diagnostic_text(&format!("{error:#}")),
    );
    match error {
        OpenAIError::Reqwest(error) if error.is_timeout() => {
            anyhow::anyhow!("{api} request timed out")
        }
        OpenAIError::ApiError(error) => {
            anyhow::anyhow!("{api} HTTP status {}", error.status_code)
        }
        OpenAIError::JSONDeserialize(error, _) if api == "Chat Completions" => {
            let reason = match error.classify() {
                serde_json::error::Category::Eof => "incomplete JSON",
                serde_json::error::Category::Syntax => "invalid JSON syntax",
                _ => "invalid JSON structure",
            };
            anyhow::anyhow!(
                "decoding {api} response: {reason} (line {}, column {})",
                error.line(),
                error.column()
            )
        }
        OpenAIError::JSONDeserialize(error, raw) => response_decode_error(&error, &raw, api),
        _ => anyhow::anyhow!("{api} HTTP request failed"),
    }
}

fn response_decode_error(error: &serde_json::Error, raw: &str, api: &str) -> anyhow::Error {
    // 只展示已知字段和事件名称；错误值、完整分片和供应商正文留在诊断日志。
    const FIELDS: &[&str] = &[
        "type",
        "sequence_number",
        "item_id",
        "output_index",
        "content_index",
        "summary_index",
        "part",
        "delta",
        "text",
        "refusal",
        "annotations",
        "item",
        "id",
        "role",
        "status",
        "content",
        "arguments",
        "call_id",
        "name",
        "response",
        "object",
        "created_at",
        "model",
        "output",
        "usage",
        "input_tokens",
        "output_tokens",
        "total_tokens",
        "input_tokens_details",
        "cached_tokens",
    ];
    let message = error.to_string();
    let missing = message
        .strip_prefix("missing field `")
        .and_then(|rest| rest.split('`').next())
        .filter(|field| FIELDS.contains(field));
    let reason = if let Some(field) = missing {
        format!("missing field `{field}`")
    } else {
        match error.classify() {
            serde_json::error::Category::Eof => "incomplete JSON",
            serde_json::error::Category::Syntax => "invalid JSON syntax",
            _ if message.starts_with("unknown variant ") => "unsupported enum value",
            _ if message.starts_with("invalid type:") => "invalid field type",
            _ => "invalid JSON structure",
        }
        .to_owned()
    };
    let value = serde_json::from_str::<serde_json::Value>(raw).ok();
    let event = value
        .as_ref()
        .and_then(|value| value.get("type"))
        .and_then(serde_json::Value::as_str)
        .filter(|event| {
            matches!(
                *event,
                "response.created"
                    | "response.in_progress"
                    | "response.completed"
                    | "response.failed"
                    | "response.incomplete"
                    | "error"
                    | "response.output_item.added"
                    | "response.output_item.done"
                    | "response.content_part.added"
                    | "response.content_part.done"
                    | "response.output_text.delta"
                    | "response.output_text.done"
                    | "response.refusal.delta"
                    | "response.refusal.done"
                    | "response.reasoning_summary_part.added"
                    | "response.reasoning_summary_part.done"
                    | "response.reasoning_summary_text.delta"
                    | "response.reasoning_summary_text.done"
                    | "response.reasoning_text.delta"
                    | "response.reasoning_text.done"
                    | "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done"
            )
        })
        .unwrap_or("unknown event");
    let location = if error.line() > 0 {
        format!("; line {}, column {}", error.line(), error.column())
    } else {
        String::new()
    };
    anyhow::anyhow!("decoding {api} response: {reason} (event: {event}{location})")
}

/// middleware 通过自构造 code 标记传递 Retry-After 整数秒；错误正文仅写诊断日志。
/// 脱敏错误只展示状态码，不输出该内部标记。
const RETRY_AFTER_CODE_PREFIX: &str = "pp-retry-after:";

/// Safety valve so a provider answering `Retry-After: 0` cannot turn the
/// retry budget into a tight request loop.
const REQUEST_MAX_ATTEMPTS: u32 = 10;

pub(super) fn retry_after_code(response: &reqwest::Response) -> Option<String> {
    let seconds = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(format!("{RETRY_AFTER_CODE_PREFIX}{seconds}"))
}

fn retry_after_seconds(error: &OpenAIError) -> Option<u64> {
    let response = match error {
        OpenAIError::ApiError(response) => response,
        _ => return None,
    };
    response
        .api_error
        .code
        .as_deref()?
        .strip_prefix(RETRY_AFTER_CODE_PREFIX)?
        .parse::<u64>()
        .ok()
}

/// 分类已脱敏的供应商错误；协议和流解析错误使用 ModelError。
fn request_retry_reason(error: &OpenAIError) -> Option<crate::events::RequestRetryReason> {
    use crate::events::RequestRetryReason;
    match error {
        OpenAIError::ApiError(response) => match response.status_code.as_u16() {
            429 => Some(RequestRetryReason::RateLimited),
            status => Some(RequestRetryReason::HttpStatus(status)),
        },
        OpenAIError::Reqwest(error) if error.is_timeout() => Some(RequestRetryReason::Timeout),
        OpenAIError::Reqwest(error) if error.is_connect() || error.is_request() => {
            Some(RequestRetryReason::Connection)
        }
        _ => None,
    }
}
