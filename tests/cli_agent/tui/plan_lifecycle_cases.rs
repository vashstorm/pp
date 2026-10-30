use super::*;

#[test]
fn plan_tui_toc_navigates_view_without_executing() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let content = format!(
            "# Overview\n\n{}\n\n## Implementation\n\n{}\n\n## Validation\n\n{}",
            "- research detail\n".repeat(35),
            "- [ ] implementation detail\n".repeat(35),
            "- [ ] validation detail\n".repeat(35),
        );
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        plan_drive_ready(&mut pty, &fixture, protocol, "Plan with sections", &content);
        pty.input("\x1b");
        pty.wait_for("关闭计划面板", |screen| {
            !screen.contents().contains("TOC")
        });
        pty.input("/plan view\r");
        plan_wait_review(&mut pty, 1);
        pty.wait_for("计划目录与正文分栏", |screen| {
            screen.contents().contains("TOC")
                && screen.rows(0, 26).any(|row| row.contains("Implementation"))
                && !screen
                    .rows(28, 72)
                    .any(|row| row.contains("validation detail"))
        });
        pty.input("\t\t\t\x1b[F");
        pty.wait_for("选择目录末项", |screen| {
            screen.rows(0, 26).any(|row| row.contains("❯   Validation"))
        });
        no_more(&fixture);
        pty.input("\r");
        pty.wait_for("目录跳转至正文标题", |screen| {
            screen
                .rows(28, 72)
                .nth(2)
                .is_some_and(|row| row.starts_with("Validation"))
                && screen
                    .rows(28, 72)
                    .any(|row| row.contains("validation detail"))
                && screen.contents().contains("Execute in a new session")
        });
        no_more(&fixture);
        pty.resize(30, 50);
        pty.wait_for("窄窗口收起目录", |screen| {
            !screen.contents().contains("TOC") && screen.contents().contains("Plan")
        });
        pty.resize(30, 100);
        pty.wait_for("恢复目录分栏", |screen| {
            screen.contents().contains("TOC")
        });
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            content
        );
        assert!(!project.path().join("forbidden-after-submit").exists());
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn plan_tui_model_compaction_new_and_tree_never_restore_execute_authority() {
    for protocol in ["completion", "response"] {
        let other = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let base = format!("http://{}/v1", fixture.server_addr());
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
            !screen.contents().contains("Execute (keep context)")
        });
        tree_tui_open(&mut pty, "plan_submit");
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
        pty.text("No saved plan.");
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
        pty.ready();
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(other));
        assert!(
            body["tools"].is_null() || body["tools"].as_array().is_some_and(Vec::is_empty),
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
}

#[test]
fn plan_tui_mcp_is_stopped_in_plan_and_cleanup_tracks_execute_reconnection() {
    for protocol in ["completion", "response"] {
        for off in [false, true] {
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
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            plan_enter(&mut pty, &fixture);
            cancel_mcp_assert_pid_stopped(&project.path().join("mcp-log.pid"));
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "Plan before MCP reconnection",
                PLAN_TUI_BODY,
            );
            assert_eq!(
                cancel_mcp_log(project.path())
                    .iter()
                    .filter(|entry| entry["method"] == "initialize")
                    .count(),
                1
            );
            let id = saved_session_id(home.path());
            pty.input("\x03");
            pty.finish();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
            pty.ready();
            plan_wait_mode(&mut pty, "Plan");
            no_more(&fixture);
            pty.input("/plan view\r");
            plan_wait_review(&mut pty, 1);
            no_more(&fixture);
            assert_eq!(
                cancel_mcp_log(project.path())
                    .iter()
                    .filter(|entry| entry["method"] == "initialize")
                    .count(),
                1
            );
            if off {
                pty.input("\x1b");
                pty.wait_for("closing review before off", |screen| {
                    !screen.contents().contains("Execute (keep context)")
                });
                // 裸 /plan 直接切换退出：无本地确认框，已保存草稿保留在磁盘。
                pty.input("/plan\r");
                plan_wait_execute(&mut pty);
                no_more(&fixture);
                pty.input("Explicit normal execution after leaving Plan\r");
            } else {
                plan_choose(&mut pty, 1);
            }
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "mcp__local__echo"
                        || tool["name"] == "mcp__local__echo"),
                "{body}"
            );
            assert_eq!(
                cancel_mcp_log(project.path())
                    .iter()
                    .filter(|entry| entry["method"] == "initialize")
                    .count(),
                2
            );
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
    }
}

