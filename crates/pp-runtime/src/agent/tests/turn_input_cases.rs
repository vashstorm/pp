use super::*;

#[test]
fn runtime_input_steering_applies_at_committed_boundary_and_replays_once() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let serving = protocol.to_owned();
        let server_thread = std::thread::spawn(move || {
            // 第一轮：最终回复结束；补充在请求进行中入队，须在完整提交边界后消费。
            for text in ["first reply", "second reply", "resumed reply"] {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
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
        // 任务结束已关闭队列：此后入队的补充不得被 continue_session 领取。
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
        // 已应用的输入各以普通 User 记录持久化一次，事件不入 journal。
        let id = agent.session_id().unwrap().to_owned();
        drop(agent);
        let restored = persistence::load(
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
}

#[test]
fn runtime_input_acceptance_arrivals_wait_for_the_next_committed_boundary() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        let serving = protocol.to_owned();
        let handle = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for text in ["first", "second", "third"] {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                bodies.push(serde_json::from_reader::<_, Value>(request.as_reader()).unwrap());
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
fn runtime_input_claimed_batch_cancel_restores_only_unaccepted_items() {
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
        let restored = persistence::load(
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
}

#[test]
fn runtime_input_persistence_failure_rolls_back_append_and_restores_batch() {
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
}

#[test]
fn runtime_input_queued_during_summary_waits_for_next_normal_request() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        // 首个完整交换是可压缩内容；最后的完整交换与待回复后缀必须保留。
        let big = "filler ".repeat(20000);
        seed_exchange(&mut agent, &big, "ack");
        seed_exchange(&mut agent, "warmup", "ready");
        // 压缩后用量 = 会话减去首个交换加摘要，必须低于阈值且留出余量。
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
        // 摘要请求本身加输出上限必须放进窗口，且阈值仍要触发压缩。
        let window = pre_tokens + 5120;
        agent.current_model.context_window = Some(window);
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<Value>();
        let serving = protocol.to_owned();
        let server_thread = std::thread::spawn(move || {
            // 请求 1：临时摘要请求，不得包含摘要期间入队的补充。
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
            assert!(
                !body.to_string().contains("after summary"),
                "summary request consumed queued input"
            );
            tx.send(body).unwrap();
            final_for(&serving, request, "compact summary", true);
            // 请求 2：摘要后的普通请求，补充在该边界应用。
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
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
