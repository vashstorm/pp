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
        let mut attempted = false;
        let result: Result<CompletionTurn> = runtime.block_on(async {
            // The BYOT future borrows its resource; bind it so retries can
            // create further futures from the same borrow.
            let chat = client.chat();
            // BYOT serializes a borrowed request before returning the stream.
            let opened = cancellation
                .run_until_cancelled(async {
                    attempted = true;
                    chat.create_stream_byot::<_, ChatChunk>(request).await
                })
                .await;
            // Persist the accepted request exactly once, before judging the
            // opening outcome; a callback failure takes precedence.
            on_request_sent()?;
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = open_with_request_retry(
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
                if attempted {
                    observation.request_context = context.capture(request);
                }
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
        let mut attempted = false;
        let result: Result<ResponseTurn> = runtime.block_on(async {
            let responses = client.responses();
            let opened = cancellation
                .run_until_cancelled(async {
                    attempted = true;
                    responses.create_stream_byot::<_, ResponseEvent>(request).await
                })
                .await;
            on_request_sent()?;
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = open_with_request_retry(
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
                if attempted {
                    observation.request_context = context.capture(request);
                }
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
        // 只限制建连和连续无数据的等待，持续生成不设总耗时上限。
        .connect_timeout(timeout)
        .read_timeout(timeout)
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

/// middleware 不读取错误响应体，通过自构造 code 标记传递 Retry-After 整数秒。
/// 脱敏错误只展示状态码，不输出该内部标记。
const RETRY_AFTER_CODE_PREFIX: &str = "pp-retry-after:";

/// Safety valve so a provider answering `Retry-After: 0` cannot turn the
/// retry budget into a tight request loop.
const REQUEST_MAX_ATTEMPTS: u32 = 10;

fn retry_after_code(response: &reqwest::Response) -> Option<String> {
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

/// 只分类建连阶段的临时错误；流开始后的错误不进入重试循环。
fn request_retry_reason(error: &OpenAIError) -> Option<crate::events::RequestRetryReason> {
    use crate::events::RequestRetryReason;
    match error {
        OpenAIError::ApiError(response) => match response.status_code.as_u16() {
            429 => Some(RequestRetryReason::RateLimited),
            status @ (408 | 409 | 500..=599) => Some(RequestRetryReason::HttpStatus(status)),
            _ => None,
        },
        OpenAIError::Reqwest(error) if error.is_timeout() => Some(RequestRetryReason::Timeout),
        OpenAIError::Reqwest(error) if error.is_connect() || error.is_request() => {
            Some(RequestRetryReason::Connection)
        }
        _ => None,
    }
}

/// 优先使用 Retry-After 整数秒，否则从 1 秒开始指数退避；等待截断到剩余预算。
fn request_retry_wait(
    error: &OpenAIError,
    waited: Duration,
    budget: Duration,
    attempt: u32,
) -> Option<Duration> {
    request_retry_reason(error)?;
    if budget.is_zero() || waited >= budget || attempt >= REQUEST_MAX_ATTEMPTS - 1 {
        return None;
    }
    // 上方尝试次数限制保证移位不会溢出。
    let backoff = match retry_after_seconds(error) {
        Some(seconds) => Duration::from_secs(seconds),
        None => Duration::from_secs(1 << attempt),
    };
    Some(backoff.min(budget - waited))
}

/// 重试结束后仍只展示脱敏错误，并补充实际重试次数和累计等待时长。
fn opening_error(error: OpenAIError, api: &str, waited: Duration, retries: u32) -> anyhow::Error {
    if retries == 0 {
        return redacted_error(error, api);
    }
    anyhow::anyhow!(
        "{} (gave up after {} retries and {}s of backoff)",
        redacted_error(error, api),
        retries,
        waited.as_secs()
    )
}

/// 仅重开尚未返回 SSE 流的请求；持久化回调已执行一次，等待和重开均可取消。
async fn open_with_request_retry<S, F, Fut>(
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
        let Some(delay) = request_retry_wait(&error, waited, budget, retries) else {
            return Err(opening_error(error, api, waited, retries));
        };
        retries += 1;
        waited += delay;
        progress(AgentEvent::RequestRetrying {
            reason: request_retry_reason(&error).expect("重试条件已确认临时错误"),
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
mod tests;
