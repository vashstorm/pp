use super::*;
use crate::agent::tests::{
    EXPLORATION, ORIGIN, PLAN_CONTENT, completion_stream, err_message, final_for, plan_workflow,
    response_stream, runtime_input_agent, seed_session, write_plan_file,
};
use crate::plan::{ExecutionContext, PlanStatus, WorkMode};
use crate::tools::{ToolPolicy, UndoToken};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};

#[derive(Clone, Copy)]
enum Reply {
    Write(&'static str, &'static str),
    Final(&'static str),
    Fail,
}

fn serve(
    server: &Arc<tiny_http::Server>,
    protocol: &str,
    replies: Vec<Reply>,
) -> std::thread::JoinHandle<Vec<Value>> {
    let server = Arc::clone(server);
    let protocol = protocol.to_owned();
    std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for (index, reply) in replies.into_iter().enumerate() {
            let mut request = server
                .recv_timeout(Duration::from_secs(10))
                .unwrap()
                .expect("expected a loopback model request");
            bodies.push(serde_json::from_reader(request.as_reader()).unwrap());
            match reply {
                Reply::Write(path, content) => {
                    let arguments = json!({"path":path,"content":content}).to_string();
                    let id = format!("changes-call-{index}");
                    if protocol == "completion" {
                        completion_stream(
                            request,
                            json!({"role":"assistant","content":null,"tool_calls":[{
                                "id":id,"type":"function","index":0,
                                "function":{"name":"write","arguments":arguments}
                            }]}),
                            "tool_calls",
                        );
                    } else {
                        response_stream(
                            request,
                            &[json!({
                                "type":"function_call","id":format!("fc-{id}"),
                                "call_id":id,"name":"write","arguments":arguments,
                                "status":"completed"
                            })],
                            true,
                        );
                    }
                }
                Reply::Final(text) => final_for(&protocol, request, text, true),
                Reply::Fail => request.respond(tiny_http::Response::empty(500)).unwrap(),
            }
        }
        bodies
    })
}

fn token_for(agent: &Agent, path: &str) -> UndoToken {
    let view = agent.changes();
    assert_eq!(view.files.len(), 1, "expected one net file change");
    assert_eq!(view.files[0].path, Path::new(path));
    view.files[0].undo.clone().expect("file should be undoable")
}

fn assert_stale(agent: &Agent, token: &UndoToken, path: &Path, expected: &[u8]) {
    assert_eq!(
        err_message(agent.undo_file(token)),
        "Changes are stale; reopen /changes."
    );
    assert_eq!(std::fs::read(path).unwrap(), expected);
}

// Commit through the real built-in tool boundary, without inventing model
// requests for lifecycle actions (tree/approval/resume) that are entirely local.
fn commit_write(agent: &Agent, path: &str, content: &str) {
    let mut committed = false;
    let result = agent.tools.execute(
        "write",
        &json!({"path":path,"content":content}).to_string(),
        "changes-fixture-write",
        ToolPolicy::Execute,
        &CancellationToken::new(),
        &mut |event| {
            if matches!(event, AgentEvent::FileChanged { .. }) {
                committed = true;
            }
        },
    );
    let output: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(output["ok"], true, "write must commit: {}", result.output);
    assert!(result.stop_error.is_none());
    assert!(committed, "the real commit event must have been emitted");
    assert_eq!(
        std::fs::read(agent.tools.cwd().join(path)).unwrap(),
        content.as_bytes()
    );
}

#[test]
fn changes_steering_and_continue_preserve_first_committed_baseline() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let path = dir.path().join("demo.txt");
        let original = b"user-local\r\nkeep";
        std::fs::write(&path, original).unwrap();
        let worker = serve(
            &server,
            protocol,
            vec![
                Reply::Write("demo.txt", "first\n"),
                Reply::Write("./demo.txt", "steered\n"),
                Reply::Final("steering done"),
                Reply::Write("demo.txt", "continued\n"),
                Reply::Final("continuation done"),
            ],
        );
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        let mut enqueued = false;
        let mut accepted = Vec::new();
        assert_eq!(
            agent
                .run(
                    "edit the file",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::FileChanged { .. } if !enqueued => {
                            enqueued = true;
                            queue
                                .enqueue(
                                    input::PendingKind::Steering,
                                    "revise within the same task".into(),
                                    None,
                                    false,
                                )
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { text, .. } => accepted.push(text),
                        _ => {}
                    },
                )
                .unwrap(),
            RunOutcome::Completed
        );
        assert_eq!(accepted, ["revise within the same task"]);
        assert_eq!(std::fs::read(&path).unwrap(), b"steered\n");
        let before_continue = token_for(&agent, "demo.txt");
        assert_eq!(
            agent
                .continue_session(&CancellationToken::new(), None, &mut |_| {})
                .unwrap(),
            RunOutcome::Completed
        );
        worker.join().unwrap();
        let token = token_for(&agent, "demo.txt");
        assert_eq!(token.task_id, before_continue.task_id);
        assert_stale(&agent, &before_continue, &path, b"continued\n");
        let result = agent.undo_file(&token).unwrap();
        assert!(result.view.files.is_empty());
        assert!(result.warning.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_stale(&agent, &token, &path, original);
        assert!(server.try_recv().unwrap().is_none());
    }
}

