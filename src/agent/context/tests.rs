use super::*;
use crate::context::estimate_tokens;
use async_openai::types::chat::{
    ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
    FunctionCall,
};
use async_openai::types::responses as r;

const INCOMPLETE: &str = "cannot compact: unsupported or incomplete history";

fn chat_system(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::System(
        async_openai::types::chat::ChatCompletionRequestSystemMessage {
            content: text.into(),
            name: None,
        },
    )
}

fn chat_user(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(
        async_openai::types::chat::ChatCompletionRequestUserMessage {
            content: text.into(),
            name: None,
        },
    )
}

fn chat_assistant(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        async_openai::types::chat::ChatCompletionRequestAssistantMessage {
            content: Some(
                async_openai::types::chat::ChatCompletionRequestAssistantMessageContent::Text(
                    text.into(),
                ),
            ),
            ..Default::default()
        },
    )
}

fn chat_assistant_calls(ids: &[&str]) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Assistant(
        async_openai::types::chat::ChatCompletionRequestAssistantMessage {
            tool_calls: Some(
                ids.iter()
                    .map(|id| {
                        ChatCompletionMessageToolCalls::Function(ChatCompletionMessageToolCall {
                            id: (*id).into(),
                            function: FunctionCall {
                                name: "read".into(),
                                arguments: format!("{{ \"path\" : \"{id}\" }}"),
                            },
                        })
                    })
                    .collect(),
            ),
            ..Default::default()
        },
    )
}

fn chat_tool(id: &str, text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Tool(
        async_openai::types::chat::ChatCompletionRequestToolMessage {
            tool_call_id: id.into(),
            content: text.into(),
        },
    )
}

fn chat_summary_user() -> ChatCompletionRequestMessage {
    chat_user("[Context summary]\nremembered constraint\n[/Context summary]")
}

fn res_user(text: &str) -> r::InputItem {
    r::InputItem::EasyMessage(r::EasyInputMessage {
        role: r::Role::User,
        content: r::EasyInputContent::Text(text.into()),
        ..Default::default()
    })
}

fn res_reasoning(id: &str, encrypted: Option<&str>) -> r::InputItem {
    r::InputItem::Item(r::Item::Reasoning(r::ReasoningItem {
        id: Some(id.into()),
        summary: vec![r::SummaryPart::SummaryText(r::SummaryTextContent {
            text: "thought".into(),
        })],
        content: None,
        encrypted_content: encrypted.map(str::to_string),
        status: Some(r::OutputStatus::Completed),
    }))
}

fn res_assistant(id: &str, text: &str) -> r::InputItem {
    r::InputItem::Item(r::Item::Message(r::MessageItem::Output(r::OutputMessage {
        id: id.into(),
        role: r::AssistantRole::Assistant,
        status: r::OutputStatus::Completed,
        phase: None,
        content: vec![r::OutputMessageContent::OutputText(r::OutputTextContent {
            annotations: Vec::new(),
            logprobs: None,
            text: text.into(),
        })],
    })))
}

fn res_call(call_id: &str) -> r::InputItem {
    r::InputItem::Item(r::Item::FunctionCall(r::FunctionToolCall {
        call_id: call_id.into(),
        name: "read".into(),
        arguments: format!("{{ \"path\" : \"{call_id}\" }}"),
        id: Some(format!("provider-{call_id}")),
        status: Some(r::OutputStatus::Completed),
        namespace: None,
        caller: None,
        r#async: None,
    }))
}

fn res_output(call_id: &str, text: &str) -> r::InputItem {
    r::InputItem::Item(r::Item::FunctionCallOutput(
        r::FunctionCallOutputItemParam {
            call_id: Some(call_id.into()),
            output: r::FunctionCallOutput::Text(text.into()),
            id: None,
            status: None,
            name: None,
            namespace: None,
            caller: None,
        },
    ))
}

fn unsupported_chat() -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Function(
        async_openai::types::chat::ChatCompletionRequestFunctionMessage {
            name: "legacy".into(),
            content: Some("result".into()),
        },
    )
}

