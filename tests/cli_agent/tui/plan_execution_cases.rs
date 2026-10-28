use super::*;

#[test]
fn plan_tui_approval_uses_reviewed_bytes_and_tracks_only_verified_progress() {
    for protocol in ["completion", "response"] {
        for fresh in [false, true] {
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
                    .any(|tool| tool["function"]["name"] == "plan_search"
                        || tool["name"] == "plan_search")
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
            // 批准后正文已从 drafts 移入 plans；按当前位置重新取路径。
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
            let mut resumed =
                Pty::start_with_args(project.path(), home.path(), &["-r", &execution_id]);
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
    }
}

#[test]
fn plan_tui_rejects_real_side_effects_but_reads_searches_and_asks() {
    for protocol in ["completion", "response"] {
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
                "deny-grep",
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
                "grep",
                json!({"pattern":"needle","path":"src"}),
            ),
            ("research-find", "glob", json!({"path":"src/agent.rs"})),
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
}

#[test]
fn plan_tui_pending_input_never_crosses_review_into_execute() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("Plan with pending input\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.input("/plan\r");
        pty.wait_for("busy mode switch refused", |screen| {
            cancel_normalize(screen).contains("CancelthecurrenttaskbeforechangingPlanmode.")
        });
        pty.input("\x01\x0bsteering requirement\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "pending-write",
                    "plan_write",
                    json!({"title":"Pending","file_name":"create-canary.md","content":PLAN_TUI_BODY,"expected_revision":0})
                ),
                plan_call(
                    protocol,
                    "pending-submit",
                    "plan_submit",
                    json!({"expected_revision":1})
                )
            ]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1["error"],
            "Apply pending input before submitting the plan."
        );
        assert_eq!(
            cancel_user_messages(&body, protocol).last().unwrap(),
            "steering requirement"
        );
        pty.input("followup research\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "pending-submit-again",
                "plan_submit",
                json!({"expected_revision":1})
            )]),
        );
        plan_wait_review(&mut pty, 1);
        no_more(&fixture);
        plan_choose(&mut pty, 1);
        no_more(&fixture);
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x1b");
        pty.wait_for("Plan panel closed before queue resume", |screen| {
            !screen.contents().contains("Execute (keep context)")
                && cancel_cursor_on_input_row(screen)
        });
        pty.function_key('r');
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol).last().unwrap(),
            "followup research"
        );
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "followup-denied",
                "write",
                json!({"path":"canary.txt","content":"unauthorized"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1["error"],
            "Tool is not available in Plan mode."
        );
        plan_final(request, protocol, "Followup researched without execution");
        pty.ready();
        plan_wait_mode(&mut pty, "Plan");
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_tui_discussion_preserves_main_draft_and_does_not_force_a_rewrite() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("Plan canary with discussion\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        pty.input("preserved main draft");
        pty.draft("preserved main draft");
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "discuss-write",
                    "plan_write",
                    json!({"title":"Canary","file_name":"create-canary.md","content":PLAN_TUI_BODY,"expected_revision":0})
                ),
                plan_call(
                    protocol,
                    "discuss-submit",
                    "plan_submit",
                    json!({"expected_revision":1})
                )
            ]),
        );
        plan_wait_review(&mut pty, 1);
        plan_choose(&mut pty, 4);
        pty.input("discard this question\x1b");
        plan_wait_review(&mut pty, 1);
        no_more(&fixture);
        pty.input("\r");
        pty.input("Why is file creation before failure verification?\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            cancel_user_messages(&body, protocol)
                .last()
                .unwrap()
                .contains("Why is file creation before failure verification?")
        );
        assert!(body.to_string().contains("Plan canary with discussion"));
        plan_final(
            request,
            protocol,
            "The first step establishes the file; the second checks failures. No rewrite needed.",
        );
        pty.ready();
        pty.draft("preserved main draft");
        plan_wait_mode(&mut pty, "Plan");
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            PLAN_TUI_BODY
        );
        let id = saved_session_id(home.path());
        let journal = plan_journal(home.path(), &id);
        let workflow = journal
            .iter()
            .rev()
            .find(|record| record["customType"] == "pp.workflow")
            .unwrap();
        assert_eq!(workflow["data"]["workflow"]["plan"]["status"], "draft");
        assert_eq!(workflow["data"]["workflow"]["plan"]["revision"], 1);
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_tui_ready_keeps_input_and_output_and_sends_discussion() {
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
            "Plan the canary",
            PLAN_TUI_BODY,
        );
        pty.input("\t继续讨论具体边界");
        pty.draft("继续讨论具体边界");
        let text = pty.parser.screen().contents();
        assert!(text.contains("Create canary"), "{text}");
        assert!(text.contains("❯ Execute in a new session"), "{text}");
        assert!(!text.contains("1. Execute"), "{text}");
        assert!(!text.contains("1–6 select"), "{text}");
        assert!(text.contains("Save draft and leave Plan mode"), "{text}");
        assert!(text.contains("┌ Plan"), "{text}");
        assert!(text.contains("╭ Actions"), "{text}");
        assert!(
            !text.contains("Plan ready · /plan view to review"),
            "{text}"
        );
        assert!(cancel_cursor_on_input_row(pty.parser.screen()), "{text}");
        pty.resize(24, 90);
        pty.draft("继续讨论具体边界");
        pty.input("\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol).last().unwrap(),
            "继续讨论具体边界"
        );
        assert!(!body.to_string().contains("[Approved Markdown snapshot]"));
        plan_final(request, protocol, "Discussion continued");
        pty.ready();
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            PLAN_TUI_BODY
        );
        assert!(!project.path().join("canary.txt").exists());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}
