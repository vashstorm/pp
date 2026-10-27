use crate::cancellation::{CancellationToken, Cancelled};
use crate::context::TokenUsage;
use crate::{config::Config, events::AgentEvent};
use anyhow::{Result, bail, ensure};
use async_openai::{
    Client,
    config::OpenAIConfig,
    error::{ApiError, ApiErrorResponse, OpenAIError},
    middleware::ReqwestService,
    types::{
        chat::{
            ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
            CreateChatCompletionRequest, CreateChatCompletionResponse, FinishReason, Role,
        },
        responses::{
            CreateResponse, OutputItem, OutputMessageContent, OutputStatus, Response, Status,
        },
    },
};
use futures_util::StreamExt;
use reqwest::{header::HeaderValue, redirect::Policy};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tokio::runtime::Runtime;

mod streaming;

use streaming::{ChatChunk, ChatStream, ResponseEvent, ResponsesStream};

pub(crate) struct OpenAiClient {
    client: Client<OpenAIConfig>,
    runtime: Option<Runtime>,
    read_http: Option<reqwest::Client>,
    request_timeout: Duration,
    rate_limit_retry_budget: Duration,
    stats_context: Option<crate::stats::RequestContext>,
    stats_key: Option<String>,
}

pub(crate) struct ResponseTurn {
    pub(crate) output: Vec<OutputItem>,
    pub(crate) text: String,
    pub(crate) has_calls: bool,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) refusal: bool,
}

pub(crate) struct CompletionTurn {
    pub(crate) message: ChatCompletionRequestAssistantMessage,
    pub(crate) usage: Option<TokenUsage>,
    /// True only when the model refused without any normal non-empty content.
    pub(crate) refusal: bool,
}

impl OpenAiClient {
    pub(crate) fn set_stats_context(&mut self, context: crate::stats::RequestContext) {
        self.stats_context = Some(context);
        self.stats_key = None;
    }
    pub(crate) fn take_stats_key(&mut self) -> Option<String> {
        self.stats_key.take()
    }

