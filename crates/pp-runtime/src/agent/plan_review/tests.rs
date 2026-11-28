use super::*;
use crate::agent::tests::{
    EXPLORATION, ORIGIN, PLAN_CONTENT, completion_stream, err_message, final_for, journal_workflow,
    plan_workflow, response_stream, runtime_input_agent, seed_session, write_plan_file,
};
use axum::{body::Body, response::Response};
use pp_core::model::ThinkLevel;
use pp_core::plan::PlanStatus;
use pp_core::plan::WorkMode;
use pp_test_support::{CapturedRequest, TestServer};
use serde_json::{Value, json};
use std::sync::Arc;

const REPORT: &str = "## Findings\n\n- Minor: verification step is vague.\n";

#[test]
fn model_roles_review_uses_its_own_model_protocol_and_effort_without_changing_main() {
    for protocol in ["completion", "response"] {
        let server = Arc::new(TestServer::start().unwrap());
        let address = server.addr().to_string();
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, token) = ready_reviewer(
            dir.path(),
            if protocol == "completion" {
                "response"
            } else {
                "completion"
            },
            &address,
        );
        let path = dir.path().join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["audit"] = json!({"api_key":"fixture-key", "protocol":protocol,
            "base_url":format!("http://{address}/v1"), "models":{"review":{"model":"audit-model"}}});
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        agent
            .save_model_role(
                pp_config::config::ModelRole::Review,
                Some(pp_config::config::RoleAssignment {
                    fallback: None,
                    enabled: true,
                    model: "audit/review".into(),
                    effort: pp_core::model::ThinkLevel::High,
                }),
            )
            .unwrap();
        let main = serde_json::to_value(&agent.session).unwrap();
        let worker = Arc::clone(&server);
        let handle = std::thread::spawn(move || {
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = request.json().unwrap();
            assert_eq!(body["model"], "audit-model");
            let effort = if protocol == "completion" {
                &body["reasoning_effort"]
            } else {
                &body["reasoning"]["effort"]
            };
            assert_eq!(effort, "high");
            final_for(protocol, request, REPORT, false);
        });
        let report = agent
            .review_plan(
                token,
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        handle.join().unwrap();
        assert_eq!(report.model_reference, "audit/review");
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), main);
    }
}

#[test]
fn fallback_plan_review_switches_scratch_protocol_without_changing_main() {
    for protocol in ["completion", "response"] {
        let server = Arc::new(TestServer::start().unwrap());
        let address = server.addr().to_string();
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, token) = ready_reviewer(dir.path(), protocol, &address);
        let backup_protocol = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        let path = dir.path().join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["audit"] = json!({"api_key":"fixture-key","protocol":backup_protocol,
            "base_url":format!("http://{address}/v1"),"models":{"backup":{"model":"review-backup"}}});
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        agent
            .save_model_role(
                pp_config::config::ModelRole::Review,
                Some(pp_config::config::RoleAssignment {
                    enabled: true,
                    model: "fixture/one".into(),
                    effort: ThinkLevel::None,
                    fallback: Some(pp_config::config::RoleModel {
                        model: "audit/backup".into(),
                        effort: ThinkLevel::High,
                    }),
                }),
            )
            .unwrap();
        let before = serde_json::to_value(&agent.session).unwrap();
        let worker = Arc::clone(&server);
        let handle = std::thread::spawn(move || {
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            request
                .respond(Response::builder().status(503).body(Body::empty()).unwrap())
                .unwrap();
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(
                request.target(),
                if backup_protocol == "completion" {
                    "/v1/chat/completions"
                } else {
                    "/v1/responses"
                }
            );
            let body: Value = request.json().unwrap();
            assert_eq!(body["model"], "review-backup");
            final_for(backup_protocol, request, REPORT, false);
        });
        let report = agent
            .review_plan(
                token,
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        handle.join().unwrap();
        assert_eq!(report.model_reference, "audit/backup");
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
    }
}

// --- fixtures ---------------------------------------------------------

