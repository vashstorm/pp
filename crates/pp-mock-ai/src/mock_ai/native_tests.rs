//! Round-trip regression of the native `OpenAiClient` against the mock Responses service.
//!
//! Only the public client entry point and real HTTP are used here; URL routing, FIFO, 409, rate
//! limiting and raw SSE encoding are covered by the protocol tests in the same directory.

mod integration {
    use super::super::{MockEndpoint, MockServer};
    use anyhow::Result;
    use async_openai::types::responses::{CreateResponse, FunctionToolCall, OutputItem};
    use axum::{body::Body, response::Response};
    use pp_core::cancellation::CancellationToken;
    use pp_core::context::TokenUsage;
    use pp_core::events::AgentEvent;
    use pp_llm::{MockScripts, OpenAiClient, ResponseTurn};
    use pp_test_support::TestServer;
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
        // 400/409 must return immediately so the default retry budget does not slow the assertions down.
        settings.rate_limit_retry_budget_seconds = 0;
        OpenAiClient::new(&pp_config::config::Config {
            auth: None,
            thinking: pp_config::models::ModelThinking::without_reasoning(),
            protocol: pp_config::models::Protocol::Response,
            api_key: "pp-mock-key".into(),
            model: model.into(),
            model_reference: reference.into(),
            context_window: Some(128_000),
            prompt_cache_key: true,
            prompt_cache_retention: None,
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

    /// The native client returns at `response.completed`; wait for the server to write the chunk terminator and release busy.
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
    fn native_client_script_lifecycle_and_reasoning_usage() {
        {
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

            // The fast client still has its own first pending entry; it is not lost because root already consumed its own.
            let (fast_first, _) = run_response(
                &mut fast,
                &CreateResponse::default(),
                &CancellationToken::new(),
            );
            assert_eq!(fast_first.unwrap().text, "fast-initial");
            settle();

            // While the queue still holds data, both the other script and the wrong model carried by a later request must be ignored.
            root.set_mock_scripts(scripts(&[(
                "mock-root/primary",
                json!([{"content": "must-not-be-used"}]),
            )]))
            .unwrap();
            let wrong_model = CreateResponse {
                model: Some("wrong-model".into()),
                ..Default::default()
            };
            let (second, second_events) =
                run_response(&mut root, &wrong_model, &CancellationToken::new());
            let second = second.unwrap();
            assert_eq!(second.text, "second");
            assert_eq!(assistant_text(&second_events), "second");
            assert!(!assistant_text(&second_events).contains("must-not-be-used"));
            settle();

            // The pending entry is consumed; once the script is exhausted nothing is replayed or faked as a successful reply.
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
        {
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
                reasoning_tokens: Some(0),
            };
            assert_eq!(turn.usage, Some(usage));
            assert_eq!(finished(&events), vec![(true, Some(usage))]);
            wait_exhausted(&endpoint, "items");
        }
    }

    #[test]
    fn native_client_missing_script_no_http_and_no_stats() {
        {
            let sentinel = TestServer::start().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let mut client = make_client(
                &dir,
                &format!("http://{}/mock/missing/root/v1", sentinel.addr()),
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
                    let _ = request
                        .respond(Response::builder().status(400).body(Body::empty()).unwrap());
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
        {
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
                database: pp_stats::Database::new(dir.path().join("stats/stats.db")),
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
    }
}
