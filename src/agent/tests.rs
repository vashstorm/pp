use super::turn::apply_todo_change;
use super::*;
use serde_json::{Value, json};

mod model_role_cases;
mod plan_model_cases;
mod runtime_model_cases;
mod session_task_cases;
mod tree_todo_cases;
mod turn_input_cases;

pub(super) fn runtime_input_agent(
    dir: &std::path::Path,
    protocol: &str,
    server_addr: &str,
    settings_extra: &str,
) -> Agent {
    let settings = dir.join("settings.yml");
    let models = dir.join("models.yml");
    std::fs::write(
        &settings,
        format!("# 测试设置\nrate_limit_retry_budget_seconds: 0\n{settings_extra}"),
    )
    .unwrap();
    let model = json!({"model":"one"});
    std::fs::write(
        &models,
        json!({"providers":{"fixture": {
            "api_key":"fixture-key",
            "protocol":protocol,
            "base_url":format!("http://{server_addr}/v1"),
            "models":{"one":model}
        }}})
        .to_string(),
    )
    .unwrap();
    Agent::new(
        ModelCatalog::from_files(&settings, &models).unwrap(),
        Tools::new(dir.to_owned(), EditMode::Replace).unwrap(),
        dir.join("sessions"),
    )
    .unwrap()
}

// --- shared Plan-mode test fixtures (plan_review / plan_workflow) ---
pub(super) const PLAN_CONTENT: &str =
    "# Create hello\n\n- [ ] Create hello.txt and read back hello.\n";

pub(super) const ORIGIN: &str = "ORIGIN-MARKER create hello with newline question";

pub(super) const EXPLORATION: &str = "EXPLORATION-MARKER planning chat";

pub(super) fn seed_session(agent: &mut Agent, user: &str) {
    // A real task appends the user message natively before the journal
    // records the projection and state.
    super::append_user_to(&mut agent.session, user);
    agent
        .session
        .set_tools(agent.tools.all_definitions(agent.workflow.mode));
    let metadata = Metadata {
        cwd: agent.tools.cwd(),
        model_reference: &agent.current_model.reference,
        think_level: agent.think_level,
        edit_mode: agent.tools.edit_mode(),
        tool_limits: agent.tools.limits,
        base_system: &agent.base_system,
        prompt_source: agent.prompt_source.as_ref(),
    };
    agent
        .journal
        .sent(
            &metadata,
            (&agent.session).into(),
            &agent.workflow,
            Some(user),
            None,
        )
        .unwrap();
}

pub(super) fn plan_workflow(
    content: &str,
    origin: &str,
    status: crate::plan::PlanStatus,
) -> crate::plan::WorkflowState {
    let mut document = crate::plan::PlanDocument::new().unwrap();
    if !content.is_empty() {
        document.revision = 1;
        document.title = "Create hello".into();
        document.content = content.into();
    }
    document.origin_prompt = origin.into();
    document.status = status;
    // 旧会话语义：无 AI 文件名，位于 Plans，走 <id>.md 兜底路径。
    document.location = crate::plan::PlanLocation::Plans;
    crate::plan::WorkflowState {
        mode: crate::plan::WorkMode::Plan,
        plan: Some(document),
        review: None,
        ..Default::default()
    }
}

