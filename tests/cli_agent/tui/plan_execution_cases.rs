use super::*;

// New-session execution start point: approval, execution and -r resume.
fn plan_tui_approval_case(fresh: bool) {
    let protocol = "response";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
    plan_enter(&mut pty, &fixture);
    pty.input("Plan the canary ORIGIN_ONLY_MARKER\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_final(
        request,
        protocol,
        "EXPLORATION_ONLY_MARKER: verified current code",
    );
    pty.ready();
    plan_drive_ready(
        &mut pty,
        &fixture,
        protocol,
        "Finalize the plan",
        PLAN_TUI_BODY,
    );
    let origin_id = saved_session_id(home.path());
    let path = plan_path(project.path());
    let approved = PLAN_TUI_BODY.replace("approved bytes", "reviewed bytes");
    std::fs::write(&path, &approved).unwrap();
    // The original review token cannot approve external bytes.
    plan_choose(&mut pty, usize::from(!fresh));
    plan_wait_review(&mut pty, 2);
    no_more(&fixture);
    assert!(!project.path().join("canary.txt").exists());
    plan_choose(&mut pty, usize::from(!fresh));
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    let encoded = body.to_string();
    assert!(encoded.contains("reviewed bytes"), "{body}");
    assert!(encoded.contains("[Approved Markdown snapshot]"), "{body}");
    assert_eq!(encoded.contains("ORIGIN_ONLY_MARKER"), !fresh, "{body}");
    assert_eq!(
        encoded.contains("EXPLORATION_ONLY_MARKER"),
        !fresh,
        "{body}"
    );
    assert_eq!(
        cancel_user_messages(&body, protocol).len(),
        if fresh { 1 } else { 3 }
    );
    let tools = body["tools"].as_array().unwrap();
    assert!(
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "write" || tool["name"] == "write")
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["function"]["name"] == "plan_search" || tool["name"] == "plan_search")
    );
    let ids = resume_boundary_session_ids(home.path());
    assert_eq!(ids.len(), if fresh { 2 } else { 1 });
    let execution_id = if fresh {
        ids.iter().find(|id| **id != origin_id).unwrap().clone()
    } else {
        origin_id.clone()
    };
    if fresh {
        assert!(cancel_tool_outputs(&body, protocol).is_empty());
        let source = plan_journal(home.path(), &origin_id);
        let workflow = source
            .iter()
            .rev()
            .find(|record| record["customType"] == "pp.workflow")
            .unwrap();
        assert_eq!(workflow["data"]["workflow"]["mode"], "plan");
        assert_eq!(workflow["data"]["workflow"]["plan"]["status"], "executed");
    } else {
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 4);
        assert_eq!(
            outputs[2].1["error"],
            "Not executed: plan submitted for review."
        );
        assert_eq!(
            outputs[3].1["error"],
            "Not executed: plan submitted for review."
        );
    }
    plan_calls(
        request,
        protocol,
        json!([
            plan_call(
                protocol,
                "execute-write",
                "write",
                json!({"path":"canary.txt","content":"reviewed bytes"})
            ),
            plan_call(
                protocol,
                "execute-read",
                "read",
                json!({"path":"canary.txt"})
            )
        ]),
    );
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert_eq!(
        std::fs::read(project.path().join("canary.txt")).unwrap(),
        b"reviewed bytes"
    );
    assert_eq!(
        cancel_tool_outputs(&body, protocol).last().unwrap().1["content"],
        "reviewed bytes"
    );
    // After approval the body has moved from drafts to plans; take the path again from its current location.
    let path = plan_path(project.path());
    let relative = path.strip_prefix(project.path()).unwrap().to_str().unwrap();
    plan_calls(
        request,
        protocol,
        json!([plan_call(
            protocol,
            "execute-progress",
            "edit",
            json!({"path":relative,"edits":[{"oldText":"- [ ] Create canary.txt","newText":"- [x] Create canary.txt"}]})
        )]),
    );
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    // The provider is still holding an execution request while local cached view opens.
    pty.input("/plan view\r");
    pty.wait_for("cached verified execution progress", |screen| {
        let text = screen.contents();
        text.contains("1/2") && text.contains('☑') && text.contains('☐')
    });
    plan_wait_execute(&mut pty);
    no_more(&fixture);
    pty.input("\x1b");
    plan_calls(
        request,
        protocol,
        json!([plan_call(
            protocol,
            "execute-failure",
            "bash",
            json!({"command":"exit 7"})
        )]),
    );
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert_eq!(
        cancel_tool_outputs(&body, protocol).last().unwrap().1["exit_code"],
        7
    );
    pty.input("\x1b");
    cancel_wait_cancelled_ready(&mut pty);
    drop(request);
    let latest = std::fs::read_to_string(&path).unwrap();
    assert!(latest.contains("- [x] Create canary.txt"));
    assert!(latest.contains("- [ ] Verify the failure boundary"));
    let journal = plan_journal(home.path(), &execution_id);
    let approval = journal
        .iter()
        .filter(|record| record["type"] == "message" && record["message"]["role"] == "user")
        .find(|record| {
            record["message"]["content"]
                .as_str()
                .is_some_and(|text| text.contains("[Approved Markdown snapshot]"))
        })
        .unwrap();
    assert!(
        approval["message"]["content"]
            .as_str()
            .unwrap()
            .contains("- [ ] Create canary.txt")
    );
    assert!(
        !approval["message"]["content"]
            .as_str()
            .unwrap()
            .contains("- [x] Create canary.txt")
    );
    pty.input("\x03");
    pty.finish();
    let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &execution_id]);
    resumed.ready();
    plan_wait_execute(&mut resumed);
    resumed.input("/plan view\r");
    resumed.wait_for("restored partial execution progress", |screen| {
        screen.contents().contains("1/2")
    });
    no_more(&fixture);
    resumed.input("\x03");
    resumed.finish();
}

