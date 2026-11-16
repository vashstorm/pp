//! 请求前的项目指令发布、用量记账与两种协议原生历史追加。
use super::*;

pub(in crate::agent) type ReportedProjectLoads = std::collections::BTreeMap<String, String>;

pub(in crate::agent) fn reported_project_load(
    replay: &[persistence::ReplayRecord],
) -> ReportedProjectLoads {
    replay
        .iter()
        .filter_map(|record| match record {
            persistence::ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
                path,
                content,
            }) => Some((path.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

pub(in crate::agent) fn publish_project_load(
    pending: &mut Vec<crate::project_instructions::Document>,
    reported: &mut ReportedProjectLoads,
    journal: &mut SessionJournal,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    while let Some(document) = pending.first() {
        let path = document.path.display().to_string();
        let event = AgentEvent::ProjectInstructionsLoaded {
            path: path.clone(),
            content: document.content.clone(),
        };
        journal.event(&event);
        journal.check()?;
        emit(event);
        reported.insert(path, pending.remove(0).content);
    }
    Ok(())
}

pub(super) fn push_developer(request: &mut CreateChatCompletionRequest, text: String) {
    request
        .messages
        .push(ChatCompletionRequestMessage::Developer(
            async_openai::types::chat::ChatCompletionRequestDeveloperMessage {
                content: text.into(),
                ..Default::default()
            },
        ));
}

/// 记录一次普通请求的用量：`last_request` 只保存最近一次，会话累计只增不减。
pub(super) fn record_usage(
    last_request: &mut Option<TokenUsage>,
    session_output_tokens: &mut u64,
    usage: Option<TokenUsage>,
) {
    *last_request = usage;
    if let Some(usage) = usage {
        *session_output_tokens = session_output_tokens.saturating_add(usage.output_tokens);
    }
}

/// 收集流式事件中的思考内容，重试/失败时重置。
pub(super) fn collect_thinking_event(
    event: &AgentEvent,
    thinking: &mut String,
    summary: &mut String,
) {
    match event {
        AgentEvent::AssistantStarted | AgentEvent::ModelAttemptFailed { .. } => {
            thinking.clear();
            summary.clear();
        }
        AgentEvent::ThinkingDelta { text } => thinking.push_str(text),
        AgentEvent::ReasoningSummaryDelta { text } => summary.push_str(text),
        _ => {}
    }
}

/// 记录本轮已提交的 LLM 思考数据。
pub(super) fn record_thinking(
    history: &mut Vec<super::super::context_diagnosis::ThinkingEntry>,
    thinking: String,
    summary: String,
) {
    let entry = super::super::context_diagnosis::ThinkingEntry { thinking, summary };
    if !entry.is_empty() {
        history.push(entry);
    }
}

/// Append a user message to either protocol's native history; shared by the
/// agent loop and the fresh-context execution candidate session.
pub(in crate::agent) fn append_input_to(
    session: &mut Session,
    input: &crate::user_input::UserInput,
) {
    use crate::user_input::Part;
    use async_openai::types::{chat as c, responses as r};
    if !input.has_images() {
        let text = input
            .parts
            .iter()
            .filter_map(|p| {
                if let Part::Text(t) = p {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        append_user_to(session, &text);
        return;
    }
    match session {
        Session::Completion(request) => request.messages.push(
            c::ChatCompletionRequestMessage::User(c::ChatCompletionRequestUserMessage {
                content: c::ChatCompletionRequestUserMessageContent::Array(
                    crate::user_input::chat_parts(&input.parts),
                ),
                ..Default::default()
            }),
        ),
        Session::Response(request) => {
            response_input(request).push(r::InputItem::EasyMessage(r::EasyInputMessage {
                role: r::Role::User,
                content: r::EasyInputContent::ContentList(crate::user_input::response_parts(
                    &input.parts,
                )),
                ..Default::default()
            }))
        }
    }
}

pub(in crate::agent) fn append_user_to(session: &mut Session, prompt: &str) {
    match session {
        Session::Completion(request) => request.messages.push(ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: prompt.into(),
                ..Default::default()
            },
        )),
        Session::Response(request) => {
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::User,
                content: prompt.into(),
                ..Default::default()
            }))
        }
    }
}

pub(in crate::agent) fn append_developer_to(session: &mut Session, text: String) {
    match session {
        Session::Completion(request) => {
            request
                .messages
                .push(ChatCompletionRequestMessage::Developer(
                    async_openai::types::chat::ChatCompletionRequestDeveloperMessage {
                        content: text.into(),
                        ..Default::default()
                    },
                ))
        }
        Session::Response(request) => {
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::Developer,
                content: text.into(),
                ..Default::default()
            }))
        }
    }
}

pub(in crate::agent) fn response_input(request: &mut CreateResponse) -> &mut Vec<InputItem> {
    let InputParam::Items(input) = &mut request.input else {
        unreachable!("agent always uses an item array for Responses history");
    };
    input
}