    pub(crate) fn new(config: &Config) -> Result<Self> {
        // Validate before the SDK constructs its authorization header (which can panic).
        HeaderValue::from_str(&format!("Bearer {}", config.api_key)).map_err(|_| {
            anyhow::anyhow!("configuring HTTP authorization: invalid API key header")
        })?;
        let request_timeout = Duration::from_secs(config.settings.request_timeout_seconds);
        let rate_limit_retry_budget =
            Duration::from_secs(config.settings.rate_limit_retry_budget_seconds);
        // Explicitly override every environment-derived SDK setting.
        let sdk_config = OpenAIConfig::new()
            .with_api_key(config.api_key.clone())
            .with_api_base(config.base_url.as_str().trim_end_matches('/'))
            .with_org_id("")
            .with_project_id("");
        let client = build_client(sdk_config, request_timeout)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| anyhow::anyhow!("initializing model runtime failed"))?;
        Ok(Self {
            client,
            runtime: Some(runtime),
            read_http: None,
            request_timeout,
            rate_limit_retry_budget,
            stats_context: None,
            stats_key: None,
        })
    }

    /// Rebuilds the runtime and HTTP pool for the first request after
    /// `cancel_pending_io` retired them. Both are replaced together so no
    /// connection outlives the runtime that created it; ordinary requests
    /// never reconstruct anything.
    fn ensure_ready(&mut self) -> Result<()> {
        if self.runtime.is_some() {
            return Ok(());
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| anyhow::anyhow!("initializing model runtime failed"))?;
        let client = build_client(self.client.config().clone(), self.request_timeout)?;
        self.client = client;
        self.read_http = None;
        self.runtime = Some(runtime);
        Ok(())
    }

    /// Retires the runtime once the synchronous caller's `block_on` has
    /// already returned for a cancelled task. `shutdown_background`
    /// synchronously drops the SDK's owned stream tasks and closes their
    /// connections without waiting for unstoppable blocking DNS work; the
    /// next request rebuilds everything lazily via `ensure_ready`.
    pub(crate) fn cancel_pending_io(&mut self) {
        self.read_http = None;
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// 网页请求使用独立连接池，绝不复用模型认证头或限流重试策略。
    pub(crate) fn fetch_read_url(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<crate::tools::read::HttpResponse> {
        use crate::tools::read::{HttpResponse, MAX_INPUT_BYTES};
        self.ensure_ready()?;
        let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid read URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "read URL requires HTTP/HTTPS without embedded credentials"
        );
        if self.read_http.is_none() {
            self.read_http = Some(
                reqwest::Client::builder()
                    .redirect(Policy::limited(5))
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|_| anyhow::anyhow!("initializing read HTTP client failed"))?,
            );
        }
        let client = self
            .read_http
            .as_ref()
            .expect("read HTTP client initialized");
        let runtime = self.runtime.as_ref().expect("runtime initialized");
        runtime.block_on(async {
            let fetch = async {
                let mut response = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("read HTTP request failed"))?;
                anyhow::ensure!(
                    response.status().is_success(),
                    "read HTTP status {}",
                    response.status().as_u16()
                );
                let final_url = response.url().to_string();
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let mut bytes = Vec::new();
                while let Some(chunk) = response
                    .chunk()
                    .await
                    .map_err(|_| anyhow::anyhow!("reading HTTP body failed"))?
                {
                    anyhow::ensure!(
                        bytes.len().saturating_add(chunk.len()) <= MAX_INPUT_BYTES,
                        "read HTTP body exceeds 20 MiB limit"
                    );
                    bytes.extend_from_slice(&chunk);
                }
                Ok(HttpResponse {
                    bytes,
                    final_url,
                    content_type,
                })
            };
            cancellation
                .run_until_cancelled(fetch)
                .await
                .unwrap_or_else(|| Err(Cancelled.into()))
        })
    }

    pub(crate) fn complete(
        &mut self,
        request: &CreateChatCompletionRequest,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<CompletionTurn> {
        self.ensure_ready()?;
        let started_at = Instant::now();
        let record = !cancellation.is_cancelled() && self.stats_context.is_some();
        let mut observation = crate::stats::Observation::new();
        emit(AgentEvent::ModelRequestStarted);
        let mut progress = |event: AgentEvent| {
            if record {
                observation.event(&event, started_at);
            }
            emit(event);
        };
        let client = &self.client;
        let runtime = self
            .runtime
            .as_ref()
            .expect("ensure_ready leaves a runtime in place");
        let budget = self.rate_limit_retry_budget;
        let mut output = ChatStream::default();
        let mut snapshot = serde_json::Value::Null;
        let result: Result<CompletionTurn> = runtime.block_on(async {
            // The BYOT future borrows its resource; bind it so retries can
            // create further futures from the same borrow.
            let chat = client.chat();
            // BYOT serializes a borrowed request before returning the stream.
            let opened = cancellation
                .run_until_cancelled(chat.create_stream_byot::<_, ChatChunk>(request))
                .await;
            // Persist the accepted request exactly once, before judging the
            // opening outcome; a callback failure takes precedence.
            on_request_sent()?;
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = open_with_rate_limit_retry(
                cancellation,
                budget,
                "Chat Completions",
                &mut progress,
                opened,
                || chat.create_stream_byot::<_, ChatChunk>(request),
            )
            .await?;
            loop {
                match cancellation.run_until_cancelled(stream.next()).await {
                    None => return Err(Cancelled.into()),
                    Some(None) => break,
                    Some(Some(chunk)) => {
                        output.push(
                            chunk.map_err(|error| redacted_error(error, "Chat Completions"))?,
                            &mut progress,
                        )?;
                    }
                }
            }
            if record {
                snapshot = output.stats_snapshot();
            }
            let written = output.cache_write_tokens;
            let mut turn = into_completion_turn(std::mem::take(&mut output).finish()?)?;
            if let Some(usage) = &mut turn.usage {
                usage.cache_write_tokens = written;
            }
            Ok(turn)
        });
        let elapsed_ms = elapsed_millis(started_at);
        if record {
            if snapshot.is_null() {
                snapshot = output.stats_snapshot();
            }
            observation.stop_reason = snapshot["finish_reason"].as_str().map(str::to_owned);
            observation.usage = result
                .as_ref()
                .ok()
                .and_then(|turn| turn.usage)
                .or_else(|| {
                    let usage = &snapshot["usage"];
                    Some(TokenUsage {
                        input_tokens: usage["prompt_tokens"].as_u64()?,
                        output_tokens: usage["completion_tokens"].as_u64()?,
                        cached_tokens: usage["prompt_tokens_details"]["cached_tokens"].as_u64(),
                        cache_write_tokens: usage["prompt_tokens_details"]["cache_write_tokens"]
                            .as_u64(),
                    })
                });
            observation.output = snapshot;
            if let Some(context) = &self.stats_context {
                self.stats_key = crate::stats::save(
                    context,
                    &request.model,
                    "chat-completions",
                    observation,
                    started_at,
                    &result,
                );
            }
        }
        // Every outcome, including cancellation and callback failures, closes the
        // request exactly once with the authoritative usage when there is one.
        emit(AgentEvent::ModelRequestFinished {
            elapsed_ms,
            usage: result.as_ref().ok().and_then(|turn| turn.usage),
            success: result.is_ok(),
        });
        result
    }

    pub(crate) fn respond(
        &mut self,
        request: &CreateResponse,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ResponseTurn> {
        self.ensure_ready()?;
        let started_at = Instant::now();
        let record = !cancellation.is_cancelled() && self.stats_context.is_some();
        let mut observation = crate::stats::Observation::new();
        emit(AgentEvent::ModelRequestStarted);
        let mut progress = |event: AgentEvent| {
            if record {
                observation.event(&event, started_at);
            }
            emit(event);
        };
        let client = &self.client;
        let runtime = self
            .runtime
            .as_ref()
            .expect("ensure_ready leaves a runtime in place");
        let mut snapshot = serde_json::Value::Null;
        let mut tool_deltas = Vec::new();
        let mut stop_reason = None;
        let mut reported_usage = None;
        let budget = self.rate_limit_retry_budget;
        let result: Result<ResponseTurn> = runtime.block_on(async {
            let responses = client.responses();
            let opened = cancellation
                .run_until_cancelled(
                    responses.create_stream_byot::<_, ResponseEvent>(request),
                )
                .await;
            on_request_sent()?;
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = open_with_rate_limit_retry(
                cancellation,
                budget,
                "Responses",
                &mut progress,
                opened,
                || responses.create_stream_byot::<_, ResponseEvent>(request),
            )
            .await?;
            let mut output = ResponsesStream::default();
            loop {
                match cancellation.run_until_cancelled(stream.next()).await {
                    None => return Err(Cancelled.into()),
                    Some(None) => {
                        bail!("Responses protocol: stream ended before response.completed")
                    }
                    Some(Some(event)) => {
                        let ResponseEvent(event, written) = event.map_err(|error| redacted_error(error, "Responses"))?;
                        if record {
                            use async_openai::types::responses::ResponseStreamEvent;
                            let response = match &event {
                                ResponseStreamEvent::ResponseCompleted(done) => Some(&done.response),
                                ResponseStreamEvent::ResponseFailed(failed) => Some(&failed.response),
                                ResponseStreamEvent::ResponseIncomplete(incomplete) => Some(&incomplete.response),
                                _ => None,
                            };
                            if let Some(response) = response {
                                snapshot = serde_json::to_value(&response.output).unwrap_or_default();
                                reported_usage = response.usage.as_ref().map(|usage| TokenUsage {
                                    input_tokens: u64::from(usage.input_tokens),
                                    output_tokens: u64::from(usage.output_tokens),
                                    cache_write_tokens: written,
                                    cached_tokens: Some(u64::from(usage.input_tokens_details.cached_tokens)),
                                });
                                stop_reason = Some(if response.status != Status::Completed {
                                    response.incomplete_details.as_ref().map_or("failed", |d|d.reason.as_str())
                                } else if response.output.iter().any(|item|matches!(item,OutputItem::FunctionCall(_))) {
                                    "tool_calls"
                                } else { "stop" }.to_owned());
                            }
                            if let ResponseStreamEvent::ResponseFunctionCallArgumentsDelta(delta) = &event {
                                tool_deltas.push(serde_json::json!({"item_id":delta.item_id,"delta":delta.delta}));
                            }
                        }
                        if let Some(mut turn) = output.push(event, &mut progress)? {
                            if let Some(usage) = &mut turn.usage {
                                usage.cache_write_tokens = written;
                            }
                            // response.completed is authoritative; do not wait for HTTP EOF.
                            return Ok(turn);
                        }
                    }
                }
            }
        });
        let elapsed_ms = elapsed_millis(started_at);
        if record {
            observation.usage = result
                .as_ref()
                .ok()
                .and_then(|turn| turn.usage)
                .or(reported_usage);
            observation.stop_reason = stop_reason;
            observation.output = if snapshot.is_null() {
                serde_json::json!({"tool_argument_deltas":tool_deltas})
            } else {
                snapshot
            };
            if let Some(context) = &self.stats_context {
                self.stats_key = crate::stats::save(
                    context,
                    request.model.as_deref().unwrap_or_default(),
                    "responses",
                    observation,
                    started_at,
                    &result,
                );
            }
        }
        // The stream may still be open on the wire (or already cancelled); the
        // finish event reports this request, not HTTP EOF.
        emit(AgentEvent::ModelRequestFinished {
            elapsed_ms,
            usage: result.as_ref().ok().and_then(|turn| turn.usage),
            success: result.is_ok(),
        });
        result
    }
}

/// Saturates instead of wrapping: an `elapsed_ms` beyond `u64` is far past any
/// meaningful request duration.
fn elapsed_millis(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn build_client(sdk_config: OpenAIConfig, timeout: Duration) -> Result<Client<OpenAIConfig>> {
    let http = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| anyhow::anyhow!("initializing HTTP client failed"))?;
    // Keep status codes even for non-JSON errors, without reading or exposing their bodies.
    let service = tower::ServiceBuilder::new()
        .and_then(|response: reqwest::Response| async move {
            if response.status().is_success() {
                Ok(response)
            } else {
                Err(OpenAIError::ApiError(ApiErrorResponse {
                    status_code: response.status(),
                    api_error: ApiError {
                        message: String::new(),
                        r#type: None,
                        param: None,
                        code: retry_after_code(&response),
                        misalignment: None,
                    },
                }))
            }
        })
        .service(ReqwestService::new(http.clone()));
    Ok(Client::build(http, sdk_config).with_http_service(service))
}

fn redacted_error(error: OpenAIError, api: &str) -> anyhow::Error {
    match error {
        OpenAIError::Reqwest(error) if error.is_timeout() => {
            anyhow::anyhow!("{api} request timed out")
        }
        OpenAIError::ApiError(error) => {
            anyhow::anyhow!("{api} HTTP status {}", error.status_code)
        }
        OpenAIError::JSONDeserialize(..) => {
            anyhow::anyhow!("decoding {api} response: invalid JSON or response structure")
        }
        _ => anyhow::anyhow!("{api} HTTP request failed"),
    }
}

/// The middleware never reads response bodies, so the integer seconds of a
/// 429's `Retry-After` header travel through the otherwise-empty `code`
/// field for the retry loop. `redacted_error` only prints status codes, so
/// this marker never reaches user-visible output.
const RETRY_AFTER_CODE_PREFIX: &str = "pp-retry-after:";

/// Safety valve so a provider answering `Retry-After: 0` cannot turn the
/// retry budget into a tight request loop.
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 10;

fn retry_after_code(response: &reqwest::Response) -> Option<String> {
    if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
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
    if response.status_code != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return None;
    }
    response
        .api_error
        .code
        .as_deref()?
        .strip_prefix(RETRY_AFTER_CODE_PREFIX)?
        .parse::<u64>()
        .ok()
}

