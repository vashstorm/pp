use super::*;

#[test]
fn plan_statusline_shows_updated_role_model_after_effort_change() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        "default_model: fixture/primary\nrate_limit_retry_budget_seconds: 0\nmodel_roles:\n  plan: {model: fixture/old-plan, effort: high}\n",
    )
    .unwrap();
    let mut models: Value =
        serde_json::from_str(&response_models(&fixture, Some("completion"))).unwrap();
    models["providers"]["fixture"]["models"]["old-plan"] = json!({"model":"old-plan-model"});
    models["providers"]["fixture"]["models"]["new-plan"] = json!({"model":"new-plan-model"});
    std::fs::write(config_path(home.path(), "models.yml"), models.to_string()).unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);

    pty.ready_model("fixture/primary");
    pty.input("/effort medium\r");
    pty.ready_effort("medium");
    runtime_role_save(&mut pty, 4, "new-plan");
    pty.ready_model("fixture/primary");

    pty.input("/plan\r");
    plan_wait_mode(&mut pty, "Plan");
    pty.ready_model("fixture/new-plan");
    pty.ready_effort("high");
    pty.input("\x03");
    pty.finish();
    no_more(&fixture);
}

#[test]
fn plan_statusline_switches_when_role_model_is_saved_during_plan() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = plan_start_with_role_models(&fixture, home.path(), project.path(), "completion");
    plan_enter(&mut pty, &fixture);
    pty.ready_model("fixture/planner");

    runtime_role_save(&mut pty, 4, "primary");
    pty.ready_model("fixture/primary");
    plan_wait_mode(&mut pty, "Plan");
    pty.ready_effort("high");

    pty.input("Continue planning with the updated model\r");
    let (request, body) = pty.request_at(&fixture, api_path("completion"));
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(body["reasoning_effort"], "high");
    plan_final(request, "completion", "Planning continued.");
    pty.text("Planning continued.");
    pty.input("\x03");
    pty.finish();
    no_more(&fixture);
}

