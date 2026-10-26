//! 工具图片在原生协议中的适配。Chat 的附件必须放在完整结果批次之后。
use crate::tools::read::ToolImage;
use anyhow::{Result, bail, ensure};
use async_openai::types::{chat as c, responses as r};

const PREFIX: &str = "pp.read_image:";

pub(super) fn response_output(text: String, images: Vec<ToolImage>) -> r::FunctionCallOutput {
    if images.is_empty() {
        return r::FunctionCallOutput::Text(text);
    }
    let mut parts = vec![r::InputContent::InputText(r::InputTextContent {
        text,
        prompt_cache_breakpoint: None,
    })];
    parts.extend(images.into_iter().map(|image| {
        r::InputContent::InputImage(r::InputImageContent {
            image_url: Some(image.data_url),
            ..Default::default()
        })
    }));
    r::FunctionCallOutput::Content(parts)
}

pub(super) fn append_chat(
    messages: &mut Vec<c::ChatCompletionRequestMessage>,
    images: Vec<(String, Vec<ToolImage>)>,
) {
    for (id, images) in images {
        if images.is_empty() {
            continue;
        }
        let mut parts = vec![c::ChatCompletionRequestUserMessageContentPart::Text(
            c::ChatCompletionRequestMessageContentPartText {
                text: format!("{PREFIX}{id}"),
                prompt_cache_breakpoint: None,
            },
        )];
        parts.extend(images.into_iter().map(|image| {
            c::ChatCompletionRequestUserMessageContentPart::ImageUrl(
                c::ChatCompletionRequestMessageContentPartImage {
                    image_url: c::ImageUrl {
                        url: image.data_url,
                        detail: None,
                    },
                    prompt_cache_breakpoint: None,
                },
            )
        }));
        messages.push(c::ChatCompletionRequestMessage::User(
            c::ChatCompletionRequestUserMessage {
                content: c::ChatCompletionRequestUserMessageContent::Array(parts),
                ..Default::default()
            },
        ));
    }
}

pub(super) fn chat_id(message: &c::ChatCompletionRequestMessage) -> Option<&str> {
    let c::ChatCompletionRequestMessage::User(message) = message else {
        return None;
    };
    let c::ChatCompletionRequestUserMessageContent::Array(parts) = &message.content else {
        return None;
    };
    let Some(c::ChatCompletionRequestUserMessageContentPart::Text(first)) = parts.first() else {
        return None;
    };
    first.text.strip_prefix(PREFIX).filter(|id| !id.is_empty())
}

pub(super) fn chat_images(message: &c::ChatCompletionRequestMessage) -> Result<Vec<ToolImage>> {
    let c::ChatCompletionRequestMessage::User(message) = message else {
        bail!("invalid read image attachment");
    };
    let c::ChatCompletionRequestUserMessageContent::Array(parts) = &message.content else {
        bail!("invalid read image attachment");
    };
    ensure!(parts.len() > 1, "empty read image attachment");
    parts[1..]
        .iter()
        .map(|part| {
            let c::ChatCompletionRequestUserMessageContentPart::ImageUrl(image) = part else {
                bail!("invalid read image attachment");
            };
            ensure!(
                image.image_url.url.starts_with("data:image/"),
                "invalid read image attachment"
            );
            Ok(ToolImage {
                data_url: image.image_url.url.clone(),
            })
        })
        .collect()
}

/// 只认可紧接完整 read 工具批次、成功结果和唯一 call ID 的内部附件。
pub(super) fn validate_chat(messages: &[c::ChatCompletionRequestMessage]) -> Result<()> {
    let mut attached = std::collections::HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        let Some(id) = chat_id(message) else {
            continue;
        };
        ensure!(attached.insert(id), "duplicate read image attachment");
        let mut cursor = index;
        while cursor > 0 && chat_id(&messages[cursor - 1]).is_some() {
            cursor -= 1;
        }
        let mut found = false;
        while cursor > 0 {
            let c::ChatCompletionRequestMessage::Tool(tool) = &messages[cursor - 1] else {
                break;
            };
            if tool.tool_call_id == id {
                let c::ChatCompletionRequestToolMessageContent::Text(text) = &tool.content else {
                    bail!("invalid read image result");
                };
                let result: serde_json::Value = serde_json::from_str(text)
                    .map_err(|_| anyhow::anyhow!("invalid read image result"))?;
                ensure!(
                    result["ok"] == true && result["kind"] == "image",
                    "invalid read image result"
                );
                found = true;
            }
            cursor -= 1;
        }
        let Some(c::ChatCompletionRequestMessage::Assistant(assistant)) =
            cursor.checked_sub(1).map(|i| &messages[i])
        else {
            bail!("orphan read image attachment");
        };
        ensure!(found && assistant.tool_calls.iter().flatten().any(|call| matches!(call, c::ChatCompletionMessageToolCalls::Function(call) if call.id == id && call.function.name == "read")), "orphan read image attachment");
        // 不克隆载荷，仅校验数据形状。
        let c::ChatCompletionRequestMessage::User(user) = message else {
            unreachable!()
        };
        let c::ChatCompletionRequestUserMessageContent::Array(parts) = &user.content else {
            unreachable!()
        };
        ensure!(
            parts.len() == 2
                && matches!(&parts[1], c::ChatCompletionRequestUserMessageContentPart::ImageUrl(image) if image.image_url.url.starts_with("data:image/")),
            "invalid read image attachment"
        );
    }
    Ok(())
}

pub(super) fn merge_chat(
    input: &mut [r::InputItem],
    message: &c::ChatCompletionRequestMessage,
) -> Result<bool> {
    let Some(id) = chat_id(message) else {
        return Ok(false);
    };
    let images = chat_images(message)?;
    let output = input
        .iter_mut()
        .rev()
        .find_map(|item| match item {
            r::InputItem::Item(r::Item::FunctionCallOutput(output))
                if output.call_id.as_deref() == Some(id) =>
            {
                Some(output)
            }
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("orphan read image attachment"))?;
    let r::FunctionCallOutput::Text(text) = &mut output.output else {
        bail!("duplicate read image attachment");
    };
    output.output = response_output(std::mem::take(text), images);
    Ok(true)
}
