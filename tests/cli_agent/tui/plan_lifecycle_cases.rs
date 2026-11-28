use super::*;

#[test]
fn plan_tui_model_compaction_new_and_tree_never_restore_execute_authority() {
    let protocol = "response";
    let other = if protocol == "completion" {
        "response"
    } else {
        "completion"
    };
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let base = format!("http://{}/v1", fixture.addr());
    crate::write_settings(
        home.path(),
        r#"{"default_model":"first/primary","notification_timeout_seconds":1}"#,
    )
    .unwrap();
    std::fs::write(config_path(home.path(),"models.yml"),json!({"providers":{
            "first": {"protocol":protocol,"api_key":"fixture-key","base_url":base,"models":{"primary": {"model":"first-model"}}},
            "second": {"protocol":other,"api_key":"fixture-key","base_url":base,"models":{"primary": {"model":"second-model"}}}}}).to_string()).unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("Execute history before entering Plan\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_final(request, protocol, "Old Execute history");
    pty.completed_after("Old Execute history");
    pty.ready();
    plan_enter(&mut pty, &fixture);
    plan_drive_ready(
        &mut pty,
        &fixture,
        protocol,
        "Plan before rewinding history",
        PLAN_TUI_BODY,
    );
    let path = plan_path(project.path());
    pty.input("\x1b");
    pty.wait_for("closing review for tree", |screen| {
        !screen.contents().contains("Execute with context")
    });
    tree_tui_open(&mut pty, "Plan submit");
    tree_tui_rewind(
        &mut pty,
        &[TREE_TUI_HOME],
        "Execute history before entering Plan",
    );
    plan_wait_mode(&mut pty, "Plan");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAN_TUI_BODY);
    pty.input("\x01\x0b/new\r");
    pty.wait_for("new session welcome after reset", |screen| {
        screen.contents().contains("▄▄▄██▀▀██") && input_is_ready(screen)
    });
    plan_wait_mode(&mut pty, "Plan");
    pty.input("/plan view\r");
    pty.draft("");
    assert!(!pty.parser.screen().contents().contains("No saved plan."));
    no_more(&fixture);
    pty.input("/model second/primary\r");
    pty.ready_model("second/primary");
    plan_wait_mode(&mut pty, "Plan");
    pty.input("Research after model change\r");
    let (request, body) = pty.request_at(&fixture, api_path(other));
    assert!(body.to_string().contains("PLAN MODE"));
    plan_calls(
        request,
        other,
        json!([plan_call(
            other,
            "switched-denied",
            "write",
            json!({"path":"unauthorized","content":"wrong"})
        )]),
    );
    let (request, body) = pty.request_at(&fixture, api_path(other));
    assert_eq!(
        cancel_tool_outputs(&body, other).last().unwrap().1["error"],
        "Tool is not available in Plan mode."
    );
    plan_final(request, other, &"old static evidence ".repeat(500));
    pty.ready();
    pty.input("Second research exchange\r");
    let (request, _) = pty.request_at(&fixture, api_path(other));
    plan_final(request, other, "Recent verified static facts");
    pty.completed_after("Recent verified static facts");
    pty.input("/compact\r");
    let (request, body) = pty.request_at(&fixture, api_path(other));
    assert!(
        body["tools"].is_null()
            || body["tools"].as_array().is_some_and(Vec::is_empty)
            || body["tool_choice"] == "none",
        "{body}"
    );
    plan_final(
        request,
        other,
        "Static research summary; implementation remains unapproved.",
    );
    pty.wait_for("manual Plan compaction finished", |screen| {
        screen.contents().contains("Context compacted:")
    });
    pty.ready();
    plan_wait_mode(&mut pty, "Plan");
    pty.input("Research after compaction\r");
    let (request, body) = pty.request_at(&fixture, api_path(other));
    assert!(body.to_string().contains("PLAN MODE"));
    plan_calls(
        request,
        other,
        json!([plan_call(
            other,
            "compact-denied",
            "bash",
            json!({"command":"touch unauthorized"})
        )]),
    );
    let (request, body) = pty.request_at(&fixture, api_path(other));
    assert_eq!(
        cancel_tool_outputs(&body, other).last().unwrap().1["error"],
        "Tool is not available in Plan mode."
    );
    plan_final(request, other, "Still Plan after every lifecycle change");
    pty.ready();
    assert!(!project.path().join("unauthorized").exists());
    assert_eq!(std::fs::read_to_string(path).unwrap(), PLAN_TUI_BODY);
    pty.input("\x03");
    pty.finish();
    no_more(&fixture);
}

