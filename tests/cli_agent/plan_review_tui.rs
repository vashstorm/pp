const PLAN_REVIEW_REPORT: &str = "## Major — verify failure boundary\n\nPlan item 2 lacks the exact failure input. Evidence: `evidence.rs::planned` currently returns 1. Impact: the acceptance check is ambiguous. Recommendation: specify a failing input and expected result. REVIEW_REPORT_ONLY_MARKER\n\nStatic reading only; no builds or tests were run.\n";

fn plan_wait_report(pty: &mut Pty, marker: &str, outdated: bool) {
    pty.wait_for("complete revision-bound review report", |screen| {
        let text = screen.contents();
        text.contains(marker)
            && text.contains("fixture/primary")
            && text.contains("outdated") == outdated
            && text.contains("Execute with context")
    });
}

// 审批动作 0/1/4 各自独立：新会话执行、保留上下文执行、恢复后返回讨论。
fn plan_review_case(action: usize) {
    for protocol in ["completion", "response"] {
        let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let remote = super::mcp_remote::fixture::RemoteServer::new("json");
            let mut config = remote.config("http", 3000);
            config["mcpServers"]["remote"]["read_only_tools"] = json!(["echo"]);
            std::fs::write(config_path(home.path(), "mcp.json"), config.to_string()).unwrap();
            std::fs::write(
                project.path().join("evidence.rs"),
                "fn planned() -> u8 { 1 }\n",
            )
            .unwrap();
            system_prompt::write(project.path(), ".pp/SYSTEM_TEMPLATE.md", system_prompt::LIVE_TEMPLATE);
            system_prompt::write(project.path(), ".pp/APPEND_SYSTEM.md", "plan-append-once");
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            plan_enter(&mut pty, &fixture);
            pty.input("Plan creation ORIGIN_REVIEW_MARKER\r");
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            system_prompt::assert_live(&body, protocol, "plan");
            assert!(body["tools"].to_string().contains("mcp__remote__echo"));
            assert!(!body["tools"].to_string().contains("mcp__remote__fail"));
            plan_calls(request, protocol, json!([
                plan_call(protocol, "plan-remote-read", "mcp__remote__echo", json!({"text":"plan public evidence"})),
                plan_call(protocol, "plan-remote-forbidden", "mcp__remote__fail", json!({}))
            ]));
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let outputs = cancel_tool_outputs(&body, protocol);
            assert_eq!(outputs[0].1["result"]["content"][0]["text"], "plan public evidence");
            assert_eq!(outputs[1].1["error"], "Tool is not available in Plan mode.");
            plan_final(
                request,
                protocol,
                "PLANNING_CHAT_ONLY_MARKER static exploration",
            );
            pty.ready();
            plan_drive_ready_with_mcp(
                &mut pty,
                &fixture,
                protocol,
                "Finalize independent plan",
                PLAN_TUI_BODY,
                &["mcp__remote__echo"],
            );
            if action == 4 {
                let id = saved_session_id(home.path());
                pty.input("\x03");
                pty.finish();
                pty = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
                pty.ready();
                plan_wait_mode(&mut pty, "Plan");
                pty.input("/plan view\r");
                plan_wait_review(&mut pty, 1);
            }
            let path = plan_path(project.path());
            let original = std::fs::read(&path).unwrap();
            plan_choose(&mut pty, 3);
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            system_prompt::assert_live(&body, protocol, "review");
            let system = project_instructions::system(&body, protocol);
            assert_eq!(system.matches("plan-append-once").count(), 1);
            assert!(system.find("plan-append-once").unwrap() < system.find("PLAN REVIEW MODE").unwrap());
            let encoded = body.to_string();
            assert!(encoded.contains("ORIGIN_REVIEW_MARKER"));
            assert!(encoded.contains("Create canary.txt"));
            assert!(!encoded.contains("PLANNING_CHAT_ONLY_MARKER"));
            assert_eq!(cancel_user_messages(&body, protocol).len(), 1);
            let names: Vec<_> = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .or_else(|| tool["name"].as_str())
                        .unwrap()
                })
                .collect();
            assert_eq!(names.len(), 5);
            assert!(
                names.iter().all(|name| matches!(
                    *name,
                    "read" | "grep" | "glob" | "lsp" | "mcp__remote__echo"
                ))
            );
            let calls = [
                ("review-ask", "ask", ask_single_arguments()),
                (
                    "review-plan-write",
                    "write",
                    json!({"path":".pp/drafts/create-canary.md","content":"- [x] wrong"}),
                ),
                (
                    "review-submit",
                    "plan_submit",
                    json!({"expected_revision":1}),
                ),
                (
                    "review-write",
                    "write",
                    json!({"path":"canary.txt","content":"wrong"}),
                ),
                (
                    "review-edit",
                    "edit",
                    json!({"path":path.strip_prefix(project.path()).unwrap(),"edits":[{"oldText":"- [ ]","newText":"- [x]"}]}),
                ),
                (
                    "review-shell",
                    "bash",
                    json!({"command":"touch review-forbidden"}),
                ),
                (
                    "review-grep",
                    "plan_search",
                    json!({"query":"planned","path":"evidence.rs"}),
                ),
                (
                    "review-find",
                    "find",
                    json!({"args":[".","-exec","touch","review-forbidden",";"]}),
                ),
                ("review-mcp", "mcp__remote__fail", json!({})),
                ("review-read", "read", json!({"path":"evidence.rs"})),
                (
                    "review-search",
                    "grep",
                    json!({"pattern":"planned","path":"evidence.rs"}),
                ),
                (
                    "review-remote-read",
                    "mcp__remote__echo",
                    json!({"text":"review public evidence"}),
                ),
            ];
            plan_calls(
                request,
                protocol,
                Value::Array(
                    calls
                        .into_iter()
                        .map(|(id, name, args)| plan_call(protocol, id, name, args))
                        .collect(),
                ),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            system_prompt::assert_live(&body, protocol, "review");
            let outputs = cancel_tool_outputs(&body, protocol);
            assert_eq!(outputs.len(), 12);
            for (_, result) in &outputs[..9] {
                assert_eq!(
                    *result,
                    json!({"ok":false,"error":"Tool is not available during Plan review."})
                );
            }
            assert_eq!(outputs[9].1["content"], "fn planned() -> u8 { 1 }\n");
            assert!(outputs[10].1["content"].as_str().unwrap().contains("*1|fn planned()"));
            assert_eq!(outputs[11].1["result"]["content"][0]["text"], "review public evidence");
            let calls: Vec<_> = remote.log().into_iter()
                .filter(|entry| entry["body"]["method"] == "tools/call").collect();
            assert_eq!(calls.len(), 2);
            assert!(calls.iter().all(|call| call["body"]["params"]["name"] == "echo"));
            assert_eq!(std::fs::read(&path).unwrap(), original);
            assert!(!project.path().join("canary.txt").exists());
            assert!(!project.path().join("review-forbidden").exists());
            plan_final(request, protocol, PLAN_REVIEW_REPORT);
            plan_wait_report(&mut pty, "Major", false);
            plan_wait_mode(&mut pty, "Plan");
            no_more(&fixture);
            assert_eq!(std::fs::read(&path).unwrap(), original);
            if action == 4 {
                // 从报告正文反向切换到输入框，继续讨论。
                pty.input("\x1b[ZWhich failure input should item two use?\r");
            } else {
                plan_choose(&mut pty, action);
            }
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            system_prompt::assert_live(&body, protocol, if action == 4 { "plan" } else { "execute" });
            assert!(!body.to_string().contains("review-read"));
            assert!(!body.to_string().contains("review-search"));
            assert!(!body.to_string().contains("review public evidence"));
            assert_eq!(body["tools"].to_string().contains("mcp__remote__fail"), action != 4);
            assert!(body.to_string().contains("PLANNING_CHAT_ONLY_MARKER") == (action != 0));
            assert!(body.to_string().contains("REVIEW_REPORT_ONLY_MARKER") == (action == 4));
            if action == 4 {
                let messages = cancel_user_messages(&body, protocol);
                let last = messages.last().unwrap();
                assert!(last.contains("Which failure input should item two use?"));
                assert!(last.contains(PLAN_REVIEW_REPORT));
                plan_final(
                    request,
                    protocol,
                    "Discussing the advisory finding without changing the plan",
                );
                pty.ready();
                plan_wait_mode(&mut pty, "Plan");
                assert_eq!(std::fs::read(&path).unwrap(), original);
            } else {
                plan_calls(
                    request,
                    protocol,
                    json!([
                        plan_call(
                            protocol,
                            "advisory-write",
                            "write",
                            json!({"path":"canary.txt","content":"approved bytes"})
                        ),
                        plan_call(
                            protocol,
                            "advisory-read",
                            "read",
                            json!({"path":"canary.txt"})
                        )
                    ]),
                );
                let (request, body) = pty.request_at(&fixture, api_path(protocol));
                assert_eq!(
                    cancel_tool_outputs(&body, protocol).last().unwrap().1["content"],
                    "approved bytes"
                );
                plan_calls(
                    request,
                    protocol,
                    json!([plan_call(
                        protocol,
                        "advisory-progress",
                        "edit",
                        json!({"path":path.strip_prefix(project.path()).unwrap(),"edits":[{"oldText":"- [ ] Create canary.txt","newText":"- [x] Create canary.txt"}]})
                    )]),
                );
                let (request, _) = pty.request_at(&fixture, api_path(protocol));
                plan_final(
                    request,
                    protocol,
                    "Executed despite the advisory Major finding. Shall we continue with the remaining plan items?",
                );
                pty.ready();
                plan_wait_execute(&mut pty);
                assert_eq!(
                    std::fs::read(project.path().join("canary.txt")).unwrap(),
                    b"approved bytes"
                );
            }
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_review_readonly_report_never_blocks_fresh_execution() {
    plan_review_case(0);
}

#[test]
fn plan_review_readonly_report_never_blocks_context_execution() {
    plan_review_case(1);
}

#[test]
fn plan_review_report_resume_discusses_without_changing_plan() {
    plan_review_case(4);
}

#[test]
fn plan_review_tui_binds_reports_to_revision_and_restores_without_replaying() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        plan_drive_ready(
            &mut pty,
            &fixture,
            protocol,
            "Plan for review lifecycle",
            PLAN_TUI_BODY,
        );
        let path = plan_path(project.path());
        plan_choose(&mut pty, 3);
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_final(
            request,
            protocol,
            "Review ONE: no substantiated issue. Static reading only.",
        );
        plan_wait_report(&mut pty, "Review ONE", false);
        plan_choose(&mut pty, 3);
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        std::fs::write(&path, format!("{PLAN_TUI_BODY}\nExternal clarification\n")).unwrap();
        plan_final(
            request,
            protocol,
            "Review TWO: static opinion for the old revision.",
        );
        plan_wait_report(&mut pty, "Review TWO", true);
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .matches("- [x]")
                .count(),
            0
        );
        no_more(&fixture);
        plan_choose(&mut pty, 3);
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.input("preserved reviewer draft");
        pty.draft("preserved reviewer draft");
        pty.input("\r");
        pty.wait_for("review rejects draft submission", |screen| {
            screen.contents().contains("Plan review is running")
        });
        pty.draft("preserved reviewer draft");
        pty.input("\x1b");
        plan_wait_review(&mut pty, 2);
        pty.input("\x1b[C");
        plan_wait_report(&mut pty, "Review TWO", true);
        drop(request);
        no_more(&fixture);
        let id = saved_session_id(home.path());
        let before = std::fs::read(&path).unwrap();
        pty.input("\x03");
        pty.finish();
        let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        resumed.ready();
        plan_wait_mode(&mut resumed, "Plan");
        no_more(&fixture);
        resumed.input("/plan view\r");
        plan_wait_review(&mut resumed, 2);
        resumed.input("\x1b[C");
        plan_wait_report(&mut resumed, "Review TWO", true);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        no_more(&fixture);
        resumed.input("\x03");
        resumed.finish();
    }
}
