use super::*;

#[test]
fn yolo_executes_flagged_bash_without_permission_in_both_protocols() {
    for protocol in ["completion", "response"] {
        for (cli_flag, policy) in [
            (Some("--yolo"), "deny"),
            (Some("danger-full-access"), "deny"),
            (None, "allow"),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().canonicalize().unwrap().join("removable");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("fixture"), "temporary fixture").unwrap();
            let settings = json!({
                "rate_limit_retry_budget_seconds": 0,
                "tools": {"permission": {"bash": policy}}
            });
            let models = if protocol == "completion" {
                fixture_models(&server)
            } else {
                response_models(&server, Some("response"))
            };
            write_settings(dir.path(), settings.to_string()).unwrap();
            std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
            let args: Vec<&str> = match cli_flag {
                Some("--yolo") => vec![
                    "--yolo",
                    "--allowed-tools",
                    "Bash",
                    "-p",
                    "clean temporary fixture",
                ],
                Some(mode) => vec![
                    "--permission-mode",
                    mode,
                    "--allowed-tools",
                    "Bash",
                    "-p",
                    "clean temporary fixture",
                ],
                None => vec!["--allowed-tools", "Bash", "-p", "clean temporary fixture"],
            };
            let cli = Cli::spawn(dir.path(), &args, &[]);
            let path = if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            };
            let (request, _) = receive_at(&server, path);
            // 仅删除夹具目录；绝对路径递归删除会命中原有强制权限检查规则。
            let command = format!(
                "rm -rf -- '{}'; printf yolo-completed",
                target.to_str().unwrap().replace('\'', "'\\''")
            );
            let arguments = json!({"command": command});
            if protocol == "completion" {
                calls_reply(request, json!([call("cleanup", "bash", arguments)]));
            } else {
                response_reply(
                    request,
                    json!([response_call("cleanup", "bash", arguments)]),
                );
            }
            let (request, body) = receive_at(&server, path);
            let result = if protocol == "completion" {
                body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["role"] == "tool")
                    .unwrap()["content"]
                    .as_str()
                    .unwrap()
            } else {
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|item| item["type"] == "function_call_output")
                    .unwrap()["output"]
                    .as_str()
                    .unwrap()
            };
            let result: Value = serde_json::from_str(result).unwrap();
            assert_eq!(result["ok"], true, "{result}");
            assert_eq!(result["output"], "yolo-completed", "{result}");
            assert!(!target.exists());
            if protocol == "completion" {
                final_reply(request, "cleanup completed");
            } else {
                response_final(request, "cleanup completed");
            }
            let output = cli.finish();
            assert!(output.status.success(), "{output:?}");
            no_more(&server);
        }
    }
}

#[test]
fn permission_cli_defaults_to_workspace_write_and_supports_yolo_flag() {
    let home = tempfile::tempdir().unwrap();
    write_settings(
        home.path(),
        "tools: { sandbox: { mode: auto, network: false } }",
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        "providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:9/v1\n    models:\n      primary:\n        model: fixture-model\n",
    )
    .unwrap();

    // 1. 默认启动为 Ask for approval (workspace-write)，且不持久化旧 preset。
    let default_out = Cli::spawn(home.path(), &["/permissions"], &[]).finish();
    assert!(default_out.status.success(), "{default_out:?}");
    let default_err = String::from_utf8_lossy(&default_out.stderr);
    assert!(
        default_err.contains("Permission mode: Ask for approval (workspace-write)"),
        "{default_err}"
    );
    assert!(
        default_err.contains("network=off (loopback only)")
            || default_err.contains("network=isolated"),
        "{default_err}"
    );

    // 2. `--yolo` 与 `--permission-mode danger-full-access` 均切到 Full Access (danger-full-access) 并关闭沙盒。
    for args in [
        vec!["--yolo", "/permissions"],
        vec!["--permission-mode", "danger-full-access", "/permissions"],
    ] {
        let output = Cli::spawn(home.path(), &args, &[]).finish();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Permission mode: Full Access (danger-full-access)"),
            "{stderr}"
        );
        assert!(stderr.contains("effective=off, configured=off"), "{stderr}");
        assert!(stderr.contains("network=unrestricted"), "{stderr}");
    }
    assert!(!config_path(home.path(), "permission.toml").exists());
}

