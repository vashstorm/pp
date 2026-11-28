use super::*;

#[test]
fn read_image_history_roundtrip_keeps_attachment_in_complete_exchange() {
    let image_result =
        serde_json::json!({"ok":true,"kind":"image","content":"Image fixture"}).to_string();
    let image = response_item(PortableItem::Output {
        id: "image",
        text: Cow::Owned(image_result),
        images: vec![pp_tools::read::ToolImage {
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
    let message = r::InputItem::Item(r::Item::Message(r::MessageItem::Output(r::OutputMessage {
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
fn model_switch_rejects_nontext_and_missing_call_id_without_mutating_source() {
    {
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

    {
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
                    c::ChatCompletionRequestAssistantMessageContentPart::Text(text("assistant")),
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
                        name: "find".into(),
                        arguments: r#"{ "args" : ["."] }"#.into(),
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
