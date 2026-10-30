use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{StatusCode, header::HeaderMap};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::auth::{Credential, Provider};

const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const KIMI_CLIENT: &str = "17e5f671-d194-4dfb-9706-5516cb48c098";
const GITHUB_CLIENT: &str = "Ov23li8tweQw6odWQebz";
const CODEX_TOKEN: &str = "https://auth.openai.com/oauth/token";
const KIMI_TOKEN: &str = "https://auth.kimi.com/api/oauth/token";

// 保持认证网络的同步入口；Tokio 仍封装在 LLM 层。
pub(crate) struct Http {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

impl Http {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| anyhow::anyhow!("failed to initialize the auth runtime"))?,
            client: client()?,
        })
    }

    pub(crate) fn login(
        &self,
        provider: Provider,
        cancellation: &crate::cancellation::CancellationToken,
        show: impl FnOnce(&str, &str),
    ) -> Result<Credential> {
        self.runtime.block_on(async {
            cancellation
                .run_until_cancelled(login(&self.client, provider, show))
                .await
                .unwrap_or_else(|| Err(crate::cancellation::Cancelled.into()))
        })
    }

    pub(crate) fn discover(
        &self,
        provider: Provider,
        credential: &Credential,
        cancellation: &crate::cancellation::CancellationToken,
    ) -> Result<std::collections::BTreeMap<String, crate::auth::models::Model>> {
        let base = credential
            .api_endpoint
            .as_deref()
            .unwrap_or(provider.base_url());
        let mut url = format!("{base}/models");
        if provider == Provider::OpenaiCodex {
            url.push_str("?client_version=0.159.0");
        }
        self.runtime.block_on(async {
            cancellation
                .run_until_cancelled(discover_at(&self.client, provider, credential, &url))
                .await
                .unwrap_or_else(|| Err(crate::cancellation::Cancelled.into()))
        })
    }

    pub(crate) fn renew(
        &self,
        provider: Provider,
        credential: &Credential,
        cancellation: &crate::cancellation::CancellationToken,
    ) -> Result<Credential> {
        self.runtime.block_on(async {
            cancellation
                .run_until_cancelled(refresh(provider, credential))
                .await
                .unwrap_or_else(|| Err(crate::cancellation::Cancelled.into()))
        })
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .user_agent(concat!("pp/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| anyhow::anyhow!("failed to initialize the auth HTTP client"))
}

async fn discover_at(
    client: &reqwest::Client,
    provider: Provider,
    credential: &Credential,
    url: &str,
) -> Result<std::collections::BTreeMap<String, crate::auth::models::Model>> {
    let (status, value) = response_json(
        client
            .get(url)
            .headers(headers(
                provider,
                credential.account_id.as_deref(),
                credential.device_id.as_deref(),
            )?)
            .bearer_auth(&credential.access_token),
    )
    .await?;
    ensure!(
        status != StatusCode::NOT_FOUND && status != StatusCode::METHOD_NOT_ALLOWED,
        "This provider does not expose a supported model discovery endpoint."
    );
    success(status)?;
    discovered_models(provider, &value)
}

fn discovered_models(
    provider: Provider,
    value: &Value,
) -> Result<std::collections::BTreeMap<String, crate::auth::models::Model>> {
    let values = if provider == Provider::OpenaiCodex {
        &value["models"]
    } else {
        &value["data"]
    };
    let values = values
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("invalid model catalog format"))?;
    ensure!(values.len() <= 4096, "model catalog is too large");
    let mut models = std::collections::BTreeMap::new();
    for value in values {
        if value["visibility"].as_str().is_some_and(|v| v != "list")
            || value["model_picker_enabled"] == false
            || value["capabilities"]["supports"]["tool_calls"] == false
            || value["supports_tool_calls"] == false
            || value["capabilities"]["type"]
                .as_str()
                .is_some_and(|v| v != "chat")
            || value["policy"]["state"]
                .as_str()
                .is_some_and(|v| v != "enabled")
        {
            continue;
        }
        let mut protocol = provider.protocol();
        if let Some(endpoints) = value["supported_endpoints"].as_array() {
            // 同一目录中的模型可能使用不同协议，优先采用明确声明的 Responses。
            if matches!(provider, Provider::GithubCopilot | Provider::CommandCode)
                && endpoints.iter().any(|v| v.as_str() == Some("/responses"))
            {
                protocol = crate::models::Protocol::Response;
            }
            let expected = if protocol == crate::models::Protocol::Response {
                "/responses"
            } else {
                "/chat/completions"
            };
            if !endpoints.iter().any(|v| v.as_str() == Some(expected)) {
                continue;
            }
        }
        let id = if provider == Provider::OpenaiCodex {
            value["slug"].as_str()
        } else {
            value["id"].as_str()
        }
        .filter(|id| !id.trim().is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        .ok_or_else(|| anyhow::anyhow!("invalid model identifier"))?;
        // 供应商明确声明必须思考的模型无法走当前非思考 Chat 适配。
        if matches!(provider, Provider::KimiCode | Provider::Glm | Provider::Zai)
            && value["always_thinking"] == true
        {
            continue;
        }
        let context_window = value["context_window"]
            .as_u64()
            .or_else(|| value["context_length"].as_u64())
            .or_else(|| value["capabilities"]["limits"]["max_context_window_tokens"].as_u64())
            .filter(|size| *size > 0);
        models.insert(
            id.to_owned(),
            crate::auth::models::Model {
                model: id.to_owned(),
                protocol: matches!(provider, Provider::GithubCopilot | Provider::CommandCode)
                    .then_some(protocol),
                context_window,
            },
        );
    }
    ensure!(
        !models.is_empty(),
        "No compatible models were returned by this provider."
    );
    Ok(models)
}

async fn response_json(request: reqwest::RequestBuilder) -> Result<(StatusCode, Value)> {
    let mut response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("auth network request failed"))?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("failed to read the auth response"))?
    {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= 1024 * 1024,
            "auth response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) if !status.is_success() => Value::Null,
        Err(_) => anyhow::bail!("invalid data returned by the auth service"),
    };
    Ok((status, value))
}

