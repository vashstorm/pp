use super::*;

#[test]
fn wait_background_delivers_results_once_and_does_not_stream_progress_json() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = protocol.start(&server, dir.path(), Some("wait for background output"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(request,json!([protocol.call("start","bash",json!({"command":"while [ ! -f release ]; do sleep 0.01; done; printf 'wait-protocol-result'","mode":"background"}))]));
        let (request, body) = protocol.receive(&server);
        let started = protocol.tool_result(&body, "start");
        let id = started["task_id"].as_str().unwrap();
        protocol.calls_reply(
            request,
            json!([protocol.call("peek", "wait", json!({"task_ids":[id],"timeout_ms":0}))]),
        );
        let (request, body) = protocol.receive(&server);
        let peek = protocol.tool_result(&body, "peek");
        assert_eq!(peek["pending"], true);
        assert_eq!(peek["timed_out"], true);
        // 第二个 wait 在同一批内执行，不能重复交付第一个 wait 的结果。
        std::fs::write(dir.path().join("release"), "").unwrap();
        protocol.calls_reply(
            request,
            json!([
                protocol.call("wait", "wait", json!({"task_ids":[id],"timeout_ms":5000})),
                protocol.call("again", "wait", json!({"task_ids":[id]}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "wait");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(
            result["background_results"][0]["output"],
            "wait-protocol-result"
        );
        assert_eq!(
            protocol.tool_result(&body, "again")["background_results"],
            json!([])
        );
        protocol.calls_reply(
            request,
            json!([protocol.call("after-commit", "wait", json!({}))]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "after-commit")["jobs"],
            json!([])
        );
        protocol.final_reply(request, "wait protocol verified");
        let output = cli.finish();
        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("\"background_results\"") && !stderr.contains("\"duration_ms\""),
            "{stderr}"
        );
    }
}

#[test]
fn background_service_lifecycle_in_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let settings = r#"{"default_model":"fixture/primary","bash_timeout_seconds":1}"#;
        let cli =
            protocol.start_with_settings(&server, dir.path(), Some("background service"), settings);
        let service_pid = dir.path().join("service.pid");
        let start_command = format!(
            "sh -c 'printf \"%s\" \"$$\" > service.pid && exec \"$0\" stats --port 0' '{}'",
            env!("CARGO_BIN_EXE_pp").replace('\'', "'\\''")
        );
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "start",
                "bash",
                json!({"command": start_command, "mode": "background"})
            )]),
        );
        let (mut request, body) = protocol.receive(&server);
        let started = protocol.tool_result(&body, "start");
        assert_eq!(started["ok"], true, "{protocol:?}: {started}");
        let task_id = started["task_id"].as_str().unwrap().to_owned();
        assert!(task_id.starts_with("bg-"), "{protocol:?}: {started}");

        // Readiness barrier: poll bash_task status until the server prints its
        // URL, under a bounded deadline and never an unbounded poll.
        let ready = Instant::now() + Duration::from_secs(5);
        let address;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let call_id = format!("ready-{attempt}");
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    &call_id,
                    "bash_task",
                    json!({"task_id": task_id, "action": "status"})
                )]),
            );
            let (next, body) = protocol.receive(&server);
            request = next;
            let status = protocol.tool_result(&body, &call_id);
            assert_eq!(status["ok"], true, "{protocol:?}: {status}");
            assert_eq!(status["status"], "running", "{protocol:?}: {status}");
            if let Some((_, tail)) = status["output"]
                .as_str()
                .and_then(|text| text.split_once("pp stats: http://"))
            {
                address = tail.split('/').next().unwrap().to_owned();
                break;
            }
            assert!(
                Instant::now() < ready,
                "{protocol:?}: service never became ready: {status}"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // Hold this model request past the foreground deadline (1s timeout +
        // 1s grace): the background task must still be alive and serving.
        thread::sleep(Duration::from_millis(2500));
        assert!(
            rustix::process::test_kill_process(background_pid(&service_pid)).is_ok(),
            "{protocol:?}: background service died within the foreground deadline"
        );
        let probe_command = format!(
            "curl --fail --silent --show-error --max-time 1 \
             --write-out '\\nHTTP_STATUS:%{{http_code}}\\n' \
             'http://{address}/api/stats/overview?range=24h'"
        );
        protocol.calls_reply(
            request,
            json!([protocol.call("probe", "bash", json!({"command": probe_command}))]),
        );
        let (mut request, body) = protocol.receive(&server);
        let probe = protocol.tool_result(&body, "probe");
        assert_eq!(probe["ok"], true, "{protocol:?}: {probe}");
        assert_eq!(probe["exit_code"], 0, "{protocol:?}: {probe}");
        assert_eq!(probe["timed_out"], false, "{protocol:?}: {probe}");
        let captured;
        let stdout = if let Some(uri) = probe["artifact"]["uri"].as_str() {
            protocol.calls_reply(
                request,
                json!([protocol.call("probe-log", "read", json!({"path":format!("{uri}:raw")}))]),
            );
            let (next, body) = protocol.receive(&server);
            request = next;
            captured = protocol.tool_result(&body, "probe-log")["content"]
                .as_str()
                .unwrap()
                .to_owned();
            captured.as_str()
        } else {
            probe["output"].as_str().unwrap()
        };
        let (payload, tail) = stdout.rsplit_once("\nHTTP_STATUS:").unwrap_or_else(|| {
            panic!("{protocol:?}: probe output missing status marker: {stdout}")
        });
        let overview: Value = serde_json::from_str(payload.trim()).unwrap_or_else(|error| {
            panic!("{protocol:?}: probe body is not JSON ({error}): {payload}")
        });
        assert!(
            overview["summary"].get("requests").is_some(),
            "{protocol:?}: {overview}"
        );
        assert_eq!(tail, "200\n", "{protocol:?}: {stdout}");

        protocol.calls_reply(
            request,
            json!([protocol.call(
                "stop",
                "bash_task",
                json!({"task_id": task_id, "action": "stop"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let stopped = protocol.tool_result(&body, "stop");
        assert_eq!(stopped["ok"], true, "{protocol:?}: {stopped}");
        assert_eq!(stopped["task_id"], task_id.as_str());
        assert_eq!(stopped["status"], "stopped", "{protocol:?}: {stopped}");
        cancel_wait_pid_dead(&service_pid);
        assert!(
            std::net::TcpStream::connect(address.as_str()).is_err(),
            "{protocol:?}: stopped service still accepts connections on {address}"
        );
        cancel_assert_no_scratch(dir.path());

        // Stopping one background task must not cancel the running user task.
        protocol.calls_reply(
            request,
            json!([protocol.call("after", "bash", json!({"command": "printf task-continues"}))]),
        );
        let (request, body) = protocol.receive(&server);
        let after = protocol.tool_result(&body, "after");
        assert_eq!(after["ok"], true, "{protocol:?}: {after}");
        assert_eq!(after["exit_code"], 0, "{protocol:?}: {after}");
        assert_eq!(after["output"], "task-continues", "{protocol:?}: {after}");
        protocol.final_reply(request, "background-service-verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert_eq!(
            output.stdout, b"background-service-verified\n",
            "{protocol:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("task {task_id}: running")),
            "{protocol:?}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("task {task_id}: stopped")),
            "{protocol:?}: {stderr}"
        );
        assert!(
            stderr.contains("pp stats: http://127.0.0.1:"),
            "{protocol:?}: {stderr}"
        );
        assert!(stderr.contains("HTTP_STATUS"), "{protocol:?}: {stderr}");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn background_task_cleanup_at_run_boundaries() {
    for protocol in PROTOCOLS {
        for boundary in [RunBoundary::FinalAnswer, RunBoundary::ModelError] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let settings =
                r#"{"default_model":"fixture/primary","rate_limit_retry_budget_seconds":0}"#;
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
                    request.respond(Response::from_string(
                        json!({"error":{"message":"fixture failure","type":"server_error"}}).to_string()
                    ).with_status_code(500)).unwrap();
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
}

#[test]
fn grep_hashline_edit_and_pagination_roundtrip_both_protocols() {
    for protocol in PROTOCOLS {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        for n in 0..21 {
            std::fs::write(
                dir.path().join(format!("{n:02}.txt")),
                "before\nneedle\nafter\n",
            )
            .unwrap();
        }
        let cli = protocol.start_with_settings(
            &provider,
            dir.path(),
            Some("search and edit"),
            r#"{"edit":"hashline"}"#,
        );
        let (request, body) = protocol.receive(&provider);
        let tool = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool.get("function").unwrap_or(tool))
            .find(|tool| tool["name"] == "grep")
            .unwrap();
        assert!(tool["parameters"]["properties"]["pattern"].is_object());
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "page",
                    "grep",
                    json!({"pattern":"needle","path":"*.txt","skip":20})
                ),
                protocol.call("file", "grep", json!({"pattern":"needle","path":"00.txt"}))
            ]),
        );
        let (request, body) = protocol.receive(&provider);
        assert_eq!(
            protocol.tool_result(&body, "page")["details"]["fileCount"],
            1
        );
        let result = protocol.tool_result(&body, "file");
        let header = hashline_header(result["content"].as_str().unwrap());
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "edit",
                "edit",
                json!({"input":format!("{header}\nPUT 2.=2:\n+changed")})
            )]),
        );
        let (request, body) = protocol.receive(&provider);
        assert_eq!(protocol.tool_result(&body, "edit")["ok"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("00.txt")).unwrap(),
            "before\nchanged\nafter\n"
        );
        protocol.final_reply(request, "done");
        let result = cli.finish();
        assert!(result.status.success());
        no_more(&provider);
    }
}
