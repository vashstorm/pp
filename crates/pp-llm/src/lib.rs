use anyhow::Result;
use async_openai::{
    Client,
    config::{Config as _, OpenAIConfig},
    error::{ApiError, ApiErrorResponse, OpenAIError},
    middleware::ReqwestService,
    types::{chat::ChatCompletionRequestAssistantMessage, responses::OutputItem},
};
use pp_config::config::Config;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::context::TokenUsage;
use reqwest::{header::HeaderValue, redirect::Policy};
use std::time::{Duration, Instant};
use tokio::runtime::Runtime;

use pp_config::auth::http as auth;
mod chat;
pub mod http;
pub(crate) mod parse;
mod responses;
pub mod retry;
mod streaming;
pub mod web_search;

use retry::retry_after_code;

pub const EMPTY_SUMMARY: &str = "context compaction failed: expected a non-empty summary";

/// Script table of the embedded mock server keyed by model reference; sent with requests in dev builds only.
#[cfg(debug_assertions)]
pub type MockScripts =
    std::sync::Arc<std::collections::BTreeMap<String, std::sync::Arc<serde_json::Value>>>;

enum UsageStore {
    Pending(std::path::PathBuf),
    Ready(pp_config::agent_store::AgentStore),
    Unavailable,
}

pub struct OpenAiClient {
    auth: Option<pp_config::auth::Config>,
    client: Client<OpenAIConfig>,
    runtime: Option<Runtime>,
    read_http: Option<reqwest::Client>,
    search_http: Option<reqwest::Client>,
    request_timeout: Duration,
    rate_limit_retry_budget: Duration,
    stats_context: Option<pp_stats::RequestContext>,
    stats_key: Option<String>,
    task_accounting: Option<pp_config::task_status::Ledger>,
    model_reference: String,
    // Script table of the mock client indexed by model reference; only present in dev builds.
    #[cfg(debug_assertions)]
    mock_scripts: Option<MockScripts>,
    // Whether the next real model request carries the initial script; per client.
    #[cfg(debug_assertions)]
    mock_script_pending: bool,
    // Lazily opened and reused agent.db connection; opened on the first usage count and never reopened for the rest of the client lifetime after a failure.
    usage_store: UsageStore,
    // Only tests hold the temp dir lifetime, keeping the usage-count database path valid for the client's lifetime.
    #[cfg(test)]
    usage_dir: Option<tempfile::TempDir>,
}

pub struct ResponseTurn {
    pub output: Vec<OutputItem>,
    pub text: String,
    pub has_calls: bool,
    pub usage: Option<TokenUsage>,
    pub refusal: bool,
}

pub struct CompletionTurn {
    pub message: ChatCompletionRequestAssistantMessage,
    pub usage: Option<TokenUsage>,
    /// True only when the model refused without any normal non-empty content.
    pub refusal: bool,
}