/// Only opening-phase HTTP 429 responses are retryable; the stream has not
/// started, so no partial output exists.
fn is_rate_limited(error: &OpenAIError) -> bool {
    matches!(
        error,
        OpenAIError::ApiError(response)
            if response.status_code == reqwest::StatusCode::TOO_MANY_REQUESTS
    )
}

/// Wait before the next opening attempt of a rate-limited request: the
/// provider's `Retry-After` seconds win when present (zero retries at
/// once), otherwise the delay doubles per attempt starting at one second.
/// Waits are clipped to the remaining budget; `None` means stop retrying.
fn rate_limit_wait(
    error: &OpenAIError,
    waited: Duration,
    budget: Duration,
    attempt: u32,
) -> Option<Duration> {
    if !is_rate_limited(error) {
        return None;
    }
    if budget.is_zero() || waited >= budget || attempt >= RATE_LIMIT_MAX_ATTEMPTS {
        return None;
    }
    // The attempt guard above bounds the shift well below overflow.
    let backoff = match retry_after_seconds(error) {
        Some(seconds) => Duration::from_secs(seconds),
        None => Duration::from_secs(1 << attempt),
    };
    Some(backoff.min(budget - waited))
}

/// Final error for a failed opening phase: anything but an exhausted 429
/// retry budget keeps the plain redacted message, matching requests that
/// never retried.
fn opening_error(error: OpenAIError, api: &str, waited: Duration, retries: u32) -> anyhow::Error {
    if !is_rate_limited(&error) || retries == 0 {
        return redacted_error(error, api);
    }
    anyhow::anyhow!(
        "{} (rate limited; gave up after {} retries and {}s of backoff)",
        redacted_error(error, api),
        retries,
        waited.as_secs()
    )
}

/// Opens a stream, retrying the opening phase while the provider answers
/// 429 within the wait budget. Retrying is safe only before the first
/// chunk: no partial output has been committed, and `on_request_sent` has
/// already fired exactly once for the first attempt. Both the backoff sleep
/// and every reopening stay cancellable.
async fn open_with_rate_limit_retry<S, F, Fut>(
    cancellation: &CancellationToken,
    budget: Duration,
    api: &str,
    progress: &mut dyn FnMut(AgentEvent),
    mut opened: Result<S, OpenAIError>,
    mut reopen: F,
) -> Result<S>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<S, OpenAIError>>,
{
    let mut waited = Duration::ZERO;
    let mut retries = 0u32;
    loop {
        let error = match opened {
            Ok(stream) => return Ok(stream),
            Err(error) => error,
        };
        let Some(delay) = rate_limit_wait(&error, waited, budget, retries) else {
            return Err(opening_error(error, api, waited, retries));
        };
        retries += 1;
        waited += delay;
        progress(AgentEvent::RateLimited {
            attempt: retries,
            wait_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        });
        match cancellation
            .run_until_cancelled(tokio::time::sleep(delay))
            .await
        {
            Some(()) => {}
            None => return Err(Cancelled.into()),
        }
        match cancellation.run_until_cancelled(reopen()).await {
            Some(next) => opened = next,
            None => return Err(Cancelled.into()),
        }
    }
}

