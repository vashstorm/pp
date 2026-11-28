//! Model failure classification, error redaction and retry budget.

use anyhow::Result;
use async_openai::error::OpenAIError;
use std::time::{Duration, Instant};

use pp_core::cancellation::Cancelled;
use pp_core::events::AgentEvent;

use super::{OpenAiClient, elapsed_millis};

/// Model-boundary errors stay apart from local transaction errors so disk writes or tool side effects are never retried.
#[derive(Debug)]
pub(super) struct ModelFailure {
    pub(super) message: String,
    pub(super) reason: pp_core::events::RequestRetryReason,
    pub(super) retry_after: Option<u64>,
    pub(super) context_overflow: bool,
}
impl std::fmt::Display for ModelFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ModelFailure {}

pub fn model_request_failed(error: &anyhow::Error) -> bool {
    error.is::<ModelFailure>()
}

/// The provider rejected the request for exceeding the context window; a deterministic input limit that must not use exponential backoff.
pub fn is_context_overflow(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ModelFailure>()
        .is_some_and(|failure| failure.context_overflow)
}

pub(super) fn provider_failure(error: OpenAIError, api: &str) -> anyhow::Error {
    let reason =
        request_retry_reason(&error).unwrap_or(pp_core::events::RequestRetryReason::ModelError);
    let retry_after = retry_after_seconds(&error);
    let context_overflow = is_context_overflow_error(&error);
    let error = redacted_error(error, api);
    let message = error.to_string();
    ModelFailure {
        message,
        reason,
        retry_after,
        context_overflow,
    }
    .into()
}

pub(super) struct RequestAttempts {
    pub(super) attempt: u32,
    pub(super) waited: Duration,
    pub(super) budget: Duration,
    pub(super) delay: Option<Duration>,
    pub(super) started: Instant,
    pub(super) request_context: Option<pp_stats::RequestSnapshot>,
    pub(super) pending: Option<pp_stats::Observation>,
    pub(super) pending_open: bool,
    pub(super) partial: bool,
    pub(super) summary: bool,
    pub(super) accounting_id: u64,
    pub(super) accounting_rate: Option<pp_config::models::ModelCost>,
    pub(super) prompt_tokens: Option<u64>,
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
            accounting_rate: None,
            prompt_tokens: None,
        }
    }
    pub(super) fn finish<T>(&mut self, result: &Result<T>) {
        self.delay = result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ModelFailure>())
            .and_then(|error| {
                if error.context_overflow
                    || self.attempt >= REQUEST_MAX_ATTEMPTS
                    || self.waited >= self.budget
                {
                    return None;
                }
                let delay = error
                    .retry_after
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| Duration::from_secs(1 << self.attempt));
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
        self.finish_pending_open(attempts, model, protocol, None, emit);
    }

    pub(super) fn finish_pending_open(
        &mut self,
        attempts: &mut RequestAttempts,
        model: &str,
        protocol: &str,
        failure: Option<&anyhow::Error>,
        emit: &mut dyn FnMut(AgentEvent),
    ) {
        if !attempts.pending_open {
            return;
        }
        attempts.pending_open = false;
        self.record_model_usage();
        self.settle_task_request(attempts.accounting_id, attempts.accounting_rate, None);
        if let Some(mut observation) = attempts.pending.take() {
            observation.request_context = attempts.request_context.take();
            if let Some(context) = &self.stats_context {
                let recorded: Result<()> = match failure {
                    Some(err) if !err.chain().any(|cause| cause.is::<Cancelled>()) => {
                        Err(anyhow::anyhow!("{err:#}"))
                    }
                    _ => Err(Cancelled.into()),
                };
                self.stats_key = pp_stats::save(
                    context,
                    model,
                    protocol,
                    observation,
                    attempts.started,
                    &recorded,
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
    // Capture the raw parse context the SDK still holds, then build the redacted terminal message.
    tracing::error!(
        event = "ai_transport_error",
        pid = std::process::id(),
        api,
        detail = pp_core::logging::diagnostic_text(&format!("{error:#}")),
    );
    match error {
        OpenAIError::Reqwest(error) if error.is_timeout() => {
            anyhow::anyhow!("{api} request timed out")
        }
        OpenAIError::ApiError(error) => {
            anyhow::anyhow!("{api} HTTP status {}", error.status_code)
        }
        OpenAIError::StreamError(_) => {
            anyhow::anyhow!("{api} response stream interrupted")
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
    // Show only known fields and event names; error values, full chunks and provider bodies stay in diagnostic logs.
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

/// Middleware passes integer-seconds Retry-After through a self-constructed code marker; the error body goes to diagnostic logs only.
/// The redacted error shows the status code only and never prints that internal marker.
const RETRY_AFTER_CODE_PREFIX: &str = "pp-retry-after:";
/// Middleware passes the context-window-overflow classification through a self-constructed code marker; the raw response body never reaches the terminal.
pub(super) const CONTEXT_OVERFLOW_CODE: &str = "pp-context-overflow";

/// Safety valve so a provider answering `Retry-After: 0` cannot turn the
/// retry budget into a tight request loop.
const REQUEST_MAX_ATTEMPTS: u32 = 10;

/// Detect context-window overflow from the HTTP status code and a bounded error body (so middleware can decide before redaction drops the body).
pub(super) fn body_indicates_context_overflow(status: reqwest::StatusCode, body: &[u8]) -> bool {
    if !matches!(status.as_u16(), 400 | 413 | 422) {
        return false;
    }
    if status.as_u16() == 413 {
        return true;
    }
    let text = String::from_utf8_lossy(body);
    text_indicates_context_overflow(&text)
}

pub(super) fn text_indicates_context_overflow(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("context window")
        || lower.contains("prompt is too long")
        || lower.contains("input is too long")
        || lower.contains("too many tokens")
        || lower.contains("string_above_max_length")
}

fn is_context_overflow_error(error: &OpenAIError) -> bool {
    match error {
        OpenAIError::ApiError(response) => {
            response.api_error.code.as_deref() == Some(CONTEXT_OVERFLOW_CODE)
                || response
                    .api_error
                    .code
                    .as_deref()
                    .is_some_and(text_indicates_context_overflow)
                || text_indicates_context_overflow(&response.api_error.message)
        }
        _ => false,
    }
}

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

/// Classify an already redacted provider error; protocol and stream parse errors use ModelError.
fn request_retry_reason(error: &OpenAIError) -> Option<pp_core::events::RequestRetryReason> {
    use pp_core::events::RequestRetryReason;
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
