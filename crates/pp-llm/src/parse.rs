//! 两种协议的响应 → turn 转换与严格校验。

use anyhow::{Result, bail, ensure};
use async_openai::types::{
    chat::{
        ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessage,
        CreateChatCompletionResponse, FinishReason, Role,
    },
    responses::{OutputItem, OutputMessageContent, OutputStatus, Response, Status},
};
use std::collections::HashSet;

use pp_core::context::TokenUsage;

use super::{CompletionTurn, ResponseTurn};

pub(super) fn into_completion_turn(
    response: CreateChatCompletionResponse,
) -> Result<CompletionTurn> {
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

pub(super) fn into_response_turn(response: Response) -> Result<ResponseTurn> {
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
