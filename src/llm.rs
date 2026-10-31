use crate::cancellation::{CancellationToken, Cancelled};
use crate::context::TokenUsage;
use crate::{config::Config, events::AgentEvent};
use anyhow::{Result, bail, ensure};
use async_openai::{
    Client,
    config::{Config as _, OpenAIConfig},
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

pub(crate) mod auth;
mod streaming;

use streaming::{ChatChunk, ChatStream, ResponseEvent, ResponsesStream};

/// 模型边界错误与本地事务错误分开，防止重试磁盘写入或工具副作用。
#[derive(Debug)]
struct ModelFailure {
    message: String,
    reason: crate::events::RequestRetryReason,
    retry_after: Option<u64>,
}
impl std::fmt::Display for ModelFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ModelFailure {}

pub(crate) const EMPTY_SUMMARY: &str = "context compaction failed: expected a non-empty summary";

pub(crate) fn model_request_failed(error: &anyhow::Error) -> bool {
    error.is::<ModelFailure>()
}

fn provider_failure(error: OpenAIError, api: &str) -> anyhow::Error {
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

struct RequestAttempts {
    attempt: u32,
    waited: Duration,
    budget: Duration,
    delay: Option<Duration>,
    started: Instant,
    request_context: Option<crate::stats::RequestSnapshot>,
    pending: Option<crate::stats::Observation>,
    pending_open: bool,
    partial: bool,
    summary: bool,
}
impl RequestAttempts {
    fn new(budget: Duration) -> Self {
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
        }
    }
    fn finish<T>(&mut self, result: &Result<T>) {
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

pub(crate) struct OpenAiClient {
    auth: Option<crate::auth::Config>,
    client: Client<OpenAIConfig>,
    runtime: Option<Runtime>,
    read_http: Option<reqwest::Client>,
    request_timeout: Duration,
    rate_limit_retry_budget: Duration,
    stats_context: Option<crate::stats::RequestContext>,
    stats_key: Option<String>,
    model_reference: String,
    // 复用的 agent.db 连接；构造时打开失败则整个客户端周期不计数并按需警告，不做重开回退。
    usage_store: Option<crate::agent_store::AgentStore>,
    // 仅测试持有临时目录生命周期，保证计数数据库路径在客户端存续期内有效。
    #[cfg(test)]
    usage_dir: Option<tempfile::TempDir>,
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
    pub(crate) fn same_source(&self, config: &Config) -> bool {
        if self.auth != config.auth {
            return false;
        }
        self.auth.is_some()
            || (self.client.config().api_base() == config.base_url.as_str().trim_end_matches('/')
                && self
                    .client
                    .config()
                    .headers()
                    .get(reqwest::header::AUTHORIZATION)
                    .is_some_and(|header| {
                        header.as_bytes() == format!("Bearer {}", config.api_key).as_bytes()
                    }))
    }
    pub(crate) fn set_stats_context(&mut self, context: crate::stats::RequestContext) {
        self.stats_context = Some(context);
        self.stats_key = None;
    }
    pub(crate) fn model_reference(&self) -> &str {
        &self.model_reference
    }

    pub(crate) fn copy_stats_context(&self, target: &mut Self) {
        target.stats_context = self.stats_context.clone().map(|mut context| {
            context.model_reference = target.model_reference.clone();
            context
        });
    }

    fn finish_backoff(
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

    pub(crate) fn take_stats_key(&mut self) -> Option<String> {
        self.stats_key.take()
    }

    /// 每次逻辑模型请求收尾恰好调用一次的使用计数；与 stats 保存无关。
    /// 失败只发固定脱敏警告，不更改已发生的模型结果，也不重试请求。
    fn record_model_usage(&mut self) {
        let counted = match &mut self.usage_store {
            Some(store) => store.increment_model_usage(&self.model_reference).is_ok(),
            None => false,
        };
        if !counted {
            crate::agent_store::warn_usage_unavailable();
        }
    }

    pub(crate) fn new(config: &Config) -> Result<Self> {
        crate::logging::register_secret(&config.api_key);
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
            auth: config.auth.clone(),
            client,
            runtime: Some(runtime),
            read_http: None,
            request_timeout,
            rate_limit_retry_budget,
            stats_context: None,
            stats_key: None,
            model_reference: config.model_reference.clone(),
            // 构造期打开失败保持静默；实际计数失败时才发脱敏警告。
            usage_store: crate::agent_store::AgentStore::open(&config.agent_db_path).ok(),
            #[cfg(test)]
            usage_dir: None,
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

    fn prepare_auth(
        &mut self,
        cancellation: &CancellationToken,
        initiator: &str,
        has_images: bool,
    ) -> Result<()> {
        let Some(auth) = &self.auth else {
            return Ok(());
        };
        // auth.path 是 agent.db；启动与 auth::service 已完成一次性迁移，
        // 持锁期间只读/写 auth_credentials，绝不触发迁移或读取旧 auth.yml。
        let _lock = crate::auth::StoreLock::acquire(&auth.path, cancellation)?;
        let mut store = crate::auth::Store::load(&auth.path)?;
        let credential = store.credentials.get(&auth.provider).ok_or_else(|| {
            anyhow::anyhow!(
                "{} is not signed in; use /login or select another model in /model",
                auth.provider.id()
            )
        })?;
        anyhow::ensure!(
            credential.generation == auth.generation,
            "Authorization changed; use /model to reselect the model before continuing."
        );
        if credential
            .expires_at
            .is_some_and(|expiry| expiry <= crate::auth::now().saturating_add(60))
        {
            let renewed = self
                .runtime
                .as_ref()
                .expect("运行时已初始化")
                .block_on(async {
                    cancellation
                        .run_until_cancelled(auth::refresh(auth.provider, credential))
                        .await
                        .unwrap_or_else(|| Err(Cancelled.into()))
                })?;
            crate::cancellation::check(cancellation)?;
            store.credentials.insert(auth.provider, renewed);
            store.save(&auth.path)?;
        }
        let credential = &store.credentials[&auth.provider];
        crate::logging::register_secret(&credential.access_token);
        if let Some(token) = &credential.refresh_token {
            crate::logging::register_secret(token);
        }
        credential.validate(auth.provider)?;
        let mut sdk = OpenAIConfig::new()
            .with_api_key(credential.access_token.clone())
            .with_api_base(
                credential
                    .api_endpoint
                    .as_deref()
                    .unwrap_or(auth.provider.base_url()),
            )
            .with_org_id("")
            .with_project_id("");
        let mut headers = auth::headers(
            auth.provider,
            credential.account_id.as_deref(),
            credential.device_id.as_deref(),
        )?;
        if auth.provider == crate::auth::Provider::GithubCopilot {
            headers.insert(
                "x-github-api-version",
                HeaderValue::from_static("2026-08-01"),
            );
            headers.insert(
                "x-initiator",
                HeaderValue::from_str(initiator).expect("固定请求来源"),
            );
            headers.insert(
                "x-interaction-type",
                HeaderValue::from_str(&format!("conversation-{initiator}")).expect("固定交互类型"),
            );
            if has_images {
                headers.insert("copilot-vision-request", HeaderValue::from_static("true"));
            }
        }
        for (name, value) in &headers {
            sdk = sdk
                .with_header(name.clone(), value.as_bytes())
                .map_err(|_| anyhow::anyhow!("invalid auth request header"))?;
        }
        if self.client.config().api_base() != sdk.api_base()
            || self.client.config().headers() != sdk.headers()
        {
            self.client = build_client(sdk, self.request_timeout)?;
        }
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
        self.complete_request(request, cancellation, on_request_sent, emit, false)
    }

    pub(crate) fn complete_summary(
        &mut self,
        request: &CreateChatCompletionRequest,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<CompletionTurn> {
        self.complete_request(request, cancellation, on_request_sent, emit, true)
    }

    fn complete_request(
        &mut self,
        request: &CreateChatCompletionRequest,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
        summary: bool,
    ) -> Result<CompletionTurn> {
        let mut attempts = RequestAttempts::new(self.rate_limit_retry_budget);
        attempts.summary = summary;
        let mut sent = false;
        let mut once_sent = || {
            if sent {
                return Ok(());
            }
            on_request_sent()?;
            sent = true;
            Ok(())
        };
        loop {
            if cancellation.is_cancelled() && attempts.attempt > 0 {
                self.finish_backoff(&mut attempts, &request.model, "chat-completions", emit);
                return Err(Cancelled.into());
            }
            attempts.attempt += 1;
            if attempts.attempt > 1 && !attempts.pending_open {
                emit(AgentEvent::AssistantStarted);
            }
            let outcome =
                self.complete_attempt(request, cancellation, &mut once_sent, emit, &mut attempts);
            if let Err(error) = &outcome
                && model_request_failed(error)
                && attempts.partial
            {
                emit(AgentEvent::ModelAttemptFailed {
                    attempt: attempts.attempt,
                });
            }
            let Some(delay) = attempts.delay.take() else {
                return outcome.map_err(|mut error| {
                    if attempts.attempt > 1
                        && let Some(failure) = error.downcast_mut::<ModelFailure>()
                    {
                        failure.message = format!(
                            "{} (gave up after {} retries and {}s of backoff)",
                            failure.message,
                            attempts.attempt - 1,
                            attempts.waited.as_secs()
                        );
                    }
                    error
                });
            };
            let reason = outcome
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<ModelFailure>())
                .expect("重试只接受模型错误")
                .reason;
            tracing::warn!(event = "ai_request_retry", pid = std::process::id(), attempt = attempts.attempt, wait_ms = elapsed_duration(delay), reason = %reason);
            emit(AgentEvent::RequestRetrying {
                reason,
                attempt: attempts.attempt,
                wait_ms: elapsed_duration(delay),
            });
            attempts.waited += delay;
            let waited = self
                .runtime
                .as_ref()
                .expect("请求已初始化 runtime")
                .block_on(async {
                    cancellation
                        .run_until_cancelled(tokio::time::sleep(delay))
                        .await
                        .ok_or(Cancelled)
                });
            if waited.is_err() {
                self.finish_backoff(&mut attempts, &request.model, "chat-completions", emit);
                return Err(Cancelled.into());
            }
        }
    }

    fn complete_attempt(
        &mut self,
        request: &CreateChatCompletionRequest,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
        attempts: &mut RequestAttempts,
    ) -> Result<CompletionTurn> {
        self.ensure_ready()?;
        let wire = auth::wire_request(request, self.auth.as_ref().map(|auth| auth.provider))?;
        let (initiator, has_images) = auth::request_metadata(&wire);
        self.prepare_auth(cancellation, initiator, has_images)?;
        let new_request = attempts.attempt == 1 || !attempts.pending_open;
        if new_request {
            attempts.started = Instant::now();
        }
        let started_at = attempts.started;
        let record = !cancellation.is_cancelled() && self.stats_context.is_some();
        let mut observation = crate::stats::Observation::new();
        let mut partial = false;
        if new_request {
            emit(AgentEvent::ModelRequestStarted);
        }
        let mut progress = |event: AgentEvent| {
            if matches!(&event, AgentEvent::AssistantDelta { text, .. } | AgentEvent::ThinkingDelta { text } if !text.is_empty())
            {
                partial = true;
            }
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
        let mut output = ChatStream::default();
        let mut failure_context = crate::logging::FailureContext::default();
        let mut snapshot = serde_json::Value::Null;
        let mut attempted = false;
        let mut stream_opened = false;
        let mut local_failure = false;
        let result: Result<CompletionTurn> = runtime.block_on(async {
            // The BYOT future borrows its resource; bind it so retries can
            // create further futures from the same borrow.
            let chat = client.chat();
            // BYOT serializes a borrowed request before returning the stream.
            let opened = cancellation
                .run_until_cancelled(async {
                    attempted = true;
                    chat.create_stream_byot::<_, serde_json::Value>(&wire).await
                })
                .await;
            // Persist the accepted request exactly once, before judging the
            // opening outcome; a callback failure takes precedence.
            if let Err(error) = on_request_sent() {
                local_failure = true;
                return Err(error);
            }
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = opened.map_err(|error| provider_failure(error, "Chat Completions"))?;
            stream_opened = true;
            loop {
                match cancellation.run_until_cancelled(stream.next()).await {
                    None => return Err(Cancelled.into()),
                    Some(None) => break,
                    Some(Some(chunk)) => {
                        let chunk =
                            chunk.map_err(|error| provider_failure(error, "Chat Completions"))?;
                        failure_context.push(&chunk.to_string());
                        output.push(ChatChunk::decode(chunk)?, &mut progress)?;
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
        let result = result.and_then(|turn| {
            if attempts.summary {
                ensure!(
                    !turn.refusal
                        && turn
                            .message
                            .tool_calls
                            .as_ref()
                            .is_none_or(|calls| calls.is_empty()),
                    "{EMPTY_SUMMARY}"
                );
            }
            Ok(turn)
        });
        let result = result.map_err(|error| {
            if local_failure
                || error.is::<Cancelled>()
                || model_request_failed(&error)
                || !attempted
            {
                error
            } else {
                let message = error.to_string();
                tracing::error!(event = "model_response_error", pid = std::process::id(), detail = %crate::logging::diagnostic_text(&format!("{error:#}")));
                ModelFailure { message, reason: crate::events::RequestRetryReason::ModelError, retry_after: None }.into()
            }
        });
        attempts.partial = partial;
        attempts.finish(&result);
        let save_attempt = stream_opened || attempts.delay.is_none();
        attempts.pending_open = attempted && !save_attempt;
        let elapsed_ms = elapsed_millis(started_at);
        failure_context.log(
            "Chat Completions",
            &self.model_reference,
            &result,
            elapsed_ms,
        );
        // 纯建连失败合并为一组；流已经打开的失败独立统计。发送前取消不计数。
        // 计数写入先于 stats，退避中取消由 finish_backoff 收尾。
        if attempted && save_attempt {
            self.record_model_usage();
        }
        if record {
            if attempted
                && attempts.request_context.is_none()
                && let Some(context) = &self.stats_context
            {
                attempts.request_context = context.capture(&wire);
            }
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
                    observation.request_context = attempts.request_context.take();
                }
                if save_attempt {
                    attempts.pending = None;
                    self.stats_key = crate::stats::save(
                        context,
                        &request.model,
                        "chat-completions",
                        observation,
                        started_at,
                        &result,
                    );
                } else {
                    attempts.request_context = observation.request_context.take();
                    attempts.pending = Some(observation);
                }
            }
        }
        // Every outcome, including cancellation and callback failures, closes the
        // request exactly once with the authoritative usage when there is one.
        if save_attempt {
            emit(AgentEvent::ModelRequestFinished {
                elapsed_ms,
                usage: result.as_ref().ok().and_then(|turn| turn.usage),
                success: result.is_ok(),
            });
        }
        result
    }

    pub(crate) fn respond(
        &mut self,
        request: &CreateResponse,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ResponseTurn> {
        self.respond_request(request, cancellation, on_request_sent, emit, false)
    }

    pub(crate) fn respond_summary(
        &mut self,
        request: &CreateResponse,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ResponseTurn> {
        self.respond_request(request, cancellation, on_request_sent, emit, true)
    }

    fn respond_request(
        &mut self,
        request: &CreateResponse,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
        summary: bool,
    ) -> Result<ResponseTurn> {
        let mut attempts = RequestAttempts::new(self.rate_limit_retry_budget);
        attempts.summary = summary;
        let mut sent = false;
        let mut once_sent = || {
            if sent {
                return Ok(());
            }
            on_request_sent()?;
            sent = true;
            Ok(())
        };
        loop {
            if cancellation.is_cancelled() && attempts.attempt > 0 {
                self.finish_backoff(
                    &mut attempts,
                    request.model.as_deref().unwrap_or_default(),
                    "responses",
                    emit,
                );
                return Err(Cancelled.into());
            }
            attempts.attempt += 1;
            if attempts.attempt > 1 && !attempts.pending_open {
                emit(AgentEvent::AssistantStarted);
            }
            let outcome =
                self.respond_attempt(request, cancellation, &mut once_sent, emit, &mut attempts);
            if let Err(error) = &outcome
                && model_request_failed(error)
                && attempts.partial
            {
                emit(AgentEvent::ModelAttemptFailed {
                    attempt: attempts.attempt,
                });
            }
            let Some(delay) = attempts.delay.take() else {
                return outcome.map_err(|mut error| {
                    if attempts.attempt > 1
                        && let Some(failure) = error.downcast_mut::<ModelFailure>()
                    {
                        failure.message = format!(
                            "{} (gave up after {} retries and {}s of backoff)",
                            failure.message,
                            attempts.attempt - 1,
                            attempts.waited.as_secs()
                        );
                    }
                    error
                });
            };
            let reason = outcome
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<ModelFailure>())
                .expect("重试只接受模型错误")
                .reason;
            tracing::warn!(event = "ai_request_retry", pid = std::process::id(), attempt = attempts.attempt, wait_ms = elapsed_duration(delay), reason = %reason);
            emit(AgentEvent::RequestRetrying {
                reason,
                attempt: attempts.attempt,
                wait_ms: elapsed_duration(delay),
            });
            attempts.waited += delay;
            let waited = self
                .runtime
                .as_ref()
                .expect("请求已初始化 runtime")
                .block_on(async {
                    cancellation
                        .run_until_cancelled(tokio::time::sleep(delay))
                        .await
                        .ok_or(Cancelled)
                });
            if waited.is_err() {
                self.finish_backoff(
                    &mut attempts,
                    request.model.as_deref().unwrap_or_default(),
                    "responses",
                    emit,
                );
                return Err(Cancelled.into());
            }
        }
    }

    fn respond_attempt(
        &mut self,
        request: &CreateResponse,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
        attempts: &mut RequestAttempts,
    ) -> Result<ResponseTurn> {
        self.ensure_ready()?;
        let wire = auth::wire_request(request, self.auth.as_ref().map(|auth| auth.provider))?;
        let (initiator, has_images) = auth::request_metadata(&wire);
        self.prepare_auth(cancellation, initiator, has_images)?;
        let new_request = attempts.attempt == 1 || !attempts.pending_open;
        if new_request {
            attempts.started = Instant::now();
        }
        let started_at = attempts.started;
        let record = !cancellation.is_cancelled() && self.stats_context.is_some();
        let mut observation = crate::stats::Observation::new();
        let mut partial = false;
        if new_request {
            emit(AgentEvent::ModelRequestStarted);
        }
        let mut progress = |event: AgentEvent| {
            if matches!(&event, AgentEvent::AssistantDelta { text, .. } | AgentEvent::ThinkingDelta { text } if !text.is_empty())
            {
                partial = true;
            }
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
        let mut failure_context = crate::logging::FailureContext::default();
        let mut attempted = false;
        let mut stream_opened = false;
        let mut local_failure = false;
        let result: Result<ResponseTurn> = runtime.block_on(async {
            let responses = client.responses();
            let opened = cancellation
                .run_until_cancelled(async {
                    attempted = true;
                    responses.create_stream_byot::<_, ResponseEvent>(&wire).await
                })
                .await;
            if let Err(error) = on_request_sent() { local_failure = true; return Err(error); }
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = opened.map_err(|error| provider_failure(error, "Responses"))?;
            stream_opened = true;
            let mut output = ResponsesStream::default();
            loop {
                match cancellation.run_until_cancelled(stream.next()).await {
                    None => return Err(Cancelled.into()),
                    Some(None) => {
                        bail!("Responses protocol: stream ended before response.completed")
                    }
                    Some(Some(event)) => {
                        let ResponseEvent(event, written, raw) = event.map_err(|error| provider_failure(error, "Responses"))?;
                        failure_context.push(&raw);
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
        let result = result.and_then(|turn| {
            if attempts.summary {
                ensure!(
                    !turn.refusal && !turn.has_calls && !turn.text.trim().is_empty(),
                    "{EMPTY_SUMMARY}"
                );
            }
            Ok(turn)
        });
        let result = result.map_err(|error| {
            if local_failure
                || error.is::<Cancelled>()
                || model_request_failed(&error)
                || !attempted
            {
                error
            } else {
                let message = error.to_string();
                tracing::error!(event = "model_response_error", pid = std::process::id(), detail = %crate::logging::diagnostic_text(&format!("{error:#}")));
                ModelFailure { message, reason: crate::events::RequestRetryReason::ModelError, retry_after: None }.into()
            }
        });
        attempts.partial = partial;
        attempts.finish(&result);
        let save_attempt = stream_opened || attempts.delay.is_none();
        attempts.pending_open = attempted && !save_attempt;
        let elapsed_ms = elapsed_millis(started_at);
        failure_context.log("Responses", &self.model_reference, &result, elapsed_ms);
        // 与 complete 相同的逻辑请求级计数边界；必须在 stats 保存之前。
        if attempted && save_attempt {
            self.record_model_usage();
        }
        if record {
            if attempted
                && attempts.request_context.is_none()
                && let Some(context) = &self.stats_context
            {
                attempts.request_context = context.capture(&wire);
            }
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
                    observation.request_context = attempts.request_context.take();
                }
                if save_attempt {
                    attempts.pending = None;
                    self.stats_key = crate::stats::save(
                        context,
                        request.model.as_deref().unwrap_or_default(),
                        "responses",
                        observation,
                        started_at,
                        &result,
                    );
                } else {
                    attempts.request_context = observation.request_context.take();
                    attempts.pending = Some(observation);
                }
            }
        }
        // The stream may still be open on the wire (or already cancelled); the
        // finish event reports this request, not HTTP EOF.
        if save_attempt {
            emit(AgentEvent::ModelRequestFinished {
                elapsed_ms,
                usage: result.as_ref().ok().and_then(|turn| turn.usage),
                success: result.is_ok(),
            });
        }
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
    // HTTP 失败现场只写诊断日志；终端与重试分类仍仅使用状态码。
    let service = tower::ServiceBuilder::new()
        .and_then(|mut response: reqwest::Response| async move {
            if response.status().is_success() {
                Ok(response)
            } else {
                let status_code = response.status();
                let retry_after = retry_after_code(&response);
                let request_id = response
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let mut body = Vec::new();
                let mut complete = false;
                // 错误页可能无限输出或不结束；诊断读取最多 64 KiB、至多一秒。
                let read = tokio::time::timeout(Duration::from_secs(1), async {
                    while let Some(chunk) = response.chunk().await? {
                        let remaining = 64 * 1024 - body.len();
                        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                        if body.len() == 64 * 1024 {
                            return Ok::<_, reqwest::Error>(());
                        }
                    }
                    complete = true;
                    Ok(())
                })
                .await;
                tracing::error!(
                    event = "ai_http_error",
                    pid = std::process::id(),
                    status = status_code.as_u16(),
                    request_id = crate::logging::diagnostic_text(&request_id),
                    body = crate::logging::diagnostic_text(&String::from_utf8_lossy(&body)),
                    body_complete = complete,
                    body_read_failed = !matches!(read, Ok(Ok(()))),
                );
                Err(OpenAIError::ApiError(ApiErrorResponse {
                    status_code,
                    api_error: ApiError {
                        message: String::new(),
                        r#type: None,
                        param: None,
                        code: retry_after,
                        misalignment: None,
                    },
                }))
            }
        })
        .service(ReqwestService::new(http.clone()));
    Ok(Client::build(http, sdk_config).with_http_service(service))
}

fn redacted_error(error: OpenAIError, api: &str) -> anyhow::Error {
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
        OpenAIError::JSONDeserialize(..) => {
            anyhow::anyhow!("decoding {api} response: invalid JSON or response structure")
        }
        _ => anyhow::anyhow!("{api} HTTP request failed"),
    }
}

/// middleware 通过自构造 code 标记传递 Retry-After 整数秒；错误正文仅写诊断日志。
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

fn elapsed_duration(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
