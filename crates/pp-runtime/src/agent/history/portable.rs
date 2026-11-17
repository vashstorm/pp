use super::UNSUPPORTED;
use anyhow::{Result, bail, ensure};
use async_openai::types::responses as r;
use std::borrow::Cow;

// These borrowed views exist only during a switch, never as a second session history.
pub(super) enum PortableItem<'a> {
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
        images: Vec<pp_tools::read::ToolImage>,
    },
}

pub(super) fn easy_message(role: r::Role, text: String) -> r::InputItem {
    r::InputItem::EasyMessage(r::EasyInputMessage {
        role,
        content: text.into(),
        ..Default::default()
    })
}

pub(super) fn response_item(item: PortableItem<'_>) -> r::InputItem {
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
                output: super::super::tool_images::response_output(text.into_owned(), images),
                id: None,
                status: None,
                name: None,
                namespace: None,
                caller: None,
            }),
        ),
    }
}

pub(super) fn portable_user(parts: Vec<crate::user_input::Part>) -> PortableItem<'static> {
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

pub(super) fn visit_response(
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
        if let Some(portable) = parse_input_item(item)? {
            visit(portable)?;
        }
    }
    Ok(())
}

fn parse_input_item(item: &r::InputItem) -> Result<Option<PortableItem<'_>>> {
    match item {
        r::InputItem::EasyMessage(message) => parse_easy_message(message).map(Some),
        r::InputItem::Item(r::Item::Message(r::MessageItem::Input(message))) => {
            parse_input_message(message).map(Some)
        }
        r::InputItem::Item(r::Item::Message(r::MessageItem::Output(message))) => {
            Ok(Some(parse_output_message(message)))
        }
        r::InputItem::Item(r::Item::Reasoning(reasoning)) => Ok(parse_reasoning(reasoning)),
        r::InputItem::Item(r::Item::FunctionCall(call)) => {
            ensure!(!call.call_id.is_empty(), UNSUPPORTED);
            Ok(Some(PortableItem::Call {
                id: &call.call_id,
                name: &call.name,
                arguments: &call.arguments,
            }))
        }
        r::InputItem::Item(r::Item::FunctionCallOutput(output)) => {
            parse_function_output(output).map(Some)
        }
        _ => bail!(UNSUPPORTED),
    }
}

fn parse_easy_message(message: &r::EasyInputMessage) -> Result<PortableItem<'_>> {
    if message.role == r::Role::User {
        match &message.content {
            r::EasyInputContent::Text(text) => {
                Ok(PortableItem::Message(message.role, Cow::Borrowed(text)))
            }
            r::EasyInputContent::ContentList(parts) => Ok(portable_user(user_parts(parts)?)),
        }
    } else {
        Ok(PortableItem::Message(
            message.role,
            easy_text(&message.content)?,
        ))
    }
}

fn parse_input_message(message: &r::InputMessage) -> Result<PortableItem<'_>> {
    let role = match message.role {
        r::InputRole::User => r::Role::User,
        r::InputRole::System => r::Role::System,
        r::InputRole::Developer => r::Role::Developer,
    };
    if role == r::Role::User {
        Ok(portable_user(user_parts(&message.content)?))
    } else {
        Ok(PortableItem::Message(
            role,
            Cow::Owned(input_text(&message.content)?),
        ))
    }
}

fn parse_output_message(message: &r::OutputMessage) -> PortableItem<'static> {
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

fn parse_reasoning(reasoning: &r::ReasoningItem) -> Option<PortableItem<'static>> {
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
    has_text.then_some(PortableItem::Message(r::Role::Assistant, Cow::Owned(text)))
}

fn parse_function_output(output: &r::FunctionCallOutputItemParam) -> Result<PortableItem<'_>> {
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
                        images.push(pp_tools::read::ToolImage {
                            data_url: url.clone(),
                        });
                    }
                    _ => bail!(UNSUPPORTED),
                }
            }
            (Cow::Owned(text), images)
        }
    };
    Ok(PortableItem::Output { id, text, images })
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