fn success(status: StatusCode) -> Result<()> {
    ensure!(
        status.is_success(),
        "auth service HTTP status {}",
        status.as_u16()
    );
    Ok(())
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("auth response is missing required fields"))
}

fn seconds(value: &Value, field: &str, default: u64) -> u64 {
    value[field]
        .as_u64()
        .or_else(|| value[field].as_str()?.parse().ok())
        .unwrap_or(default)
}

fn oauth_credential(
    value: &Value,
    provider: Provider,
    previous: Option<&Credential>,
) -> Result<Credential> {
    let access_token = string(value, "access_token")?.to_owned();
    let refresh_token = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .or_else(|| previous.and_then(|old| old.refresh_token.clone()));
    let expires = seconds(value, "expires_in", 0);
    ensure!(expires > 0, "auth response is missing an expiry");
    let account_id = if provider == Provider::OpenaiCodex {
        codex_account_id(&access_token)
            .or_else(|| value["id_token"].as_str().and_then(codex_account_id))
            .or_else(|| previous.and_then(|old| old.account_id.clone()))
    } else {
        None
    };
    let credential = Credential {
        generation: previous
            .map(|old| old.generation.clone())
            .unwrap_or_default(),
        access_token,
        refresh_token,
        expires_at: Some(crate::auth::now().saturating_add(expires)),
        account_id,
        api_endpoint: None,
        device_id: previous.and_then(|old| old.device_id.clone()),
    };
    credential.validate(provider)?;
    Ok(credential)
}