#[test]
fn plan_tui_prompt_enters_before_request_and_reuses_plan_mode() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut settings = super::super::mcp::settings("");
        settings["notification_timeout_seconds"] = json!(1);
        settings["tools"] = json!({
            "permission": {
                "mcp__local__echo": "allow",
                "mcp__local__fail": "deny"
            }
        });
        crate::write_settings(home.path(), settings.to_string()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("/plan Research the login flow\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        plan_wait_mode(&mut pty, "Plan");
        // 进入 Plan 后复用 MCP 进程，并向模型提供发现的工具。
        cancel_mcp_assert_pid_alive(&project.path().join("mcp-log.pid"));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["Research the login flow"]
        );
        let tools = body["tools"].as_array().unwrap();
        assert!(!tools.is_empty());
        for tool in tools {
            let name = if protocol == "completion" {
                &tool["function"]["name"]
            } else {
                &tool["name"]
            };
            assert!(
                matches!(
                    name.as_str(),
                    Some(
                        "read"
                            | "rg"
                            | "glob"
                            | "web_search"
                            | "lsp"
                            | "ask"
                            | "todo"
                            | "write"
                            | "plan_submit"
                            | "task"
                            | "wait"
                            | "mcp__local__echo"
                            | "mcp__local__fail"
                    )
                ),
                "{body}"
            );
        }
        // 即使模型返回隐藏的写入调用，也必须已经启用 Plan 执行门禁。
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "plan-mcp",
                    "mcp__local__echo",
                    json!({"text":"evidence"})
                ),
                plan_call(protocol, "deny-mcp", "mcp__local__fail", json!({})),
                plan_call(
                    protocol,
                    "deny-write",
                    "write",
                    json!({"path":"forbidden.txt","content":"wrong"})
                )
            ]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs[0].1["result"]["content"][0]["text"], "中文结果");
        assert_eq!(
            outputs[1].1["error"],
            "mcp__local__fail is blocked by permission policy."
        );
        assert_eq!(
            outputs[2].1,
            json!({"ok":false,"error":"Tool is not available in Plan mode."})
        );
        assert!(!project.path().join("forbidden.txt").exists());
        plan_final(request, protocol, "Research complete");
        pty.wait_for("首个规划请求完成", |screen| {
            screen.contents().contains("Research complete")
        });
        pty.ready();
        pty.input("/plan Continue the research\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["Research the login flow", "Continue the research"]
        );
        plan_final(request, protocol, "Follow-up complete");
        pty.wait_for("追加规划请求完成", |screen| {
            screen.contents().contains("Follow-up complete")
        });
        pty.ready();
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn plan_toggle_waits_for_prompt_and_preserves_drafts() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        pty.ready();
        plan_wait_execute(&mut pty);

        // 裸 /plan 只待命：无计划绑定、无 Markdown 文件、无模型请求。
        pty.input("/plan\r");
        pty.draft("");
        assert!(
            !pty.parser
                .screen()
                .contents()
                .contains("Plan mode is active.")
        );
        plan_wait_mode(&mut pty, "Plan");
        no_more(&fixture);
        for location in ["drafts", "plans", "done"] {
            assert!(
                !project
                    .path()
                    .join(".pp")
                    .join(location)
                    .join("create-canary.md")
                    .exists()
            );
        }

        // 空待命可直接退出；再次进入仍待命。
        pty.input("/plan\r");
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        pty.input("/plan\r");
        plan_wait_mode(&mut pty, "Plan");
        no_more(&fixture);

        // 首条任务建立绑定：AI 命名首存生成 drafts/create-canary.md。
        plan_drive_ready(
            &mut pty,
            &fixture,
            protocol,
            "Plan the canary standby task",
            PLAN_TUI_BODY,
        );
        let first = plan_path(project.path());
        assert_eq!(
            first,
            project.path().join(".pp/drafts/create-canary.md"),
            "{first:?}"
        );

        // 已保存草稿退出：直接切换，无确认框，草稿文件与字节保留。
        pty.input("\x1b");
        pty.wait_for("closing review before toggle out", |screen| {
            !screen.contents().contains("Execute with context")
        });
        pty.input("/plan\r");
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), PLAN_TUI_BODY);

        // 再次进入只待命；下一条任务得到另一个计划文件，旧稿仍保留。
        pty.input("/plan\r");
        plan_wait_mode(&mut pty, "Plan");
        no_more(&fixture);
        pty.input("Plan the second standby task\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "second-write",
                    "write",
                    json!({"path":".pp/drafts/second-canary.md","content":PLAN_TUI_BODY})
                ),
                plan_call(
                    protocol,
                    "second-submit",
                    "plan_submit",
                    json!({"expected_revision":1})
                )
            ]),
        );
        plan_wait_review(&mut pty, 1);
        no_more(&fixture);
        let second = project.path().join(".pp/drafts/second-canary.md");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), PLAN_TUI_BODY);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), PLAN_TUI_BODY);

        // 已提交任务但尚未首存：请求出错后也能在本地确认放弃并退出。
        pty.input("\x1b");
        pty.wait_for("closing review before unsaved exit", |screen| {
            !screen.contents().contains("Execute with context")
        });
        pty.input("/plan\r");
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        pty.input("/plan\r");
        plan_wait_mode(&mut pty, "Plan");
        no_more(&fixture);
        pty.input("Research without saving anything\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_final(
            request,
            protocol,
            "Findings are only in chat; nothing saved yet.",
        );
        pty.ready();
        pty.input("Continue the unsaved research\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        request
            .respond(Response::builder().status(400).body(Body::empty()).unwrap())
            .unwrap();
        pty.text("HTTP status 400");
        pty.ready();
        pty.input("/plan\r");
        pty.text("Discard unsaved changes?");
        no_more(&fixture);
        // 默认 Enter 和 Esc 都保留当前规划；只有选中放弃并确认才退出。
        pty.input("\r");
        pty.wait_for("默认取消放弃", |screen| {
            !screen.contents().contains("Discard unsaved changes?") && input_is_ready(screen)
        });
        plan_wait_mode(&mut pty, "Plan");
        pty.input("/plan\r");
        pty.text("Discard unsaved changes?");
        pty.input("\x1b");
        pty.wait_for("Esc 取消放弃", |screen| {
            !screen.contents().contains("Discard unsaved changes?") && input_is_ready(screen)
        });
        plan_wait_mode(&mut pty, "Plan");
        no_more(&fixture);
        pty.input("/plan\r");
        pty.text("Discard unsaved changes?");
        pty.input("\x1b[B\r");
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), PLAN_TUI_BODY);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), PLAN_TUI_BODY);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn plan_tui_rejects_removed_new_and_off_subcommands() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        pty.ready();
        plan_wait_execute(&mut pty);
        // 旧子命令只报本地错误：不切换模式、不发请求、输入原样保留。
        for command in ["/plan new", "/plan off"] {
            pty.input(&format!("{command}\r"));
            pty.text("Use /plan to toggle; manage plans with /plan-list.");
            pty.draft(command);
            // Ctrl+A/Ctrl+K 清空保留的草稿，避免拼进下一条命令。
            pty.input("\x01\x0b");
        }
        plan_wait_execute(&mut pty);
        no_more(&fixture);
        assert!(!project.path().join(".pp").join("plans").exists());
        assert!(!project.path().join(".pp").join("drafts").exists());
        pty.input("\x03");
        pty.finish();
    }
}
