//! 运行中权限切换与工具批内 Hook 联动场景。

mod integration {
    use super::super::*;
    use pp_test_support::TestServer;
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    fn hook_script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path.display().to_string()
    }

    fn hook_yaml(event: &str, script: &str) -> String {
        format!("{event}:\n  - hooks:\n      - type: command\n        command: '{script}'\n")
    }

    fn tool_outputs(protocol: &str, body: &Value) -> Vec<(String, String)> {
        match protocol {
            "completion" => body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["role"] == "tool")
                .map(|entry| {
                    (
                        entry["tool_call_id"].as_str().unwrap().to_owned(),
                        entry["content"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect(),
            _ => body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["type"] == "function_call_output")
                .map(|entry| {
                    (
                        entry["call_id"].as_str().unwrap().to_owned(),
                        entry["output"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn hooks_and_next_tool_call_pick_up_mid_batch_permission_switch() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let modes_log = dir.path().join("modes.log");
            let script = hook_script(
                dir.path(),
                "record_mode.sh",
                &format!(
                    "python3 -c 'import json,sys; d=json.load(sys.stdin); print(d[\"tool_use_id\"]+\":\"+d[\"permission_mode\"])' >> {}",
                    modes_log.display()
                ),
            );
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent = runtime_agent(
                dir.path(),
                protocol,
                &server.addr().to_string(),
                "",
                Some(&hook_yaml("PostToolUse", &script)),
            );
            let gate = agent.permission_gate();
            gate.select(Some(pp_config::config::PermissionMode::ReadOnly));
            let prompt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let count_ref = Arc::clone(&prompt_count);
            agent.set_permission_handler(Box::new(move |_request, _| {
                let n = count_ref.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(n, 0, "切换为 YOLO 后同批次后续调用不得再次询问");
                gate.select(Some(pp_config::config::PermissionMode::DangerFullAccess));
                Ok(pp_permission::PermissionDecision::Approve)
            }));

            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let handle = std::thread::spawn(move || {
                let first = worker
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                tool_batch_for(
                    &serving,
                    first,
                    &[
                        ("write", r#"{"path":"one.txt","content":"1"}"#),
                        ("write", r#"{"path":"two.txt","content":"2"}"#),
                    ],
                );
                let second = worker
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let second_body = second.json().unwrap();
                final_for(&serving, second, "done", true);
                second_body
            });

            let outcome = agent
                .run(
                    "write two files",
                    &CancellationToken::new(),
                    None,
                    &mut |_| {},
                )
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            let second_body = handle.join().unwrap();
            assert_eq!(prompt_count.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("one.txt")).unwrap(),
                "1"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("two.txt")).unwrap(),
                "2"
            );

            let outputs = tool_outputs(protocol, &second_body);
            assert_eq!(outputs.len(), 2, "{outputs:?}");
            for (_, out) in outputs {
                let val: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(val["ok"], true, "{val}");
            }
            let logged = std::fs::read_to_string(&modes_log).unwrap();
            let lines: Vec<_> = logged.lines().collect();
            assert_eq!(
                lines,
                vec!["call-1:bypassPermissions", "call-2:bypassPermissions"],
                "{logged}"
            );
        }
    }
}
