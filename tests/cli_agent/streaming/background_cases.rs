use super::*;

#[test]
fn slow_background_task_cleanup_at_run_boundaries() {
    let protocol = Protocol::Responses;
    for boundary in [RunBoundary::FinalAnswer, RunBoundary::ModelError] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let settings = r#"{"default_model":"fixture/primary","rate_limit_retry_budget_seconds":0}"#;
        std::fs::write(dir.path().join("marker"), "marker\n").unwrap();
        let service_pid = dir.path().join("service.pid");
        let second_pid = dir.path().join("second.pid");
        let mut cli = protocol.start_with_settings(&server, dir.path(), None, settings);
        cli.input("start a background task\n");
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
                request,
                json!([protocol.call(
                    "start",
                    "bash",
                    json!({
                        // TERM 清理同时写满两条管道；任务收尾必须持续排空至标记落盘。
                        "command": "cleanup() { for ((i=0;i<64;i++)); do printf '%4096s' ''; printf '%4096s' '' >&2; done; printf done > cleanup.done; exit 0; }; trap cleanup TERM; sleep 60 & service=$!; printf '%s' \"$service\" > service.pid; wait \"$service\"; while :; do sleep 60 & wait $!; done",
                        "mode": "background"
                    })
                )]),
            );
        let (request, body) = protocol.receive(&server);
        let started = protocol.tool_result(&body, "start");
        assert_eq!(started["ok"], true, "{protocol:?} {boundary:?}: {started}");
        let old_id = started["task_id"].as_str().unwrap().to_owned();
        assert!(rustix::process::test_kill_process(background_pid(&service_pid)).is_ok());

        match boundary {
            RunBoundary::FinalAnswer => {
                protocol.final_reply(request, "boundary-answer");
                cli.wait_for(Pipe::Stdout, "boundary-answer");
            }
            RunBoundary::ModelError => {
                request
                        .respond(
                            Response::builder()
                                .status(500)
                                .body(Body::from(
                                    json!({"error":{"message":"fixture failure","type":"server_error"}})
                                        .to_string(),
                                ))
                                .unwrap(),
                        )
                        .unwrap();
                cli.wait_for(Pipe::Stderr, "error: ");
            }
        }
        cancel_wait_pid_dead(&service_pid);
        // 任务收尾与 agent 运行边界异步可见：scratch 目录只在解释器（含
        // TERM trap）结束后才删除，先以它同步，再断言 trap 的落盘标记。
        cancel_assert_no_scratch(dir.path());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("cleanup.done")).unwrap(),
            "done",
            "{protocol:?} {boundary:?}: TERM 清理未完成"
        );
        cli.assert_running();

        cli.input("next round\n");
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "old-status",
                    "bash_task",
                    json!({"task_id": old_id, "action": "status"})
                ),
                protocol.call(
                    "second-start",
                    "bash",
                    json!({
                        "command": "sh -c 'printf \"%s\" \"$$\" > second.pid && exec sleep 60'",
                        "mode": "background"
                    })
                )
            ]),
        );
        let (request, body) = protocol.receive(&server);
        let old_status = protocol.tool_result(&body, "old-status");
        assert_eq!(
            old_status["ok"], false,
            "{protocol:?} {boundary:?}: {old_status}"
        );
        assert!(
            old_status["error"]
                .as_str()
                .is_some_and(|error| error.contains("Unknown background task")),
            "{protocol:?} {boundary:?}: {old_status}"
        );
        let second = protocol.tool_result(&body, "second-start");
        assert_eq!(second["ok"], true, "{protocol:?} {boundary:?}: {second}");
        assert_eq!(
            second["status"], "running",
            "{protocol:?} {boundary:?}: {second}"
        );
        let second_id = second["task_id"].as_str().unwrap().to_owned();
        assert_ne!(second_id, old_id, "{protocol:?} {boundary:?}");
        assert!(rustix::process::test_kill_process(background_pid(&second_pid)).is_ok());
        protocol.final_reply(request, "boundary-cleanup-complete");
        cli.wait_for(Pipe::Stdout, "boundary-cleanup-complete");
        cancel_wait_pid_dead(&service_pid);
        cancel_wait_pid_dead(&second_pid);
        cancel_assert_no_scratch(dir.path());
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{protocol:?} {boundary:?}: {output:?}"
        );
        let expected_stdout = match boundary {
            RunBoundary::FinalAnswer => "boundary-answer\nboundary-cleanup-complete\n",
            RunBoundary::ModelError => "boundary-cleanup-complete\n",
        };
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            expected_stdout,
            "{protocol:?} {boundary:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("task {old_id}: running")),
            "{protocol:?} {boundary:?}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("task {second_id}: running")),
            "{protocol:?} {boundary:?}: {stderr}"
        );
        assert_clean_pipes(&output);
        no_more(&server);
    }
}
