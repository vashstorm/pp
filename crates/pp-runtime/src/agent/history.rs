use super::Session;
use anyhow::{Result, bail, ensure};
use async_openai::types::{chat as c, responses as r};
use pp_config::config::EditMode;
use pp_config::models::Protocol;
use std::borrow::Cow;

const UNSUPPORTED: &str = "cannot switch model: unsupported history item";

// These borrowed views exist only during a switch, never as a second session history.
enum PortableItem<'a> {
    User(Vec<crate::user_input::Part>),
    Message(r::Role, Cow<'a, str>),
    Call {
        id: &'a str,
        name: &'a str,
        arguments: &'a str,
    },
    Output {
        id: &'a str,
        text: Cow<'a, str>,
        images: Vec<crate::tools::read::ToolImage>,
    },
}

pub(super) fn convert(
    session: &Session,
    model: String,
    protocol: Protocol,
    edit_mode: EditMode,
) -> Result<Session> {
    match session {
        Session::Completion(source) => {
            super::tool_images::validate_chat(&source.messages)?;
            ensure!(protocol == Protocol::Response, UNSUPPORTED);
            let first_system = source
                .messages
                .iter()
                .position(|message| matches!(message, c::ChatCompletionRequestMessage::System(_)));
            let system = match first_system.map(|index| &source.messages[index]) {
                Some(c::ChatCompletionRequestMessage::System(message)) => {
                    system_text(&message.content)
                }
                _ => String::new(),
            };
            let mut target = Session::new(model, protocol, system, edit_mode);
            let Session::Response(request) = &mut target else {
                unreachable!()
            };
            if first_system.is_none() {
                request.instructions = None;
            }
            let r::InputParam::Items(input) = &mut request.input else {
                unreachable!()
            };
            for (index, message) in source.messages.iter().enumerate() {
                if Some(index) != first_system {
                    if super::tool_images::merge_chat(input, message)? {
                        continue;
                    }
                    completion_items(message, input)?;
                }
            }
            Ok(target)
        }
        Session::Response(source) => {
            let mut target = Session::new(
                model,
                protocol,
                source.instructions.clone().unwrap_or_default(),
                edit_mode,
            );
            match &mut target {
                Session::Response(request) => {
                    if source.instructions.is_none() {
                        request.instructions = None;
                    }
                    let r::InputParam::Items(input) = &mut request.input else {
                        unreachable!()
                    };
                    visit_response(source, |item| {
                        input.push(response_item(item));
                        Ok(())
                    })?;
                }
                Session::Completion(request) => {
                    if source.instructions.is_none() {
                        request.messages.clear();
                    }
                    let mut batch = AssistantBatch::default();
                    let mut pending = std::collections::HashSet::new();
                    let mut attachments = Vec::new();
                    visit_response(source, |item| {
                        match item {
                            PortableItem::User(parts) => {
                                batch.flush(&mut request.messages);
                                request.messages.push(c::ChatCompletionRequestMessage::User(
                                    c::ChatCompletionRequestUserMessage {
                                        content: c::ChatCompletionRequestUserMessageContent::Array(
                                            crate::user_input::chat_parts(&parts),
                                        ),
                                        ..Default::default()
                                    },
                                ));
                            }
                            PortableItem::Message(r::Role::Assistant, text) => batch.text(text),
                            PortableItem::Call {
                                id,
                                name,
                                arguments,
                            } => {
                                pending.insert(id.to_owned());
                                batch
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
                                batch.flush(&mut request.messages);
                                request
                                    .messages
                                    .push(completion_message(role, text.into_owned()));
                            }
                            PortableItem::Output { id, text, images } => {
                                batch.flush(&mut request.messages);
                                attachments.push((id.to_owned(), images));
                                request.messages.push(c::ChatCompletionRequestMessage::Tool(
                                    c::ChatCompletionRequestToolMessage {
                                        tool_call_id: id.to_owned(),
                                        content: text.into_owned().into(),
                                    },
                                ));
                                pending.remove(id);
                                if pending.is_empty() {
                                    super::tool_images::append_chat(
                                        &mut request.messages,
                                        std::mem::take(&mut attachments),
                                    );
                                }
                            }
                        }
                        Ok(())
                    })?;
                    batch.flush(&mut request.messages);
                }
            }
            Ok(target)
        }
    }
}

