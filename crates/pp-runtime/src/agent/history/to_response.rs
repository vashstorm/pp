use super::UNSUPPORTED;
use super::portable::{PortableItem, easy_message, portable_user, response_item, visit_response};
use crate::agent::Session;
use anyhow::{Result, bail, ensure};
use async_openai::types::{chat as c, responses as r};
use pp_config::config::EditMode;
use pp_config::models::Protocol;
use std::borrow::Cow;

pub(super) fn from_completion(
    source: &c::CreateChatCompletionRequest,
    model: String,
    edit_mode: EditMode,
) -> Result<Session> {
    crate::agent::tool_images::validate_chat(&source.messages)?;
    let (first_system, system) = extract_first_system(&source.messages);
    let mut target = Session::new(model, Protocol::Response, system, edit_mode);
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
        if Some(index) == first_system {
            continue;
        }
        if crate::agent::tool_images::merge_chat(input, message)? {
            continue;
        }
        push_completion_message(message, input)?;
    }
    Ok(target)
}

pub(super) fn from_response(
    source: &r::CreateResponse,
    model: String,
    edit_mode: EditMode,
) -> Result<Session> {
    let mut target = Session::new(
        model,
        Protocol::Response,
        source.instructions.clone().unwrap_or_default(),
        edit_mode,
    );
    let Session::Response(request) = &mut target else {
        unreachable!()
    };
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
    Ok(target)
}

fn extract_first_system(messages: &[c::ChatCompletionRequestMessage]) -> (Option<usize>, String) {
    let first_system = messages
        .iter()
        .position(|message| matches!(message, c::ChatCompletionRequestMessage::System(_)));
    let system = match first_system.map(|index| &messages[index]) {
        Some(c::ChatCompletionRequestMessage::System(message)) => system_text(&message.content),
        _ => String::new(),
    };
    (first_system, system)
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

fn push_completion_message(
    message: &c::ChatCompletionRequestMessage,
    input: &mut Vec<r::InputItem>,
) -> Result<()> {
    use c::ChatCompletionRequestMessage as Message;
    match message {
        Message::System(message) => {
            input.push(easy_message(r::Role::System, system_text(&message.content)));
        }
        Message::Developer(message) => {
            input.push(easy_message(
                r::Role::Developer,
                developer_text(&message.content),
            ));
        }
        Message::User(message) => {
            let parts = completion_user_parts(&message.content)?;
            input.push(response_item(portable_user(parts)));
        }
        Message::Assistant(message) => push_assistant_message(message, input)?,
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

fn completion_user_parts(
    content: &c::ChatCompletionRequestUserMessageContent,
) -> Result<Vec<crate::user_input::Part>> {
    match content {
        c::ChatCompletionRequestUserMessageContent::Text(text) => {
            Ok(vec![crate::user_input::Part::Text(text.clone())])
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
            .collect(),
    }
}

#[allow(deprecated)]
fn push_assistant_message(
    message: &c::ChatCompletionRequestAssistantMessage,
    input: &mut Vec<r::InputItem>,
) -> Result<()> {
    ensure!(
        message.audio.is_none() && message.function_call.is_none(),
        UNSUPPORTED
    );
    let mut text = match &message.content {
        Some(c::ChatCompletionRequestAssistantMessageContent::Text(text)) => Some(text.clone()),
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
    Ok(())
}