/// A Ready revision-1 plan with its file and a seeded session, plus the
/// matching review token.
fn ready_reviewer(dir: &std::path::Path, protocol: &str, address: &str) -> (Agent, ReviewToken) {
    let mut agent = runtime_input_agent(dir, protocol, address, "");
    agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
    write_plan_file(&agent, PLAN_CONTENT);
    seed_session(&mut agent, EXPLORATION);
    let review = agent.read_plan().unwrap();
    (agent, review.token)
}

fn previous_report(agent: &Agent) -> PlanReviewReport {
    PlanReviewReport {
        plan_id: agent.workflow.plan.as_ref().unwrap().id.clone(),
        revision: agent.workflow.plan.as_ref().unwrap().revision,
        model_reference: "fixture/one".into(),
        content: "# Previous report\n\nOlder findings.\n".into(),
    }
}

// --- protocol stream fixtures ----------------------------------------
// The SSE/stream builders are shared with `crate::agent::tests`. The
// reviewer fixture intentionally omits `response.content_part.done`.

fn refusal_for(protocol: &str, request: CapturedRequest) {
    match protocol {
        "completion" => completion_stream(
            request,
            json!({"role":"assistant","content":null,"refusal":"Cannot help"}),
            "stop",
        ),
        _ => response_stream(
            request,
            std::slice::from_ref(&json!({
                "type":"message", "id":"msg-refusal", "role":"assistant",
                "status":"completed",
                "content":[{"type":"refusal","refusal":"Cannot help",
                            "annotations":[],"logprobs":null}]
            })),
            false,
        ),
    }
}

fn calls_for(protocol: &str, request: CapturedRequest, calls: &[(&str, &str, Value)]) {
    if protocol == "completion" {
        let calls: Vec<_> = calls
            .iter()
            .enumerate()
            .map(|(index, (id, name, args))| {
                json!({"id":id, "type":"function", "index":index,
                       "function":{"name":name,"arguments":args.to_string()}})
            })
            .collect();
        completion_stream(
            request,
            json!({"role":"assistant","content":null,"tool_calls":calls}),
            "tool_calls",
        );
    } else {
        let calls: Vec<_> = calls
            .iter()
            .map(|(id, name, args)| {
                json!({"type":"function_call", "id":format!("fc-{id}"),
                       "call_id":id, "name":name,
                       "arguments":args.to_string(), "status":"completed"})
            })
            .collect();
        response_stream(request, &calls, false);
    }
}

fn request_body(request: &CapturedRequest) -> Value {
    request.json().unwrap()
}

fn history_entries(body: &Value, protocol: &str) -> Vec<Value> {
    match protocol {
        "completion" => body["messages"].as_array().unwrap().clone(),
        _ => body["input"].as_array().unwrap().clone(),
    }
}

// --- unit behavior ----------------------------------------------------

