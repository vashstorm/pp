//! 原生 `OpenAiClient` 对 mock Responses 服务的往返行为回归。
//!
//! 这里只走公开客户端入口与真实 HTTP；URL 路由、FIFO、409、限速与原始
//! SSE 编码已由同目录的协议测试覆盖。

use super::{MockEndpoint, MockServer};
use anyhow::Result;
use async_openai::types::responses::{CreateResponse, FunctionToolCall, OutputItem};
use pp_core::cancellation::CancellationToken;
use pp_core::context::TokenUsage;
use pp_core::events::AgentEvent;
use pp_llm::{MockScripts, OpenAiClient, ResponseTurn};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};

fn make_client(
    dir: &tempfile::TempDir,
    base_url: &str,
    reference: &str,
    model: &str,
) -> OpenAiClient {
    let mut settings: pp_config::config::Settings = serde_json::from_value(json!({})).unwrap();
    // 400/409 必须立即返回，避免默认重试预算拖慢断言。
    settings.rate_limit_retry_budget_seconds = 0;
    OpenAiClient::new(&pp_config::config::Config {
        auth: None,
        thinking: pp_config::models::ModelThinking::without_reasoning(),
        protocol: pp_config::models::Protocol::Response,
        api_key: "pp-mock-key".into(),
        model: model.into(),
        model_reference: reference.into(),
        context_window: Some(128_000),
        base_url: base_url.parse().unwrap(),
        agent_db_path: dir.path().join("agent.db"),
        settings,
    })
    .unwrap()
}

fn mock_base(endpoint: &MockEndpoint) -> String {
    format!("http://127.0.0.1:{}", endpoint.port())
}

fn scripts(entries: &[(&str, Value)]) -> MockScripts {
    Arc::new(
        entries
            .iter()
            .map(|(reference, script)| ((*reference).to_owned(), Arc::new(script.clone())))
            .collect(),
    )
}

fn run_response(
    client: &mut OpenAiClient,
    request: &CreateResponse,
    cancellation: &CancellationToken,
) -> (Result<ResponseTurn>, Vec<(Instant, AgentEvent)>) {
    let mut events = Vec::new();
    let result = client.respond(request, cancellation, &mut || Ok(()), &mut |event| {
        events.push((Instant::now(), event))
    });
    (result, events)
}