// Plan and Execute share the same set of MCP processes and tools.
fn plan_mcp_initialized(dir: &Path) -> usize {
    cancel_mcp_log(dir)
        .iter()
        .filter(|entry| entry["method"] == "initialize")
        .count()
}

fn plan_mcp_tool_names<'a>(body: &'a Value, protocol: &str) -> Vec<&'a str> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| {
            let definition = if protocol == "completion" {
                &tool["function"]
            } else {
                tool
            };
            definition["name"].as_str()
        })
        .collect()
}

fn plan_mcp_session(
    protocol: &str,
) -> (Arc<TestServer>, tempfile::TempDir, tempfile::TempDir, Pty) {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut settings = super::super::mcp::settings("");
    settings["notification_timeout_seconds"] = json!(1);
    crate::write_settings(home.path(), settings.to_string()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        response_models(&fixture, Some(protocol)),
    )
    .unwrap();
    let pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
    (fixture, home, project, pty)
}

// Processes stay resident: entering Plan keeps them connected, keep-context execution reuses the same connections, and they are only reaped on exit.
fn plan_mcp_resident_case() {
    let protocol = "response";
    let (fixture, _home, project, mut pty) = plan_mcp_session(protocol);
    pty.ready();
    // First Execute request: the MCP tool is in the table and the fixture handshakes only once.
    pty.input("Execute before Plan\r");
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert!(
        plan_mcp_tool_names(&body, protocol).contains(&"mcp__local__echo"),
        "{body}"
    );
    plan_final(request, protocol, "Execute ready");
    pty.completed_after("Execute ready");
    pty.ready();
    assert_eq!(plan_mcp_initialized(project.path()), 1);
    plan_enter(&mut pty, &fixture);
    // Entering Plan no longer disconnects the process: the PID is alive and there is no new handshake.
    cancel_mcp_assert_pid_alive(&project.path().join("mcp-log.pid"));
    assert_eq!(plan_mcp_initialized(project.path()), 1);
    // Tools without a read-only marker also enter the Plan tool table.
    plan_drive_ready_with_mcp(
        &mut pty,
        &fixture,
        protocol,
        "Plan before execution",
        PLAN_TUI_BODY,
        &["mcp__local__echo", "mcp__local__fail"],
    );
    cancel_mcp_assert_pid_alive(&project.path().join("mcp-log.pid"));
    assert_eq!(plan_mcp_initialized(project.path()), 1);
    // Keep-context execution keeps reusing the same connections.
    plan_choose(&mut pty, 1);
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert!(
        plan_mcp_tool_names(&body, protocol).contains(&"mcp__local__echo"),
        "{body}"
    );
    plan_calls(
        request,
        protocol,
        json!([plan_call(
            protocol,
            "execute-mcp",
            "mcp__local__echo",
            json!({"text":"approved tool call"})
        )]),
    );
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    let outputs = cancel_tool_outputs(&body, protocol);
    let mcp = outputs.iter().find(|(id, _)| id == "execute-mcp").unwrap();
    assert_eq!(mcp.1["ok"], true);
    assert_eq!(mcp.1["result"]["content"][0]["text"], "中文结果");
    // Execution still uses the same handshake and the same process; the unfinished follow-up request is cancelled afterwards.
    assert_eq!(plan_mcp_initialized(project.path()), 1);
    cancel_mcp_assert_pid_alive(&project.path().join("mcp-log.pid"));
    pty.input("\x03");
    pty.finish();
    drop(request);
    assert_eq!(plan_mcp_initialized(project.path()), 1);
    cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
}