fn codex_account_id(token: &str) -> Option<String> {
    let mut parts = token.split('.');
    parts.next()?;
    let payload = URL_SAFE_NO_PAD.decode(parts.next()?).ok()?;
    let value: Value = serde_json::from_slice(&payload).ok()?;
    value["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

async fn login(
    client: &reqwest::Client,
    provider: Provider,
    show: impl FnOnce(&str, &str),
) -> Result<Credential> {
    if provider == Provider::OpenaiCodex {
        return codex_login(client, show).await;
    }
    let kimi = provider == Provider::KimiCode;
    ensure!(
        kimi || provider == Provider::GithubCopilot,
        "this provider signs in with an API key"
    );
    let (device_url, token_url, client_id) = if kimi {
        (
            "https://auth.kimi.com/api/oauth/device_authorization",
            KIMI_TOKEN,
            KIMI_CLIENT,
        )
    } else {
        (
            "https://github.com/login/device/code",
            "https://github.com/login/oauth/access_token",
            GITHUB_CLIENT,
        )
    };
    device_login(
        client,
        provider,
        device_url,
        token_url,
        client_id,
        "https://api.github.com/copilot_internal/user",
        show,
    )
    .await
}

async fn device_login(
    client: &reqwest::Client,
    provider: Provider,
    device_url: &str,
    token_url: &str,
    client_id: &str,
    github_user_url: &str,
    show: impl FnOnce(&str, &str),
) -> Result<Credential> {
    let kimi = provider == Provider::KimiCode;
    let device_id = if kimi { Some(random_id()?) } else { None };
    let headers = headers(provider, None, device_id.as_deref())?;
    let mut form = vec![("client_id", client_id)];
    if !kimi {
        form.push(("scope", "read:user"));
    }
    let (status, device) =
        response_json(client.post(device_url).headers(headers.clone()).form(&form)).await?;
    success(status)?;
    let code = string(&device, "device_code")?;
    let user_code = string(&device, "user_code")?;
    let url = device["verification_uri_complete"]
        .as_str()
        .unwrap_or(string(&device, "verification_uri")?);
    validate_verification_url(url, provider)?;
    show(url, user_code);
    let duration = seconds(&device, "expires_in", 600).clamp(1, 1800);
    let deadline = Instant::now() + Duration::from_secs(duration);
    let mut interval = seconds(&device, "interval", 5).clamp(1, 60);
    loop {
        poll_wait(deadline, interval).await?;
        let (status, value) =
            response_json(client.post(token_url).headers(headers.clone()).form(&[
                ("client_id", client_id),
                ("device_code", code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ]))
            .await?;
        match value["error"].as_str() {
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval = interval.saturating_add(5);
                continue;
            }
            Some("access_denied") => anyhow::bail!("user denied the authorization"),
            Some("expired_token") => anyhow::bail!("verification code expired; sign in again"),
            Some(_) => anyhow::bail!("device authorization failed; sign in again"),
            None => {}
        }
        success(status)?;
        let mut credential = if kimi {
            oauth_credential(&value, provider, None)?
        } else {
            Credential::api_key(string(&value, "access_token")?.to_owned())
        };
        credential.device_id = device_id;
        if !kimi {
            // 账号套餐决定 API 域名；只接受 GitHub Copilot 的 HTTPS 域名。
            let (status, user) = response_json(
                client
                    .get(github_user_url)
                    .header(
                        "Authorization",
                        format!("token {}", credential.access_token),
                    )
                    .headers(headers),
            )
            .await?;
            success(status)?;
            if let Some(endpoint) = user["endpoints"]["api"].as_str() {
                ensure!(
                    crate::auth::copilot_endpoint(endpoint),
                    "Copilot returned an invalid API endpoint"
                );
                credential.api_endpoint = Some(endpoint.trim_end_matches('/').to_owned());
            }
        }
        return Ok(credential);
    }
}

fn validate_verification_url(url: &str, provider: Provider) -> Result<()> {
    let url = reqwest::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid auth page URL"))?;
    let allowed = match provider {
        Provider::KimiCode => "kimi.com",
        _ => "github.com",
    };
    ensure!(
        url.scheme() == "https"
            && url.username().is_empty()
            && url.password().is_none()
            && url
                .host_str()
                .is_some_and(|host| host == allowed || host.ends_with(&format!(".{allowed}"))),
        "invalid auth page URL"
    );
    Ok(())
}

async fn poll_wait(deadline: Instant, seconds: u64) -> Result<()> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(
        !remaining.is_zero(),
        "timed out waiting for device authorization; sign in again"
    );
    tokio::time::sleep(Duration::from_secs(seconds).min(remaining)).await;
    ensure!(
        Instant::now() < deadline,
        "timed out waiting for device authorization; sign in again"
    );
    Ok(())
}

async fn codex_login(
    client: &reqwest::Client,
    show: impl FnOnce(&str, &str),
) -> Result<Credential> {
    codex_login_at(
        client,
        "https://auth.openai.com/api/accounts/deviceauth/usercode",
        "https://auth.openai.com/api/accounts/deviceauth/token",
        CODEX_TOKEN,
        show,
    )
    .await
}

async fn codex_login_at(
    client: &reqwest::Client,
    device_url: &str,
    poll_url: &str,
    token_url: &str,
    show: impl FnOnce(&str, &str),
) -> Result<Credential> {
    let (status, device) = response_json(
        client
            .post(device_url)
            .json(&json!({"client_id": CODEX_CLIENT})),
    )
    .await?;
    success(status)?;
    let id = string(&device, "device_auth_id")?;
    let user_code = string(&device, "user_code")?;
    show("https://auth.openai.com/codex/device", user_code);
    let deadline = Instant::now() + Duration::from_secs(900);
    let interval = seconds(&device, "interval", 5)
        .clamp(1, 60)
        .saturating_add(3);
    loop {
        poll_wait(deadline, interval).await?;
        let (status, value) = response_json(
            client
                .post(poll_url)
                .json(&json!({"device_auth_id":id,"user_code":user_code})),
        )
        .await?;
        if matches!(status, StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
            continue;
        }
        success(status)?;
        let (status, token) = response_json(client.post(token_url).form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CODEX_CLIENT),
            ("code", string(&value, "authorization_code")?),
            ("code_verifier", string(&value, "code_verifier")?),
            (
                "redirect_uri",
                "https://auth.openai.com/deviceauth/callback",
            ),
        ]))
        .await?;
        success(status)?;
        return oauth_credential(&token, Provider::OpenaiCodex, None);
    }
}

