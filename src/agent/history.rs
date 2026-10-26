use super::Session;
use crate::config::EditMode;
use crate::models::Protocol;
use anyhow::{Result, bail, ensure};
use async_openai::types::{chat as c, responses as r};
use std::borrow::Cow;

const UNSUPPORTED: &str = "cannot switch model: unsupported history item";

// These borrowed views exist only during a switch, never as a second session history.
enum PortableItem<'a> {
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
            let text = match &message.content {
                c::ChatCompletionRequestUserMessageContent::Text(text) => text.clone(),
                c::ChatCompletionRequestUserMessageContent::Array(parts) => {
                    let mut text = String::new();
                    for part in parts {
                        let c::ChatCompletionRequestUserMessageContentPart::Text(part) = part
                        else {
                            bail!(UNSUPPORTED);
                        };
                        text.push_str(&part.text);
                    }
                    text
                }
            };
            input.push(easy_message(r::Role::User, text));
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
                PortableItem::Message(message.role, easy_text(&message.content)?)
            }
            r::InputItem::Item(r::Item::Message(r::MessageItem::Input(message))) => {
                let role = match message.role {
                    r::InputRole::User => r::Role::User,
                    r::InputRole::System => r::Role::System,
                    r::InputRole::Developer => r::Role::Developer,
                };
                PortableItem::Message(role, Cow::Owned(input_text(&message.content)?))
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
mod tests {
    use super::*;

    #[test]
    fn read_image_history_roundtrip_keeps_attachment_in_complete_exchange() {
        let image_result =
            serde_json::json!({"ok":true,"kind":"image","content":"Image fixture"}).to_string();
        let image = response_item(PortableItem::Output {
            id: "image",
            text: Cow::Owned(image_result),
            images: vec![crate::tools::read::ToolImage {
                data_url: "data:image/png;base64,AAAA".into(),
            }],
        });
        let source = response(vec![
            easy_message(r::Role::User, "look".into()),
            call("image"),
            call("text"),
            image,
            output("text"),
            easy_message(r::Role::Assistant, "done".into()),
        ]);
        let chat = convert(
            &source,
            "chat".into(),
            Protocol::Completion,
            EditMode::Hashline,
        )
        .unwrap();
        let Session::Completion(request) = &chat else {
            panic!()
        };
        super::super::tool_images::validate_chat(&request.messages).unwrap();
        assert!(matches!(
            request.messages[3],
            c::ChatCompletionRequestMessage::Tool(_)
        ));
        assert!(matches!(
            request.messages[4],
            c::ChatCompletionRequestMessage::Tool(_)
        ));
        assert_eq!(
            super::super::tool_images::chat_id(&request.messages[5]),
            Some("image")
        );
        assert_eq!(
            super::super::context::completion_boundaries(&request.messages).unwrap(),
            vec![6, 7]
        );
        let roundtrip = convert(
            &chat,
            "response".into(),
            Protocol::Response,
            EditMode::Replace,
        )
        .unwrap();
        let Session::Response(request) = &roundtrip else {
            panic!()
        };
        let r::InputParam::Items(items) = &request.input else {
            panic!()
        };
        assert_eq!(
            super::super::context::response_boundaries(items).unwrap(),
            vec![5, 6]
        );
        let r::InputItem::Item(r::Item::FunctionCallOutput(output)) = &items[3] else {
            panic!()
        };
        let r::FunctionCallOutput::Content(parts) = &output.output else {
            panic!()
        };
        assert!(
            matches!(&parts[1], r::InputContent::InputImage(image) if image.image_url.as_deref() == Some("data:image/png;base64,AAAA"))
        );
        // 序列化恢复后仍能转换，不重新读取任何路径。
        let restored: Session =
            serde_json::from_str(&serde_json::to_string(&roundtrip).unwrap()).unwrap();
        assert!(
            convert(
                &restored,
                "again".into(),
                Protocol::Completion,
                EditMode::Replace
            )
            .is_ok()
        );
    }

    fn response(items: Vec<r::InputItem>) -> Session {
        let mut session = Session::new(
            "old".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        );
        let Session::Response(request) = &mut session else {
            unreachable!()
        };
        request.input = r::InputParam::Items(items);
        session
    }

    fn call(id: &str) -> r::InputItem {
        r::InputItem::Item(r::Item::FunctionCall(r::FunctionToolCall {
            call_id: id.into(),
            name: "read".into(),
            arguments: "{ \"path\" : \"notes\" }".into(),
            id: Some(format!("provider-{id}")),
            status: Some(r::OutputStatus::Completed),
            namespace: Some("provider-only".into()),
            caller: None,
            r#async: Some(false),
        }))
    }

    fn output(id: &str) -> r::InputItem {
        response_item(PortableItem::Output {
            id,
            text: Cow::Borrowed("original result"),
            images: Vec::new(),
        })
    }

    fn text_part(text: &str) -> r::InputContent {
        r::InputContent::InputText(r::InputTextContent {
            text: text.into(),
            prompt_cache_breakpoint: None,
        })
    }

    #[test]
    fn model_switch_response_batches_interleaved_text_refusal_and_calls() {
        let message =
            r::InputItem::Item(r::Item::Message(r::MessageItem::Output(r::OutputMessage {
                content: vec![
                    r::OutputMessageContent::OutputText(r::OutputTextContent {
                        annotations: Vec::new(),
                        logprobs: None,
                        text: "commentary".into(),
                    }),
                    r::OutputMessageContent::Refusal(r::RefusalContent {
                        refusal: "; refusal".into(),
                    }),
                ],
                id: "provider-message".into(),
                role: r::AssistantRole::Assistant,
                phase: None,
                status: r::OutputStatus::Completed,
            })));
        let source = response(vec![
            easy_message(r::Role::User, "user".into()),
            message,
            call("a"),
            easy_message(r::Role::Assistant, "between calls".into()),
            call("b"),
            output("a"),
            output("b"),
            call("c"),
            output("c"),
            easy_message(r::Role::Assistant, "final".into()),
        ]);
        let Session::Completion(request) = convert(
            &source,
            "new".into(),
            Protocol::Completion,
            EditMode::Replace,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(request.messages.len(), 8);
        let c::ChatCompletionRequestMessage::Assistant(batch) = &request.messages[2] else {
            panic!()
        };
        assert_eq!(
            batch.content,
            Some(c::ChatCompletionRequestAssistantMessageContent::Text(
                "commentary; refusal\nbetween calls".into()
            ))
        );
        let calls = batch.tool_calls.as_ref().unwrap();
        for (call, id) in calls.iter().zip(["a", "b"]) {
            let c::ChatCompletionMessageToolCalls::Function(call) = call else {
                panic!()
            };
            assert_eq!(call.id, id);
            assert_eq!(call.function.arguments, "{ \"path\" : \"notes\" }");
        }
        assert_eq!(calls.len(), 2);
        let c::ChatCompletionRequestMessage::Assistant(batch) = &request.messages[5] else {
            panic!()
        };
        assert!(batch.content.is_none());
        assert_eq!(batch.tool_calls.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn model_switch_rejects_nontext_or_missing_call_id_without_mutating_source() {
        let source = response(vec![
            easy_message(r::Role::User, "preserved".into()),
            r::InputItem::EasyMessage(r::EasyInputMessage {
                role: r::Role::User,
                content: r::EasyInputContent::ContentList(vec![
                    text_part("text"),
                    r::InputContent::InputImage(r::InputImageContent::default()),
                ]),
                ..Default::default()
            }),
        ]);
        let Session::Response(original) = &source else {
            panic!()
        };
        let snapshot = original.clone();
        for protocol in [Protocol::Completion, Protocol::Response] {
            let error = convert(&source, "new".into(), protocol, EditMode::Replace)
                .err()
                .unwrap();
            assert_eq!(error.to_string(), UNSUPPORTED);
            let Session::Response(original) = &source else {
                panic!()
            };
            assert_eq!(original, &snapshot);
        }
        let mut invalid = output("a");
        let r::InputItem::Item(r::Item::FunctionCallOutput(output)) = &mut invalid else {
            panic!()
        };
        output.call_id = None;
        assert_eq!(
            convert(
                &response(vec![invalid]),
                "new".into(),
                Protocol::Completion,
                EditMode::Replace,
            )
            .err()
            .unwrap()
            .to_string(),
            UNSUPPORTED
        );
    }

    #[test]
    fn model_switch_portable_response_preserves_order_and_strips_replay_metadata() {
        let reasoning = r::InputItem::Item(r::Item::Reasoning(r::ReasoningItem {
            id: Some("provider-reasoning".into()),
            summary: vec![r::SummaryPart::SummaryText(r::SummaryTextContent {
                text: "summary".into(),
            })],
            content: Some(vec![r::ReasoningItemContent::ReasoningText(
                r::ReasoningTextContent {
                    text: "readable".into(),
                },
            )]),
            encrypted_content: Some("opaque".into()),
            status: Some(r::OutputStatus::Completed),
        }));
        let source = response(vec![
            call("a"),
            reasoning,
            call("b"),
            output("a"),
            output("b"),
        ]);
        let Session::Response(request) =
            convert(&source, "new".into(), Protocol::Response, EditMode::Replace).unwrap()
        else {
            panic!()
        };
        let r::InputParam::Items(items) = &request.input else {
            panic!()
        };
        assert_eq!(items.len(), 5);
        assert_eq!(
            items[1],
            easy_message(r::Role::Assistant, "summary\nreadable".into())
        );
        for index in [0, 2] {
            let r::InputItem::Item(r::Item::FunctionCall(call)) = &items[index] else {
                panic!()
            };
            assert!(call.id.is_none() && call.status.is_none() && call.namespace.is_none());
            assert!(call.caller.is_none() && call.r#async.is_none());
        }
        let Session::Completion(request) = convert(
            &Session::Response(request),
            "again".into(),
            Protocol::Completion,
            EditMode::Replace,
        )
        .unwrap() else {
            panic!()
        };
        let c::ChatCompletionRequestMessage::Assistant(batch) = &request.messages[1] else {
            panic!()
        };
        assert_eq!(
            batch.content,
            Some(c::ChatCompletionRequestAssistantMessageContent::Text(
                "summary\nreadable".into()
            ))
        );
        assert_eq!(batch.tool_calls.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn model_switch_text_arrays_and_tool_arguments_survive_round_trip() {
        let mut source = Session::new(
            "old".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        );
        let Session::Completion(request) = &mut source else {
            panic!()
        };
        let text = |value: &str| c::ChatCompletionRequestMessageContentPartText {
            text: value.into(),
            prompt_cache_breakpoint: None,
        };
        request.messages = vec![
            c::ChatCompletionRequestMessage::System(c::ChatCompletionRequestSystemMessage {
                content: c::ChatCompletionRequestSystemMessageContent::Array(vec![
                    c::ChatCompletionRequestSystemMessageContentPart::Text(text("sys")),
                    c::ChatCompletionRequestSystemMessageContentPart::Text(text("tem")),
                ]),
                ..Default::default()
            }),
            completion_message(r::Role::Developer, "developer".into()),
            c::ChatCompletionRequestMessage::User(c::ChatCompletionRequestUserMessage {
                content: c::ChatCompletionRequestUserMessageContent::Array(vec![
                    c::ChatCompletionRequestUserMessageContentPart::Text(text("user ")),
                    c::ChatCompletionRequestUserMessageContentPart::Text(text("body")),
                ]),
                ..Default::default()
            }),
            c::ChatCompletionRequestMessage::Assistant(c::ChatCompletionRequestAssistantMessage {
                content: Some(c::ChatCompletionRequestAssistantMessageContent::Array(
                    vec![
                        c::ChatCompletionRequestAssistantMessageContentPart::Text(text(
                            "assistant",
                        )),
                        c::ChatCompletionRequestAssistantMessageContentPart::Refusal(
                            c::ChatCompletionRequestMessageContentPartRefusal {
                                refusal: "; refusal".into(),
                            },
                        ),
                    ],
                )),
                tool_calls: Some(vec![c::ChatCompletionMessageToolCalls::Function(
                    c::ChatCompletionMessageToolCall {
                        id: "call".into(),
                        function: c::FunctionCall {
                            name: "read".into(),
                            arguments: "{ \"path\" : \"notes\" }".into(),
                        },
                    },
                )]),
                ..Default::default()
            }),
            c::ChatCompletionRequestMessage::Tool(c::ChatCompletionRequestToolMessage {
                tool_call_id: "call".into(),
                content: c::ChatCompletionRequestToolMessageContent::Array(vec![
                    c::ChatCompletionRequestToolMessageContentPart::Text(text("tool ")),
                    c::ChatCompletionRequestToolMessageContentPart::Text(text("result")),
                ]),
            }),
            completion_message(r::Role::System, "later system".into()),
        ];
        let response = convert(
            &source,
            "response".into(),
            Protocol::Response,
            EditMode::Replace,
        )
        .unwrap();
        let Session::Response(request) = &response else {
            panic!()
        };
        assert_eq!(request.instructions.as_deref(), Some("system"));
        let r::InputParam::Items(items) = &request.input else {
            panic!()
        };
        assert_eq!(
            items[0],
            easy_message(r::Role::Developer, "developer".into())
        );
        assert_eq!(items[1], easy_message(r::Role::User, "user body".into()));
        assert_eq!(
            items[2],
            easy_message(r::Role::Assistant, "assistant; refusal".into())
        );
        assert_eq!(
            items[4],
            response_item(PortableItem::Output {
                id: "call",
                text: Cow::Borrowed("tool result"),
                images: Vec::new(),
            })
        );
        assert_eq!(
            items[5],
            easy_message(r::Role::System, "later system".into())
        );
        let completion = convert(
            &response,
            "completion".into(),
            Protocol::Completion,
            EditMode::Replace,
        )
        .unwrap();
        let Session::Response(round_trip) = convert(
            &completion,
            "response".into(),
            Protocol::Response,
            EditMode::Replace,
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(round_trip.instructions, request.instructions);
        assert_eq!(round_trip.input, request.input);
    }

    #[test]
    fn model_switch_completion_rejects_nontext_without_mutating_source() {
        let mut source = Session::new(
            "old".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        );
        let Session::Completion(request) = &mut source else {
            panic!()
        };
        request.messages.push(c::ChatCompletionRequestMessage::User(
            c::ChatCompletionRequestUserMessage {
                content: c::ChatCompletionRequestUserMessageContent::Array(vec![
                    c::ChatCompletionRequestUserMessageContentPart::ImageUrl(
                        c::ChatCompletionRequestMessageContentPartImage::default(),
                    ),
                ]),
                ..Default::default()
            },
        ));
        let snapshot = request.clone();
        assert_eq!(
            convert(
                &source,
                "response".into(),
                Protocol::Response,
                EditMode::Replace
            )
            .err()
            .unwrap()
            .to_string(),
            UNSUPPORTED
        );
        let Session::Completion(request) = &source else {
            panic!()
        };
        assert_eq!(request, &snapshot);
    }
}
