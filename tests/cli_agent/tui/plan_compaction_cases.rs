use super::*;

// 压缩后执行或回退：每个 outcome 是独立会话，按用例拆分以便并行与定位失败。
fn plan_tui_compact_then_execute_case(outcome: &str) {
    for protocol in ["completion", "response"] {
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
        assert!(
            summary
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty)),
            "{summary}"
        );
        plan_wait_mode(&mut pty, "Plan");
        match outcome {
            "failure" => request
                .respond(Response::empty(tiny_http::StatusCode(500)))
                .unwrap(),
            "cancel" => {
                pty.input("\x1b");
                pty.wait_for("取消压缩并恢复计划菜单", |screen| {
                    screen.contents().contains("Plan execution cancelled")
                        || screen.contents().contains("cancelled")
                });
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
            // 终答显式等待用户：批准任务仍未勾选，等待识别阻止完成提醒追加请求。
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
}

#[test]
fn plan_tui_compact_success_executes_with_summary() {
    plan_tui_compact_then_execute_case("success");
}

#[test]
fn plan_tui_compact_failure_keeps_plan_without_execution() {
    plan_tui_compact_then_execute_case("failure");
}

#[test]
fn plan_tui_compact_cancel_keeps_plan_without_execution() {
    plan_tui_compact_then_execute_case("cancel");
}

#[test]
fn plan_tui_compact_external_revision_keeps_plan_without_execution() {
    plan_tui_compact_then_execute_case("changed");
}

#[test]
fn plan_tui_compact_oversized_summary_keeps_plan_without_execution() {
    plan_tui_compact_then_execute_case("unchanged");
}
