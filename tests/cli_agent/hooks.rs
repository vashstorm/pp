//! 用户级 command hooks 的真实 CLI 表面：输入拒绝、参数改写与 stderr 诊断。

use super::*;

fn hook_script(dir: &std::path::Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path.display().to_string()
}

#[test]
fn hooks_cli_rejected_prompt_exits_without_a_model_request() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let script = hook_script(
        dir.path(),
        "deny.sh",
        "cat > /dev/null\nprintf 'blocked by policy' >&2\nexit 2",
    );
    write_settings(
        dir.path(),
        json!({
            "rate_limit_retry_budget_seconds": 0,
            "hooks": {"UserPromptSubmit": [{"hooks": [
                {"type": "command", "command": script}
            ]}]}
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let output = Cli::spawn(dir.path(), &["-p", "hello"], &[]).finish();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("blocked by policy"), "{stderr}");
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
    no_more(&server);
}

#[test]
fn hooks_cli_rewrites_arguments_and_reports_sanitized_diagnostics() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let written = dir.path().join("written.txt");
        // 改写目标是工作区相对路径，避免临时目录在 macOS 上的符号链接拼写差异。
        let rewrite = hook_script(
            dir.path(),
            "rewrite.sh",
            "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":{\"path\":\"written.txt\",\"content\":\"rewritten\"}}}'",
        );
        // 诊断里带 ESC 序列：默认的 exit 码策略让任务继续，但必须净化显示。
        let notice = hook_script(
            dir.path(),
            "notice.sh",
            r#"cat > /dev/null
printf '{"systemMessage":"hook\\u001b[2Jnotice"}'"#,
        );
        write_settings(
            dir.path(),
            json!({
                "rate_limit_retry_budget_seconds": 0,
                "hooks": {
                    "PreToolUse": [{"matcher": "write", "hooks": [
                        {"type": "command", "command": rewrite}
                    ]}],
                    "PostToolUse": [{"hooks": [
                        {"type": "command", "command": notice}
                    ]}],
                }
            })
            .to_string(),
        )
        .unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
        let cli = Cli::spawn(dir.path(), &["-p", "write one file"], &[]);
        let path = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (request, _) = receive_at(&server, path);
        let arguments = json!({"path": "target.txt", "content": "original"});
        if protocol == "completion" {
            calls_reply(request, json!([call("write-1", "write", arguments)]));
        } else {
            response_reply(
                request,
                json!([response_call("write-1", "write", arguments)]),
            );
        }
        let (request, body) = receive_at(&server, path);
        let tool = if protocol == "completion" {
            body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["role"] == "tool")
                .cloned()
                .unwrap()
        } else {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .cloned()
                .unwrap()
        };
        let result = if protocol == "completion" {
            result(&tool, "write-1")
        } else {
            response_result(&tool, "write-1")
        };
        assert_eq!(result["ok"], true, "{result}");
        // 原生调用与磁盘都只看到改写后的目标。
        let native_arguments = if protocol == "completion" {
            tool["content"].as_str().unwrap().to_owned()
        } else {
            tool["output"].as_str().unwrap().to_owned()
        };
        assert!(!dir.path().join("target.txt").exists());
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "rewritten",
            "{native_arguments}"
        );
        if protocol == "completion" {
            final_reply(request, "done");
        } else {
            response_final(request, "done");
        }
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("PostToolUse hook:"), "{stderr}");
        assert!(stderr.contains("notice"), "{stderr}");
        assert!(
            !stderr.contains('\u{1b}'),
            "hook 诊断不得输出原始终端转义：{stderr:?}"
        );
    }
}
