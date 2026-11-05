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

use retry::retry_after_code;

pub const EMPTY_SUMMARY: &str = "context compaction failed: expected a non-empty summary";

/// 内嵌 mock server 的按模型引用分组脚本表；仅在开发构建中随请求发送。
#[cfg(debug_assertions)]
pub type MockScripts =
    std::sync::Arc<std::collections::BTreeMap<String, std::sync::Arc<serde_json::Value>>>;

pub struct OpenAiClient {
    auth: Option<pp_config::auth::Config>,
    client: Client<OpenAIConfig>,
    runtime: Option<Runtime>,
    read_http: Option<reqwest::Client>,
    request_timeout: Duration,
    rate_limit_retry_budget: Duration,
    stats_context: Option<pp_stats::RequestContext>,
    stats_key: Option<String>,
    task_accounting: Option<pp_config::task_status::Ledger>,
    model_reference: String,
    // mock 客户端的脚本表按模型引用索引；仅在开发构建中存在。
    #[cfg(debug_assertions)]
    mock_scripts: Option<MockScripts>,
    // 下一条真正发出的模型请求是否携带初始脚本；每个客户端独立。
    #[cfg(debug_assertions)]
    mock_script_pending: bool,
    // 复用的 agent.db 连接；构造时打开失败则整个客户端周期不计数并按需警告，不做重开回退。
    usage_store: Option<pp_config::agent_store::AgentStore>,
    // 仅测试持有临时目录生命周期，保证计数数据库路径在客户端存续期内有效。
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
        // mock 客户端不写全局统计，也不保留被晚到的真实上下文调用覆盖的现场。
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

    /// 安装 mock 脚本并重建绕过系统代理的模型 HTTP 连接；仅开发构建可用。
    #[cfg(debug_assertions)]
    pub fn set_mock_scripts(&mut self, scripts: MockScripts) -> Result<()> {
        // 先完成可能失败的连接重建，再切换状态，失败时保持原客户端不变。
        let client = build_client(self.client.config().clone(), self.request_timeout, true)?;
        self.client = client;
        self.mock_scripts = Some(scripts);
        self.mock_script_pending = true;
        // 构造期可能已安装真实统计上下文；mock 运行不写统计库。
        self.stats_context = None;
        self.stats_key = None;
        Ok(())
    }

    /// 借用脚本表用于父子 agent 继承；不消费初始脚本。
    #[cfg(debug_assertions)]
    pub fn mock_scripts(&self) -> Option<&MockScripts> {
        self.mock_scripts.as_ref()
    }

    /// 取当前模型引用对应的初始脚本；pending 只消费一次，后续工具续轮不再携带。
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

    fn settle_task_request(&self, id: u64, usage: Option<TokenUsage>) {
        if let Some(ledger) = &self.task_accounting {
            ledger.settle(id, &self.model_reference, usage);
        }
    }

    pub fn copy_stats_context(&self, target: &mut Self) {
        // mock 客户端之间/与真实客户端之间都不传播统计上下文，避免绕过 set_stats_context 的忽略规则。
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

    /// 每次逻辑模型请求收尾恰好调用一次的使用计数；与 stats 保存无关。
    /// 失败只发固定脱敏警告，不更改已发生的模型结果，也不重试请求。
    fn record_model_usage(&mut self) {
        // mock 请求不是真实模型用量；不写真实统计库也不发不可用警告。
        #[cfg(debug_assertions)]
        if self.mock_scripts.is_some() {
            return;
        }
        let counted = match &mut self.usage_store {
            Some(store) => store.increment_model_usage(&self.model_reference).is_ok(),
            None => false,
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
            // 构造期打开失败保持静默；实际计数失败时才发脱敏警告。
            usage_store: pp_config::agent_store::AgentStore::open(&config.agent_db_path).ok(),
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
        // mock 客户端重建时同样绕过系统代理；普通客户端保持既有代理行为。
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
        // auth.path 是 agent.db；启动与 auth::service 已完成一次性迁移，
        // 持锁期间只读/写 auth_credentials，绝不触发迁移或读取旧 auth.yml。
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
                .expect("运行时已初始化")
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
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }

    /// 网页请求使用独立连接池，绝不复用模型认证头或限流重试策略。
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
        // 只限制建连和连续无数据的等待，持续生成不设总耗时上限。
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .redirect(Policy::none())
        .retry(reqwest::retry::never());
    if bypass_proxy {
        // 内嵌 mock server 只监听回环地址，不能被用户代理环境变量拦截。
        builder = builder.no_proxy();
    }
    let http = builder
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
                    request_id = pp_core::logging::diagnostic_text(&request_id),
                    body = pp_core::logging::diagnostic_text(&String::from_utf8_lossy(&body)),
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

#[cfg(test)]
mod tests;

fn elapsed_duration(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
