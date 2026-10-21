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
    request_timeout: Duration,
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
    pub(crate) fn new(config: &Config) -> Result<Self> {
        // Validate before the SDK constructs its authorization header (which can panic).
        HeaderValue::from_str(&format!("Bearer {}", config.api_key)).map_err(|_| {
            anyhow::anyhow!("configuring HTTP authorization: invalid API key header")
        })?;
        let request_timeout = Duration::from_secs(config.settings.request_timeout_seconds);
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
            request_timeout,
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
        self.runtime = Some(runtime);
        Ok(())
    }

    /// Retires the runtime once the synchronous caller's `block_on` has
    /// already returned for a cancelled task. `shutdown_background`
    /// synchronously drops the SDK's owned stream tasks and closes their
    /// connections without waiting for unstoppable blocking DNS work; the
    /// next request rebuilds everything lazily via `ensure_ready`.
    pub(crate) fn cancel_pending_io(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
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
        emit(AgentEvent::ModelRequestStarted);
        let client = &self.client;
        let runtime = self
            .runtime
            .as_ref()
            .expect("ensure_ready leaves a runtime in place");
        let result: Result<CompletionTurn> = runtime.block_on(async {
            // BYOT serializes a borrowed request before returning the stream.
            let opened = cancellation
                .run_until_cancelled(client.chat().create_stream_byot::<_, ChatChunk>(request))
                .await;
            // Persist the accepted request exactly once, before judging the
            // opening outcome; a callback failure takes precedence.
            on_request_sent()?;
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = opened.map_err(|error| redacted_error(error, "Chat Completions"))?;
            let mut output = ChatStream::default();
            loop {
                match cancellation.run_until_cancelled(stream.next()).await {
                    None => return Err(Cancelled.into()),
                    Some(None) => break,
                    Some(Some(chunk)) => {
                        output.push(
                            chunk.map_err(|error| redacted_error(error, "Chat Completions"))?,
                            emit,
                        )?;
                    }
                }
            }
            into_completion_turn(output.finish()?)
        });
        // Every outcome, including cancellation and callback failures, closes the
        // request exactly once with the authoritative usage when there is one.
        emit(AgentEvent::ModelRequestFinished {
            elapsed_ms: elapsed_millis(started_at),
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
        emit(AgentEvent::ModelRequestStarted);
        let client = &self.client;
        let runtime = self
            .runtime
            .as_ref()
            .expect("ensure_ready leaves a runtime in place");
        let result: Result<ResponseTurn> = runtime.block_on(async {
            let opened = cancellation
                .run_until_cancelled(
                    client
                        .responses()
                        .create_stream_byot::<_, ResponseEvent>(request),
                )
                .await;
            on_request_sent()?;
            let opened = match opened {
                Some(opened) => opened,
                None => return Err(Cancelled.into()),
            };
            let mut stream = opened.map_err(|error| redacted_error(error, "Responses"))?;
            let mut output = ResponsesStream::default();
            loop {
                match cancellation.run_until_cancelled(stream.next()).await {
                    None => return Err(Cancelled.into()),
                    Some(None) => {
                        bail!("Responses protocol: stream ended before response.completed")
                    }
                    Some(Some(event)) => {
                        if let Some(turn) = output.push(
                            event.map_err(|error| redacted_error(error, "Responses"))?.0,
                            emit,
                        )? {
                            // response.completed is authoritative; do not wait for HTTP EOF.
                            return Ok(turn);
                        }
                    }
                }
            }
        });
        // The stream may still be open on the wire (or already cancelled); the
        // finish event reports this request, not HTTP EOF.
        emit(AgentEvent::ModelRequestFinished {
            elapsed_ms: elapsed_millis(started_at),
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
                        code: None,
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

fn into_completion_turn(response: CreateChatCompletionResponse) -> Result<CompletionTurn> {
    // Extract server-side usage before the message validation merges or drops fields.
    let usage = response.usage.map(|usage| TokenUsage {
        input_tokens: u64::from(usage.prompt_tokens),
        output_tokens: u64::from(usage.completion_tokens),
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

    fn decode(message: Value, finish: &str) -> Result<CompletionTurn> {
        into_completion_turn(serde_json::from_value(json!({
            "id":"chatcmpl-fixture", "object":"chat.completion", "created":0, "model":"fixture",
            "choices":[{"index":0,"message":message,"finish_reason":finish}]
        }))?)
    }

    fn test_client(base_url: &str) -> OpenAiClient {
        OpenAiClient::new(&Config {
            api_key: "private-key".into(),
            model: "fixture".into(),
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
            base_url: base_url.parse().unwrap(),
            context_window: None,
            settings: crate::config::Settings {
                debug: false,
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
                    cached_tokens: expected
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