#[test]
fn plan_tui_submission_requires_deliberate_action_and_survives_resize_and_resume() {
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
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            PLAN_TUI_BODY
        );
        assert!(!project.path().join("forbidden-after-submit").exists());
        assert!(!project.path().join("canary.txt").exists());
        let text = pty.parser.screen().contents();
        let fresh = text.find("Execute in a new session").unwrap();
        let keep = text.find("Execute (keep context)").unwrap();
        let discuss = text.find("Continue discussing Plan").unwrap();
        let review = text.find("Review Plan").unwrap();
        let compact = text.find("Compact context and execute").unwrap();
        let leave = text.find("Save draft and leave Plan mode").unwrap();
        assert!(
            fresh < keep
                && keep < compact
                && compact < review
                && review < discuss
                && discuss < leave,
            "{text}"
        );
        assert!(text.contains('☐'), "{text}");
        no_more(&fixture);
        pty.resize(4, 20);
        pty.wait_for("narrow mode indicator", |screen| {
            screen.contents().contains("Plan")
                || screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains('P'))
        });
        pty.resize(30, 100);
        plan_wait_review(&mut pty, 1);
        pty.input("\x1b");
        plan_wait_mode(&mut pty, "Plan");
        pty.input("开始吧\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            cancel_user_messages(&body, protocol)
                .last()
                .unwrap()
                .contains("开始吧")
        );
        plan_final(request, protocol, "Still discussing, not executing");
        pty.ready();
        assert!(!project.path().join("canary.txt").exists());
        plan_wait_mode(&mut pty, "Plan");
        let id = saved_session_id(home.path());
        pty.input("\x03");
        pty.finish();
        for flag in ["-r", "-c"] {
            let mut resumed = Pty::start_with_args(project.path(), home.path(), &[flag, &id]);
            resumed.ready();
            plan_wait_mode(&mut resumed, "Plan");
            no_more(&fixture);
            resumed.input("/quit\r");
            resumed.finish();
        }
    }
}

#[test]
fn plan_ai_filename_is_stable_and_never_overwrites() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        for location in ["drafts", "plans", "done"] {
            let directory = project.path().join(".pp").join(location);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("occupied-{location}.md")),
                b"external bytes",
            )
            .unwrap();
        }
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("Specify the canary change\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut calls = Vec::new();
        for location in ["drafts", "plans", "done"] {
            calls.push(plan_call(
                protocol,
                &format!("collision-{location}"),
                "plan_write",
                json!({"title":"Canary","file_name":format!("occupied-{location}.md"),"content":PLAN_TUI_BODY,"expected_revision":0}),
            ));
        }
        calls.push(plan_call(
            protocol,
            "bad-path",
            "plan_write",
            json!({"title":"Canary","file_name":"../escape.md","content":PLAN_TUI_BODY,"expected_revision":0}),
        ));
        calls.push(plan_call(
            protocol,
            "named-save",
            "plan_write",
            json!({"title":"Canary","file_name":"create-canary.md","content":PLAN_TUI_BODY,"expected_revision":0}),
        ));
        plan_calls(request, protocol, json!(calls));
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        for location in ["drafts", "plans", "done"] {
            let output = &outputs
                .iter()
                .find(|(id, _)| id == &format!("collision-{location}"))
                .unwrap()
                .1;
            assert_eq!(output["ok"], false);
            assert_eq!(
                output["error"],
                "Plan file name already exists; choose another file_name."
            );
            assert_eq!(
                std::fs::read(
                    project
                        .path()
                        .join(".pp")
                        .join(location)
                        .join(format!("occupied-{location}.md"))
                )
                .unwrap(),
                b"external bytes"
            );
        }
        assert_eq!(
            outputs.iter().find(|(id, _)| id == "bad-path").unwrap().1["ok"],
            false
        );
        let saved = &outputs.iter().find(|(id, _)| id == "named-save").unwrap().1;
        assert_eq!(saved["path"], ".pp/drafts/create-canary.md");
        assert_eq!(saved["revision"], 1);
        let revised = format!("{PLAN_TUI_BODY}\nVerified design constraint.\n");
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "stable-save",
                    "plan_write",
                    json!({"title":"Canary","file_name":"create-canary.md","content":revised,"expected_revision":1})
                ),
                plan_call(
                    protocol,
                    "rename-refused",
                    "plan_write",
                    json!({"title":"Canary","file_name":"another-name.md","content":"- [ ] wrong","expected_revision":2})
                )
            ]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(
            outputs
                .iter()
                .find(|(id, _)| id == "stable-save")
                .unwrap()
                .1["revision"],
            2
        );
        assert_eq!(
            outputs
                .iter()
                .find(|(id, _)| id == "rename-refused")
                .unwrap()
                .1["error"],
            "Rename plans through /plan-list."
        );
        assert_eq!(
            std::fs::read_to_string(plan_path(project.path())).unwrap(),
            revised
        );
        assert!(!project.path().join(".pp/drafts/another-name.md").exists());
        plan_final(request, protocol, "Named draft saved");
        pty.wait_for("named draft save finished", |screen| {
            screen.contents().contains("Named draft saved")
        });
        pty.ready();
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn plan_tui_success_archives_completed_plan() {
    for protocol in ["completion", "response"] {
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
        // 本地批准 Execute (plan only)：固定 Fresh 新会话，只携带计划。
        plan_choose(&mut pty, 0);
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let encoded = body.to_string();
        assert!(encoded.contains("approved bytes"), "{body}");
        assert!(encoded.contains("[Approved Markdown snapshot]"), "{body}");
        assert!(!encoded.contains("ORIGIN_ONLY_MARKER"), "{body}");
        assert_eq!(cancel_user_messages(&body, protocol).len(), 1, "{body}");
        assert!(cancel_tool_outputs(&body, protocol).is_empty());
        let ids = resume_boundary_session_ids(home.path());
        assert_eq!(ids.len(), 2, "{ids:?}");
        let execution_id = ids.iter().find(|id| **id != origin_id).unwrap().clone();
        // 执行：真实写入 canary 并读回。
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
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            std::fs::read(project.path().join("canary.txt")).unwrap(),
            b"approved bytes"
        );
        assert_eq!(
            cancel_tool_outputs(&body, protocol).last().unwrap().1["content"],
            "approved bytes"
        );
        // 按批准集合逐项勾选两个任务。
        let plan_relative = ".pp/plans/create-canary.md";
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "progress-1",
                "edit",
                json!({"path":plan_relative,"edits":[{"oldText":"- [ ] Create canary.txt","newText":"- [x] Create canary.txt"}]})
            )]),
        );
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "progress-2",
                "edit",
                json!({"path":plan_relative,"edits":[{"oldText":"- [ ] Verify the failure boundary","newText":"- [x] Verify the failure boundary"}]})
            )]),
        );
        // 最终成功回复：成功收尾自动归档，不再发起任何模型请求。
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_final(request, protocol, "Both tasks verified and read back.");
        pty.completed();
        // 同名文件进入 done，plans 源不存在，勾选进度保留。
        assert!(!project.path().join(".pp/plans/create-canary.md").exists());
        let archived =
            std::fs::read_to_string(project.path().join(".pp/done/create-canary.md")).unwrap();
        assert!(archived.contains("- [x] Create canary.txt"), "{archived}");
        assert!(
            archived.contains("- [x] Verify the failure boundary"),
            "{archived}"
        );
        // /plan view：Completed、2/2 与实际 done 路径。
        pty.input("/plan view\r");
        pty.wait_for("archived completed plan view", |screen| {
            let text = screen.contents();
            text.contains("2/2")
                && text.contains("Completed")
                && text.contains(".pp/done/create-canary.md")
        });
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        pty.input("\x1b");
        // 执行会话 journal：Completed + done 位置。
        let workflow = plan_journal(home.path(), &execution_id)
            .into_iter()
            .rev()
            .find(|record| record["customType"] == "pp.workflow")
            .unwrap();
        assert_eq!(workflow["data"]["workflow"]["plan"]["status"], "completed");
        assert_eq!(workflow["data"]["workflow"]["plan"]["location"], "done");
        // 旧源规划会话恢复：按 ID 协调到 done，只读显示同一路径。
        pty.input("\x03");
        pty.finish();
        let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &origin_id]);
        resumed.ready();
        resumed.input("/plan view\r");
        resumed.wait_for("source session reads the archived plan", |screen| {
            let text = screen.contents();
            text.contains("2/2")
                && text.contains("Completed")
                && text.contains(".pp/done/create-canary.md")
        });
        no_more(&fixture);
        resumed.input("\x03");
        resumed.finish();
    }
}