impl OpenAiClient {
    pub fn same_source(&self, config: &Config) -> bool {
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
    pub fn set_stats_context(&mut self, context: pp_stats::RequestContext) {
        // Mock clients write no global stats and keep no snapshot that a late real-context call could overwrite.
        #[cfg(debug_assertions)]
        if self.mock_scripts.is_some() {
            return;
        }
        self.stats_context = Some(context);
        self.stats_key = None;
    }
    pub fn model_reference(&self) -> &str {
        &self.model_reference
    }

    /// Install mock scripts and rebuild the model HTTP connection bypassing system proxies; dev builds only.
    #[cfg(debug_assertions)]
    pub fn set_mock_scripts(&mut self, scripts: MockScripts) -> Result<()> {
        // Finish the fallible connection rebuild first, then switch state; on failure the client is left unchanged.
        let client = build_client(self.client.config().clone(), self.request_timeout, true)?;
        self.client = client;
        self.mock_scripts = Some(scripts);
        self.mock_script_pending = true;
        // Construction may already have installed a real stats context; mock runs write no stats database.
        self.stats_context = None;
        self.stats_key = None;
        Ok(())
    }

    /// Borrow the script table for parent/child agent inheritance; does not consume the initial script.
    #[cfg(debug_assertions)]
    pub fn mock_scripts(&self) -> Option<&MockScripts> {
        self.mock_scripts.as_ref()
    }

    /// Take the initial script for the current model reference; pending is consumed once and later tool continuation turns carry none.
    #[cfg(debug_assertions)]
    fn take_mock_script(&mut self) -> Result<Option<std::sync::Arc<serde_json::Value>>> {
        if !self.mock_script_pending {
            return Ok(None);
        }
        let reference = &self.model_reference;
        let script = self
            .mock_scripts
            .as_ref()
            .and_then(|scripts| scripts.get(reference))
            .ok_or_else(|| {
                anyhow::anyhow!("mock script missing for model reference {reference}")
            })?;
        self.mock_script_pending = false;
        Ok(Some(script.clone()))
    }

    pub fn set_task_accounting(&mut self, ledger: Option<pp_config::task_status::Ledger>) {
        self.task_accounting = ledger;
    }

    fn settle_task_request(
        &self,
        id: u64,
        price: Option<pp_config::models::ModelCost>,
        usage: Option<TokenUsage>,
    ) {
        if let Some(ledger) = &self.task_accounting {
            ledger.settle(id, price, usage);
        }
    }

    pub fn copy_stats_context(&self, target: &mut Self) {
        // Never propagate the stats context between mock clients or to real ones, so the `set_stats_context` ignore rules cannot be bypassed.
        #[cfg(debug_assertions)]
        if self.mock_scripts.is_some() || target.mock_scripts.is_some() {
            return;
        }
        target.task_accounting = self.task_accounting.clone();
        target.stats_context = self.stats_context.clone().map(|mut context| {
            context.model_reference = target.model_reference.clone();
            context
        });
    }

    pub fn take_stats_key(&mut self) -> Option<String> {
        self.stats_key.take()
    }

    /// Usage counter invoked exactly once at the end of each logical model request; unrelated to stats persistence.
    /// A failure only emits a fixed redacted warning; it never alters the model result already produced and never retries the request.
    fn record_model_usage(&mut self) {
        // Mock requests are not real model usage; write no real stats database and emit no unavailable warning.
        #[cfg(debug_assertions)]
        if self.mock_scripts.is_some() {
            return;
        }
        if let UsageStore::Pending(path) = &self.usage_store {
            self.usage_store = match pp_config::agent_store::AgentStore::open(path) {
                Ok(store) => UsageStore::Ready(store),
                Err(_) => UsageStore::Unavailable,
            };
        }
        let counted = match &mut self.usage_store {
            UsageStore::Ready(store) => store.increment_model_usage(&self.model_reference).is_ok(),
            UsageStore::Pending(_) | UsageStore::Unavailable => false,
        };
        if !counted {
            pp_config::agent_store::warn_usage_unavailable();
        }
    }

    pub fn new(config: &Config) -> Result<Self> {
        pp_core::logging::register_secret(&config.api_key);
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
        let client = build_client(sdk_config, request_timeout, false)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| anyhow::anyhow!("initializing model runtime failed"))?;
        Ok(Self {
            auth: config.auth.clone(),
            client,
            runtime: Some(runtime),
            read_http: None,
            search_http: None,
            request_timeout,
            rate_limit_retry_budget,
            stats_context: None,
            stats_key: None,
            task_accounting: None,
            model_reference: config.model_reference.clone(),
            #[cfg(debug_assertions)]
            mock_scripts: None,
            #[cfg(debug_assertions)]
            mock_script_pending: false,
            // Opened lazily on the first logical request count; the redacted warning is emitted only after a failure.
            usage_store: UsageStore::Pending(config.agent_db_path.clone()),
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
        // Mock client rebuilds bypass system proxies too; plain clients keep the existing proxy behavior.
        #[cfg(debug_assertions)]
        let bypass_proxy = self.mock_scripts.is_some();
        #[cfg(not(debug_assertions))]
        let bypass_proxy = false;
        let client = build_client(
            self.client.config().clone(),
            self.request_timeout,
            bypass_proxy,
        )?;
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
        // auth.path is agent.db; startup and auth::service already ran the one-time migration,
        // so holding the lock only reads/writes auth_credentials and never triggers migration or reads the old auth.yml.
        let _lock = pp_config::auth::StoreLock::acquire(&auth.path, cancellation)?;
        let mut store = pp_config::auth::Store::load(&auth.path)?;
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
            .is_some_and(|expiry| expiry <= pp_config::auth::now().saturating_add(60))
        {
            let renewed = self
                .runtime
                .as_ref()
                .expect("runtime is initialized")
                .block_on(async {
                    cancellation
                        .run_until_cancelled(auth::refresh(auth.provider, credential))
                        .await
                        .unwrap_or_else(|| Err(Cancelled.into()))
                })?;
            pp_core::cancellation::check(cancellation)?;
            store.credentials.insert(auth.provider, renewed);
            store.save(&auth.path)?;
        }
        let credential = &store.credentials[&auth.provider];
        pp_core::logging::register_secret(&credential.access_token);
        if let Some(token) = &credential.refresh_token {
            pp_core::logging::register_secret(token);
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
        if auth.provider == pp_config::auth::Provider::GithubCopilot {
            headers.insert(
                "x-github-api-version",
                HeaderValue::from_static("2026-08-01"),
            );
            headers.insert(
                "x-initiator",
                HeaderValue::from_str(initiator).expect("fixed request origin"),
            );
            headers.insert(
                "x-interaction-type",
                HeaderValue::from_str(&format!("conversation-{initiator}"))
                    .expect("fixed interaction type"),
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
            self.client = build_client(sdk, self.request_timeout, false)?;
        }
        Ok(())
    }

    /// Retires the runtime once the synchronous caller's `block_on` has
    /// already returned for a cancelled task. `shutdown_background`
    /// synchronously drops the SDK's owned stream tasks and closes their
    /// connections without waiting for unstoppable blocking DNS work; the
    /// next request rebuilds everything lazily via `ensure_ready`.
    pub fn cancel_pending_io(&mut self) {
        self.read_http = None;
        self.search_http = None;
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// Web read uses its own connection pool and never reuses model auth headers or the rate-limit retry policy.
    pub fn fetch_read_url(
        &mut self,
        url: &str,
        cancellation: &CancellationToken,
    ) -> Result<crate::http::HttpResponse> {
        use crate::http::HttpResponse;
        use crate::http::MAX_INPUT_BYTES;
        self.ensure_ready()?;
        let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid read URL"))?;
        anyhow::ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.username().is_empty()
                && url.password().is_none(),
            "read URL requires HTTP/HTTPS without embedded credentials"
        );
        if self.read_http.is_none() {
            let policy = Policy::custom(|attempt| {
                if attempt.previous().len() > 5 {
                    return attempt.error("too many redirects");
                }
                let next = attempt.url();
                if !matches!(next.scheme(), "http" | "https")
                    || !next.username().is_empty()
                    || next.password().is_some()
                {
                    return attempt
                        .error("redirect URL requires HTTP/HTTPS without embedded credentials");
                }
                attempt.follow()
            });
            self.read_http = Some(
                reqwest::Client::builder()
                    .redirect(policy)
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

    /// Web search uses its own connection pool without model auth headers; concurrent searches and soft/hard timeouts run inside the current-thread runtime.
    pub fn search_web(
        &mut self,
        request: &web_search::WebSearchRequest,
        settings: &pp_config::config::WebSearchSettings,
        allow_external_network: bool,
        cancellation: &CancellationToken,
    ) -> Result<web_search::WebSearchResponse> {
        self.ensure_ready()?;
        if self.search_http.is_none() {
            self.search_http = Some(build_search_http_client()?);
        }
        let client = self
            .search_http
            .as_ref()
            .expect("web search HTTP client initialized");
        let runtime = self.runtime.as_ref().expect("runtime initialized");
        run_search_web(
            client,
            runtime,
            request,
            settings,
            allow_external_network,
            cancellation,
        )
    }
}

fn build_search_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .redirect(Policy::limited(5))
        .build()
        .map_err(|_| anyhow::anyhow!("initializing web search HTTP client failed"))
}

fn run_search_web(
    client: &reqwest::Client,
    runtime: &Runtime,
    request: &web_search::WebSearchRequest,
    settings: &pp_config::config::WebSearchSettings,
    allow_external_network: bool,
    cancellation: &CancellationToken,
) -> Result<web_search::WebSearchResponse> {
    if let Some(searxng) = &settings.searxng {
        if let Some(token) = &searxng.token {
            pp_core::logging::register_secret(token);
        }
        if let Some(password) = &searxng.basic_password {
            pp_core::logging::register_secret(password);
        }
    }
    runtime.block_on(async {
        cancellation
            .run_until_cancelled(web_search::execute_search(
                client,
                request,
                settings,
                allow_external_network,
                cancellation,
            ))
            .await
            .unwrap_or_else(|| Err(Cancelled.into()))
    })
}

/// Dev builds only: run a single synchronous web search without model configuration, reused by the `/test-web-search` fallback path.
#[cfg(debug_assertions)]
pub fn search_web_standalone(
    request: &web_search::WebSearchRequest,
    settings: &pp_config::config::WebSearchSettings,
    allow_external_network: bool,
    cancellation: &CancellationToken,
) -> Result<web_search::WebSearchResponse> {
    let client = build_search_http_client()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| anyhow::anyhow!("initializing web search runtime failed"))?;
    run_search_web(
        &client,
        &runtime,
        request,
        settings,
        allow_external_network,
        cancellation,
    )
}

/// Saturates instead of wrapping: an `elapsed_ms` beyond `u64` is far past any
/// meaningful request duration.
fn elapsed_millis(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn build_client(
    sdk_config: OpenAIConfig,
    timeout: Duration,
    bypass_proxy: bool,
) -> Result<Client<OpenAIConfig>> {
    let mut builder = reqwest::Client::builder()
        // Limit only the connect and consecutive idle waits; steady generation has no total duration cap.
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .redirect(Policy::none())
        .retry(reqwest::retry::never());
    if bypass_proxy {
        // The embedded mock server listens on loopback only and must not be intercepted by user proxy env vars.
        builder = builder.no_proxy();
    }
    let http = builder
        .build()
        .map_err(|_| anyhow::anyhow!("initializing HTTP client failed"))?;
    // HTTP failure context goes to diagnostic logs only; terminal output and retry classification still use the status code alone.
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
                // An error page may stream forever or never end; diagnostic reads cap at 64 KiB and one second.
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
                    request_id = pp_core::logging::diagnostic_text(&request_id),
                    body = pp_core::logging::diagnostic_text(&String::from_utf8_lossy(&body)),
                    body_complete = complete,
                    body_read_failed = !matches!(read, Ok(Ok(()))),
                );
                let code = if retry::body_indicates_context_overflow(status_code, &body) {
                    Some(retry::CONTEXT_OVERFLOW_CODE.to_owned())
                } else {
                    retry_after
                };
                Err(OpenAIError::ApiError(ApiErrorResponse {
                    status_code,
                    api_error: ApiError {
                        message: String::new(),
                        r#type: None,
                        param: None,
                        code,
                        misalignment: None,
                    },
                }))
            }
        })
        .service(ReqwestService::new(http.clone()));
    Ok(Client::build(http, sdk_config).with_http_service(service))
}

#[cfg(test)]
mod tests;

fn elapsed_duration(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
