use super::*;

#[test]
fn yolo_executes_flagged_bash_without_approval_in_both_protocols() {
    for protocol in ["completion", "response"] {
        for (mode, policy, override_yolo) in [
            (None, "prompt", false),
            (Some("yolo"), "deny", false),
            (Some("write"), "deny", true),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().canonicalize().unwrap().join("removable");
            std::fs::create_dir(&target).unwrap();
            std::fs::write(target.join("fixture"), "temporary fixture").unwrap();
            let mut settings = json!({
                "rate_limit_retry_budget_seconds": 0,
                "tools": {"approval": {"bash": policy}}
            });
            if let Some(mode) = mode {
                settings["tools"]["approvalMode"] = json!(mode);
            }
            let models = if protocol == "completion" {
                fixture_models(&server)
            } else {
                response_models(&server, Some("response"))
            };
            write_settings(dir.path(), settings.to_string()).unwrap();
            std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
            let args = if override_yolo {
                vec!["--approval-mode", "yolo", "-p", "clean temporary fixture"]
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
            // 仅删除夹具目录；绝对路径递归删除会命中原有强制审批规则。
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