fn unsupported_response_item() -> r::InputItem {
    r::InputItem::Item(r::Item::WebSearchCall(r::WebSearchToolCall {
        action: None,
        id: "web-1".into(),
        status: r::WebSearchCallStatus::Completed,
    }))
}

fn assert_incomplete_chat(messages: Vec<ChatCompletionRequestMessage>) {
    let error = completion_boundaries(&messages).unwrap_err().to_string();
    assert_eq!(error, INCOMPLETE);
}

fn assert_incomplete_responses(items: Vec<r::InputItem>) {
    let error = response_boundaries(&items).unwrap_err().to_string();
    assert_eq!(error, INCOMPLETE);
}

#[test]
fn context_chat_plain_final_replies_close_each_exchange() {
    let messages = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer one"),
        chat_user("second"),
        chat_assistant("answer two"),
    ];
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![3, 5]);
}

#[test]
fn context_chat_tool_batch_closes_only_after_all_results() {
    let messages = vec![
        chat_system("system"),
        chat_user("read both"),
        chat_assistant_calls(&["a", "b"]),
        chat_tool("a", "result a"),
        chat_tool("b", "result b"),
        chat_user("second"),
        chat_assistant("answer two"),
    ];
    // The boundary sits after the complete batch (index 5), never between
    // the two results or between the assistant and its results.
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![5, 7]);
}

#[test]
fn context_chat_consecutive_tool_batches_are_separate_exchanges() {
    let messages = vec![
        chat_system("system"),
        chat_user("task"),
        chat_assistant_calls(&["a"]),
        chat_tool("a", "result a"),
        chat_assistant_calls(&["b"]),
        chat_tool("b", "result b"),
        chat_assistant("final answer"),
    ];
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![4, 6, 7]);
}

#[test]
fn context_chat_batch_results_in_any_order_close_together() {
    let messages = vec![
        chat_system("system"),
        chat_user("read both"),
        chat_assistant_calls(&["a", "b"]),
        chat_tool("b", "result b"),
        chat_tool("a", "result a"),
    ];
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![5]);
}

#[test]
fn context_chat_pending_user_gets_no_boundary() {
    let messages = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer one"),
        chat_user("unanswered"),
    ];
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![3]);
}

#[test]
fn context_chat_multiple_users_join_the_next_exchange() {
    let messages = vec![
        chat_system("system"),
        chat_user("part one"),
        chat_user("part two"),
        chat_assistant("answer"),
    ];
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![4]);
}

#[test]
fn context_chat_empty_or_pending_only_histories_have_no_boundaries() {
    assert_eq!(completion_boundaries(&[]).unwrap(), Vec::<usize>::new());
    let system_only = vec![chat_system("system")];
    assert_eq!(
        completion_boundaries(&system_only).unwrap(),
        Vec::<usize>::new()
    );
    let pending = vec![chat_system("system"), chat_user("unanswered")];
    assert_eq!(
        completion_boundaries(&pending).unwrap(),
        Vec::<usize>::new()
    );
}

#[test]
fn context_chat_existing_summary_is_a_regular_user_prefix() {
    // A previously compacted history keeps its summary as an ordinary user
    // message: it joins the following assistant reply and is not treated as a
    // special boundary of its own.
    let messages = vec![
        chat_system("system"),
        chat_summary_user(),
        chat_assistant("kept reply"),
        chat_user("later"),
        chat_assistant("later reply"),
    ];
    assert_eq!(completion_boundaries(&messages).unwrap(), vec![3, 5]);
}