#[test]
fn changes_rejected_entry_retains_but_next_accepted_run_replaces_task() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let path = dir.path().join("demo.txt");
        std::fs::write(&path, b"before either task\n").unwrap();
        let worker = serve(
            &server,
            protocol,
            vec![
                Reply::Write("demo.txt", "first task\n"),
                Reply::Final("first done"),
            ],
        );
        agent
            .run("first task", &CancellationToken::new(), None, &mut |_| {})
            .unwrap();
        worker.join().unwrap();
        let first = token_for(&agent, "demo.txt");

        assert!(
            agent
                .run(" \t\n", &CancellationToken::new(), None, &mut |_| {})
                .is_err()
        );
        assert_eq!(token_for(&agent, "demo.txt"), first);
        let instructions = dir.path().join("AGENTS.md");
        std::fs::write(&instructions, [0xff]).unwrap();
        assert!(
            agent
                .run(
                    "blocked by rules",
                    &CancellationToken::new(),
                    None,
                    &mut |_| {}
                )
                .is_err()
        );
        assert_eq!(token_for(&agent, "demo.txt"), first);
        assert_eq!(std::fs::read(&path).unwrap(), b"first task\n");
        assert!(server.try_recv().unwrap().is_none());
        std::fs::remove_file(instructions).unwrap();

        let worker = serve(
            &server,
            protocol,
            vec![
                Reply::Final("no writes in this task"),
                Reply::Write("demo.txt", "third task\n"),
                Reply::Final("third done"),
            ],
        );
        agent
            .run("only answer", &CancellationToken::new(), None, &mut |_| {})
            .unwrap();
        assert!(agent.changes().files.is_empty());
        assert_stale(&agent, &first, &path, b"first task\n");
        agent
            .run("third task", &CancellationToken::new(), None, &mut |_| {})
            .unwrap();
        worker.join().unwrap();
        let third = token_for(&agent, "demo.txt");
        assert_ne!(third.task_id, first.task_id);
        agent.undo_file(&third).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"first task\n");
    }
}

#[test]
fn changes_failed_and_cancelled_runs_keep_successful_commits() {
    for protocol in ["completion", "response"] {
        for cancel_after_commit in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            let path = dir.path().join("demo.txt");
            std::fs::write(&path, b"user bytes\r\nlast").unwrap();
            commit_write(&agent, "previous.txt", "previous task\n");
            let mut replies = vec![Reply::Write("demo.txt", "committed before interruption\n")];
            if !cancel_after_commit {
                replies.push(Reply::Fail);
            }
            let worker = serve(&server, protocol, replies);
            let cancellation = CancellationToken::new();
            let mut committed = false;
            let outcome = agent.run("write then stop", &cancellation, None, &mut |event| {
                if matches!(event, AgentEvent::FileChanged { .. }) {
                    committed = true;
                    if cancel_after_commit {
                        cancellation.cancel();
                    }
                }
            });
            worker.join().unwrap();
            assert!(committed);
            if cancel_after_commit {
                assert_eq!(outcome.unwrap(), RunOutcome::Cancelled);
            } else {
                assert!(outcome.is_err());
            }
            assert_eq!(
                std::fs::read(&path).unwrap(),
                b"committed before interruption\n"
            );
            let token = token_for(&agent, "demo.txt");
            agent.undo_file(&token).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"user bytes\r\nlast");
            assert_eq!(
                std::fs::read(dir.path().join("previous.txt")).unwrap(),
                b"previous task\n"
            );
        }
    }
}

