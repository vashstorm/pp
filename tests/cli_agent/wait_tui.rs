// 通过真实 PTY 验证等待刷新、Steering、取消和两种协议的结果回填。
fn wait_tui_start(
    protocol: &str,
    fixture: &Server,
    home: &std::path::Path,
    project: &std::path::Path,
) -> (Pty, String, Request) {
    std::fs::write(config_path(home, "settings.yml"), CANCEL_SETTINGS).unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(fixture, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project, home, &[]);
    pty.ready();
    pty.input("wait lifecycle\r");
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    background_reply_call(
        request,
        protocol,
        "wait-start",
        "bash",
        json!({"command":"sh -c 'printf \"%s\" \"$$\" > wait.pid; printf \"wait-start-log\\n\"; while [ ! -f wait-release ]; do sleep 0.01; done; printf \"wait-complete-log\\n\"'","mode":"background"}),
    );
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    let result = cancel_tool_outputs(&body, protocol)
        .into_iter()
        .find(|(id, _)| id == "wait-start")
        .unwrap()
        .1;
    let id = result["task_id"].as_str().unwrap().to_owned();
    (pty, id, request)
}

#[test]
fn wait_tui_progress_result_and_steering_interrupt_work_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (mut pty, id, request) =
            wait_tui_start(protocol, &fixture, home.path(), project.path());
        background_reply_call(
            request,
            protocol,
            "wait-first",
            "wait",
            json!({"task_ids":[id],"timeout_ms":10000}),
        );
        pty.wait_for("等待框立即显示目标与数量进度", |screen| {
            let text = screen.contents();
            text.contains("Wait") && text.contains("0/1 settled") && text.contains("[bash]")
        });
        pty.wait_for("后台无新输出时耗时仍刷新", |screen| {
            screen.contents().contains("0m01s")
        });
        pty.input("\x0f");
        pty.input("steer wait now\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let result = cancel_tool_outputs(&body, protocol)
            .into_iter()
            .find(|(id, _)| id == "wait-first")
            .unwrap()
            .1;
        assert_eq!(result["interrupted"], true);
        assert_eq!(result["pending"], true);
        assert!(
            cancel_user_messages(&body, protocol)
                .iter()
                .any(|s| s == "steer wait now")
        );
        pty.wait_for("Steering 返回后冻结原等待框", |screen| {
            screen.contents().contains("interrupted by steering")
        });
        background_reply_call(
            request,
            protocol,
            "wait-second",
            "wait",
            json!({"task_ids":[id],"timeout_ms":10000}),
        );
        pty.wait_for("重新等待仍在运行的任务", |screen| {
            screen.contents().contains("0/1 settled")
        });
        std::fs::write(project.path().join("wait-release"), "").unwrap();
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let result = cancel_tool_outputs(&body, protocol)
            .into_iter()
            .find(|(id, _)| id == "wait-second")
            .unwrap()
            .1;
        assert_eq!(result["background_results"][0]["status"], "exited");
        assert!(
            result["background_results"][0]["output"]
                .as_str()
                .unwrap()
                .contains("wait-complete-log")
        );
        pty.wait_for("最终等待结果和原 Bash 卡片均更新", |screen| {
            let text = cancel_normalize(screen);
            text.contains("1/1settled") && text.contains(&format!("Task{id}:exited"))
        });
        ask_final_reply(request, protocol, "wait lifecycle verified");
        pty.text("wait lifecycle verified");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        // 每 500ms 的快照不进入持久化事件。
        for file in std::fs::read_dir(home.path().join(".pp/agent/sessions"))
            .unwrap()
            .flatten()
        {
            if file.path().extension().is_some_and(|e| e == "jsonl") {
                let text = std::fs::read_to_string(file.path()).unwrap();
                assert!(!text.contains("tool_output_snapshot"));
            }
        }
    }
}

#[test]
fn wait_tui_esc_cancels_background_and_restores_input_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (mut pty, id, request) =
            wait_tui_start(protocol, &fixture, home.path(), project.path());
        background_reply_call(
            request,
            protocol,
            "wait-cancel",
            "wait",
            json!({"task_ids":[id]}),
        );
        pty.wait_for("进入等待屏障", |screen| {
            screen.contents().contains("0/1 settled")
        });
        pty.input("\x1b");
        cancel_wait_pid_dead(&project.path().join("wait.pid"));
        cancel_wait_cancelled_ready(&mut pty);
        cancel_assert_no_scratch(project.path());
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
    }
}
