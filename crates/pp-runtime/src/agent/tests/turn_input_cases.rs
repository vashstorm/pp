mod integration {
    use super::super::*;
    use pp_test_support::TestServer;
    use std::sync::Arc;

    #[test]
    fn runtime_input_steering_and_arrivals_at_committed_boundary() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<Value>();
            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let server_thread = std::thread::spawn(move || {
                // First turn: the final reply ends it; supplementary input enqueued while the request is in flight must be consumed at the next full commit boundary.
                for text in ["first reply", "second reply", "resumed reply"] {
                    let request = worker
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body: Value = request.json().unwrap();
                    tx.send(body).unwrap();
                    final_for(&serving, request, text, true);
                }
            });
            let mut enqueued = false;
            let mut accepted = Vec::new();
            let outcome = agent
                .run(
                    "initial prompt",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::ModelRequestStarted if !enqueued => {
                            enqueued = true;
                            queue
                                .enqueue(
                                    input::PendingKind::Steering,
                                    "steer once".into(),
                                    None,
                                    false,
                                )
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { id, text } => accepted.push((id, text)),
                        _ => {}
                    },
                )
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            assert_eq!(accepted, vec![(1, "steer once".to_owned())]);
            let first = rx.recv().unwrap();
            let second = rx.recv().unwrap();
            assert_eq!(user_messages(protocol, &first), ["initial prompt"]);
            assert_eq!(
                user_messages(protocol, &second),
                ["initial prompt", "steer once"]
            );
            assert!(
                second.to_string().contains("first reply"),
                "follow-on request must replay the committed native history"
            );
            assert_eq!(queue.snapshot_since(None).unwrap().unwrap().steering(), 0);
            // The task ended and closed the queue: supplementary input enqueued afterwards must not be claimed by continue_session.
            queue
                .enqueue(
                    input::PendingKind::Steering,
                    "left pending".into(),
                    None,
                    false,
                )
                .unwrap();
            let outcome = agent
                .continue_session(&CancellationToken::new(), Some(&queue), &mut |event| {
                    if let AgentEvent::UserInputAccepted { .. } = event {
                        panic!("closed queue must not be claimed by a new session")
                    }
                })
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            let third = rx.recv().unwrap();
            assert_eq!(
                user_messages(protocol, &third),
                ["initial prompt", "steer once"]
            );
            assert_eq!(queue.snapshot_since(None).unwrap().unwrap().steering(), 1);
            server_thread.join().unwrap();
            // Each applied input is persisted once as an ordinary User record; events are not journaled.
            let id = agent.session_id().unwrap().to_owned();
            drop(agent);
            let restored = persistence::load_test(
                &dir.path().join("sessions"),
                &dir.path().join("sessions/.locks"),
                Some(&id),
                true,
            )
            .unwrap();
            let users = restored
                .replay
                .iter()
                .filter_map(|record| match record {
                    persistence::ReplayRecord::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(users, vec!["initial prompt", "steer once"]);
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let handle = std::thread::spawn(move || {
                let mut bodies = Vec::new();
                for text in ["first", "second", "third"] {
                    let request = worker
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    bodies.push(request.json().unwrap());
                    final_for(&serving, request, text, true);
                }
                bodies
            });
            let mut initial_started = false;
            agent
                .run(
                    "initial",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::AssistantStarted if !initial_started => {
                            initial_started = true;
                            queue
                                .enqueue(input::PendingKind::Steering, "S1".into(), None, false)
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { text, .. } if text == "S1" => {
                            queue
                                .enqueue(input::PendingKind::Steering, "S2".into(), None, false)
                                .unwrap();
                        }
                        _ => {}
                    },
                )
                .unwrap();
            let bodies = handle.join().unwrap();
            assert_eq!(user_messages(protocol, &bodies[0]), ["initial"]);
            assert_eq!(user_messages(protocol, &bodies[1]), ["initial", "S1"]);
            assert_eq!(user_messages(protocol, &bodies[2]), ["initial", "S1", "S2"]);
        }
    }

    #[test]
    fn runtime_input_cancel_persistence_failure_and_summary_boundary() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            for text in ["accepted", "unaccepted one", "unaccepted two"] {
                queue
                    .enqueue(input::PendingKind::Steering, text.into(), None, false)
                    .unwrap();
            }
            agent.append_user("initial");
            let mut initial = Some("initial");
            let cancellation = CancellationToken::new();
            let mut accepted = Vec::new();
            let error = agent
                .apply_pending(
                    queue.claim_steering(false).unwrap(),
                    Some(&queue),
                    &mut initial,
                    &cancellation,
                    &mut |event| {
                        if let AgentEvent::UserInputAccepted { text, .. } = event {
                            accepted.push(text);
                            cancellation.cancel();
                        }
                    },
                )
                .unwrap_err();
            assert!(error.is::<Cancelled>());
            assert!(initial.is_none());
            assert_eq!(accepted, ["accepted"]);
            assert_eq!(
                queue.claim_next().unwrap().unwrap().source,
                "unaccepted one"
            );
            assert_eq!(
                queue.claim_next().unwrap().unwrap().source,
                "unaccepted two"
            );
            assert!(queue.claim_next().unwrap().is_none());
            let id = agent.session_id().unwrap().to_owned();
            drop(agent);
            let restored = persistence::load_test(
                &dir.path().join("sessions"),
                &dir.path().join("sessions/.locks"),
                Some(&id),
                true,
            )
            .unwrap();
            let users: Vec<_> = restored
                .replay
                .iter()
                .filter_map(|record| match record {
                    persistence::ReplayRecord::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(users, ["initial", "accepted"]);
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            // A real filesystem failure at journal creation, without changing process limits.
            std::fs::write(dir.path().join("sessions"), "not a directory").unwrap();
            let before = serde_json::to_value(&agent.session).unwrap();
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            for text in ["one", "two"] {
                queue
                    .enqueue(input::PendingKind::Steering, text.into(), None, false)
                    .unwrap();
            }
            let mut accepted = Vec::new();
            let error = agent
                .apply_pending(
                    queue.claim_steering(false).unwrap(),
                    Some(&queue),
                    &mut None,
                    &CancellationToken::new(),
                    &mut |event| {
                        if let AgentEvent::UserInputAccepted { text, .. } = event {
                            accepted.push(text);
                        }
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains("persisting session"));
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
            assert!(accepted.is_empty());
            assert!(agent.journal.check().is_err());
            assert_eq!(queue.claim_next().unwrap().unwrap().source, "one");
            assert_eq!(queue.claim_next().unwrap().unwrap().source, "two");
            assert!(queue.claim_next().unwrap().is_none());
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
            // The first full exchange is compactable content; the last full exchange and the not-yet-answered suffix must be kept.
            let big = "filler ".repeat(20000);
            seed_exchange(&mut agent, &big, "ack");
            seed_exchange(&mut agent, "warmup", "ready");
            // Post-compaction usage = session minus the first exchange plus the summary; it must sit below the threshold with margin to spare.
            let total_bytes = serde_json::to_value(&agent.session)
                .unwrap()
                .to_string()
                .len() as u64;
            let big_bytes = serde_json::to_string(&big).unwrap().len() as u64;
            assert!(
                total_bytes / 4 < big_bytes,
                "test needs a dominant compactable exchange"
            );
            let prompt_bytes = serde_json::to_string("final question").unwrap().len() as u64;
            let pre_tokens = (total_bytes + prompt_bytes).div_ceil(3);
            // The summary request itself plus the output cap must fit the window, and the threshold must still trigger compaction.
            let window = pre_tokens + 5120;
            agent.current_model.context_window = Some(window);
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<Value>();
            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let server_thread = std::thread::spawn(move || {
                // Request 1: the temporary summary request must not include supplementary input enqueued during summarization.
                let request = worker
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = request.json().unwrap();
                assert!(
                    !body.to_string().contains("after summary"),
                    "summary request consumed queued input"
                );
                tx.send(body).unwrap();
                final_for(&serving, request, "compact summary", true);
                // Request 2: the normal request after summarization, where supplementary input is applied at this boundary.
                let request = worker
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = request.json().unwrap();
                tx.send(body).unwrap();
                final_for(&serving, request, "final answer", true);
            });
            let mut accepted = Vec::new();
            let outcome = agent
                .run(
                    "final question",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::CompactionStarted { automatic: true } => {
                            queue
                                .enqueue(
                                    input::PendingKind::Steering,
                                    "after summary".into(),
                                    None,
                                    false,
                                )
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { id, text } => accepted.push((id, text)),
                        _ => {}
                    },
                )
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            server_thread.join().unwrap();
            drop(rx.recv().unwrap());
            let normal = rx.recv().unwrap();
            assert_eq!(
                user_messages(protocol, &normal).last().map(String::as_str),
                Some("after summary")
            );
            assert_eq!(accepted, vec![(1, "after summary".to_owned())]);
            let snapshot = queue.snapshot_since(None).unwrap().unwrap();
            assert_eq!((snapshot.steering(), snapshot.follow_up()), (0, 0));
        }
    }
}