pub(super) fn write_plan_file(agent: &Agent, content: &str) {
    let document = agent.workflow.plan.as_ref().unwrap();
    let path = agent
        .tools
        .cwd()
        .join(document.path().expect("fixture plan has a saved path"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Latest persisted workflow record, read directly from the JSONL journal
/// without taking the session's exclusive flock (the agent holds it).
pub(super) fn journal_workflow(dir: &std::path::Path, id: &str) -> crate::plan::WorkflowState {
    let path = dir.join("sessions").join(format!("{id}.jsonl"));
    let text = std::fs::read_to_string(path).unwrap();
    let mut latest = None;
    for line in text.lines() {
        let record: serde_json::Value = serde_json::from_str(line).unwrap();
        if matches!(
            record["customType"].as_str(),
            Some("pp.workflow" | "pp.session_state")
        ) {
            latest = Some(serde_json::from_value(record["data"]["workflow"].clone()).unwrap());
        }
    }
    latest.expect("workflow record persisted")
}

/// Error text for result types without a `Debug` implementation.
pub(super) fn err_message<T>(result: Result<T>) -> String {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error.to_string(),
    }
}

pub(super) fn sse_stream(request: tiny_http::Request, events: &[Value]) {
    use std::io::Write as _;
    let mut writer = request.into_writer();
    writer
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
              Transfer-Encoding: chunked\r\n\r\n",
        )
        .unwrap();
    for event in events {
        let frame = match event {
            Value::String(text) => format!("data: {text}\n\n"),
            event => format!("data: {event}\n\n"),
        };
        write!(writer, "{:x}\r\n{}\r\n", frame.len(), frame).unwrap();
    }
    writer.write_all(b"0\r\n\r\n").unwrap();
    writer.flush().unwrap();
}

pub(super) fn completion_stream(request: tiny_http::Request, message: Value, finish: &str) {
    let chunk = |delta: Value, finish: Option<&str>| {
        json!({
            "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
            "created":1, "model":"one",
            "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
        })
    };
    sse_stream(
        request,
        &[
            chunk(message, None),
            chunk(json!({}), Some(finish)),
            Value::String("[DONE]".into()),
        ],
    );
}

/// `emit_part_done` covers both server shapes: the reviewer fixture omits
/// `response.content_part.done`, while the chat fixture sends it.
pub(super) fn response_stream(request: tiny_http::Request, output: &[Value], emit_part_done: bool) {
    let mut events = Vec::new();
    let mut sequence = 0u64;
    for (output_index, item) in output.iter().enumerate() {
        if let Some(parts) = item["content"].as_array() {
            for (content_index, part) in parts.iter().enumerate() {
                let (event_type, text) = match part["type"].as_str() {
                    Some("output_text") => ("response.output_text.delta", part["text"].as_str()),
                    Some("refusal") => ("response.refusal.delta", part["refusal"].as_str()),
                    _ => continue,
                };
                events.push(json!({
                    "type":event_type, "sequence_number":sequence,
                    "item_id":item["id"], "output_index":output_index,
                    "content_index":content_index, "delta":text.unwrap(),
                    "logprobs":[]
                }));
                sequence += 1;
                if emit_part_done {
                    events.push(json!({
                        "type":"response.content_part.done", "sequence_number":sequence,
                        "item_id":item["id"], "output_index":output_index,
                        "content_index":content_index, "part":part
                    }));
                    sequence += 1;
                }
            }
        }
        events.push(json!({
            "type":"response.output_item.done", "sequence_number":sequence,
            "output_index":output_index, "item":item
        }));
        sequence += 1;
    }
    events.push(json!({
        "type":"response.completed", "sequence_number":sequence,
        "response":{
            "id":"resp-fixture", "object":"response", "created_at":1,
            "model":"one", "status":"completed", "output":output
        }
    }));
    sse_stream(request, &events);
}

pub(super) fn final_for(
    protocol: &str,
    request: tiny_http::Request,
    text: &str,
    emit_part_done: bool,
) {
    match protocol {
        "completion" => {
            completion_stream(request, json!({"role":"assistant","content":text}), "stop")
        }
        _ => response_stream(
            request,
            std::slice::from_ref(&json!({
                "type":"message", "id":"msg-final", "role":"assistant",
                "phase":"final_answer", "status":"completed",
                "content":[{"type":"output_text","text":text,
                            "annotations":[],"logprobs":null}]
            })),
            emit_part_done,
        ),
    }
}

fn tool_call_for(protocol: &str, request: tiny_http::Request, name: &str, arguments: &str) {
    match protocol {
        "completion" => completion_stream(
            request,
            json!({"role":"assistant","content":null,"tool_calls":[{
                "id":"call-1", "type":"function", "index":0,
                "function":{"name":name,"arguments":arguments}
            }]}),
            "tool_calls",
        ),
        _ => response_stream(
            request,
            std::slice::from_ref(&json!({
                "type":"function_call", "id":"fc-1", "call_id":"call-1",
                "name":name, "arguments":arguments, "status":"completed"
            })),
            true,
        ),
    }
}

fn message_text(item: &Value) -> String {
    match &item["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part["type"].as_str().unwrap() {
                "input_text" | "output_text" => part["text"].as_str().unwrap(),
                other => panic!("unexpected content type {other}"),
            })
            .collect(),
        content => panic!("unexpected message content {content}"),
    }
}

fn user_messages(protocol: &str, body: &Value) -> Vec<String> {
    let entries = match protocol {
        "completion" => body["messages"].as_array().unwrap(),
        _ => body["input"].as_array().unwrap(),
    };
    entries
        .iter()
        .filter(|entry| entry["role"] == "user")
        .map(message_text)
        .collect()
}

fn seed_exchange(agent: &mut Agent, user: &str, assistant: &str) {
    agent.append_user(user);
    match &mut agent.session {
        Session::Completion(request) => {
            request.messages.push(ChatCompletionRequestMessage::Assistant(
                async_openai::types::chat::ChatCompletionRequestAssistantMessage {
                    content: Some(
                        async_openai::types::chat::ChatCompletionRequestAssistantMessageContent::Text(
                            assistant.into(),
                        ),
                    ),
                    ..Default::default()
                },
            ));
        }
        Session::Response(request) => {
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::Assistant,
                content: assistant.into(),
                ..Default::default()
            }));
        }
    }
}
