use super::*;

// Execute or back out after compaction: each outcome is a separate session, split per case for parallel runs and easier failure isolation.
fn plan_tui_compact_then_execute_case(outcome: &str) {
    let protocol = "response";
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
    plan_enter(&mut pty, &fixture);
    pty.input("Research OLD_PLAN_CONTEXT\r");
    let (request, _) = pty.request_at(&fixture, api_path(protocol));
    plan_final(request, protocol, "OLD_RESEARCH_ONLY_MARKER");
    pty.ready();
    plan_drive_ready(
        &mut pty,
        &fixture,
        protocol,
        "Finalize the plan",
        PLAN_TUI_BODY,
    );
    let id = saved_session_id(home.path());
    let path = plan_path(project.path());
    plan_choose(&mut pty, 2);
    let (request, summary) = pty.request_at(&fixture, api_path(protocol));
    assert!(
        summary
            .to_string()
            .contains("Summarize the preceding conversation"),
        "{summary}"
    );
    assert_eq!(summary["tool_choice"], "none", "{summary}");
    plan_wait_mode(&mut pty, "Plan");
    match outcome {
        "failure" => request
            .respond(Response::builder().status(500).body(Body::empty()).unwrap())
            .unwrap(),
        "cancel" => {
            pty.input("\x1b");
            pty.wait_for(
                "cancelling the compaction restores the plan menu",
                |screen| {
                    screen.contents().contains("Plan execution cancelled")
                        || screen.contents().contains("cancelled")
                },
            );
            drop(request);
        }
        "unchanged" => plan_final(request, protocol, &"verbose summary ".repeat(500)),
        "changed" => {
            std::fs::write(&path, format!("{PLAN_TUI_BODY}\nExternal revision\n")).unwrap();
            plan_final(request, protocol, "SUMMARY_OF_OLD_PLAN_CONTEXT");
        }
        _ => plan_final(request, protocol, "SUMMARY_OF_OLD_PLAN_CONTEXT"),
    }
    if outcome == "success" {
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let encoded = body.to_string();
        assert!(encoded.contains("SUMMARY_OF_OLD_PLAN_CONTEXT"), "{body}");
        assert!(!encoded.contains("OLD_RESEARCH_ONLY_MARKER"), "{body}");
        assert!(encoded.contains("[Approved Markdown snapshot]"), "{body}");
        assert!(encoded.contains("approved bytes"), "{body}");
        assert_eq!(saved_session_id(home.path()), id);
        // The final answer explicitly waits on the user: the approval task is still unchecked, so the wait acknowledgement blocks the completion notice's extra request.
        plan_final(
            request,
            protocol,
            "Execution finished with tasks still open. Which task should I resume first?",
        );
        pty.ready();
        plan_wait_execute(&mut pty);
    } else {
        plan_wait_review(&mut pty, if outcome == "changed" { 2 } else { 1 });
        plan_wait_mode(&mut pty, "Plan");
        assert!(!project.path().join("canary.txt").exists());
    }
    no_more(&fixture);
    pty.input("\x03");
    pty.finish();
}

#[test]
fn plan_tui_compact_success_executes_with_summary() {
    plan_tui_compact_then_execute_case("success");
}