#[test]
fn changes_model_effort_compaction_retain_undo_and_reset_invalidates_it() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        // The second configured model uses the same isolated loopback provider.
        std::fs::write(
            dir.path().join("models.yml"),
            json!({"providers":{"fixture":{
                "api_key":"fixture-key","protocol":protocol,
                "base_url":format!("http://{}/v1", server.server_addr()),
                "models":{"one":{"model":"one"},"two":{"model":"two"}}
            }}})
            .to_string(),
        )
        .unwrap();
        agent.catalog = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        let path = dir.path().join("demo.txt");
        std::fs::write(&path, b"original\n").unwrap();
        let worker = serve(
            &server,
            protocol,
            vec![
                Reply::Write("demo.txt", "tool bytes\n"),
                Reply::Final("finished writing"),
                Reply::Final("The write completed; demo.txt contains tool bytes."),
            ],
        );
        agent
            .run("write file", &CancellationToken::new(), None, &mut |_| {})
            .unwrap();
        let token = token_for(&agent, "demo.txt");
        agent.set_think_level(ThinkLevel::Low).unwrap();
        assert_eq!(token_for(&agent, "demo.txt"), token);
        agent.switch_model("fixture/two").unwrap();
        assert_eq!(token_for(&agent, "demo.txt"), token);
        assert!(matches!(
            agent
                .compact(&CancellationToken::new(), &mut |_| {})
                .unwrap(),
            context::CompactionOutcome::Compacted { .. }
        ));
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(token_for(&agent, "demo.txt"), token);
        agent.undo_file(&token).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"original\n");

        commit_write(&agent, "demo.txt", "not rolled back by reset\n");
        let before_reset = token_for(&agent, "demo.txt");
        agent.reset();
        assert!(agent.changes().files.is_empty());
        assert_stale(&agent, &before_reset, &path, b"not rolled back by reset\n");
        assert!(server.try_recv().unwrap().is_none());
    }
}

#[test]
fn changes_plan_view_is_local_but_api_refuses_undo() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let path = dir.path().join("demo.txt");
        std::fs::write(&path, b"user content\n").unwrap();
        commit_write(&agent, "demo.txt", "agent content\n");
        let token = token_for(&agent, "demo.txt");
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(token_for(&agent, "demo.txt"), token);
        assert_eq!(
            err_message(agent.undo_file(&token)),
            "Undo is unavailable in Plan mode."
        );
        assert_eq!(token_for(&agent, "demo.txt"), token);
        assert_eq!(std::fs::read(&path).unwrap(), b"agent content\n");
        assert!(server.try_recv().unwrap().is_none());
    }
}

#[test]
fn changes_tree_committed_navigation_clears_but_failed_commit_retains() {
    for protocol in ["completion", "response"] {
        for fail_commit in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, "before the tool write");
            let path = dir.path().join("demo.txt");
            std::fs::write(&path, b"before navigation\n").unwrap();
            commit_write(&agent, "demo.txt", "disk survives navigation\n");
            seed_session(&mut agent, "after the tool write");
            let token = token_for(&agent, "demo.txt");
            let target = agent.session_tree().unwrap().tree.nodes[0].id.clone();
            if fail_commit {
                let path = dir
                    .path()
                    .join("sessions")
                    .join(format!("{}.jsonl", agent.session_id().unwrap()));
                agent.journal.use_read_only_file(&path);
            }
            let result = agent.navigate_tree(&target);
            if fail_commit {
                assert!(err_message(result).contains("persisting session"));
                assert_eq!(token_for(&agent, "demo.txt"), token);
                agent.undo_file(&token).unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), b"before navigation\n");
            } else {
                result.unwrap();
                assert!(agent.changes().files.is_empty());
                assert_stale(&agent, &token, &path, b"disk survives navigation\n");
            }
        }
    }
}