fn into_completion_turn(response: CreateChatCompletionResponse) -> Result<CompletionTurn> {
    // Extract server-side usage before the message validation merges or drops fields.
    let usage = response.usage.map(|usage| TokenUsage {
        input_tokens: u64::from(usage.prompt_tokens),
        output_tokens: u64::from(usage.completion_tokens),
        cache_write_tokens: None,
        cached_tokens: usage
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens)
            .map(u64::from),
    });
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Chat Completions protocol: empty choices"))?;
    ensure!(
        matches!(
            choice.finish_reason,
            Some(FinishReason::Stop | FinishReason::ToolCalls)
        ),
        "Chat Completions protocol: unsupported finish_reason (expected stop or tool_calls; response may be truncated)"
    );
    let message = choice.message;
    ensure!(
        message.role == Role::Assistant,
        "Chat Completions protocol: expected assistant role"
    );
    let calls = message.tool_calls.unwrap_or_default();
    let mut ids = HashSet::with_capacity(calls.len());
    for call in &calls {
        let ChatCompletionMessageToolCalls::Function(call) = call else {
            bail!("Chat Completions protocol: expected function tool call");
        };
        ensure!(
            !call.id.trim().is_empty(),
            "Chat Completions protocol: blank tool call id"
        );
        ensure!(
            !call.function.name.trim().is_empty(),
            "Chat Completions protocol: blank tool name"
        );
        ensure!(
            ids.insert(call.id.as_str()),
            "Chat Completions protocol: duplicate tool call id"
        );
    }
    // Record refusal before the content merge below replaces it: only a refusal
    // without any normal non-empty content counts as a true refusal.
    let refusal = !message
        .content
        .as_ref()
        .is_some_and(|text| !text.trim().is_empty())
        && message
            .refusal
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());
    let content = message
        .content
        .filter(|text| !text.trim().is_empty())
        .or_else(|| message.refusal.filter(|text| !text.trim().is_empty()));
    if content.is_none() && calls.is_empty() {
        bail!("Chat Completions protocol: response has neither text nor tool calls");
    }
    Ok(CompletionTurn {
        message: ChatCompletionRequestAssistantMessage {
            content: content.map(Into::into),
            tool_calls: (!calls.is_empty()).then_some(calls),
            ..Default::default()
        },
        usage,
        refusal,
    })
}

fn into_response_turn(response: Response) -> Result<ResponseTurn> {
    ensure!(
        response.status == Status::Completed
            && response.error.is_none()
            && response.incomplete_details.is_none(),
        "Responses protocol: expected a completed response without errors"
    );
    let mut ids = HashSet::new();
    for item in &response.output {
        match item {
            OutputItem::FunctionCall(call) => {
                ensure!(
                    call.status
                        .is_none_or(|status| status == OutputStatus::Completed),
                    "Responses protocol: incomplete function call"
                );
                ensure!(
                    !call.call_id.trim().is_empty(),
                    "Responses protocol: blank tool call id"
                );
                ensure!(
                    !call.name.trim().is_empty(),
                    "Responses protocol: blank tool name"
                );
                ensure!(
                    ids.insert(call.call_id.as_str()),
                    "Responses protocol: duplicate tool call id"
                );
            }
            OutputItem::Message(message) => {
                ensure!(
                    message.status == OutputStatus::Completed,
                    "Responses protocol: incomplete message"
                );
            }
            OutputItem::Reasoning(reasoning) => {
                ensure!(
                    reasoning
                        .status
                        .is_none_or(|status| status == OutputStatus::Completed),
                    "Responses protocol: incomplete reasoning item"
                );
            }
            _ => bail!(
                "Responses protocol: unsupported output item (expected message, reasoning, or function_call)"
            ),
        }
    }
    let has_calls = !ids.is_empty();
    // Preserve server totals and the cached input breakdown; cached and
    // reasoning tokens are already included in their respective totals.
    let usage = response.usage.map(|usage| TokenUsage {
        input_tokens: u64::from(usage.input_tokens),
        output_tokens: u64::from(usage.output_tokens),
        cache_write_tokens: None,
        cached_tokens: Some(u64::from(usage.input_tokens_details.cached_tokens)),
    });
    let has_text = |output: &[OutputItem]| {
        output.iter().any(|item| match item {
            OutputItem::Message(message) => message.content.iter().any(|part| {
                matches!(part, OutputMessageContent::OutputText(text) if !text.text.trim().is_empty())
            }),
            _ => false,
        })
    };
    let refusal = !has_text(&response.output)
        && response.output.iter().any(|item| match item {
            OutputItem::Message(message) => message
                .content
                .iter()
                .any(|part| matches!(part, OutputMessageContent::Refusal(_))),
            _ => false,
        });
    let output = response.output;
    let text = response_text(&output);
    ensure!(
        has_calls || !text.trim().is_empty(),
        "Responses protocol: response has neither text nor tool calls"
    );
    Ok(ResponseTurn {
        output,
        text,
        has_calls,
        usage,
        refusal,
    })
}

