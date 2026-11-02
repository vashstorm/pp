//! Responses 协议的单次请求、流式收集与重试循环。

use anyhow::{Result, bail, ensure};
use async_openai::types::responses::{CreateResponse, OutputItem, Status};
use futures_util::StreamExt;
use std::time::Instant;

use crate::cancellation::{CancellationToken, Cancelled};
use crate::context::TokenUsage;
use crate::events::AgentEvent;

use super::auth;
use super::retry::{ModelFailure, RequestAttempts, model_request_failed, provider_failure};
use super::streaming::{ResponseEvent, ResponsesStream};
use super::{EMPTY_SUMMARY, OpenAiClient, ResponseTurn, elapsed_duration, elapsed_millis};

impl OpenAiClient {
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
            attempts.accounting_id = crate::task_status::request_id();
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
        let accounting_usage = result
            .as_ref()
            .ok()
            .and_then(|turn| turn.usage)
            .or(reported_usage);
        if attempted && save_attempt {
            self.settle_task_request(attempts.accounting_id, accounting_usage);
        }
        if record {
            if attempted
                && attempts.request_context.is_none()
                && let Some(context) = &self.stats_context
            {
                attempts.request_context = context.capture(&wire);
            }
            observation.usage = accounting_usage;
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