fn easy_message(role: r::Role, text: String) -> r::InputItem {
    r::InputItem::EasyMessage(r::EasyInputMessage {
        role,
        content: text.into(),
        ..Default::default()
    })
}

fn response_item(item: PortableItem<'_>) -> r::InputItem {
    match item {
        PortableItem::User(parts) => r::InputItem::EasyMessage(r::EasyInputMessage {
            role: r::Role::User,
            content: r::EasyInputContent::ContentList(crate::user_input::response_parts(&parts)),
            ..Default::default()
        }),
        PortableItem::Message(role, text) => easy_message(role, text.into_owned()),
        PortableItem::Call {
            id,
            name,
            arguments,
        } => r::InputItem::Item(r::Item::FunctionCall(r::FunctionToolCall {
            call_id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
            id: None,
            status: None,
            namespace: None,
            caller: None,
            r#async: None,
        })),
        PortableItem::Output { id, text, images } => r::InputItem::Item(
            r::Item::FunctionCallOutput(r::FunctionCallOutputItemParam {
                call_id: Some(id.to_owned()),
                output: super::tool_images::response_output(text.into_owned(), images),
                id: None,
                status: None,
                name: None,
                namespace: None,
                caller: None,
            }),
        ),
    }
}

macro_rules! text_content {
    ($function:ident, $content:ident, $part:ident) => {
        fn $function(content: &c::$content) -> String {
            match content {
                c::$content::Text(text) => text.clone(),
                c::$content::Array(parts) => parts
                    .iter()
                    .map(|part| {
                        let c::$part::Text(text) = part;
                        text.text.as_str()
                    })
                    .collect(),
            }
        }
    };
}
text_content!(
    system_text,
    ChatCompletionRequestSystemMessageContent,
    ChatCompletionRequestSystemMessageContentPart
);
text_content!(
    developer_text,
    ChatCompletionRequestDeveloperMessageContent,
    ChatCompletionRequestDeveloperMessageContentPart
);
text_content!(
    tool_text,
    ChatCompletionRequestToolMessageContent,
    ChatCompletionRequestToolMessageContentPart
);