pub(super) async fn refresh(provider: Provider, previous: &Credential) -> Result<Credential> {
    let client = client()?;
    let (url, client_id) = match provider {
        Provider::OpenaiCodex => (CODEX_TOKEN, CODEX_CLIENT),
        Provider::KimiCode => (KIMI_TOKEN, KIMI_CLIENT),
        _ => anyhow::bail!("this provider does not support token refresh"),
    };
    refresh_at(&client, provider, previous, url, client_id).await
}

async fn refresh_at(
    client: &reqwest::Client,
    provider: Provider,
    previous: &Credential,
    url: &str,
    client_id: &str,
) -> Result<Credential> {
    let token = previous
        .refresh_token
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("missing refresh token; sign in again"))?;
    let (status, value) = response_json(
        client
            .post(url)
            .headers(headers(
                provider,
                previous.account_id.as_deref(),
                previous.device_id.as_deref(),
            )?)
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", client_id),
                ("refresh_token", token),
            ]),
    )
    .await?;
    ensure!(
        status.is_success(),
        "{} token refresh failed (HTTP {}); sign in again",
        provider.id(),
        status.as_u16()
    );
    oauth_credential(&value, provider, Some(previous))
}

fn random_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| anyhow::anyhow!("failed to generate a device identifier"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) fn headers(
    provider: Provider,
    account_id: Option<&str>,
    device_id: Option<&str>,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let mut insert = |name: &'static str, value: &str| -> Result<()> {
        let mut value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| anyhow::anyhow!("invalid auth request header"))?;
        if name == "chatgpt-account-id" {
            value.set_sensitive(true);
        }
        headers.insert(name, value);
        Ok(())
    };
    insert("accept", "application/json")?;
    match provider {
        Provider::GithubCopilot => {
            insert("user-agent", "copilot/1.0.82")?;
            insert("editor-version", "copilot/1.0.82")?;
            insert("copilot-integration-id", "copilot-chat")?;
            insert("copilot-harness-id", "copilot-sdk")?;
            insert("openai-intent", "conversation-agent")?;
        }
        Provider::OpenaiCodex => {
            if let Some(id) = account_id {
                insert("chatgpt-account-id", id)?;
            }
            insert("openai-beta", "responses=experimental")?;
            insert("originator", "pp")?;
            insert("version", "0.159.0")?;
        }
        Provider::KimiCode => {
            insert("user-agent", concat!("KimiCLI/", env!("CARGO_PKG_VERSION")))?;
            insert("x-msh-platform", "kimi_cli")?;
            insert("x-msh-version", env!("CARGO_PKG_VERSION"))?;
            insert("x-msh-device-model", std::env::consts::ARCH)?;
            insert("x-msh-os-version", std::env::consts::OS)?;
            insert("x-msh-device-name", "pp")?;
            if let Some(id) = device_id {
                insert("x-msh-device-id", id)?;
            }
        }
        _ => {}
    }
    Ok(headers)
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub(super) enum WireRequest<'a, T> {
    Original(&'a T),
    Adapted(Value),
}