// Process boundary: restoring a Plan rebuilds the MCP connections, and switching to Execute keeps reusing them.
fn plan_mcp_process_boundary_case(off: bool) {
    let protocol = "response";
    let (fixture, home, project, mut pty) = plan_mcp_session(protocol);
    pty.ready();
    plan_enter(&mut pty, &fixture);
    // The connection the launch process established under Execute is still alive after entering Plan.
    cancel_mcp_assert_pid_alive(&project.path().join("mcp-log.pid"));
    plan_drive_ready_with_mcp(
        &mut pty,
        &fixture,
        protocol,
        "Plan before MCP reconnection",
        PLAN_TUI_BODY,
        &["mcp__local__echo", "mcp__local__fail"],
    );
    assert_eq!(plan_mcp_initialized(project.path()), 1);
    let id = saved_session_id(home.path());
    pty.input("\x03");
    pty.finish();
    cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
    let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo", "-r", &id]);
    pty.ready();
    plan_wait_mode(&mut pty, "Plan");
    no_more(&fixture);
    pty.input("/plan view\r");
    plan_wait_review(&mut pty, 1);
    no_more(&fixture);
    pty.input("\x1b");
    pty.wait_for("closing review before checking MCP status", |screen| {
        !screen.contents().contains("Execute with context")
    });
    pty.input("/mcp list\r");
    pty.wait_for("restored Plan MCP connection", |screen| {
        screen.rows(0, 100).any(|row| {
            row.contains("local") && row.contains("connected") && !row.contains("not connected")
        })
    });
    assert_eq!(plan_mcp_initialized(project.path()), 2);
    pty.input("\x1b");
    pty.wait_for("closing MCP status", |screen| {
        !screen.contents().contains("connected")
    });
    if off {
        // A bare /plan switches straight out: no local confirmation dialog and the saved draft stays on disk.
        pty.input("/plan\r");
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        pty.input("Explicit normal execution after leaving Plan\r");
    } else {
        pty.input("/plan view\r");
        plan_wait_review(&mut pty, 1);
        plan_choose(&mut pty, 1);
    }
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    assert!(
        plan_mcp_tool_names(&body, protocol).contains(&"mcp__local__echo"),
        "{body}"
    );
    // Reuse the connection established while restoring the Plan.
    assert_eq!(plan_mcp_initialized(project.path()), 2);
    plan_calls(
        request,
        protocol,
        json!([
            plan_call(
                protocol,
                "execute-mcp",
                "mcp__local__echo",
                json!({"text":"approved tool call"})
            ),
            plan_call(
                protocol,
                "mcp-canary",
                "write",
                json!({"path":"canary.txt","content":"MCP handoff executed"})
            )
        ]),
    );
    let (request, body) = pty.request_at(&fixture, api_path(protocol));
    let outputs = cancel_tool_outputs(&body, protocol);
    let mcp = outputs.iter().find(|(id, _)| id == "execute-mcp").unwrap();
    assert_eq!(mcp.1["ok"], true);
    assert_eq!(mcp.1["result"]["content"][0]["text"], "中文结果");
    assert_eq!(
        std::fs::read(project.path().join("canary.txt")).unwrap(),
        b"MCP handoff executed"
    );
    pty.input("\x03");
    pty.finish();
    drop(request);
    cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
}