#[test]
fn plan_review_validation_cancel_and_failure_preservation() {
    {
        assert_eq!(final_report("  report  ").unwrap(), "  report  ");
        assert_eq!(
            err_message(final_report("   ")),
            "Plan review produced no report."
        );
        assert_eq!(
            err_message(final_report("bad\0report")),
            "Plan review report contains NUL."
        );
        let oversize = "x".repeat(PLAN_MAX_BYTES + 1);
        assert_eq!(
            err_message(final_report(&oversize)),
            "Plan review report is too large."
        );
        assert_eq!(PLAN_MAX_BYTES, 262_144);
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, token) = ready_reviewer(dir.path(), protocol, "127.0.0.1:1");
        let queue = InputQueue::default();
        let session_before = serde_json::to_value(&agent.session).unwrap();

        // Execute mode never reviews.
        agent.workflow.mode = WorkMode::Execute;
        let error = err_message(agent.review_plan(
            token.clone(),
            &queue,
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert_eq!(error, "Plan review is no longer current.");
        agent.workflow.mode = WorkMode::Plan;

        // Draft plans cannot be reviewed.
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Draft;
        let error = err_message(agent.review_plan(
            token.clone(),
            &queue,
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert_eq!(error, "Plan review is no longer current.");
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Ready;

        // A token bound to other bytes is stale.
        let mut stale = token.clone();
        stale.content = format!("{PLAN_CONTENT}\ndrift\n");
        let error =
            err_message(agent.review_plan(stale, &queue, &CancellationToken::new(), &mut |_| {}));
        assert_eq!(error, "Plan review is no longer current.");

        // Pending steering input blocks the review.
        queue.begin_run().unwrap();
        queue
            .enqueue(
                crate::agent::input::PendingKind::Steering,
                "interrupt".into(),
                None,
                false,
            )
            .unwrap();
        let error = err_message(agent.review_plan(
            token.clone(),
            &queue,
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert_eq!(error, "Apply pending input before approving the plan.");
        queue.close().unwrap();

        // An externally changed file is not silently reviewed; the
        // pending queue above must not mask the file check.
        write_plan_file(&agent, &format!("{PLAN_CONTENT}\nexternal edit\n"));
        let error = err_message(agent.review_plan(
            token.clone(),
            &InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert_eq!(
            error,
            "Plan changed; review the new revision before executing."
        );

        // None of the failures changed the session or granted a report.
        assert_eq!(
            serde_json::to_value(&agent.session).unwrap(),
            session_before
        );
        assert!(agent.workflow.review.is_none());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, token) = ready_reviewer(dir.path(), protocol, "127.0.0.1:1");
        let previous = previous_report(&agent);
        agent.workflow.review = Some(previous.clone());
        let session_before = serde_json::to_value(&agent.session).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = err_message(agent.review_plan(
            token,
            &InputQueue::default(),
            &cancellation,
            &mut |_| {},
        ));
        assert!(error.contains("cancel"), "unexpected error: {error}");
        assert_eq!(agent.workflow.review, Some(previous));
        assert_eq!(
            serde_json::to_value(&agent.session).unwrap(),
            session_before
        );
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
    }

    for protocol in ["completion", "response"] {
        // Provider refusal replaces nothing.
        let dir = tempfile::tempdir().unwrap();
        let server = Arc::new(TestServer::start().unwrap());
        let (mut agent, token) = ready_reviewer(dir.path(), protocol, &server.addr().to_string());
        let previous = previous_report(&agent);
        agent.workflow.review = Some(previous.clone());
        let session_before = serde_json::to_value(&agent.session).unwrap();
        let worker = Arc::clone(&server);
        let handle = std::thread::spawn(move || {
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            refusal_for(protocol, request);
        });
        let error = err_message(agent.review_plan(
            token,
            &InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        ));
        handle.join().unwrap();
        assert_eq!(error, "Plan review was refused by the model.");
        assert_eq!(agent.workflow.review, Some(previous));
        assert_eq!(
            serde_json::to_value(&agent.session).unwrap(),
            session_before
        );
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );

        // An unreachable provider fails the review without state changes.
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, token) = ready_reviewer(dir.path(), protocol, "127.0.0.1:1");
        let previous = previous_report(&agent);
        agent.workflow.review = Some(previous.clone());
        let error = err_message(agent.review_plan(
            token,
            &InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert!(!error.is_empty());
        assert_eq!(agent.workflow.review, Some(previous));
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
    }
}

// --- full review rounds ------------------------------------------------

#[test]
fn plan_review_rounds_gate_tools_and_persist_report() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("canary"), "original\n").unwrap();
        std::fs::write(dir.path().join("source.txt"), "needle here\n").unwrap();
        let server = Arc::new(TestServer::start().unwrap());
        let (mut agent, token) = ready_reviewer(dir.path(), protocol, &server.addr().to_string());
        let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let session_before = serde_json::to_value(&agent.session).unwrap();
        let journal = dir
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", agent.session_id().unwrap()));
        let lines_before = std::fs::read_to_string(&journal).unwrap().lines().count();

        let serving = protocol.to_owned();
        let worker = Arc::clone(&server);
        let handle = std::thread::spawn(move || {
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body = request_body(&request);
            // The only user message carries the origin prompt and the
            // exact plan revision, never the planning chat.
            let entries = history_entries(&body, &serving);
            let user_text = entries
                .iter()
                .filter(|entry| entry["role"] == "user")
                .map(|entry| match &entry["content"] {
                    Value::String(text) => text.clone(),
                    parts => parts
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>(),
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(user_text.contains(ORIGIN));
            assert!(user_text.contains(&plan_id));
            assert!(user_text.contains("Revision: 1"));
            assert!(user_text.contains(PLAN_CONTENT));
            assert!(!user_text.contains(EXPLORATION));
            assert_eq!(entries.iter().filter(|e| e["role"] == "user").count(), 1);
            // Review-only system instructions and read-only toolset.
            let instructions = if serving == "completion" {
                body["messages"][0]["content"].as_str().unwrap()
            } else {
                body["instructions"].as_str().unwrap()
            };
            assert!(instructions.contains("PLAN REVIEW MODE"));
            let tools: Vec<&str> = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .or(tool["name"].as_str())
                        .unwrap()
                })
                .collect();
            assert!(
                tools
                    .iter()
                    .all(|name| matches!(*name, "read" | "rg" | "glob" | "web_search" | "lsp"))
            );
            calls_for(
                &serving,
                request,
                &[
                    ("read-canary", "read", json!({"path":"canary"})),
                    ("search", "rg", json!({"pattern":"needle"})),
                    ("paths", "glob", json!({"pattern":"canary"})),
                    ("shell", "bash", json!({"command":"touch pwned"})),
                    ("native", "find", json!({"args":[]})),
                    ("ask", "ask", json!({"questions":[]})),
                    (
                        "plan-write",
                        "write",
                        json!({"path":".pp/drafts/create-canary.md","content":"- [ ] x"}),
                    ),
                    ("mystery", "mcp_unknown", json!({})),
                ],
            );
            let request = worker
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body = request_body(&request);
            // The tool batch was appended natively with paired results.
            let entries = history_entries(&body, &serving);
            let outputs = entries
                .iter()
                .filter(|item| item["role"] == "tool" || item["type"] == "function_call_output")
                .count();
            assert_eq!(outputs, 8);
            final_for(&serving, request, REPORT, false);
            plan_id
        });

        let mut results = Vec::new();
        let report = agent
            .review_plan(
                token,
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |event| {
                    if let AgentEvent::ToolFinished { id, result } = event {
                        results.push((id, result));
                    }
                },
            )
            .unwrap();
        let plan_id = handle.join().unwrap();

        assert!(server.try_recv().unwrap().is_none());
        assert_eq!(results.len(), 8);
        assert_eq!(results[0].1["ok"], true, "read result: {:?}", results[0]);
        assert_eq!(
            results[1].1["ok"], true,
            "plan_search result: {:?}",
            results[1]
        );
        assert_eq!(results[2].1["ok"], true, "glob result: {:?}", results[2]);
        assert_eq!(results[2].1["content"], "canary");
        for (id, result) in &results[3..] {
            assert_eq!(
                result["error"], "Tool is not available during Plan review.",
                "{id}: {result}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("canary")).unwrap(),
            "original\n"
        );
        assert!(!dir.path().join("pwned").exists());

        // The report is persisted as a revision-bound workflow record;
        // plan document, file, mode and session all stay untouched.
        assert_eq!(report.plan_id, plan_id);
        assert_eq!(report.revision, 1);
        assert_eq!(report.model_reference, "fixture/one");
        assert_eq!(report.content, REPORT);
        assert_eq!(agent.workflow.review.as_ref().unwrap().content, REPORT);
        assert_eq!(
            serde_json::to_value(&agent.session).unwrap(),
            session_before
        );
        let document = agent.workflow.plan.as_ref().unwrap();
        assert_eq!(document.revision, 1);
        assert_eq!(document.status, PlanStatus::Ready);
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(document.path().expect("plan has a path")))
                .unwrap(),
            PLAN_CONTENT
        );
        let id = agent.session_id().unwrap().to_owned();
        let expected = agent.workflow.clone();
        assert_eq!(journal_workflow(dir.path(), &id), expected);
        // Exactly one journal record was added: the workflow commit.
        let lines_after = std::fs::read_to_string(&journal).unwrap().lines().count();
        assert_eq!(lines_after, lines_before + 1);
    }
}