fn assistant_text(events: &[(Instant, AgentEvent)]) -> String {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            AgentEvent::AssistantDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn thinking_text(events: &[(Instant, AgentEvent)]) -> String {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            AgentEvent::ThinkingDelta { text } | AgentEvent::ReasoningSummaryDelta { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

fn finished(events: &[(Instant, AgentEvent)]) -> Vec<(bool, Option<TokenUsage>)> {
    events
        .iter()
        .filter_map(|(_, event)| match event {
            AgentEvent::ModelRequestFinished { success, usage, .. } => Some((*success, *usage)),
            _ => None,
        })
        .collect()
}

fn function_calls(turn: &ResponseTurn) -> Vec<&FunctionToolCall> {
    turn.output
        .iter()
        .filter_map(|item| match item {
            OutputItem::FunctionCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

fn tool_delta_text(events: &[(Instant, AgentEvent)]) -> String {
    let mut text = String::new();
    let mut expected_index = None;
    for (_, event) in events {
        if let AgentEvent::ToolArgumentsDelta {
            tool,
            index,
            text: part,
        } = event
        {
            assert_eq!(tool.as_str(), "write");
            assert!(expected_index.is_none_or(|expected| expected == *index));
            expected_index = Some(*index);
            text.push_str(part);
        }
    }
    assert!(expected_index.is_some(), "missing write argument deltas");
    text
}

/// 原生客户端在 `response.completed` 即返回；等待服务端写完 chunk 结尾并释放 busy。
fn settle() {
    thread::sleep(Duration::from_millis(50));
}

fn wait_exhausted(endpoint: &MockEndpoint, run: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match endpoint.assert_exhausted(run) {
            Ok(()) => return,
            Err(error) => {
                assert!(
                    Instant::now() < deadline,
                    "mock run was not exhausted: {error}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

#[test]
fn native_client_consumes_initial_scripts_once_and_reloads_after_exhaustion() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let base = mock_base(&endpoint);
    let dir = tempfile::tempdir().unwrap();
    let mut root = make_client(
        &dir,
        &format!("{base}/mock/fifo/root/v1"),
        "mock-root/primary",
        "pp-mock-root",
    );
    let mut fast = make_client(
        &dir,
        &format!("{base}/mock/fifo/fast/v1"),
        "mock-fast/primary",
        "pp-mock-fast",
    );
    let table = scripts(&[
        (
            "mock-root/primary",
            json!([
                {"tool": {"name": "read", "arguments": {"path": "src/lib.rs"}}},
                {"content": "second"}
            ]),
        ),
        ("mock-fast/primary", json!([{"content": "fast-initial"}])),
    ]);
    root.set_mock_scripts(Arc::clone(&table)).unwrap();
    fast.set_mock_scripts(Arc::clone(&table)).unwrap();
    assert!(Arc::ptr_eq(root.mock_scripts().unwrap(), &table));

    let (first, _) = run_response(
        &mut root,
        &CreateResponse::default(),
        &CancellationToken::new(),
    );
    let first = first.unwrap();
    assert!(first.has_calls);
    assert!(first.text.is_empty());
    let calls = function_calls(&first);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].arguments, r#"{"path":"src/lib.rs"}"#);
    assert!(!calls[0].call_id.is_empty());

    // fast 客户端仍有自己的首次 pending，不会因 root 已消费而丢失。
    let (fast_first, _) = run_response(
        &mut fast,
        &CreateResponse::default(),
        &CancellationToken::new(),
    );
    assert_eq!(fast_first.unwrap().text, "fast-initial");
    settle();

    // 队列仍有数据时，后续请求携带的另一套脚本与错误 model 都必须被忽略。
    root.set_mock_scripts(scripts(&[(
        "mock-root/primary",
        json!([{"content": "must-not-be-used"}]),
    )]))
    .unwrap();
    let wrong_model = CreateResponse {
        model: Some("wrong-model".into()),
        ..Default::default()
    };
    let (second, second_events) = run_response(&mut root, &wrong_model, &CancellationToken::new());
    let second = second.unwrap();
    assert_eq!(second.text, "second");
    assert_eq!(assistant_text(&second_events), "second");
    assert!(!assistant_text(&second_events).contains("must-not-be-used"));
    settle();

    // pending 已被消费；脚本耗尽后不再重放或伪造成功内容。
    let (exhausted, _) = run_response(
        &mut root,
        &CreateResponse::default(),
        &CancellationToken::new(),
    );
    let error = exhausted.err().unwrap();
    assert!(error.to_string().contains("HTTP status 400"), "{error}");

    root.set_mock_scripts(scripts(&[(
        "mock-root/primary",
        json!([{"response": "new-batch"}]),
    )]))
    .unwrap();
    let (reloaded, reloaded_events) = run_response(
        &mut root,
        &CreateResponse::default(),
        &CancellationToken::new(),
    );
    assert_eq!(reloaded.unwrap().text, "new-batch");
    assert_eq!(assistant_text(&reloaded_events), "new-batch");
    wait_exhausted(&endpoint, "fifo");
}

#[test]
fn native_client_observes_reasoning_body_arguments_and_exact_usage() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let dir = tempfile::tempdir().unwrap();
    let mut client = make_client(
        &dir,
        &format!("{}/mock/items/root/v1", mock_base(&endpoint)),
        "mock-root/primary",
        "pp-mock-root",
    );
    client
        .set_mock_scripts(scripts(&[(
            "mock-root/primary",
            json!([{
                "think": ["想", "🦀"],
                "content": ["正", "文"],
                "response": {"echo": true},
                "tool": {"name": "write", "arguments": {"path": "x", "content": "你好"}},
                "usage": {
                    "input_tokens": 120,
                    "output_tokens": 30,
                    "cached_tokens": 40,
                    "cache_write_tokens": 5
                }
            }]),
        )]))
        .unwrap();

    let (result, events) = run_response(
        &mut client,
        &CreateResponse::default(),
        &CancellationToken::new(),
    );
    let turn = result.unwrap();
    assert_eq!(thinking_text(&events), "想🦀");
    assert_eq!(assistant_text(&events), "正文{\"echo\":true}");
    assert_eq!(turn.text, "正文{\"echo\":true}");
    assert!(matches!(
        turn.output.first(),
        Some(OutputItem::Reasoning(_))
    ));
    assert!(matches!(
        turn.output.last(),
        Some(OutputItem::FunctionCall(_))
    ));
    let calls = function_calls(&turn);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "write");
    assert!(!calls[0].call_id.is_empty());
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].arguments).unwrap(),
        json!({"path": "x", "content": "你好"})
    );
    assert_eq!(tool_delta_text(&events), calls[0].arguments);

    let usage = TokenUsage {
        input_tokens: 120,
        output_tokens: 30,
        cached_tokens: Some(40),
        cache_write_tokens: Some(5),
    };
    assert_eq!(turn.usage, Some(usage));
    assert_eq!(finished(&events), vec![(true, Some(usage))]);
    wait_exhausted(&endpoint, "items");
}

#[test]
fn native_client_missing_model_script_fails_without_http() {
    let sentinel = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut client = make_client(
        &dir,
        &format!("http://{}/mock/missing/root/v1", sentinel.server_addr()),
        "mock-root/primary",
        "pp-mock-root",
    );
    client
        .set_mock_scripts(scripts(&[(
            "mock-fast/primary",
            json!([{"content": "elsewhere"}]),
        )]))
        .unwrap();

    let responder = thread::spawn(move || {
        if let Some(request) = sentinel.recv_timeout(Duration::from_secs(1)).unwrap() {
            let _ = request.respond(tiny_http::Response::empty(400));
            true
        } else {
            false
        }
    });
    for _ in 0..2 {
        let (result, _) = run_response(
            &mut client,
            &CreateResponse::default(),
            &CancellationToken::new(),
        );
        let error = result.err().unwrap();
        assert!(!pp_llm::retry::model_request_failed(&error), "{error}");
        assert!(error.to_string().contains("mock-root/primary"), "{error}");
    }
    assert!(!responder.join().unwrap(), "missing script reached HTTP");
}

#[test]
fn native_client_mock_requests_do_not_persist_stats_or_model_usage() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let dir = tempfile::tempdir().unwrap();
    let mut client = make_client(
        &dir,
        &format!("{}/mock/usage/root/v1", mock_base(&endpoint)),
        "mock-root/primary",
        "pp-mock-root",
    );
    client
        .set_mock_scripts(scripts(&[(
            "mock-root/primary",
            json!([{"content": "answer"}]),
        )]))
        .unwrap();
    client.set_stats_context(pp_stats::RequestContext {
        model_reference: "mock-root/primary".into(),
        cwd: dir.path().to_path_buf(),
        database: dir.path().join("stats/stats.db"),
        purpose: "agent",
        session_file: None,
        unlinked_requests: None,
        first_request: Arc::new(AtomicBool::new(true)),
        skills: Vec::new(),
    });

    let (result, events) = run_response(
        &mut client,
        &CreateResponse::default(),
        &CancellationToken::new(),
    );
    assert_eq!(result.unwrap().text, "answer");
    assert!(
        client.take_stats_key().is_none(),
        "mock request must not persist global stats"
    );
    let usage = pp_config::agent_store::AgentStore::open(&dir.path().join("agent.db"))
        .unwrap()
        .model_usage()
        .unwrap();
    assert!(
        usage.is_empty(),
        "mock request must not increment model usage: {usage:?}"
    );
    assert_eq!(finished(&events).len(), 1);
    wait_exhausted(&endpoint, "usage");
}
