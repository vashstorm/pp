//! Chat Completions 协议的单次请求、流式收集与重试循环。

use anyhow::{Result, ensure};
use async_openai::types::chat::CreateChatCompletionRequest;
use futures_util::StreamExt;
use std::time::Instant;

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::context::TokenUsage;
use pp_core::events::AgentEvent;

use super::parse::into_completion_turn;
use super::retry::{ModelFailure, RequestAttempts, model_request_failed, provider_failure};
use super::streaming::{ChatChunk, ChatStream};
use super::{CompletionTurn, EMPTY_SUMMARY, OpenAiClient, elapsed_duration, elapsed_millis};
use pp_config::auth::http as auth;

impl OpenAiClient {
    pub fn complete(
        &mut self,
        request: &CreateChatCompletionRequest,
        cancellation: &CancellationToken,
        on_request_sent: &mut dyn FnMut() -> Result<()>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<CompletionTurn> {
        self.complete_request(request, cancellation, on_request_sent, emit, false)
    }

    pub fn complete_summary(
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
                    model: request.model.clone(),
                    error: error.to_string(),
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
            attempts.accounting_id = pp_config::task_status::request_id();
            attempts.accounting_rate = self
                .task_accounting
                .as_ref()
                .and_then(|ledger| ledger.rate(&self.model_reference, &request.model));
        }
        let started_at = attempts.started;
        let record = !cancellation.is_cancelled() && self.stats_context.is_some();
        let mut observation = pp_stats::Observation::new();
        let mut partial = false;
        if new_request {
            emit(AgentEvent::ModelRequestStarted);
        }
        // 生成中的实时费用：请求体 ÷3 估算输入，已接收输出字节 ÷3 估算输出。
        // 落定金额仍以服务端 usage 为准，估算只在落定前存在。
        let accounting = self.task_accounting.clone();
        let accounting_id = attempts.accounting_id;
        let accounting_rate = attempts.accounting_rate;
        let prompt_tokens = if accounting_rate.is_some() {
            pp_core::context::estimate_tokens(&wire).ok()
        } else {
            None
        };
        let mut progress = |event: AgentEvent| {
            if matches!(&event, AgentEvent::AssistantDelta { text, .. } | AgentEvent::ThinkingDelta { text } if !text.is_empty())
            {
                partial = true;
            }
            if let (Some(ledger), Some(prompt_tokens)) = (&accounting, prompt_tokens)
                && let AgentEvent::ModelOutputProgress { output_bytes } = &event
            {
                ledger.progress(
                    accounting_id,
                    accounting_rate,
                    prompt_tokens,
                    output_bytes.div_ceil(3),
                );
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
        let mut failure_context = pp_core::logging::FailureContext::default();
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
            if record || self.task_accounting.is_some() {
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
                tracing::error!(event = "model_response_error", pid = std::process::id(), detail = %pp_core::logging::diagnostic_text(&format!("{error:#}")));
                ModelFailure { message, reason: pp_core::events::RequestRetryReason::ModelError, retry_after: None }.into()
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
        if snapshot.is_null() && (record || self.task_accounting.is_some()) {
            snapshot = output.stats_snapshot();
        }
        let accounting_usage = result
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
        if attempted && save_attempt {
            self.settle_task_request(attempts.accounting_id, accounting_rate, accounting_usage);
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
            observation.usage = accounting_usage;
            observation.output = snapshot;
            if let Some(context) = &self.stats_context {
                if attempted {
                    observation.request_context = attempts.request_context.take();
                }
                if save_attempt {
                    attempts.pending = None;
                    self.stats_key = pp_stats::save(
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
}