#[test]
fn plan_completion_requires_success_and_approved_tasks() {
    for protocol in ["completion", "response"] {
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
        // 同批直接勾选两项：同一批计划编辑只在批后同步一次实际正文。
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

        // 全部勾选但最终错误：流中错误使任务失败，不归档。
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        {
            let mut stream = SseReply::new(request);
            if protocol == "completion" {
                stream.event(json!({
                    "error":{"message":"fixture stream error","type":"server_error"}
                }));
            } else {
                stream.event(json!({
                    "type":"error","sequence_number":1,"code":"server_error",
                    "message":"fixture stream error","param":null
                }));
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

        // 全部勾选但取消：同样不归档。
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

        // 改写批准任务：编辑落盘后同步立即拒绝，不发起下一次模型请求。
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

        // 恢复批准正文（逐字节还原）后成功收尾：已同步未再次变化也归档。
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
}

#[test]
fn plan_tui_save_draft_and_leave_preserves_file_without_model_request() {
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
        let path = plan_path(project.path());
        let id = saved_session_id(home.path());
        plan_choose(&mut pty, 5);
        plan_wait_execute(&mut pty);
        pty.ready();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAN_TUI_BODY);
        assert!(path.starts_with(project.path().join(".pp/drafts")));
        assert!(!project.path().join("canary.txt").exists());
        let journal = plan_journal(home.path(), &id);
        let workflows: Vec<_> = journal
            .iter()
            .filter(|record| record["customType"] == "pp.workflow")
            .collect();
        assert_eq!(
            workflows.last().unwrap()["data"]["workflow"]["mode"],
            "execute"
        );
        assert!(workflows.last().unwrap()["data"]["workflow"]["plan"].is_null());
        assert!(
            workflows
                .iter()
                .any(|record| record["data"]["workflow"]["plan"]["status"] == "draft")
        );
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}
