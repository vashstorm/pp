use super::*;

#[test]
fn yolo_executes_flagged_bash_without_permission_in_both_protocols() {
    for protocol in ["completion", "response"] {
        for (mode, policy, override_yolo) in [
            (Some("yolo"), "deny", false),
            (Some("always-ask"), "deny", true),
            (None, "allow", false),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().canonicalize().unwrap().join("removable");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("fixture"), "temporary fixture").unwrap();
            let mut settings = json!({
                "rate_limit_retry_budget_seconds": 0,
                "tools": {"permission": {"bash": policy}}
            });
            if let Some(mode) = mode {
                settings["tools"]["permissionMode"] = json!(mode);
            }
            let models = if protocol == "completion" {
                fixture_models(&server)
            } else {
                response_models(&server, Some("response"))
            };
            write_settings(dir.path(), settings.to_string()).unwrap();
            std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
            let args = if override_yolo {
                vec!["--permission-mode", "yolo", "-p", "clean temporary fixture"]
            } else {
                vec!["-p", "clean temporary fixture"]
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
fn permission_cli_applies_permission_mode_and_rejects_legacy_flag() {
    let home = tempfile::tempdir().unwrap();
    write_settings(
        home.path(),
        "tools: { permissionMode: always-ask, sandbox: { mode: auto, network: false } }",
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        "providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:9/v1\n    models:\n      primary:\n        model: fixture-model\n",
    )
    .unwrap();
    let output = Cli::spawn(
        home.path(),
        &["--permission-mode", "yolo", "/permissions"],
        &[],
    )
    .finish();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Permission mode: yolo"), "{stderr}");
    assert!(stderr.contains("effective=off, configured=off"), "{stderr}");
    assert!(stderr.contains("network=unrestricted"), "{stderr}");
    assert!(!config_path(home.path(), "permission.toml").exists());

    let legacy = Cli::spawn(
        home.path(),
        &["--approval-mode", "yolo", "/permissions"],
        &[],
    )
    .finish();
    assert!(!legacy.status.success(), "{legacy:?}");
}
