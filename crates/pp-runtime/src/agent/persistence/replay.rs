//! 消息展示快照的共用回放器；展示数据不进入原生请求。
use super::*;
use pp_core::events::OutputStream;
use serde_json::{Value, json};

#[derive(Serialize, Deserialize)]
#[serde(tag = "type")]
enum Part {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        refusal: bool,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String, summary: bool },
    #[serde(rename = "toolCall", rename_all = "camelCase")]
    Call {
        id: String,
        name: String,
        // 保留原文，非对象或畸形参数不能默认为空对象。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw_arguments: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Step {
    Content {
        index: usize,
    },
    Finish {
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    Event {
        index: usize,
    },
    Result {
        index: usize,
    },
    Output {
        id: String,
        stream: OutputStream,
        text: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Meta {
    attempt: String,
    order: Vec<Step>,
}

#[derive(Default)]
pub(super) struct Capture {
    attempt: Option<String>,
    content: Vec<Part>,
    order: Vec<Step>,
    results: Vec<Value>,
    retained: usize,
    finished: bool,
}

impl Capture {
    pub fn start(&mut self, id: String) {
        *self = Self {
            attempt: Some(id),
            ..Self::default()
        };
    }

    pub fn active(&self) -> bool {
        self.attempt.is_some()
    }

    /// 返回是否由 message 投影代替本条事件；调用方仍先完成统计等副作用。
    pub fn event(&mut self, event: &AgentEvent) -> bool {
        if !self.active() {
            return false;
        }
        match event {
            AgentEvent::AssistantDelta { text, refusal } => {
                self.part(Part::Text {
                    text: text.clone(),
                    refusal: *refusal,
                });
            }
            AgentEvent::ThinkingDelta { text } => {
                self.part(Part::Thinking {
                    thinking: text.clone(),
                    summary: false,
                });
            }
            AgentEvent::ReasoningSummaryDelta { text } => {
                self.part(Part::Thinking {
                    thinking: text.clone(),
                    summary: true,
                });
            }
            AgentEvent::AssistantFinished { text } => {
                if self.text().is_empty() && !text.is_empty() {
                    self.part(Part::Text {
                        text: text.clone(),
                        refusal: false,
                    });
                }
                let displayed = self.text();
                self.order.push(Step::Finish {
                    text: (displayed != *text).then(|| text.clone()),
                });
                self.finished = true;
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                self.part(Part::Call {
                    id: id.clone(),
                    name: name.clone(),
                    raw_arguments: Some(arguments.clone()),
                    arguments: None,
                });
            }
            AgentEvent::ToolFinished { id, result } => {
                // 最终结果取代流式工具输出；没有结果的输出继续留在失败快照中。
                self.order
                    .retain(|step| !matches!(step, Step::Output { id: call, .. } if call == id));
                self.order.push(Step::Result {
                    index: self.results.len(),
                });
                let name = self.content.iter().rev().find_map(|part| match part {
                    Part::Call { id: call, name, .. } if call == id => Some(name.as_str()),
                    _ => None,
                });
                self.results
                    .push(json!({"role":"toolResult", "toolCallId":id,
                    "toolName":name, "content":[{"type":"text","text":result.to_string()}],
                    "isError":result.get("ok").and_then(Value::as_bool) == Some(false)}));
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                if let Some(Step::Output {
                    id: previous,
                    stream: previous_stream,
                    text: output,
                }) = self.order.last_mut()
                    && previous == id
                    && std::mem::discriminant(previous_stream) == std::mem::discriminant(stream)
                {
                    output.push_str(text);
                } else {
                    self.order.push(Step::Output {
                        id: id.clone(),
                        stream: *stream,
                        text: text.clone(),
                    });
                }
            }
            _ => {
                self.order.push(Step::Event {
                    index: self.retained,
                });
                self.retained += 1;
                return false;
            }
        }
        true
    }

    fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                Part::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn part(&mut self, part: Part) {
        if matches!(&part, Part::Text { text, .. } if text.is_empty())
            || matches!(&part, Part::Thinking { thinking, .. } if thinking.is_empty())
        {
            return;
        }
        if let Some(Step::Content { index }) = self.order.last()
            && let Some(previous) = self.content.get_mut(*index)
        {
            match (previous, &part) {
                (
                    Part::Text { text, refusal },
                    Part::Text {
                        text: value,
                        refusal: other,
                    },
                ) if refusal == other => {
                    text.push_str(value);
                    return;
                }
                (
                    Part::Thinking { thinking, summary },
                    Part::Thinking {
                        thinking: value,
                        summary: other,
                    },
                ) if summary == other => {
                    thinking.push_str(value);
                    return;
                }
                _ => {}
            }
        }
        self.order.push(Step::Content {
            index: self.content.len(),
        });
        self.content.push(part);
    }

    pub fn messages(&mut self, reason: &str, model: &str, completion: bool) -> Result<Vec<Value>> {
        let capture = std::mem::take(self);
        if capture.content.is_empty() && capture.results.is_empty() && !capture.finished {
            return Ok(Vec::new());
        }
        let timestamp = now_ms()?;
        let meta = Meta {
            attempt: capture.attempt.context("missing replay attempt")?,
            order: capture.order,
        };
        let mut messages = vec![json!({"role":"assistant", "content":capture.content,
            "api":if completion { "openai-completions" } else { "openai-responses" },
            "provider":model.split_once('/').map_or(model, |(provider, _)| provider),
            "model":model, "stopReason":reason,"timestamp":timestamp,"ppReplay":meta})];
        for mut result in capture.results {
            result["timestamp"] = timestamp.into();
            messages.push(result);
        }
        Ok(messages)
    }
}

#[derive(Default)]
pub(super) struct Projector {
    active: bool,
    retained: Vec<AgentEvent>,
    messages: Vec<Value>,
    records: Vec<ReplayRecord>,
}

impl Projector {
    pub fn feed(&mut self, payload: &Payload, line: &[u8]) -> Result<()> {
        match payload {
            Payload::Custom {
                custom: Custom::State(_),
            } => {}
            Payload::Message {
                message: SavedMessage::User { content, .. },
            } => {
                self.flush(false)?;
                self.records.push(ReplayRecord::User(content.clone()));
            }
            Payload::Message { .. } => {
                #[derive(Deserialize)]
                struct Record {
                    message: Value,
                }
                let record: Record = serde_json::from_slice(line)
                    .map_err(|_| anyhow::anyhow!("invalid replay message"))?;
                self.messages.push(record.message);
            }
            Payload::Custom {
                custom: Custom::Event { event },
            } => {
                if matches!(
                    event,
                    AgentEvent::AssistantStarted | AgentEvent::ModelAttemptFailed { .. }
                ) {
                    self.flush(matches!(event, AgentEvent::ModelAttemptFailed { .. }))?;
                    self.active = matches!(event, AgentEvent::AssistantStarted);
                    self.records.push(ReplayRecord::Event(event.clone()));
                } else if self.active {
                    self.retained.push(event.clone());
                } else {
                    self.records.push(ReplayRecord::Event(event.clone()));
                }
            }
            Payload::Custom {
                custom: Custom::NativeAppend(append),
            } => {
                self.restore_arguments(append);
                self.flush(true)?;
            }
            Payload::Custom {
                custom: Custom::Error { message },
            } => {
                self.flush(true)?;
                self.records.push(ReplayRecord::Error(message.clone()));
            }
            Payload::Custom {
                custom: Custom::Cancelled {},
            } => {
                self.flush(true)?;
                self.records.push(ReplayRecord::Cancelled);
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<ReplayRecord>> {
        self.flush(false)?;
        Ok(self.records)
    }

    fn restore_arguments(&mut self, append: &NativeAppend) {
        let mut arguments = HashMap::new();
        match append {
            NativeAppend::Completion { messages } => {
                for message in messages {
                    if let async_openai::types::chat::ChatCompletionRequestMessage::Assistant(
                        message,
                    ) = message
                    {
                        for call in message.tool_calls.iter().flatten() {
                            if let async_openai::types::chat::ChatCompletionMessageToolCalls::Function(call) = call {
                                arguments.insert(call.id.as_str(), call.function.arguments.as_str());
                            }
                        }
                    }
                }
            }
            NativeAppend::Response { items } => {
                for item in items {
                    if let async_openai::types::responses::InputItem::Item(
                        async_openai::types::responses::Item::FunctionCall(call),
                    ) = item
                    {
                        arguments.insert(call.call_id.as_str(), call.arguments.as_str());
                    }
                }
            }
        }
        for message in &mut self.messages {
            for part in message["content"].as_array_mut().into_iter().flatten() {
                if part["type"] == "toolCall"
                    && part.get("arguments").is_none()
                    && part.get("rawArguments").is_none()
                    && let Some(raw) = part["id"].as_str().and_then(|id| arguments.get(id))
                {
                    part["rawArguments"] = (*raw).into();
                }
            }
        }
    }

    fn flush(&mut self, committed: bool) -> Result<()> {
        let messages = std::mem::take(&mut self.messages);
        let retained = std::mem::take(&mut self.retained);
        self.active = false;
        let messages = if committed { messages } else { Vec::new() };
        let Some(assistant) = messages.iter().find(|m| m["role"] == "assistant") else {
            self.records
                .extend(retained.into_iter().map(ReplayRecord::Event));
            return Ok(());
        };
        if let Some(meta) = assistant.get("ppReplay") {
            let meta: Meta = serde_json::from_value(meta.clone())
                .map_err(|_| anyhow::anyhow!("invalid replay snapshot"))?;
            ensure!(!meta.attempt.is_empty(), "invalid replay attempt");
            let content: Vec<Part> = serde_json::from_value(assistant["content"].clone())
                .map_err(|_| anyhow::anyhow!("invalid replay content"))?;
            let results: Vec<_> = messages
                .iter()
                .filter(|m| m["role"] == "toolResult")
                .collect();
            let mut retained: Vec<_> = retained.into_iter().map(Some).collect();
            let mut text = String::new();
            for step in meta.order {
                let event = match step {
                    Step::Content { index } => {
                        match content.get(index).context("invalid replay content index")? {
                            Part::Text {
                                text: value,
                                refusal,
                            } => {
                                text.push_str(value);
                                AgentEvent::AssistantDelta {
                                    text: value.clone(),
                                    refusal: *refusal,
                                }
                            }
                            Part::Thinking { thinking, summary } => {
                                if *summary {
                                    AgentEvent::ReasoningSummaryDelta {
                                        text: thinking.clone(),
                                    }
                                } else {
                                    AgentEvent::ThinkingDelta {
                                        text: thinking.clone(),
                                    }
                                }
                            }
                            Part::Call {
                                id,
                                name,
                                raw_arguments,
                                arguments,
                            } => AgentEvent::ToolStarted {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: match raw_arguments {
                                    Some(raw) => raw.clone(),
                                    None => arguments
                                        .as_ref()
                                        .context("missing replay tool arguments")?
                                        .to_string(),
                                },
                            },
                        }
                    }
                    Step::Finish { text: final_text } => AgentEvent::AssistantFinished {
                        text: final_text.unwrap_or_else(|| text.clone()),
                    },
                    Step::Event { index } => retained
                        .get_mut(index)
                        .and_then(Option::take)
                        .context("invalid replay event index")?,
                    Step::Result { index } => {
                        let result = results.get(index).context("missing replay tool result")?;
                        AgentEvent::ToolFinished {
                            id: string(result, "toolCallId")?.into(),
                            result: tool_result(result)?,
                        }
                    }
                    Step::Output { id, stream, text } => {
                        AgentEvent::ToolOutput { id, stream, text }
                    }
                };
                self.records.push(ReplayRecord::Event(event));
            }
            ensure!(
                retained.iter().all(Option::is_none),
                "unreferenced replay event"
            );
        } else {
            // 无扩展的标准消息用于旧格式迁移。仅有结果的 call 才视为已执行。
            self.standard(&messages, retained)?;
        }
        Ok(())
    }

    fn standard(&mut self, messages: &[Value], retained: Vec<AgentEvent>) -> Result<()> {
        let mut retained: Vec<_> = retained.into_iter().map(Some).collect();
        for message in messages.iter().filter(|m| m["role"] == "assistant") {
            let content = message["content"]
                .as_array()
                .context("invalid replay assistant content")?;
            let mut text = String::new();
            for part in content {
                match part["type"].as_str() {
                    Some("text") => text.push_str(string(part, "text")?),
                    Some("refusal") => text.push_str(string(part, "refusal")?),
                    Some("thinking") => {
                        self.records
                            .push(ReplayRecord::Event(if part["summary"] == true {
                                AgentEvent::ReasoningSummaryDelta {
                                    text: string(part, "thinking")?.into(),
                                }
                            } else {
                                AgentEvent::ThinkingDelta {
                                    text: string(part, "thinking")?.into(),
                                }
                            }))
                    }
                    _ => {}
                }
            }
            if matches!(message["stopReason"].as_str(), Some("aborted" | "error")) {
                if !text.is_empty() {
                    self.records
                        .push(ReplayRecord::Event(AgentEvent::AssistantDelta {
                            text,
                            refusal: false,
                        }));
                }
            } else {
                self.records
                    .push(ReplayRecord::Event(AgentEvent::AssistantFinished { text }));
            }
            for call in content.iter().filter(|c| c["type"] == "toolCall") {
                let id = string(call, "id")?;
                let Some(result) = messages
                    .iter()
                    .find(|m| m["role"] == "toolResult" && m["toolCallId"] == id)
                else {
                    continue;
                };
                let arguments = if let Some(raw) = call["rawArguments"].as_str() {
                    raw.to_owned()
                } else {
                    call.get("arguments")
                        .context("missing replay tool arguments")?
                        .to_string()
                };
                self.records
                    .push(ReplayRecord::Event(AgentEvent::ToolStarted {
                        id: id.into(),
                        name: string(call, "name")?.into(),
                        arguments,
                    }));
                for event in &mut retained {
                    if matches!(event, Some(AgentEvent::FileChanged { id: call, .. }) if call == id)
                        || matches!(event, Some(AgentEvent::LspDiagnostics { id: Some(call), .. }) if call == id)
                    {
                        self.records
                            .push(ReplayRecord::Event(event.take().expect("matched event")));
                    }
                }
                self.records
                    .push(ReplayRecord::Event(AgentEvent::ToolFinished {
                        id: id.into(),
                        result: tool_result(result)?,
                    }));
            }
        }
        self.records
            .extend(retained.into_iter().flatten().map(ReplayRecord::Event));
        Ok(())
    }
}

/// 快照的完成正文供树摘要复用；分片之间不能人为添加空格。
pub(super) fn completed_text(message: &Value) -> Option<String> {
    let finish = message["ppReplay"]["order"]
        .as_array()?
        .iter()
        .rev()
        .find(|step| step["kind"] == "finish")?;
    match finish.get("text").and_then(Value::as_str) {
        Some(text) => Some(text.to_owned()),
        None => Some(
            message["content"]
                .as_array()?
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect(),
        ),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key].as_str().context("invalid replay message field")
}

fn tool_result(message: &Value) -> Result<Value> {
    let content = message["content"]
        .as_array()
        .context("invalid replay tool content")?;
    let mut text = String::new();
    for part in content.iter().filter(|p| p["type"] == "text") {
        text.push_str(string(part, "text")?);
    }
    Ok(serde_json::from_str(&text)
        .unwrap_or_else(|_| json!({"ok":message["isError"] != true,"content":text})))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(projector: &mut Projector, mut value: Value) -> Result<()> {
        value["id"] = "fixture".into();
        value["parentId"] = Value::Null;
        value["timestamp"] = "2026-11-12T00:00:00Z".into();
        let line = serde_json::to_vec(&value).unwrap();
        let entry: Entry = serde_json::from_slice(&line).unwrap();
        projector.feed(&entry.payload, &line)
    }

    #[test]
    fn standard_content_keeps_refusal_summary_all_text_results_and_native_argument_fallback() {
        let mut projector = Projector::default();
        feed(&mut projector,json!({"type":"message","message":{"role":"assistant","stopReason":"toolUse","content":[
            {"type":"thinking","thinking":"summary","summary":true}, {"type":"thinking","thinking":"raw","summary":false},
            {"type":"text","text":"text"},{"type":"refusal","refusal":"refusal"},{"type":"future","value":"ignored"},
            {"type":"toolCall","id":"call","name":"bash"}]}})).unwrap();
        feed(&mut projector,json!({"type":"message","message":{"role":"toolResult","toolCallId":"call","isError":true,
            "details":{"ignored":"old copy"},"content":[{"type":"text","text":"plain "},{"type":"future"},{"type":"text","text":"output"}]}})).unwrap();
        feed(&mut projector,json!({"type":"custom","customType":"pp.native_append","data":{"protocol":"completion","messages":[
            {"role":"assistant","content":null,"tool_calls":[{"id":"call","type":"function","function":{"name":"bash","arguments":"[1,2]"}}]},
            {"role":"tool","tool_call_id":"call","content":"plain output"}]}})).unwrap();
        let records = projector.finish().unwrap();
        let values: Vec<_> = records
            .into_iter()
            .map(|r| match r {
                ReplayRecord::Event(e) => serde_json::to_value(e).unwrap(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            values[0],
            json!({"reasoning_summary_delta":{"text":"summary"}})
        );
        assert_eq!(values[1], json!({"thinking_delta":{"text":"raw"}}));
        assert_eq!(
            values[2],
            json!({"assistant_finished":{"text":"textrefusal"}})
        );
        assert_eq!(values[3]["tool_started"]["arguments"], "[1,2]");
        assert_eq!(
            values[4]["tool_finished"]["result"],
            json!({"ok":false,"content":"plain output"})
        );
    }

    #[test]
    fn tool_result_joins_all_text_parts() {
        let result = tool_result(&json!({"content":[{"type":"text","text":"{\"ok\":true,"},{"type":"text","text":"\"pipes\":{\"run_id\":\"r\"}}"}]})).unwrap();
        assert_eq!(result, json!({"ok":true,"pipes":{"run_id":"r"}}));
    }
}