#[test]
fn changes_plan_fresh_commits_clear_keep_and_failed_fresh_retain() {
    for protocol in ["completion", "response"] {
        for (context, fail_commit) in [
            (ExecutionContext::Fresh, false),
            (ExecutionContext::Fresh, true),
            (ExecutionContext::Keep, false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            let path = dir.path().join("demo.txt");
            std::fs::write(&path, b"before planning\r\n").unwrap();
            commit_write(&agent, "demo.txt", "existing execution bytes\n");
            let undo = token_for(&agent, "demo.txt");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let old_session = agent.session_id().unwrap().to_owned();
            let review = agent.read_plan().unwrap().token;
            if fail_commit {
                agent.journal.use_read_only_file(
                    &dir.path()
                        .join("sessions")
                        .join(format!("{old_session}.jsonl")),
                );
            }
            let result = agent.approve_plan(
                review,
                context,
                &InputQueue::default(),
                &CancellationToken::new(),
            );
            if fail_commit {
                assert!(err_message(result).contains("persisting session"));
                assert_eq!(agent.session_id(), Some(old_session.as_str()));
                assert_eq!(agent.workflow.mode, WorkMode::Plan);
                assert_eq!(token_for(&agent, "demo.txt"), undo);
            } else {
                let preparation = result.unwrap();
                assert_eq!(preparation.workflow.mode, WorkMode::Execute);
                if context == ExecutionContext::Fresh {
                    assert_ne!(agent.session_id(), Some(old_session.as_str()));
                    assert!(agent.changes().files.is_empty());
                    assert_stale(&agent, &undo, &path, b"existing execution bytes\n");
                } else {
                    assert_eq!(agent.session_id(), Some(old_session.as_str()));
                    assert_eq!(token_for(&agent, "demo.txt"), undo);
                }
                let worker = serve(
                    &server,
                    protocol,
                    vec![
                        Reply::Write("demo.txt", "approved execution bytes\n"),
                        Reply::Final("approved execution finished"),
                    ],
                );
                assert_eq!(
                    agent
                        .continue_session(&CancellationToken::new(), None, &mut |_| {})
                        .unwrap(),
                    RunOutcome::Completed
                );
                worker.join().unwrap();
                let execution_undo = token_for(&agent, "demo.txt");
                if context == ExecutionContext::Keep {
                    assert_eq!(execution_undo.task_id, undo.task_id);
                } else {
                    assert_ne!(execution_undo.task_id, undo.task_id);
                }
                assert_stale(&agent, &undo, &path, b"approved execution bytes\n");
                agent.undo_file(&execution_undo).unwrap();
                let expected: &[u8] = if context == ExecutionContext::Keep {
                    b"before planning\r\n"
                } else {
                    b"existing execution bytes\n"
                };
                assert_eq!(std::fs::read(&path).unwrap(), expected);
                drop(preparation);
            }
            if fail_commit || context == ExecutionContext::Fresh {
                assert_eq!(std::fs::read(&path).unwrap(), b"existing execution bytes\n");
            }
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(
                std::fs::read(dir.path().join(document.path().unwrap())).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
        }
    }
}

#[test]
fn changes_resume_success_clears_failed_resume_retains() {
    const FIXTURE: &str = "PP_TEST_CHANGES_RESUME_HOME";
    let Ok(root) = std::env::var(FIXTURE) else {
        let home = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::changes_tests::changes_resume_success_clears_failed_resume_retains",
                "--nocapture",
            ])
            .env("HOME", home.path())
            .env(FIXTURE, home.path())
            .status()
            .unwrap();
        assert!(status.success());
        return;
    };
    let root = Path::new(&root);
    let global_sessions = root.join(".pp/agent/sessions");
    for protocol in ["completion", "response"] {
        let current_dir = root.join(format!("current-{protocol}"));
        let saved_dir = root.join(format!("saved-{protocol}"));
        std::fs::create_dir(&current_dir).unwrap();
        std::fs::create_dir(&saved_dir).unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_string();
        let mut current = runtime_input_agent(&current_dir, protocol, &addr, "");
        let mut saved = runtime_input_agent(&saved_dir, protocol, &addr, "");
        current.journal = persistence::SessionJournal::new(global_sessions.clone());
        saved.journal = persistence::SessionJournal::new(global_sessions.clone());
        seed_session(&mut current, "current session");
        seed_session(&mut saved, "saved session");
        commit_write(&saved, "saved.txt", "saved session tool bytes\n");
        let saved_id = saved.session_id().unwrap().to_owned();
        let current_id = current.session_id().unwrap().to_owned();
        let path = current_dir.join("demo.txt");
        std::fs::write(&path, b"current user bytes\n").unwrap();
        commit_write(&current, "demo.txt", "current tool bytes\n");
        let undo = token_for(&current, "demo.txt");

        // The candidate is still locked by its owner. A failed real resume
        // must leave the active session and its undo capability untouched.
        assert!(
            current
                .resume(&saved_id, &CancellationToken::new())
                .is_err()
        );
        assert_eq!(current.session_id(), Some(current_id.as_str()));
        assert_eq!(token_for(&current, "demo.txt"), undo);
        assert_eq!(std::fs::read(&path).unwrap(), b"current tool bytes\n");
        drop(saved);

        let preparation = current
            .resume(&saved_id, &CancellationToken::new())
            .unwrap();
        assert_eq!(preparation.session_id.as_deref(), Some(saved_id.as_str()));
        assert!(current.changes().files.is_empty());
        assert_stale(&current, &undo, &path, b"current tool bytes\n");
        assert_eq!(
            std::fs::read(saved_dir.join("saved.txt")).unwrap(),
            b"saved session tool bytes\n"
        );
        // A post-resume write takes a fresh baseline, not the old session's
        // user bytes or anything reconstructed from its replay.
        commit_write(&current, "demo.txt", "post-resume bytes\n");
        current.undo_file(&token_for(&current, "demo.txt")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"current tool bytes\n");
        assert!(server.try_recv().unwrap().is_none());
        drop(preparation);
    }
}