#[test]
fn auto_review_uses_review_role_model_to_pass_or_deny_protected_actions() {
    let main_server = server();
    let review_server = server();
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().canonicalize().unwrap();
    std::fs::create_dir_all(dir.join(".agents")).unwrap();
    std::fs::create_dir_all(dir.join(".git/hooks")).unwrap();
    write_settings(
        &dir,
        concat!(
            "rate_limit_retry_budget_seconds: 0\n",
            "default_model: main/primary\n",
            "model_roles:\n",
            "  review: { model: reviewer/guard, effort: none }\n",
            "tools:\n",
            "  sandbox:\n",
            "    mode: auto\n",
            "    network: false\n",
        ),
    )
    .unwrap();
    std::fs::write(
        config_path(&dir, "models.yml"),
        json!({
            "providers": {
                "main": {
                    "protocol": "completion",
                    "api_key": "fixture-key",
                    "base_url": format!("http://{}/v1", main_server.addr()),
                    "models": {"primary": {"model": "main-model"}}
                },
                "reviewer": {
                    "protocol": "completion",
                    "api_key": "review-key",
                    "base_url": format!("http://{}/v1", review_server.addr()),
                    "models": {"guard": {"model": "guard-model"}}
                }
            }
        })
        .to_string(),
    )
    .unwrap();

    let cli = Cli::spawn(
        &dir,
        &[
            "--permission-mode",
            "auto_review",
            "--allowed-tools",
            "Read,Write,Bash",
            "-p",
            "write helper note into .agents/note.txt",
        ],
        &[],
    );

    let (request, _) = receive_at(&main_server, "/v1/chat/completions");
    calls_reply(
        request,
        json!([call(
            "w-agents",
            "write",
            json!({"path": ".agents/note.txt", "content": "approved by review"})
        )]),
    );

    // 独立 REVIEW 角色收到审阅请求（无工具），返回 pass 后免弹窗放行写入 .agents/note.txt。
    let (rev_req, rev_body) = receive_at(&review_server, "/v1/chat/completions");
    assert_eq!(rev_body["model"], "guard-model");
    assert!(rev_body.get("tools").is_none() || rev_body["tools"].is_null());
    assert!(
        rev_body.to_string().contains(".agents/note.txt"),
        "{rev_body}"
    );
    final_reply(
        rev_req,
        r#"{"decision":"pass","reason":"Explicitly matches user prompt for .agents/note.txt"}"#,
    );

    let (request, body) = receive_at(&main_server, "/v1/chat/completions");
    let first_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap();
    let first_out: Value = serde_json::from_str(first_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(first_out["ok"], true, "{first_out}");
    assert_eq!(
        std::fs::read_to_string(dir.join(".agents/note.txt")).unwrap(),
        "approved by review"
    );

    // 接着尝试写 .git/hooks/pre-commit；REVIEW 返回 deny -> 无交互 UI 时 fail-closed 并展示风险原因。
    calls_reply(
        request,
        json!([call(
            "w-hook",
            "write",
            json!({"path": ".git/hooks/pre-commit", "content": "echo pwned"})
        )]),
    );
    let (rev_req2, rev_body2) = receive_at(&review_server, "/v1/chat/completions");
    assert!(
        rev_body2.to_string().contains(".git/hooks/pre-commit"),
        "{rev_body2}"
    );
    final_reply(
        rev_req2,
        r#"{"decision":"deny","reason":"Modifying git hooks was not requested by user"}"#,
    );

    let (request, body) = receive_at(&main_server, "/v1/chat/completions");
    let second_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "w-hook")
        .unwrap();
    let second_out: Value = serde_json::from_str(second_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(second_out["ok"], false, "{second_out}");
    let err_msg = second_out["error"].as_str().unwrap();
    assert!(
        err_msg.contains("Modifying git hooks was not requested by user"),
        "{err_msg}"
    );
    assert!(!dir.join(".git/hooks/pre-commit").exists());
    final_reply(request, "hook blocked");

    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    no_more(&main_server);
    no_more(&review_server);
}