fn response_text(output: &[OutputItem]) -> String {
    let contents = || {
        output
            .iter()
            .filter_map(|item| match item {
                OutputItem::Message(message) => Some(message.content.iter()),
                _ => None,
            })
            .flatten()
    };
    let text: String = contents()
        .filter_map(|content| match content {
            OutputMessageContent::OutputText(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect();
    if text.trim().is_empty() {
        contents()
            .filter_map(|content| match content {
                OutputMessageContent::Refusal(refusal) => Some(refusal.refusal.as_str()),
                _ => None,
            })
            .collect()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_openai::types::chat::ChatCompletionRequestAssistantMessageContent;
    use serde_json::{Value, json};
    use std::io::Write;

    #[test]
    fn read_http_redirects_without_provider_auth_and_redacts_errors() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/first", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert!(!request.headers().iter().any(|h| matches!(
                h.field.as_str().as_str().to_ascii_lowercase().as_str(),
                "authorization" | "openai-organization" | "openai-project" | "cookie"
            )));
            request
                .respond(
                    tiny_http::Response::empty(302)
                        .with_header(tiny_http::Header::from_bytes("Location", "/final").unwrap()),
                )
                .unwrap();
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(request.url(), "/final");
            request
                .respond(tiny_http::Response::from_string("web body").with_header(
                    tiny_http::Header::from_bytes("Content-Type", "text/plain").unwrap(),
                ))
                .unwrap();
        });
        let mut client = test_client("http://127.0.0.1:1/v1");
        let response = client
            .fetch_read_url(&url, &CancellationToken::new())
            .unwrap();
        assert_eq!(response.bytes, b"web body");
        assert!(response.final_url.ends_with("/final"));
        worker.join().unwrap();
        for url in [
            "http://user:secret@127.0.0.1/",
            "ftp://example.com/",
            "bad-url-secret",
        ] {
            let message = client
                .fetch_read_url(url, &CancellationToken::new())
                .err()
                .unwrap()
                .to_string();
            assert!(!message.contains("secret") && !message.contains("private-key"));
        }
    }

    #[test]
    fn read_http_limits_redirects_and_response_body_without_retries() {
        for oversize in [false, true] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/loop", server.server_addr());
            let worker = std::thread::spawn(move || {
                for _ in 0..if oversize { 1 } else { 6 } {
                    let request = server
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    if oversize {
                        let _ = request.respond(tiny_http::Response::from_data(vec![
                            b'x';
                            crate::tools::read::MAX_INPUT_BYTES
                                + 1
                        ]));
                    } else {
                        request
                            .respond(tiny_http::Response::empty(302).with_header(
                                tiny_http::Header::from_bytes("Location", "/loop").unwrap(),
                            ))
                            .unwrap();
                    }
                }
                assert!(
                    server
                        .recv_timeout(Duration::from_millis(100))
                        .unwrap()
                        .is_none()
                );
            });
            let error = test_client("http://127.0.0.1:1/v1")
                .fetch_read_url(&url, &CancellationToken::new())
                .err()
                .unwrap();
            if oversize {
                assert!(error.to_string().contains("20 MiB"));
            }
            worker.join().unwrap();
        }
    }

    #[test]
    fn read_http_gzip_limit_counts_decompressed_bytes() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/compressed", server.server_addr());
        let worker = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            let block = [b'x'; 8192];
            for _ in 0..(crate::tools::read::MAX_INPUT_BYTES / block.len() + 1) {
                encoder.write_all(&block).unwrap();
            }
            let compressed = encoder.finish().unwrap();
            assert!(compressed.len() < crate::tools::read::MAX_INPUT_BYTES);
            let _ =
                request.respond(tiny_http::Response::from_data(compressed).with_header(
                    tiny_http::Header::from_bytes("Content-Encoding", "gzip").unwrap(),
                ));
        });
        let error = test_client("http://127.0.0.1:1/v1")
            .fetch_read_url(&url, &CancellationToken::new())
            .err()
            .unwrap();
        assert!(error.to_string().contains("20 MiB"), "{error}");
        worker.join().unwrap();
    }

    #[test]
    fn read_http_status_timeout_and_cancel_leave_no_background_fetch() {
        for mode in ["status", "timeout", "cancel"] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let url = format!("http://{}/private-query", server.server_addr());
            let cancel = CancellationToken::new();
            let worker_cancel = cancel.clone();
            let (done, release) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                if mode == "status" {
                    request
                        .respond(
                            tiny_http::Response::from_string("secret-body").with_status_code(429),
                        )
                        .unwrap();
                } else {
                    let mut writer = request.into_writer();
                    if mode == "cancel" {
                        writer
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n",
                            )
                            .unwrap();
                        writer.flush().unwrap();
                        worker_cancel.cancel();
                    }
                    release.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                assert!(
                    server
                        .recv_timeout(Duration::from_millis(100))
                        .unwrap()
                        .is_none()
                );
            });
            let mut client = test_client("http://127.0.0.1:1/v1");
            if mode == "timeout" {
                client.read_http = Some(
                    reqwest::Client::builder()
                        .timeout(Duration::from_millis(100))
                        .build()
                        .unwrap(),
                );
            }
            let started = Instant::now();
            let error = client.fetch_read_url(&url, &cancel).err().unwrap();
            assert!(started.elapsed() < Duration::from_secs(2));
            if mode == "cancel" {
                assert!(error.is::<Cancelled>());
            }
            if mode == "status" {
                assert!(error.to_string().contains("429"));
            }
            assert!(
                !error.to_string().contains("secret-body")
                    && !error.to_string().contains("private-query")
            );
            let _ = done.send(());
            worker.join().unwrap();
        }
    }

    fn decode(message: Value, finish: &str) -> Result<CompletionTurn> {
        into_completion_turn(serde_json::from_value(json!({
            "id":"chatcmpl-fixture", "object":"chat.completion", "created":0, "model":"fixture",
            "choices":[{"index":0,"message":message,"finish_reason":finish}]
        }))?)
    }

    fn test_client(base_url: &str) -> OpenAiClient {
        test_client_with_rate_limit_budget(base_url, 0)
    }

    fn test_client_with_rate_limit_budget(base_url: &str, budget_seconds: u64) -> OpenAiClient {
        OpenAiClient::new(&Config {
            api_key: "private-key".into(),
            model: "fixture".into(),
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
            base_url: base_url.parse().unwrap(),
            context_window: None,
            settings: crate::config::Settings {
                grep: Default::default(),
                read: Default::default(),
                debug: false,
                notification_show_countdown: true,
                notification_timeout_seconds: 10,
                mcp_servers: Default::default(),
                skill_dirs: Vec::new(),
                skill_sources: Vec::new(),
                disabled_skills: Vec::new(),
                think_level: crate::config::ThinkLevel::None,
                edit: crate::config::EditMode::Replace,
                default_model: None,
                favorite_models: Vec::new(),
                request_timeout_seconds: 120,
                rate_limit_retry_budget_seconds: budget_seconds,
                read_max_bytes: 65_536,
                read_max_lines: 2_000,
                bash_timeout_seconds: 120,
                auto_compact: true,
                auto_compact_threshold_percent: 80,
            },
        })
        .unwrap()
    }

    fn chat_chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
            "created":1, "model":"fixture-model",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        })
    }

    fn sse_writer(request: tiny_http::Request) -> Box<dyn Write + Send> {
        let mut writer = request.into_writer();
        writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        writer.flush().unwrap();
        writer
    }

    fn sse_frame(writer: &mut Box<dyn Write + Send>, data: &str) {
        let frame = format!("data: {data}\n\n");
        write!(writer, "{:x}\r\n{}\r\n", frame.len(), frame).unwrap();
        writer.flush().unwrap();
    }

    fn sse_end(mut writer: Box<dyn Write + Send>) {
        // The peer may already be gone after cancellation; closing is best effort.
        let _ = writer.write_all(b"0\r\n\r\n");
        let _ = writer.flush();
    }

    #[test]
    fn http_errors_hide_body_and_redirects_are_not_followed() {
        for status in [302, 401, 429, 500] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", server.server_addr());
            let mut client = test_client(&base_url);
            let token = CancellationToken::new();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let request = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    request
                        .respond(
                            tiny_http::Response::from_string("private-body")
                                .with_status_code(status)
                                .with_header(
                                    tiny_http::Header::from_bytes("Location", base_url.as_str())
                                        .unwrap(),
                                ),
                        )
                        .unwrap();
                });
                let mut sent = 0;
                let mut finished = None;
                let error = client
                    .complete(
                        &CreateChatCompletionRequest::default(),
                        &token,
                        &mut || {
                            sent += 1;
                            Ok(())
                        },
                        &mut |event| {
                            if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                                finished = Some((usage, success));
                            }
                        },
                    )
                    .err()
                    .unwrap()
                    .to_string();
                assert_eq!(finished, Some((None, false)));
                assert_eq!(sent, 1);
                assert!(error.contains(&status.to_string()));
                assert!(!error.contains("private-body") && !error.contains("private-key"));
            });
            assert!(
                server
                    .recv_timeout(Duration::from_millis(50))
                    .unwrap()
                    .is_none()
            );
        }
    }

    fn api_error(status: u16, code: Option<&str>) -> OpenAIError {
        OpenAIError::ApiError(ApiErrorResponse {
            status_code: reqwest::StatusCode::from_u16(status).unwrap(),
            api_error: ApiError {
                message: String::new(),
                r#type: None,
                param: None,
                code: code.map(str::to_owned),
                misalignment: None,
            },
        })
    }

    #[test]
    fn rate_limit_wait_honors_header_exponential_fallback_and_budget() {
        let budget = Duration::from_secs(300);
        let no_header = api_error(429, None);
        assert_eq!(
            rate_limit_wait(&no_header, Duration::ZERO, budget, 0),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            rate_limit_wait(&no_header, Duration::ZERO, budget, 1),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            rate_limit_wait(&no_header, Duration::ZERO, budget, 3),
            Some(Duration::from_secs(8))
        );
        let header = api_error(429, Some("pp-retry-after:7"));
        assert_eq!(
            rate_limit_wait(&header, Duration::ZERO, budget, 0),
            Some(Duration::from_secs(7))
        );
        // Zero seconds means the provider allows an immediate retry.
        assert_eq!(
            rate_limit_wait(
                &api_error(429, Some("pp-retry-after:0")),
                Duration::ZERO,
                budget,
                5
            ),
            Some(Duration::ZERO)
        );
        // Waits are clipped to the remaining budget.
        assert_eq!(
            rate_limit_wait(&header, Duration::from_secs(296), budget, 0),
            Some(Duration::from_secs(4))
        );
        // Budget spent, budget disabled, the attempt cap, and non-429 stop retrying.
        assert_eq!(rate_limit_wait(&header, budget, budget, 0), None);
        assert_eq!(
            rate_limit_wait(&header, Duration::ZERO, Duration::ZERO, 0),
            None
        );
        assert_eq!(
            rate_limit_wait(&no_header, Duration::ZERO, budget, 10),
            None
        );
        assert_eq!(
            rate_limit_wait(&api_error(500, None), Duration::ZERO, budget, 0),
            None
        );
        // Only the middleware's own marker counts as a Retry-After value.
        assert_eq!(
            rate_limit_wait(&api_error(429, Some("7")), Duration::ZERO, budget, 0),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn opening_error_notes_exhausted_budget_only_after_retries() {
        assert_eq!(
            opening_error(api_error(429, None), "Chat Completions", Duration::ZERO, 0).to_string(),
            "Chat Completions HTTP status 429 Too Many Requests"
        );
        let message = opening_error(
            api_error(429, None),
            "Chat Completions",
            Duration::from_secs(3),
            2,
        )
        .to_string();
        assert!(message.contains("HTTP status 429"));
        assert!(message.contains("gave up after 2 retries and 3s of backoff"));
        assert_eq!(
            opening_error(
                api_error(500, None),
                "Chat Completions",
                Duration::from_secs(3),
                2
            )
            .to_string(),
            "Chat Completions HTTP status 500 Internal Server Error"
        );
    }

    #[test]
    fn rate_limited_opening_retries_then_finishes_the_request() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut client =
            test_client_with_rate_limit_budget(&format!("http://{}", server.server_addr()), 30);
        let token = CancellationToken::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let first = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                first
                    .respond(
                        tiny_http::Response::from_string("private-body fixture")
                            .with_status_code(429)
                            .with_header(
                                tiny_http::Header::from_bytes("Retry-After", "0").unwrap(),
                            ),
                    )
                    .unwrap();
                let second = server
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(second);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"recovered"}), None)
                        .to_string(),
                );
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({}), Some("stop")).to_string(),
                );
                sse_frame(&mut writer, "[DONE]");
                sse_end(writer);
            });
            let mut sent = 0;
            let mut rate_limited = Vec::new();
            let turn = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || {
                        sent += 1;
                        Ok(())
                    },
                    &mut |event| {
                        if let AgentEvent::RateLimited { attempt, wait_ms } = event {
                            rate_limited.push((attempt, wait_ms));
                        }
                    },
                )
                .unwrap();
            assert_eq!(sent, 1);
            assert_eq!(rate_limited, vec![(1, 0)]);
            assert_eq!(
                turn.message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "recovered".into()
                ))
            );
        });
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rate_limited_opening_gives_up_when_budget_is_spent() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        // A 1s budget without a Retry-After header: the first backoff of 1s
        // spends it entirely, so the second 429 fails the request.
        let mut client =
            test_client_with_rate_limit_budget(&format!("http://{}", server.server_addr()), 1);
        let token = CancellationToken::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for _ in 0..2 {
                    let request = server
                        .recv_timeout(Duration::from_secs(3))
                        .unwrap()
                        .unwrap();
                    request
                        .respond(
                            tiny_http::Response::from_string("private-body fixture")
                                .with_status_code(429),
                        )
                        .unwrap();
                }
            });
            let mut sent = 0;
            let mut rate_limited = Vec::new();
            let mut finished = None;
            let error = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || {
                        sent += 1;
                        Ok(())
                    },
                    &mut |event| match event {
                        AgentEvent::RateLimited { attempt, wait_ms } => {
                            rate_limited.push((attempt, wait_ms));
                        }
                        AgentEvent::ModelRequestFinished { success, .. } => {
                            finished = Some(success);
                        }
                        _ => {}
                    },
                )
                .err()
                .unwrap()
                .to_string();
            assert_eq!(sent, 1);
            assert_eq!(rate_limited, vec![(1, 1000)]);
            assert_eq!(finished, Some(false));
            assert!(error.contains("HTTP status 429"));
            assert!(error.contains("gave up after 1 retries and 1s of backoff"));
            assert!(!error.contains("private-body"));
        });
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cancelled_before_request_reports_cancelled_without_sending() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut client = test_client(&format!("http://{}", server.server_addr()));
        let token = CancellationToken::new();
        token.cancel();
        let mut sent = 0;
        let error = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || {
                    sent += 1;
                    Ok(())
                },
                &mut |_| {},
            )
            .err()
            .unwrap();
        assert_eq!(sent, 1);
        assert!(error.downcast_ref::<Cancelled>().is_some());
        assert!(
            server
                .recv_timeout(Duration::from_millis(50))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sent_callback_error_takes_precedence_over_cancellation() {
        let mut client = test_client("http://127.0.0.1:1");
        let token = CancellationToken::new();
        token.cancel();
        let mut sent = 0;
        let error = client
            .complete(
                &CreateChatCompletionRequest::default(),
                &token,
                &mut || {
                    sent += 1;
                    bail!("persisting session failed")
                },
                &mut |_| {},
            )
            .err()
            .unwrap();
        assert_eq!(sent, 1);
        assert_eq!(error.to_string(), "persisting session failed");
        assert!(error.downcast_ref::<Cancelled>().is_none());
    }

    #[test]
    fn cancelled_stream_is_dropped_and_next_request_rebuilds_runtime() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut client = test_client(&format!("http://{}", server.server_addr()));
        let (release_first, first_released) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                // First request: a single partial delta, then the response is held open.
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"partial"}), None).to_string(),
                );
                first_released.recv_timeout(Duration::from_secs(5)).unwrap();
                sse_end(writer);
                // Second request: a fresh client must reconnect and finish normally.
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"recovered"}), None)
                        .to_string(),
                );
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({}), Some("stop")).to_string(),
                );
                sse_frame(&mut writer, "[DONE]");
                sse_end(writer);
            });
            let token = CancellationToken::new();
            let mut sent = 0;
            let mut events = 0;
            let mut finished = None;
            let error = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || {
                        sent += 1;
                        Ok(())
                    },
                    &mut |event| match event {
                        // Cancel from the first real stream delta, not from the
                        // request lifecycle events that bracket the stream.
                        AgentEvent::AssistantDelta { .. } => {
                            events += 1;
                            token.cancel();
                        }
                        AgentEvent::ModelRequestFinished { usage, success, .. } => {
                            finished = Some((usage, success));
                        }
                        _ => {}
                    },
                )
                .err()
                .unwrap();
            assert!(events > 0);
            assert_eq!(sent, 1);
            assert_eq!(finished, Some((None, false)));
            assert!(error.downcast_ref::<Cancelled>().is_some());
            // Retire the cancelled stream's runtime; the next request rebuilds it.
            client.cancel_pending_io();
            assert!(client.runtime.is_none());
            release_first.send(()).unwrap();
            let token = CancellationToken::new();
            let mut sent = 0;
            let mut finished = None;
            let turn = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || {
                        sent += 1;
                        Ok(())
                    },
                    &mut |event| {
                        if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                            finished = Some((usage, success));
                        }
                    },
                )
                .unwrap();
            assert_eq!(finished, Some((None, true)));
            assert_eq!(sent, 1);
            assert!(client.runtime.is_some());
            assert_eq!(
                turn.message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "recovered".into()
                ))
            );
            assert!(turn.message.tool_calls.is_none());
            assert_eq!(turn.usage, None);
            assert!(!turn.refusal);
        });
    }

    #[test]
    fn refuses_truncated_or_invalid_batches() {
        let call = json!({"id":"x","type":"function","function":{"name":"write","arguments":"{}"}});
        for finish in ["length", "content_filter", "unknown"] {
            assert!(decode(json!({"role":"assistant","tool_calls":[call]}), finish).is_err());
        }
        assert!(
            decode(
                json!({"role":"assistant","tool_calls":[call,call]}),
                "tool_calls"
            )
            .is_err()
        );
        assert!(decode(json!({"role":"user","content":"wrong"}), "stop").is_err());
        assert!(decode(json!({"role":"assistant","content":null}), "stop").is_err());
        assert!(
            decode(
                json!({"role":"assistant","tool_calls":[call, {
                    "id":"custom", "type":"custom", "custom_tool":{"name":"write","input":"{}"}
                }]}),
                "tool_calls"
            )
            .is_err()
        );
    }

    #[test]
    fn accepts_refusal_and_nullable_calls() {
        for message in [
            json!({"role":"assistant","refusal":"Cannot help"}),
            json!({"role":"assistant","content":null,"tool_calls":null,"refusal":"Cannot help"}),
        ] {
            let turn = decode(message, "stop").unwrap();
            assert_eq!(
                turn.message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "Cannot help".into()
                ))
            );
            assert!(turn.message.tool_calls.is_none());
            assert!(turn.refusal);
            assert_eq!(turn.usage, None);
        }
    }

    #[test]
    fn context_chat_refusal_flag_requires_no_normal_content() {
        let refusal_only = decode(json!({"role":"assistant","refusal":"no"}), "stop").unwrap();
        assert!(refusal_only.refusal);
        let mixed = decode(
            json!({"role":"assistant","content":"answer","refusal":"no"}),
            "stop",
        )
        .unwrap();
        assert!(!mixed.refusal);
        let blank_content = decode(
            json!({"role":"assistant","content":"  ","refusal":"no"}),
            "stop",
        )
        .unwrap();
        assert!(blank_content.refusal);
        let text_only = decode(json!({"role":"assistant","content":"answer"}), "stop").unwrap();
        assert!(!text_only.refusal);
    }

    #[test]
    fn context_chat_cached_breakdown_preserves_unknown_and_zero() {
        for (details, expected) in [
            (None, None),
            (Some(json!({"cached_tokens":0})), Some(0)),
            (Some(json!({"cached_tokens":40})), Some(40)),
        ] {
            let mut response = json!({
                "id":"fixture", "object":"chat.completion", "created":1, "model":"fixture",
                "choices":[{"index":0,"message":{"role":"assistant","content":"answer"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}
            });
            if let Some(details) = details {
                response["usage"]["prompt_tokens_details"] = details;
            }
            let turn = into_completion_turn(serde_json::from_value(response).unwrap()).unwrap();
            assert_eq!(
                turn.usage,
                Some(TokenUsage {
                    input_tokens: 12000,
                    output_tokens: 300,
                    cache_write_tokens: None,
                    cached_tokens: expected,
                })
            );
        }
    }

    #[test]
    fn context_chat_usage_only_chunk_after_finish_returns_usage() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut client = test_client(&format!("http://{}", server.server_addr()));
        std::thread::scope(|scope| {
            scope.spawn(move || {
                // First request: normal chunks, then a choices-empty usage-only chunk.
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"answer"}), None).to_string(),
                );
                sse_frame(&mut writer, &chat_chunk(json!({}), Some("stop")).to_string());
                sse_frame(
                    &mut writer,
                    r#"{"id":"chatcmpl-fixture","object":"chat.completion.chunk","created":2,"model":"fixture-model","choices":[],"usage":{"prompt_tokens":12000,"completion_tokens":300,"total_tokens":12300}}"#,
                );
                sse_frame(&mut writer, "[DONE]");
                sse_end(writer);
                // Second request: no usage at all; the answer still succeeds.
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &chat_chunk(json!({"role":"assistant","content":"again"}), Some("stop"))
                        .to_string(),
                );
                sse_frame(&mut writer, "[DONE]");
                sse_end(writer);
            });
            let token = CancellationToken::new();
            let mut finished = None;
            let turn = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || Ok(()),
                    &mut |event| {
                        if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                            finished = Some((usage, success));
                        }
                    },
                )
                .unwrap();
            assert_eq!(
                turn.usage,
                Some(TokenUsage {
                    input_tokens: 12000,
                    output_tokens: 300,
                    cache_write_tokens: None,
                    cached_tokens: None,
                })
            );
            assert_eq!(finished, Some((turn.usage, true)));
            assert_eq!(
                turn.message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "answer".into()
                ))
            );
            assert!(!turn.refusal);
            let token = CancellationToken::new();
            let mut finished = None;
            let turn = client
                .complete(
                    &CreateChatCompletionRequest::default(),
                    &token,
                    &mut || Ok(()),
                    &mut |event| {
                        if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                            finished = Some((usage, success));
                        }
                    },
                )
                .unwrap();
            assert_eq!(turn.usage, None);
            assert_eq!(finished, Some((None, true)));
            assert_eq!(
                turn.message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "again".into()
                ))
            );
        });
    }

    #[test]
    fn context_responses_usage_extracted_and_missing_usage_is_none() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut client = test_client(&format!("http://{}", server.server_addr()));
        let completed = |usage: Option<Value>| {
            let mut response = json!({
                "id":"fixture", "object":"response", "created_at":0, "model":"fixture",
                "status":"completed",
                "output":[{"type":"message","id":"m","role":"assistant","status":"completed",
                    "content":[{"type":"output_text","text":"answer","annotations":[]}]}]
            });
            if let Some(usage) = usage {
                response["usage"] = usage;
            }
            json!({"type":"response.completed","sequence_number":5,"response":response}).to_string()
        };
        std::thread::scope(|scope| {
            scope.spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(
                    &mut writer,
                    &completed(Some(json!({
                        "input_tokens":12000, "output_tokens":300, "total_tokens":12300,
                        "input_tokens_details":{"cached_tokens":40},
                        "output_tokens_details":{"reasoning_tokens":50}
                    }))),
                );
                sse_end(writer);
                let request = server
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let mut writer = sse_writer(request);
                sse_frame(&mut writer, &completed(None));
                sse_end(writer);
            });
            let token = CancellationToken::new();
            let mut finished = None;
            let turn = client
                .respond(
                    &CreateResponse::default(),
                    &token,
                    &mut || Ok(()),
                    &mut |event| {
                        if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                            finished = Some((usage, success));
                        }
                    },
                )
                .unwrap();
            assert_eq!(turn.text, "answer");
            assert!(!turn.has_calls);
            assert!(!turn.refusal);
            assert_eq!(
                turn.usage,
                Some(TokenUsage {
                    input_tokens: 12000,
                    output_tokens: 300,
                    cache_write_tokens: None,
                    cached_tokens: Some(40),
                })
            );
            assert_eq!(finished, Some((turn.usage, true)));
            let token = CancellationToken::new();
            let mut finished = None;
            let turn = client
                .respond(
                    &CreateResponse::default(),
                    &token,
                    &mut || Ok(()),
                    &mut |event| {
                        if let AgentEvent::ModelRequestFinished { usage, success, .. } = event {
                            finished = Some((usage, success));
                        }
                    },
                )
                .unwrap();
            assert_eq!(turn.usage, None);
            assert_eq!(finished, Some((None, true)));
        });
    }
}