pub(super) fn wire_request<T: serde::Serialize>(
    request: &T,
    provider: Option<Provider>,
) -> Result<WireRequest<'_, T>> {
    let Some(provider) = provider else {
        return Ok(WireRequest::Original(request));
    };
    let mut body = serde_json::to_value(request)
        .map_err(|_| anyhow::anyhow!("failed to serialize the model request"))?;
    if provider == Provider::OpenaiCodex {
        let object = body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid model request format"))?;
        for field in [
            "background",
            "conversation",
            "max_output_tokens",
            "max_tool_calls",
            "metadata",
            "previous_response_id",
            "temperature",
            "top_p",
            "truncation",
            "user",
        ] {
            object.remove(field);
        }
        object.insert("store".into(), json!(false));
        object.insert("stream".into(), json!(true));
        object.entry("instructions").or_insert(json!(""));
        if let Some(items) = object.get_mut("input").and_then(Value::as_array_mut) {
            for item in items {
                if item["role"] == "system" {
                    item["role"] = json!("developer");
                }
                // store=false 的上下文完整重放，不引用服务端存储的输出项。
                if let Some(item) = item.as_object_mut() {
                    item.remove("id");
                }
            }
        }
    }
    if matches!(provider, Provider::KimiCode | Provider::Glm | Provider::Zai) {
        let object = body
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid model request format"))?;
        // SDK 原生历史不保存供应商专有 reasoning_content；明确使用非思考模式。
        ensure!(
            object
                .get("reasoning_effort")
                .is_none_or(|effort| effort == "none"),
            "this auth source does not support thinking history replay; set think_level to none"
        );
        object.remove("reasoning_effort");
        object.insert("thinking".into(), json!({"type":"disabled"}));
        if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if let Some(function) = tool["function"].as_object_mut() {
                    function.remove("strict");
                }
            }
        }
    }
    if provider == Provider::GithubCopilot && body["reasoning_effort"] == "none" {
        body.as_object_mut()
            .expect("序列化的请求是对象")
            .remove("reasoning_effort");
    }
    Ok(WireRequest::Adapted(body))
}

pub(super) fn request_metadata<T>(wire: &WireRequest<'_, T>) -> (&'static str, bool) {
    let WireRequest::Adapted(body) = wire else {
        return ("user", false);
    };
    let messages = body["messages"]
        .as_array()
        .or_else(|| body["input"].as_array());
    let tool_last = messages
        .and_then(|items| items.last())
        .is_some_and(|last| last["role"] == "tool" || last["type"] == "function_call_output");
    let images = messages.is_some_and(|items| {
        items.iter().any(|message| {
            message["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part["type"] == "image_url" || part["type"] == "input_image")
            })
        })
    });
    (if tool_last { "agent" } else { "user" }, images)
}

#[cfg(test)]
mod tests;
