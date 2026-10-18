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
use std::{collections::HashSet, time::Duration};
use tokio::runtime::Runtime;

mod streaming;

use streaming::{ChatChunk, ChatStream, ResponsesStream};

pub(crate) struct OpenAiClient {
    client: Client<OpenAIConfig>,
    runtime: Runtime,
}

pub(crate) struct ResponseTurn {
    pub(crate) output: Vec<OutputItem>,
    pub(crate) text: String,
    pub(crate) has_calls: bool,
}

impl OpenAiClient {
    pub(crate) fn new(config: &Config) -> Result<Self> {
        // Validate before the SDK constructs its authorization header (which can panic).
        HeaderValue::from_str(&format!("Bearer {}", config.api_key)).map_err(|_| {
            anyhow::anyhow!("configuring HTTP authorization: invalid API key header")
        })?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.settings.request_timeout_seconds))
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
                            code: None,
                            misalignment: None,
                        },
                    }))
                }
            })
            .service(ReqwestService::new(http.clone()));
        // Explicitly override every environment-derived SDK setting.
        let sdk_config = OpenAIConfig::new()
            .with_api_key(config.api_key.clone())
            .with_api_base(config.base_url.as_str().trim_end_matches('/'))
            .with_org_id("")
            .with_project_id("");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| anyhow::anyhow!("initializing model runtime failed"))?;
        Ok(Self {
            client: Client::build(http, sdk_config).with_http_service(service),
            runtime,
        })
    }

    pub(crate) fn complete(
        &self,
        request: &CreateChatCompletionRequest,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ChatCompletionRequestAssistantMessage> {
        self.runtime.block_on(async {
            // BYOT serializes a borrowed request before returning the stream.
            let opened = self
                .client
                .chat()
                .create_stream_byot::<_, ChatChunk>(request)
                .await;
            on_request_sent()?;
            let mut stream = opened.map_err(|error| redacted_error(error, "Chat Completions"))?;
            let mut output = ChatStream::default();
            while let Some(chunk) = stream.next().await {
                output.push(
                    chunk.map_err(|error| redacted_error(error, "Chat Completions"))?,
                    emit,
                )?;
            }
            into_message(output.finish()?)
        })
    }

    pub(crate) fn respond(
        &self,
        request: &CreateResponse,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ResponseTurn> {
        self.runtime.block_on(async {
            let opened = self
                .client
                .responses()
                .create_stream_byot::<_, async_openai::types::responses::ResponseStreamEvent>(
                    request,
                )
                .await;
            on_request_sent()?;
            let mut stream = opened.map_err(|error| redacted_error(error, "Responses"))?;
            let mut output = ResponsesStream::default();
            while let Some(event) = stream.next().await {
                if let Some(turn) = output.push(
                    event.map_err(|error| redacted_error(error, "Responses"))?,
                    emit,
                )? {
                    // response.completed is authoritative; do not wait for HTTP EOF.
                    return Ok(turn);
                }
            }
            bail!("Responses protocol: stream ended before response.completed")
        })
    }
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

fn into_message(
    response: CreateChatCompletionResponse,
) -> Result<ChatCompletionRequestAssistantMessage> {
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
    let content = message
        .content
        .filter(|text| !text.trim().is_empty())
        .or_else(|| message.refusal.filter(|text| !text.trim().is_empty()));
    if content.is_none() && calls.is_empty() {
        bail!("Chat Completions protocol: response has neither text nor tool calls");
    }
    Ok(ChatCompletionRequestAssistantMessage {
        content: content.map(Into::into),
        tool_calls: (!calls.is_empty()).then_some(calls),
        ..Default::default()
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
    let text = response_text(&response.output);
    ensure!(
        has_calls || !text.trim().is_empty(),
        "Responses protocol: response has neither text nor tool calls"
    );
    Ok(ResponseTurn {
        output: response.output,
        text,
        has_calls,
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

    fn decode(message: Value, finish: &str) -> Result<ChatCompletionRequestAssistantMessage> {
        into_message(serde_json::from_value(json!({
            "id":"chatcmpl-fixture", "object":"chat.completion", "created":0, "model":"fixture",
            "choices":[{"index":0,"message":message,"finish_reason":finish}]
        }))?)
    }

    #[test]
    fn http_errors_hide_body_and_redirects_are_not_followed() {
        for status in [302, 401, 429, 500] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", server.server_addr());
            let client = OpenAiClient::new(&Config {
                api_key: "private-key".into(),
                model: "fixture".into(),
                protocol: crate::models::Protocol::Completion,
                thinking: crate::models::ModelThinking::default(),
                base_url: base_url.parse().unwrap(),
                settings: crate::config::Settings {
                    notification_show_countdown: true,
                    notification_timeout_seconds: 10,
                    mcp_servers: Default::default(),
                    skill_dirs: Vec::new(),
                    disabled_skills: Vec::new(),
                    think_level: crate::config::ThinkLevel::None,
                    edit: crate::config::EditMode::Replace,
                    default_model: None,
                    max_model_requests: 20,
                    request_timeout_seconds: 120,
                    read_max_bytes: 65_536,
                    read_max_lines: 2_000,
                    bash_timeout_seconds: 120,
                },
            })
            .unwrap();
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
                let error = client
                    .complete(
                        &CreateChatCompletionRequest::default(),
                        &mut || Ok(()),
                        &mut |_| {},
                    )
                    .err()
                    .unwrap()
                    .to_string();
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
            let message = decode(message, "stop").unwrap();
            assert_eq!(
                message.content,
                Some(ChatCompletionRequestAssistantMessageContent::Text(
                    "Cannot help".into()
                ))
            );
            assert!(message.tool_calls.is_none());
        }
    }
}