#[test]
fn context_chat_missing_tool_result_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("read both"),
        chat_assistant_calls(&["a", "b"]),
        chat_tool("a", "result a"),
    ];
    assert_incomplete_chat(messages);

    // A trailing call without any result is equally incomplete.
    let messages = vec![
        chat_system("system"),
        chat_user("read"),
        chat_assistant_calls(&["a"]),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_duplicate_tool_result_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("read"),
        chat_assistant_calls(&["a"]),
        chat_tool("a", "result a"),
        chat_tool("a", "result a again"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_orphan_tool_result_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("question"),
        chat_assistant("answer"),
        chat_tool("unknown", "orphan"),
    ];
    assert_incomplete_chat(messages);

    // A result that references no call of the preceding batch.
    let messages = vec![
        chat_system("system"),
        chat_user("read"),
        chat_assistant_calls(&["a"]),
        chat_tool("a", "result a"),
        chat_tool("b", "orphan"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_unsupported_message_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("question"),
        unsupported_chat(),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_responses_reasoning_calls_and_results_form_one_group() {
    let items = vec![
        res_user("read both"),
        res_reasoning("reason-1", Some("encrypted-blob")),
        res_call("a"),
        res_call("b"),
        res_output("a", "result a"),
        res_output("b", "result b"),
        res_user("second"),
        res_assistant("msg-1", "answer two"),
    ];
    // The encrypted reasoning stays inside the same exchange as the calls it
    // precedes: the only boundary before the next user item sits after the
    // final result (index 6), never between reasoning and the calls.
    assert_eq!(response_boundaries(&items).unwrap(), vec![6, 8]);
}

#[test]
fn context_responses_call_group_closes_only_after_all_results() {
    let items = vec![
        res_user("read both"),
        res_call("a"),
        res_call("b"),
        res_output("b", "result b"),
        res_output("a", "result a"),
    ];
    assert_eq!(response_boundaries(&items).unwrap(), vec![5]);
}

#[test]
fn context_responses_message_inside_call_group_stays_in_group() {
    let items = vec![
        res_user("task"),
        res_reasoning("reason-1", None),
        res_assistant("msg-1", "I will read"),
        res_call("a"),
        res_output("a", "result a"),
        res_user("next"),
        res_assistant("msg-2", "answer"),
    ];
    assert_eq!(response_boundaries(&items).unwrap(), vec![5, 7]);
}

#[test]
fn context_responses_no_call_group_closes_at_next_user() {
    let items = vec![
        res_user("first"),
        res_reasoning("reason-1", Some("encrypted-blob")),
        res_assistant("msg-1", "answer one"),
        res_user("second"),
        res_reasoning("reason-2", None),
        res_assistant("msg-2", "answer two"),
    ];
    // The plain assistant output group only closes when the next user item
    // appears, so reasoning can never be cut from its output message.
    assert_eq!(response_boundaries(&items).unwrap(), vec![3, 6]);
}

#[test]
fn context_responses_no_call_group_closes_at_end() {
    let items = vec![
        res_user("first"),
        res_reasoning("reason-1", None),
        res_assistant("msg-1", "answer one"),
    ];
    assert_eq!(response_boundaries(&items).unwrap(), vec![3]);
}

#[test]
fn context_responses_pending_user_gets_no_boundary() {
    let items = vec![
        res_user("first"),
        res_assistant("msg-1", "answer one"),
        res_user("unanswered"),
    ];
    assert_eq!(response_boundaries(&items).unwrap(), vec![2]);
}

#[test]
fn context_responses_empty_or_pending_only_histories_have_no_boundaries() {
    assert_eq!(response_boundaries(&[]).unwrap(), Vec::<usize>::new());
    let pending = vec![res_user("unanswered")];
    assert_eq!(response_boundaries(&pending).unwrap(), Vec::<usize>::new());
}

#[test]
fn context_responses_existing_summary_is_a_regular_user_prefix() {
    let items = vec![
        res_user("[Context summary]\nremembered constraint\n[/Context summary]"),
        res_reasoning("reason-1", None),
        res_assistant("msg-1", "kept reply"),
        res_user("later"),
        res_assistant("msg-2", "later reply"),
    ];
    assert_eq!(response_boundaries(&items).unwrap(), vec![3, 5]);
}

#[test]
fn context_responses_missing_output_rejected() {
    let items = vec![
        res_user("read both"),
        res_call("a"),
        res_call("b"),
        res_output("a", "result a"),
    ];
    assert_incomplete_responses(items);

    let items = vec![res_user("read"), res_call("a")];
    assert_incomplete_responses(items);
}

#[test]
fn context_responses_duplicate_output_rejected() {
    let items = vec![
        res_user("read"),
        res_call("a"),
        res_output("a", "result a"),
        res_output("a", "result a again"),
    ];
    assert_incomplete_responses(items);
}

#[test]
fn context_responses_orphan_output_rejected() {
    let items = vec![
        res_user("question"),
        res_assistant("msg-1", "answer"),
        res_output("unknown", "orphan"),
    ];
    assert_incomplete_responses(items);

    let items = vec![
        res_user("read"),
        res_call("a"),
        res_output("a", "result a"),
        res_output("b", "orphan"),
    ];
    assert_incomplete_responses(items);
}

#[test]
fn context_responses_unsupported_item_rejected() {
    let items = vec![res_user("question"), unsupported_response_item()];
    assert_incomplete_responses(items);
}

fn chat_developer(text: &str) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::Developer(
        async_openai::types::chat::ChatCompletionRequestDeveloperMessage {
            content: text.into(),
            name: None,
        },
    )
}

fn res_assistant_easy(text: &str) -> r::InputItem {
    r::InputItem::EasyMessage(r::EasyInputMessage {
        role: r::Role::Assistant,
        content: r::EasyInputContent::Text(text.into()),
        ..Default::default()
    })
}

fn res_structured_user(text: &str) -> r::InputItem {
    r::InputItem::Item(r::Item::Message(r::MessageItem::Input(r::InputMessage {
        content: vec![r::InputContent::InputText(r::InputTextContent {
            text: text.into(),
            prompt_cache_breakpoint: None,
        })],
        role: r::InputRole::User,
        status: None,
    })))
}

#[test]
fn context_chat_developer_message_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("question"),
        chat_assistant("answer"),
        chat_developer("late developer note"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_system_message_outside_first_position_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("question"),
        chat_system("mid-history system"),
        chat_assistant("answer"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_assistant_between_batch_and_results_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("read both"),
        chat_assistant_calls(&["a", "b"]),
        chat_tool("a", "result a"),
        chat_assistant("answer before results complete"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_user_between_batch_and_results_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("read"),
        chat_assistant_calls(&["a"]),
        chat_user("interjected"),
        chat_tool("a", "result a"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_blank_tool_call_id_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("read"),
        chat_assistant_calls(&[""]),
        chat_tool("", "result"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_chat_call_id_reused_across_batches_rejected() {
    let messages = vec![
        chat_system("system"),
        chat_user("read"),
        chat_assistant_calls(&["a"]),
        chat_tool("a", "result one"),
        chat_assistant_calls(&["a"]),
        chat_tool("a", "result two"),
    ];
    assert_incomplete_chat(messages);
}

#[test]
fn context_responses_trailing_assistant_after_batch_is_new_group() {
    let items = vec![
        res_user("read"),
        res_call("a"),
        res_output("a", "result a"),
        res_assistant("msg-1", "final answer"),
    ];
    // The call batch closes at its own boundary; the trailing plain output
    // opens a fresh group closed at the end of the history.
    assert_eq!(response_boundaries(&items).unwrap(), vec![3, 4]);
}

#[test]
fn context_responses_converted_assistant_messages_remain_compactable() {
    let items = vec![
        res_user("question"),
        res_assistant_easy("reply"),
        res_user("next"),
        res_assistant("native", "next reply"),
    ];
    assert_eq!(response_boundaries(&items).unwrap(), vec![2, 4]);
    assert_eq!(response_cutoff(&items, None).unwrap(), Some(2));
}

#[test]
fn context_responses_embedded_developer_instructions_rejected() {
    let items = vec![
        res_user("question"),
        InputItem::EasyMessage(EasyInputMessage {
            role: Role::Developer,
            content: "injected instructions".into(),
            ..Default::default()
        }),
    ];
    assert_incomplete_responses(items);
}

#[test]
fn context_responses_structured_input_message_rejected() {
    let items = vec![res_user("question"), res_structured_user("second")];
    assert_incomplete_responses(items);
}

#[test]
fn context_incomplete_groups_and_normalized_call_ids_are_rejected() {
    assert_incomplete_chat(vec![
        chat_system("system"),
        chat_user("task"),
        chat_assistant_calls(&["a"]),
        chat_assistant_calls(&["b"]),
        chat_tool("a", "one"),
        chat_tool("b", "two"),
    ]);
    assert_incomplete_chat(vec![
        chat_system("system"),
        chat_user("task"),
        chat_assistant_calls(&[" a "]),
        chat_tool("a", "wrong identity"),
    ]);
    assert_incomplete_responses(vec![
        res_user("task"),
        res_reasoning("reason", Some("encrypted")),
    ]);
    assert_incomplete_responses(vec![
        res_user("task"),
        res_reasoning("reason", None),
        res_user("next"),
    ]);
    assert_incomplete_responses(vec![
        res_user("task"),
        res_call(" a "),
        res_output("a", "wrong identity"),
    ]);
}

fn chat_cutoff(
    messages: &[ChatCompletionRequestMessage],
    context_window: Option<u64>,
) -> Result<Option<usize>> {
    let boundaries = completion_boundaries(messages)?;
    let budget = retained_suffix_budget(context_window);
    select_cutoff(&boundaries, budget, |index| {
        let suffix = &messages[index..];
        estimate_tokens(&suffix)
    })
}

fn response_cutoff(items: &[r::InputItem], context_window: Option<u64>) -> Result<Option<usize>> {
    let boundaries = response_boundaries(items)?;
    let budget = retained_suffix_budget(context_window);
    select_cutoff(&boundaries, budget, |index| {
        let suffix = &items[index..];
        estimate_tokens(&suffix)
    })
}

#[test]
fn context_chat_retention_earliest_cutoff_when_budget_fits() {
    let messages = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer one"),
        chat_user("second"),
        chat_assistant("answer two"),
        chat_user("third"),
        chat_assistant("answer three"),
    ];
    // Every exchange is small, so the earliest safe cutoff (right after the
    // first complete exchange) already fits the default budget.
    assert_eq!(chat_cutoff(&messages, None).unwrap(), Some(3));
    assert_eq!(chat_cutoff(&messages, Some(1_000_000)).unwrap(), Some(3));
}

#[test]
fn context_chat_retention_budget_follows_window_fifth_and_cap() {
    let big = "x".repeat(55_000);
    let messages = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer one"),
        chat_user(&big),
        chat_assistant("answer two"),
        chat_user("third"),
        chat_assistant("answer three"),
    ];
    // The suffix from cutoff 3 holds the huge second exchange.
    let suffix = &messages[3..];
    let suffix_tokens = estimate_tokens(&suffix).unwrap();
    assert!(suffix_tokens > 16_384);
    // Unknown capacity keeps the fixed 16384 budget instead of an unlimited one.
    assert_eq!(chat_cutoff(&messages, None).unwrap(), Some(5));
    // The budget is capped at 16384 rather than growing with window/5.
    assert_eq!(chat_cutoff(&messages, Some(1_000_000)).unwrap(), Some(5));
    // A smaller exchange exercises window/5 below the fixed cap.
    let mut messages = messages;
    messages[3] = chat_user(&"x".repeat(1000));
    let suffix_tokens = estimate_tokens(&&messages[3..]).unwrap();
    // An exact fit keeps the earliest cutoff.
    assert_eq!(
        chat_cutoff(&messages, Some(5 * suffix_tokens)).unwrap(),
        Some(3)
    );
    // One token short pushes the cutoff to the next boundary.
    assert_eq!(
        chat_cutoff(&messages, Some(5 * (suffix_tokens - 1))).unwrap(),
        Some(5)
    );
}

#[test]
fn context_chat_retention_keeps_last_exchange_whole_when_over_budget() {
    let messages = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer one"),
        chat_user("second"),
        chat_assistant("answer two"),
    ];
    // Even a near-zero budget never splits the retained exchange: the cutoff
    // falls on the only candidate boundary rather than inside the suffix.
    assert_eq!(chat_cutoff(&messages, Some(5)).unwrap(), Some(3));
}

#[test]
fn context_chat_retention_keeps_trailing_pending_user_in_suffix() {
    let messages = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer one"),
        chat_user("second"),
        chat_assistant("answer two"),
        chat_user("unanswered"),
    ];
    assert_eq!(chat_cutoff(&messages, None).unwrap(), Some(3));
}

#[test]
fn context_chat_retention_none_without_removable_exchange() {
    assert_eq!(chat_cutoff(&[], None).unwrap(), None::<usize>);
    let pending = vec![chat_system("system"), chat_user("unanswered")];
    assert_eq!(chat_cutoff(&pending, None).unwrap(), None::<usize>);
    let single = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer"),
    ];
    assert_eq!(chat_cutoff(&single, None).unwrap(), None::<usize>);
    // A lone exchange stays even when a pending user follows it.
    let single_pending = vec![
        chat_system("system"),
        chat_user("first"),
        chat_assistant("answer"),
        chat_user("unanswered"),
    ];
    assert_eq!(chat_cutoff(&single_pending, None).unwrap(), None::<usize>);
}

#[test]
fn context_chat_retention_summary_prefix_is_removable_again() {
    // A stored summary is regular history: it can be re-summarized together
    // with the exchanges that follow it, never pinned as a fixed prefix.
    let messages = vec![
        chat_system("system"),
        chat_summary_user(),
        chat_assistant("kept reply"),
        chat_user("later"),
        chat_assistant("later reply"),
    ];
    assert_eq!(chat_cutoff(&messages, None).unwrap(), Some(3));
    let only_summary = vec![
        chat_system("system"),
        chat_summary_user(),
        chat_assistant("kept reply"),
    ];
    assert_eq!(chat_cutoff(&only_summary, None).unwrap(), None::<usize>);
}

#[test]
fn context_chat_retention_rejects_invalid_history() {
    let messages = vec![
        chat_system("system"),
        chat_user("read both"),
        chat_assistant_calls(&["a", "b"]),
        chat_tool("a", "result a"),
    ];
    assert_eq!(
        chat_cutoff(&messages, None).unwrap_err().to_string(),
        INCOMPLETE
    );
}

#[test]
fn context_responses_retention_earliest_cutoff_when_budget_fits() {
    let items = vec![
        res_user("first"),
        res_call("a"),
        res_output("a", "result a"),
        res_user("second"),
        res_assistant("msg-1", "answer two"),
    ];
    assert_eq!(response_cutoff(&items, None).unwrap(), Some(3));
}

#[test]
fn context_responses_retention_moves_past_exchange_that_exceeds_budget() {
    let big = "x".repeat(55_000);
    let items = vec![
        res_user("first"),
        res_call("a"),
        res_output("a", "result a"),
        res_user(&big),
        res_assistant("msg-1", "answer two"),
        res_user("third"),
        res_assistant("msg-2", "answer three"),
    ];
    // The suffix from cutoff 3 holds the oversized middle exchange, so the
    // cutoff moves to the next boundary and keeps the last exchange intact.
    assert_eq!(response_cutoff(&items, None).unwrap(), Some(5));
    assert_eq!(response_cutoff(&items, Some(1_000_000)).unwrap(), Some(5));
}

#[test]
fn context_responses_retention_preserves_encrypted_reasoning_in_suffix() {
    let items = vec![
        res_user("first"),
        res_call("a"),
        res_output("a", "result a"),
        res_user("second"),
        res_reasoning("reason-2", Some("opaque-native-state")),
        res_call("b"),
        res_output("b", "result b"),
    ];
    let cutoff = response_cutoff(&items, None).unwrap().unwrap();
    assert_eq!(cutoff, 3);
    // The retained suffix keeps the whole second exchange, encrypted reasoning
    // included, exactly as it appeared in the native history.
    let suffix = &items[cutoff..];
    assert_eq!(suffix.len(), 4);
    let r::InputItem::Item(r::Item::Reasoning(reasoning)) = &suffix[1] else {
        panic!("retained suffix must keep the native reasoning item");
    };
    assert_eq!(
        reasoning.encrypted_content.as_deref(),
        Some("opaque-native-state")
    );
}

#[test]
fn context_responses_retention_keeps_last_exchange_whole_when_over_budget() {
    let items = vec![
        res_user("first"),
        res_call("a"),
        res_output("a", "result a"),
        res_user("second"),
        res_assistant("msg-1", "answer two"),
    ];
    assert_eq!(response_cutoff(&items, Some(5)).unwrap(), Some(3));
}

#[test]
fn context_responses_retention_none_without_removable_exchange() {
    assert_eq!(response_cutoff(&[], None).unwrap(), None::<usize>);
    let pending = vec![res_user("unanswered")];
    assert_eq!(response_cutoff(&pending, None).unwrap(), None::<usize>);
    let single = vec![
        res_user("first"),
        res_reasoning("reason-1", Some("opaque-native-state")),
        res_assistant("msg-1", "answer one"),
    ];
    assert_eq!(response_cutoff(&single, None).unwrap(), None::<usize>);
}

#[test]
fn context_responses_retention_rejects_invalid_history() {
    let items = vec![res_user("read"), res_call("a")];
    assert_eq!(
        response_cutoff(&items, None).unwrap_err().to_string(),
        INCOMPLETE
    );
}

#[test]
fn context_automatic_candidate_must_shrink_and_be_strictly_below_threshold() {
    let mut spec = SummarySpec {
        edit_mode: crate::config::EditMode::Replace,
        fallback: None,
        fallback_used: &std::cell::Cell::new(false),
        system: None,
        cutoff: 0,
        instruction: "",
        output_limit: 0,
        context_window: Some(32768),
        threshold_percent: 80,
        automatic: true,
        before_tokens: 30000,
    };
    // 32768 * 80% = 26214.4: the integer just below must remain acceptable.
    assert!(candidate_accepted(&spec, 26214));
    assert!(!candidate_accepted(&spec, 26215));
    spec.before_tokens = 26214;
    assert!(!candidate_accepted(&spec, 26214));
    spec.context_window = Some(10000);
    spec.before_tokens = 12000;
    assert!(!candidate_accepted(&spec, 8000));
    assert!(candidate_accepted(&spec, 7999));
}

#[test]
fn context_select_cutoff_requires_two_boundaries() {
    // Without a removable older exchange there is never a cutoff, whatever the
    // budget allows.
    let cheap = |_: usize| Ok(0_u64);
    assert_eq!(select_cutoff(&[], 100, cheap).unwrap(), None::<usize>);
    assert_eq!(select_cutoff(&[5], u64::MAX, cheap).unwrap(), None::<usize>);
}

#[test]
fn context_select_cutoff_picks_earliest_fitting_boundary() {
    // Costs shrink as the cutoff moves later, mirroring real suffix costs.
    let cost = |boundary: usize| {
        Ok(match boundary {
            3 => 100_u64,
            5 => 50,
            _ => 10,
        })
    };
    // The earliest boundary fits.
    assert_eq!(select_cutoff(&[3, 5, 7], 100, cost).unwrap(), Some(3));
    // The earliest boundary no longer fits, so the next one is chosen.
    assert_eq!(select_cutoff(&[3, 5, 7], 50, cost).unwrap(), Some(5));
    // An exact fit still counts as safe.
    assert_eq!(select_cutoff(&[3, 5, 7], 99, cost).unwrap(), Some(5));
    // Nothing fits: the last exchange is kept whole on the final candidate.
    assert_eq!(select_cutoff(&[3, 5, 7], 1, cost).unwrap(), Some(5));
}