#[allow(deprecated)]
fn completion_items(
    message: &c::ChatCompletionRequestMessage,
    input: &mut Vec<r::InputItem>,
) -> Result<()> {
    use c::ChatCompletionRequestMessage as Message;
    match message {
        Message::System(message) => {
            input.push(easy_message(r::Role::System, system_text(&message.content)))
        }
        Message::Developer(message) => input.push(easy_message(
            r::Role::Developer,
            developer_text(&message.content),
        )),
        Message::User(message) => {
            let parts = match &message.content {
                c::ChatCompletionRequestUserMessageContent::Text(text) => {
                    vec![crate::user_input::Part::Text(text.clone())]
                }
                c::ChatCompletionRequestUserMessageContent::Array(parts) => parts
                    .iter()
                    .map(|part| {
                        Ok(match part {
                            c::ChatCompletionRequestUserMessageContentPart::Text(part) => {
                                crate::user_input::Part::Text(part.text.clone())
                            }
                            c::ChatCompletionRequestUserMessageContentPart::ImageUrl(part) => {
                                ensure!(part.image_url.url.starts_with("data:image/"), UNSUPPORTED);
                                crate::user_input::Part::Image(part.image_url.url.as_str().into())
                            }
                            _ => bail!(UNSUPPORTED),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };
            input.push(response_item(portable_user(parts)));
        }

        Message::Assistant(message) => {
            ensure!(
                message.audio.is_none() && message.function_call.is_none(),
                UNSUPPORTED
            );
            let mut text = match &message.content {
                Some(c::ChatCompletionRequestAssistantMessageContent::Text(text)) => {
                    Some(text.clone())
                }
                Some(c::ChatCompletionRequestAssistantMessageContent::Array(parts)) => Some(
                    parts
                        .iter()
                        .map(|part| match part {
                            c::ChatCompletionRequestAssistantMessageContentPart::Text(part) => {
                                part.text.as_str()
                            }
                            c::ChatCompletionRequestAssistantMessageContentPart::Refusal(part) => {
                                part.refusal.as_str()
                            }
                        })
                        .collect(),
                ),
                None => None,
            };
            if let Some(refusal) = &message.refusal {
                text.get_or_insert_with(String::new).push_str(refusal);
            }
            if let Some(text) = text {
                input.push(easy_message(r::Role::Assistant, text));
            }
            for call in message.tool_calls.iter().flatten() {
                let c::ChatCompletionMessageToolCalls::Function(call) = call else {
                    bail!(UNSUPPORTED)
                };
                ensure!(!call.id.is_empty(), UNSUPPORTED);
                input.push(response_item(PortableItem::Call {
                    id: &call.id,
                    name: &call.function.name,
                    arguments: &call.function.arguments,
                }));
            }
        }
        Message::Tool(message) => {
            ensure!(!message.tool_call_id.is_empty(), UNSUPPORTED);
            input.push(response_item(PortableItem::Output {
                id: &message.tool_call_id,
                text: Cow::Owned(tool_text(&message.content)),
                images: Vec::new(),
            }));
        }
        Message::Function(_) => bail!(UNSUPPORTED),
    }
    Ok(())
}

fn portable_user(parts: Vec<crate::user_input::Part>) -> PortableItem<'static> {
    if parts
        .iter()
        .any(|p| matches!(p, crate::user_input::Part::Image(_)))
    {
        PortableItem::User(parts)
    } else {
        PortableItem::Message(
            r::Role::User,
            Cow::Owned(
                parts
                    .into_iter()
                    .filter_map(|p| {
                        if let crate::user_input::Part::Text(text) = p {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .collect(),
            ),
        )
    }
}

fn user_parts(content: &[r::InputContent]) -> Result<Vec<crate::user_input::Part>> {
    content
        .iter()
        .map(|part| {
            Ok(match part {
                r::InputContent::InputText(part) => {
                    crate::user_input::Part::Text(part.text.clone())
                }
                r::InputContent::InputImage(part) => {
                    ensure!(part.file_id.is_none(), UNSUPPORTED);
                    let url = part
                        .image_url
                        .as_ref()
                        .filter(|url| url.starts_with("data:image/"))
                        .ok_or_else(|| anyhow::anyhow!(UNSUPPORTED))?;
                    crate::user_input::Part::Image(url.as_str().into())
                }
                _ => bail!(UNSUPPORTED),
            })
        })
        .collect()
}

fn input_text(content: &[r::InputContent]) -> Result<String> {
    let mut text = String::new();
    for part in content {
        let r::InputContent::InputText(part) = part else {
            bail!(UNSUPPORTED)
        };
        text.push_str(&part.text);
    }
    Ok(text)
}

fn easy_text(content: &r::EasyInputContent) -> Result<Cow<'_, str>> {
    match content {
        r::EasyInputContent::Text(text) => Ok(Cow::Borrowed(text)),
        r::EasyInputContent::ContentList(parts) => Ok(Cow::Owned(input_text(parts)?)),
    }
}

fn visit_response(
    source: &r::CreateResponse,
    mut visit: impl FnMut(PortableItem<'_>) -> Result<()>,
) -> Result<()> {
    let items = match &source.input {
        r::InputParam::Text(text) => {
            return visit(PortableItem::Message(r::Role::User, Cow::Borrowed(text)));
        }
        r::InputParam::Items(items) => items,
    };
    for item in items {
        let item = match item {
            r::InputItem::EasyMessage(message) => {
                if message.role == r::Role::User {
                    match &message.content {
                        r::EasyInputContent::Text(text) => {
                            PortableItem::Message(message.role, Cow::Borrowed(text))
                        }
                        r::EasyInputContent::ContentList(parts) => {
                            portable_user(user_parts(parts)?)
                        }
                    }
                } else {
                    PortableItem::Message(message.role, easy_text(&message.content)?)
                }
            }
            r::InputItem::Item(r::Item::Message(r::MessageItem::Input(message))) => {
                let role = match message.role {
                    r::InputRole::User => r::Role::User,
                    r::InputRole::System => r::Role::System,
                    r::InputRole::Developer => r::Role::Developer,
                };
                if role == r::Role::User {
                    portable_user(user_parts(&message.content)?)
                } else {
                    PortableItem::Message(role, Cow::Owned(input_text(&message.content)?))
                }
            }
            r::InputItem::Item(r::Item::Message(r::MessageItem::Output(message))) => {
                let text = message
                    .content
                    .iter()
                    .map(|part| match part {
                        r::OutputMessageContent::OutputText(part) => part.text.as_str(),
                        r::OutputMessageContent::Refusal(part) => part.refusal.as_str(),
                    })
                    .collect();
                PortableItem::Message(r::Role::Assistant, Cow::Owned(text))
            }
            r::InputItem::Item(r::Item::Reasoning(reasoning)) => {
                let summary = reasoning.summary.iter().map(|part| {
                    let r::SummaryPart::SummaryText(part) = part;
                    part.text.as_str()
                });
                let content = reasoning.content.iter().flatten().map(|part| {
                    let r::ReasoningItemContent::ReasoningText(part) = part;
                    part.text.as_str()
                });
                let mut text = String::new();
                let mut has_text = false;
                for (index, part) in summary.chain(content).enumerate() {
                    if index > 0 {
                        text.push('\n');
                    }
                    text.push_str(part);
                    has_text |= !part.is_empty();
                }
                if !has_text {
                    continue;
                }
                PortableItem::Message(r::Role::Assistant, Cow::Owned(text))
            }
            r::InputItem::Item(r::Item::FunctionCall(call)) => {
                ensure!(!call.call_id.is_empty(), UNSUPPORTED);
                PortableItem::Call {
                    id: &call.call_id,
                    name: &call.name,
                    arguments: &call.arguments,
                }
            }
            r::InputItem::Item(r::Item::FunctionCallOutput(output)) => {
                let Some(id) = output.call_id.as_deref().filter(|id| !id.is_empty()) else {
                    bail!(UNSUPPORTED)
                };
                let (text, images) = match &output.output {
                    r::FunctionCallOutput::Text(text) => (Cow::Borrowed(text.as_str()), Vec::new()),
                    r::FunctionCallOutput::Content(parts) => {
                        let mut text = String::new();
                        let mut images = Vec::new();
                        for part in parts {
                            match part {
                                r::InputContent::InputText(part) => text.push_str(&part.text),
                                r::InputContent::InputImage(part) => {
                                    let Some(url) = part
                                        .image_url
                                        .as_ref()
                                        .filter(|url| url.starts_with("data:image/"))
                                    else {
                                        bail!(UNSUPPORTED);
                                    };
                                    ensure!(part.file_id.is_none(), UNSUPPORTED);
                                    images.push(crate::tools::read::ToolImage {
                                        data_url: url.clone(),
                                    });
                                }
                                _ => bail!(UNSUPPORTED),
                            }
                        }
                        (Cow::Owned(text), images)
                    }
                };
                PortableItem::Output { id, text, images }
            }
            _ => bail!(UNSUPPORTED),
        };
        visit(item)?;
    }
    Ok(())
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

fn completion_message(role: r::Role, text: String) -> c::ChatCompletionRequestMessage {
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

#[cfg(test)]
mod tests;