#[test]
fn plan_tui_fresh_execution_uses_reviewed_bytes_and_tracks_only_verified_progress() {
    plan_tui_approval_case(true);
}

#[test]
fn plan_tui_rejects_real_side_effects_but_reads_searches_and_asks() {
    let protocol = "completion";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("canary.txt"), "original bytes").unwrap();
    std::fs::create_dir(project.path().join("src")).unwrap();
    std::fs::write(
        project.path().join("src/agent.rs"),
        "first\nneedle here\nlast\n",
    )
    .unwrap();
    let fake = project.path().join(".plan-fake-bin");
    std::fs::create_dir(&fake).unwrap();
    for name in ["rg", "fd"] {
        let path = fake.join(name);
        std::fs::write(&path, "#!/bin/sh\ntouch forbidden-native-search\nexit 1\n").unwrap();
        std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
    }
    std::fs::write(
        project.path().join(".ignore"),
        "# custom project ignore rules\nignored/\n-exec\n",
    )
    .unwrap();
    let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
    plan_enter(&mut pty, &fixture);
    pty.input("Research only, never overwrite the canary\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    let malicious = [
        (
            "deny-write",
            "write",
            json!({"path":"canary.txt","content":"wrong"}),
        ),
        (
            "deny-edit",
            "edit",
            json!({"path":"canary.txt","edits":[{"oldText":"original bytes","newText":"wrong"}]}),
        ),
        (
            "deny-bash",
            "bash",
            json!({"command":"touch forbidden-process"}),
        ),
        (
            "deny-background",
            "bash",
            json!({"command":"touch forbidden-process","mode":"background"}),
        ),
        (
            "deny-task",
            "bash_task",
            json!({"task_id":"unknown","action":"status"}),
        ),
        (
            "deny-rg",
            "plan_search",
            json!({"query":"needle","path":"src/agent.rs"}),
        ),
        (
            "deny-find",
            "find",
            json!({"args":[".","-exec","touch","forbidden-process",";"]}),
        ),
        (
            "deny-mcp",
            "mcp__unseen__write",
            json!({"path":"canary.txt","content":"wrong"}),
        ),
        ("research-read", "read", json!({"path":"src/agent.rs"})),
        (
            "research-search",
            "rg",
            json!({"pattern":"needle","path":"src"}),
        ),
        ("research-find", "glob", json!({"pattern":"src/agent.rs"})),
    ];
    plan_calls(
        request,
        protocol,
        Value::Array(
            malicious
                .into_iter()
                .map(|(id, name, args)| plan_call(protocol, id, name, args))
                .collect(),
        ),
    );
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    let outputs = cancel_tool_outputs(&body, protocol);
    assert_eq!(outputs.len(), 11);
    for (_, result) in &outputs[..8] {
        assert_eq!(
            *result,
            json!({"ok":false,"error":"Tool is not available in Plan mode."})
        );
    }
    assert_eq!(outputs[8].1["content"], "first\nneedle here\nlast\n");
    assert!(
        outputs[9].1["content"]
            .as_str()
            .unwrap()
            .contains("*2|needle here")
    );
    assert!(
        outputs[10].1["content"]
            .as_str()
            .unwrap()
            .contains("agent.rs")
    );
    assert_eq!(
        std::fs::read(project.path().join("canary.txt")).unwrap(),
        b"original bytes"
    );
    assert!(!project.path().join("forbidden-process").exists());
    assert!(!project.path().join("forbidden-native-search").exists());
    assert!(!project.path().join(".pp-cache").exists());
    plan_calls(
        request,
        protocol,
        json!([plan_call(
            protocol,
            "research-ask",
            "ask",
            ask_single_arguments()
        )]),
    );
    ask_wait_panel(&mut pty, "Which database should the project use?");
    pty.input("\r");
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert_eq!(
        cancel_tool_outputs(&body, protocol).last().unwrap().1["ok"],
        true
    );
    plan_final(request, protocol, "Research done, no saved plan yet");
    pty.ready();
    plan_wait_mode(&mut pty, "Plan");
    pty.input("/quit\r");
    pty.finish();
    no_more(&fixture);
}