#[test]
fn plan_completion_requires_success_and_approved_tasks() {
    let protocol = "response";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
    plan_enter(&mut pty, &fixture);
    pty.input("Plan the canary\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_final(request, protocol, "Findings verified.");
    pty.ready();
    plan_drive_ready(
        &mut pty,
        &fixture,
        protocol,
        "Finalize the plan",
        PLAN_TUI_BODY,
    );
    plan_choose(&mut pty, 0);
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_calls(
        request,
        protocol,
        json!([
            plan_call(
                protocol,
                "execute-write",
                "write",
                json!({"path":"canary.txt","content":"approved bytes"})
            ),
            plan_call(
                protocol,
                "execute-read",
                "read",
                json!({"path":"canary.txt"})
            )
        ]),
    );
    let plan_relative = ".pp/plans/create-canary.md";
    let tick = |id: &str, old: &str| {
        plan_call(
            protocol,
            id,
            "edit",
            json!({"path":plan_relative,"edits":[{"oldText":old,"newText":old.replace("- [ ]","- [x]")}]}),
        )
    };
    // Tick both tasks in the same batch: plan edits within one batch sync the real body only once after the batch.
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_calls(
        request,
        protocol,
        json!([
            tick("progress-create", "- [ ] Create canary.txt"),
            tick("progress-verify", "- [ ] Verify the failure boundary")
        ]),
    );
    let plans_path = project.path().join(".pp/plans/create-canary.md");

    // All checked but a final error: a mid-stream error fails the task, so it is not archived.
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    {
        let mut stream = request.start_sse().unwrap();
        if protocol == "completion" {
            stream
                .event(&json!({
                    "error":{"message":"fixture stream error","type":"server_error"}
                }))
                .unwrap();
        } else {
            stream
                .event(&json!({
                    "type":"error","sequence_number":1,"code":"server_error",
                    "message":"fixture stream error","param":null
                }))
                .unwrap();
        }
        stream.finish();
    }
    pty.ready();
    assert!(plans_path.exists());
    assert!(
        !project
            .path()
            .join(".pp/done")
            .join("create-canary.md")
            .exists()
    );
    pty.input("/plan view\r");
    pty.wait_for("failed task keeps executed status", |screen| {
        let text = screen.contents();
        screen.alternate_screen()
            && text.contains("Read-only view")
            && text.contains("Executed")
            && !text.contains("Completed")
    });
    pty.input("\x1b");
    pty.wait_for("closing executed plan before the next task", |screen| {
        !screen.alternate_screen() && input_is_ready(screen)
    });
    no_more(&fixture);

    // All checked but cancelled: likewise not archived.
    pty.input("Final check\r");
    let (_request, _) = pty.request_at(&fixture, api_path(protocol));
    pty.input("\x1b");
    cancel_wait_cancelled_ready(&mut pty);
    drop(_request);
    assert!(plans_path.exists());
    assert!(
        !project
            .path()
            .join(".pp/done")
            .join("create-canary.md")
            .exists()
    );
    no_more(&fixture);

    // Rewrite an approved task: the sync is rejected immediately after the edit lands, with no further model request.
    pty.input("Rewrite the second approved task\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_calls(
        request,
        protocol,
        json!([plan_call(
            protocol,
            "drift-rewrite",
            "edit",
            json!({"path":plan_relative,"edits":[{"oldText":"- [x] Verify the failure boundary","newText":"- [x] Verify the boundary twice"}]})
        )]),
    );
    pty.wait_for("rewriting an approved task is rejected", |screen| {
        screen.contents().contains("Plan tasks changed")
    });
    assert!(plans_path.exists());
    assert!(
        !project
            .path()
            .join(".pp/done")
            .join("create-canary.md")
            .exists()
    );
    no_more(&fixture);

    // Restoring the approved body (byte-for-byte) finishes successfully: an already-synced body archived even when it did not change again.
    let approved_checked = PLAN_TUI_BODY.replace("- [ ]", "- [x]");
    std::fs::write(&plans_path, &approved_checked).unwrap();
    assert_eq!(
        std::fs::read_to_string(&plans_path).unwrap(),
        approved_checked
    );
    pty.input("Recovered and completed all approved tasks\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_final(
        request,
        protocol,
        "Recovered and completed all approved tasks.",
    );
    pty.completed();
    assert!(!plans_path.exists());
    let archived =
        std::fs::read_to_string(project.path().join(".pp/done/create-canary.md")).unwrap();
    assert!(archived.contains("- [x] Create canary.txt"), "{archived}");
    assert!(
        archived.contains("- [x] Verify the failure boundary"),
        "{archived}"
    );
    pty.input("/plan view\r");
    pty.wait_for("archived after later success", |screen| {
        let text = screen.contents();
        text.contains("Completed") && text.contains(".pp/done/create-canary.md")
    });
    plan_wait_execute(&mut pty);
    pty.input("\x1b");
    pty.input("\x03");
    pty.finish();
}
