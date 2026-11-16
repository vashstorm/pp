use super::portable::{PortableItem, visit_response};
use crate::agent::Session;
use crate::tools::read::ToolImage;
use anyhow::Result;
use async_openai::types::{chat as c, responses as r};
use pp_config::config::EditMode;
use pp_config::models::Protocol;
use std::borrow::Cow;
use std::collections::HashSet;

pub(super) fn from_response(
    source: &r::CreateResponse,
    model: String,
    edit_mode: EditMode,
) -> Result<Session> {
    let mut target = Session::new(
        model,
        Protocol::Completion,
        source.instructions.clone().unwrap_or_default(),
        edit_mode,
    );
    let Session::Completion(request) = &mut target else {
        unreachable!()
    };
    if source.instructions.is_none() {
        request.messages.clear();
    }
    let mut builder = CompletionHistoryBuilder::new(&mut request.messages);
    visit_response(source, |item| {
        builder.push(item);
        Ok(())
    })?;
    builder.finish();
    Ok(target)
}

struct CompletionHistoryBuilder<'m> {
    messages: &'m mut Vec<c::ChatCompletionRequestMessage>,
    batch: AssistantBatch,
    pending: HashSet<String>,
    attachments: Vec<(String, Vec<ToolImage>)>,
}

impl<'m> CompletionHistoryBuilder<'m> {
    fn new(messages: &'m mut Vec<c::ChatCompletionRequestMessage>) -> Self {
        Self {
            messages,
            batch: AssistantBatch::default(),
            pending: HashSet::new(),
            attachments: Vec::new(),
        }
    }

    fn push(&mut self, item: PortableItem<'_>) {
        match item {
            PortableItem::User(parts) => {
                self.batch.flush(self.messages);
                self.messages.push(c::ChatCompletionRequestMessage::User(
                    c::ChatCompletionRequestUserMessage {
                        content: c::ChatCompletionRequestUserMessageContent::Array(
                            crate::user_input::chat_parts(&parts),
                        ),
                        ..Default::default()
                    },
                ));
            }
            PortableItem::Message(r::Role::Assistant, text) => self.batch.text(text),
            PortableItem::Call {
                id,
                name,
                arguments,
            } => {
                self.pending.insert(id.to_owned());
                self.batch
                    .calls
                    .push(c::ChatCompletionMessageToolCalls::Function(
                        c::ChatCompletionMessageToolCall {
                            id: id.to_owned(),
                            function: c::FunctionCall {
                                name: name.to_owned(),
                                arguments: arguments.to_owned(),
                            },
                        },
                    ));
            }
            PortableItem::Message(role, text) => {
                self.batch.flush(self.messages);
                self.messages
                    .push(completion_message(role, text.into_owned()));
            }
            PortableItem::Output { id, text, images } => {
                self.batch.flush(self.messages);
                self.attachments.push((id.to_owned(), images));
                self.messages.push(c::ChatCompletionRequestMessage::Tool(
                    c::ChatCompletionRequestToolMessage {
                        tool_call_id: id.to_owned(),
                        content: text.into_owned().into(),
                    },
                ));
                self.pending.remove(id);
                if self.pending.is_empty() {
                    crate::agent::tool_images::append_chat(
                        self.messages,
                        std::mem::take(&mut self.attachments),
                    );
                }
            }
        }
    }

    fn finish(mut self) {
        self.batch.flush(self.messages);
    }
}

#[derive(Default)]
struct AssistantBatch {
    content: Option<String>,
    calls: Vec<c::ChatCompletionMessageToolCalls>,
}

impl AssistantBatch {
    fn text(&mut self, text: Cow<'_, str>) {
        match &mut self.content {
            Some(content) => {
                content.push('\n');
                content.push_str(&text);
            }
            None => self.content = Some(text.into_owned()),
        }
    }

    fn flush(&mut self, messages: &mut Vec<c::ChatCompletionRequestMessage>) {
        if self.content.is_none() && self.calls.is_empty() {
            return;
        }
        messages.push(c::ChatCompletionRequestMessage::Assistant(
            c::ChatCompletionRequestAssistantMessage {
                content: self
                    .content
                    .take()
                    .map(c::ChatCompletionRequestAssistantMessageContent::Text),
                tool_calls: if self.calls.is_empty() {
                    None
                } else {
                    Some(std::mem::take(&mut self.calls))
                },
                ..Default::default()
            },
        ));
    }
}

pub(super) fn completion_message(role: r::Role, text: String) -> c::ChatCompletionRequestMessage {
    match role {
        r::Role::User => {
            c::ChatCompletionRequestMessage::User(c::ChatCompletionRequestUserMessage {
                content: text.into(),
                ..Default::default()
            })
        }
        r::Role::System => {
            c::ChatCompletionRequestMessage::System(c::ChatCompletionRequestSystemMessage {
                content: text.into(),
                ..Default::default()
            })
        }
        r::Role::Developer => {
            c::ChatCompletionRequestMessage::Developer(c::ChatCompletionRequestDeveloperMessage {
                content: text.into(),
                ..Default::default()
            })
        }
        r::Role::Assistant => {
            c::ChatCompletionRequestMessage::Assistant(c::ChatCompletionRequestAssistantMessage {
                content: Some(text.into()),
                ..Default::default()
            })
        }
    }
}
