// Debug 构建的 `/test-task` 与 mock 场景结束后的正常 Agent 恢复。

fn mock_tui_fixture_start(server: &TestServer, home: &Path, project: &Path) -> Pty {
    crate::write_settings(
        home,
        json!({
            "default_model": "fixture/primary",
            "notification_timeout_seconds": 1,
            "read": {"toolResultPreview": true}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(config_path(home, "models.yml"), fixture_models(server)).unwrap();
    Pty::start_with_args(project, home, &[])
}

#[test]
fn tui_mock_task_shows_subagent_activity_and_returns_three_lane_markers() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_empty_start(home.path(), project.path());
    pty.resize(90, 120);
    pty.input("/test-task\r");
    pty.wait_for("three subagent sessions appear while the batch runs", |screen| {
        let text = screen.contents();
        text.contains("Subagents:") && text.contains("3 sessions")
    });
    pty.text("completed (review-result-33)");
    pty.text("Mock test complete");
    let rendered = pty.contents().split_whitespace().collect::<Vec<_>>().join(" ");
    for summary in [
        "fast-rust-count: completed (fast-result-11)",
        "slow-symbol-lookup: completed (slow-result-22)",
        "review-events-risk: completed (review-result-33)",
    ] {
        assert!(rendered.contains(summary), "{rendered}");
    }
    pty.ready();
    pty.input("\x03");
    pty.finish();
    assert!(mock_tui_session_jsonl(home.path()).is_empty());
}

#[test]
fn tui_mock_runs_keep_the_real_model_session_and_draft_bound_to_the_normal_agent() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = mock_tui_fixture_start(&fixture, home.path(), project.path());
    pty.ready_model("fixture/primary");
    pty.input("before mock\r");
    let (request, body) = pty.request_at(&fixture, api_path("completion"));
    assert_eq!(body["model"], "fixture-model");
    final_reply(request, "before-mock-reply");
    pty.text("before-mock-reply");
    pty.ready();
    let session = saved_session_id(home.path());

    pty.input("/test-ui\r");
    pty.wait_for("mock stream starts on the temporary snapshot", |screen| {
        screen.contents().contains("UI-TEST-ASSISTANT-START")
    });
    // 运行中键入的文本只能成为草稿，不能 steering 到 mock 或真实会话。
    pty.input("should-not-steer");
    pty.draft("should-not-steer");
    pty.text("UI-TEST-ASSISTANT-END");
    pty.ready_model("fixture/primary");
    pty.draft("should-not-steer");

    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("after mock\r");
    let (request, body) = pty.request_at(&fixture, api_path("completion"));
    assert_eq!(
        body["model"], "fixture-model",
        "ordinary task must keep using the saved model"
    );
    final_reply(request, "after-mock-reply");
    pty.text("after-mock-reply");
    pty.ready();
    pty.input("\x03");
    pty.finish();

    assert_eq!(saved_session_id(home.path()), session);
    let transcript = std::fs::read_to_string(
        std::fs::read_dir(home.path().join(".pp/agent/sessions"))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|extension| extension == "jsonl"))
            .unwrap(),
    )
    .unwrap();
    for marker in ["before mock", "after mock", "before-mock-reply", "after-mock-reply"] {
        assert!(transcript.contains(marker), "missing {marker}: {transcript}");
    }
    for marker in ["UI-TEST-ASSISTANT-START", "should-not-steer"] {
        assert!(
            !transcript.contains(marker),
            "mock-only text leaked into the real session: {marker}"
        );
    }
    no_more(&fixture);
}
