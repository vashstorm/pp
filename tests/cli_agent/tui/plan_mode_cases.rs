use super::*;

#[test]
fn plan_tui_prompt_enters_before_request_and_reuses_plan_mode() {
    let protocol = "response";
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
