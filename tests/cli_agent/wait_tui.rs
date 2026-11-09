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
fn wait_tui_progress_and_queued_steering_work_in_both_protocols() {
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
        pty.wait_for("补充输入排队后等待框仍在更新", |screen| {
            screen.contents().contains("0m02s")
        });
        cancel_assert_no_request(&fixture);
        std::fs::write(project.path().join("wait-release"), "").unwrap();
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let result = cancel_tool_outputs(&body, protocol)
            .into_iter()
            .find(|(id, _)| id == "wait-first")
            .unwrap()
            .1;
        assert_eq!(result["interrupted"], false);
        assert_eq!(result["pending"], false);
        assert!(cancel_user_messages(&body, protocol).iter().any(|s| s == "steer wait now"));
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
        pty.wait_for("取消后保留任务列表和真实终态", |screen| {
            let text = screen.contents();
            text.contains("1/1 settled") && text.contains("[bash]") && text.contains("cancelled")
        });
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
    }
}

fn wait_subagent_settings() -> String {
    let mut settings: Value = serde_json::from_str(CANCEL_SETTINGS).unwrap();
    settings["model_roles"] = json!({"fast":{"model":"fixture/primary","effort":"none"}});
    settings.to_string()
}

#[test]
fn slow_wait_tui_batch_updates_one_card_and_blocks_parent_until_all_children_settle() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), wait_subagent_settings()).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&fixture, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.resize(50, 160);
        pty.input("wait batch stages\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        background_reply_call(request, protocol, "spawn", "task", json!({"context":"batch barrier", "tasks":[
            {"role":"fast","task":"stage-one"}, {"role":"fast","task":"stage-two"}, {"role":"fast","task":"stage-three"}
        ]}));
        let mut parent = None;
        let mut children: Vec<Option<Request>> = (0..3).map(|_| None).collect();
        while parent.is_none() || children.iter().any(Option::is_none) {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            if hub_is_child(&body) {
                let text = body.to_string();
                let index = ["stage-one", "stage-two", "stage-three"].iter().position(|needle| text.contains(needle)).unwrap();
                children[index] = Some(request);
            } else {
                parent = Some(request);
            }
        }
        background_reply_call(parent.take().unwrap(), protocol, "batch-wait", "wait",
            json!({"run_ids":["r1","r2","r3"],"timeout_ms":0}));
        pty.text("0/3 settled");
        pty.input("batch-steering\r");
        for (index, child) in children.iter_mut().enumerate() {
            ask_final_reply(child.take().unwrap(), protocol, &format!("stage-result-{index}"));
            pty.text(&format!("{}/3 settled", index + 1));
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Wait ·").count(), 1, "{history}");
            assert!(history.contains(&format!("{} running", 2 - index)), "{history}");
            if index < 2 {
                cancel_assert_no_request(&fixture);
            }
        }
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(cancel_user_messages(&body, protocol).iter().any(|text| text == "batch-steering"));
        let result = cancel_tool_outputs(&body, protocol).into_iter().find(|(id, _)| id == "batch-wait").unwrap().1;
        assert_eq!(result["pending"], false);
        assert_eq!(result["results"].as_array().unwrap().len(), 3);
        assert!(result["jobs"].as_array().unwrap().iter().all(|job| job["status"] == "completed"));
        // 前批已全部结束，下一次 wait 应创建新框，且不能重复交付结果。
        background_reply_call(request, protocol, "next-wait", "wait", json!({"run_ids":["r1"]}));
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let result = cancel_tool_outputs(&body, protocol).into_iter().find(|(id, _)| id == "next-wait").unwrap().1;
        assert_eq!(result["results"], json!([]));
        ask_final_reply(request, protocol, "batch-wait-finished");
        pty.text("batch-wait-finished");
        pty.ready();
        let (history, _) = pty.native_history();
        assert_eq!(history.matches("Wait ·").count(), 2, "{history}");
        assert!(!history.contains("[task] r1 · running"), "{history}");
        pty.input("/quit\r");
        pty.finish();
        let id = saved_session_id(home.path());
        let mut restored = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        restored.text("batch-wait-finished");
        restored.ready();
        restored.input("\x1b[5~");
        restored.wait_for("历史浏览显示已完成的等待批次", |screen| {
            screen.alternate_screen() && screen.contents().contains("3/3 settled")
        });
        let history = restored.contents();
        assert!(history.contains("3/3 settled"), "{history}");
        assert!(!history.contains("[task] r1 · running"), "{history}");
        restored.input("\x1b");
        restored.wait_for("关闭历史浏览并回到主屏幕", |screen| {
            !screen.alternate_screen() && input_is_ready(screen)
        });
        restored.input("/quit\r");
        restored.finish();
        cancel_assert_no_request(&fixture);
    }
}

#[test]
fn wait_tui_implicit_batch_also_queues_steering_until_all_children_settle() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), wait_subagent_settings()).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&fixture, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("implicit barrier\r");
        let mut children = hub_dispatch(&mut pty, &fixture, protocol,
            json!({"context":"implicit batch", "tasks":[{"role":"fast","task":"implicit-one"},{"role":"fast","task":"implicit-two"}]}),
            &["implicit-one", "implicit-two"]);
        pty.input("implicit-steering\r");
        ask_final_reply(children.remove(0), protocol, "implicit-result-one");
        cancel_assert_no_request(&fixture);
        ask_final_reply(children.remove(0), protocol, "implicit-result-two");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(!hub_is_child(&body));
        for needle in ["implicit-result-one", "implicit-result-two", "implicit-steering"] {
            assert!(body.to_string().contains(needle), "{body}");
        }
        ask_final_reply(request, protocol, "implicit-batch-finished");
        pty.text("implicit-batch-finished");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
    }
}

#[test]
fn wait_tui_esc_reaps_subagent_background_process_before_restoring_input() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), wait_subagent_settings()).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&fixture, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("cancel child batch\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        background_reply_call(request, protocol, "spawn", "task", json!({"context":"cancel owned work", "tasks":[{"role":"fast","task":"child background service"}]}));
        let mut parent = None;
        let mut child = None;
        while parent.is_none() || child.is_none() {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            if hub_is_child(&body) { child = Some(request); } else { parent = Some(request); }
        }
        background_reply_call(child.take().unwrap(), protocol, "child-bg", "bash",
            json!({"command":"sh -c 'printf %s $$ > child-wait.pid; while :; do sleep 0.01; done'","mode":"background"}));
        let (child_request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_is_child(&body));
        background_reply_call(parent.take().unwrap(), protocol, "child-wait", "wait", json!({"run_ids":["r1"]}));
        pty.text("0/1 settled");
        pty.input("\x1b");
        cancel_wait_pid_dead(&project.path().join("child-wait.pid"));
        cancel_wait_cancelled_ready(&mut pty);
        pty.wait_for("子 Agent 真正收尾后显示取消终态", |screen| {
            let text = screen.contents();
            text.contains("1/1 settled") && text.contains("[task] r1 · cancelled")
        });
        let registry = hub_registry(home.path());
        assert_eq!(registry["runs"]["r1"]["status"], "cancelled");
        drop(child_request);
        cancel_assert_no_request(&fixture);
        pty.input("/quit\r");
        pty.finish();
    }
}
