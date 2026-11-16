// —— 后台任务 PTY 场景（计划 TUI 部分）：真实受管进程、PID 文件栅栏、
// 可见屏幕断言，不测静态标题文案。 ——

// 受管理的后台命令：显式外部 sh 忽略 TERM，父 sh 与子 sleep 的 PID 写入
// marker 文件，停止路径必须用 grace 后的 KILL 回收整个进程组。
fn background_hold_command(parent_marker: &str, child_marker: &str, log: Option<&str>) -> String {
    let mut script = String::from("trap \"\" TERM; ");
    if let Some(log) = log {
        script.push_str(&format!("printf \"{log}\\n\"; "));
    }
    script.push_str(&format!(
        "printf \"%s\" \"$$\" > {parent_marker}; sleep 30 & child=$!; printf \"%s\" \"$child\" > {child_marker}; wait \"$child\""
    ));
    format!("sh -c '{script}'")
}

// 按协议回复单个工具调用。
fn background_reply_call(
    request: CapturedRequest,
    protocol: &str,
    id: &str,
    name: &str,
    args: Value,
) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}

// 断言请求中的工具结果与 call_id 恰好配对一次，返回其结果快照。
fn background_paired_snapshot(body: &Value, protocol: &str, call_id: &str) -> Value {
    let outputs = cancel_tool_outputs(body, protocol);
    assert_eq!(
        outputs
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [call_id],
        "后台调用必须与原 call_id 恰好一次配对"
    );
    outputs[0].1.clone()
}

// 启动受管后台任务：PID marker 证明 bash 父子真正运行；启动后的下一次模型请求
// 携带启动结果并被持有（模型停在等待响应阶段）；屏幕必须显示 task_id 与 running，
// start 的完成不能被当作服务已退出。返回 (task_id, 启动快照, 持有的请求)。
fn background_start_and_wait(
    pty: &mut Pty,
    fixture: &TestServer,
    protocol: &str,
    project: &Path,
    start: &BackgroundStart<'_>,
) -> (String, Value, CapturedRequest) {
    let [parent, child] = start.markers else {
        panic!("后台启动需要恰好两个 PID marker");
    };
    let command = background_hold_command(parent, child, start.log);
    pty.input(&format!("{}\r", start.prompt));
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    background_reply_call(
        request,
        protocol,
        start.call_id,
        "bash",
        json!({"command": command, "mode": "background"}),
    );
    cancel_wait_pid_files(project, start.markers);
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    let snapshot = background_paired_snapshot(&body, protocol, start.call_id);
    let task_id = snapshot["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("启动结果缺少 task_id：{snapshot}"))
        .to_owned();
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["status"], "running");
    pty.wait_for("屏幕显示后台 task_id 与 running 状态", |screen| {
        cancel_normalize(screen).contains(&format!("Task{task_id}:running"))
    });
    (task_id, snapshot, request)
}

// 一次后台启动的输入：marker 列表同时用于生成命令与进程级栅栏。
struct BackgroundStart<'a> {
    prompt: &'a str,
    call_id: &'a str,
    markers: &'a [&'a str],
    log: Option<&'a str>,
}

// 两协议先启动后台任务并把下一模型请求停在等待响应阶段。Esc 后必须先清理
// 父子 PID 与 scratch，再回到 Ready 且只留下取消状态行；下一用户任务正常执行，
// 不重放旧启动调用。
#[test]
fn background_esc_cancels_managed_tasks_and_cleans_before_ready_without_replay() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background esc prompt",
            call_id: "bg-esc-start",
            markers: &["bg-esc-parent.pid", "bg-esc-child.pid"],
            log: None,
        };
        let (task_id, _snapshot, held) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        let parent_before = std::fs::read(project.path().join("bg-esc-parent.pid")).unwrap();
        let child_before = std::fs::read(project.path().join("bg-esc-child.pid")).unwrap();
        pty.input("\x1b");
        // 先验证后台父子 PID 与 scratch 已清理，再观察 Ready 与取消状态行。
        cancel_wait_pid_dead(&project.path().join("bg-esc-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-esc-child.pid"));
        cancel_assert_no_scratch(project.path());
        cancel_wait_cancelled_ready(&mut pty);
        drop(held);
        // 取消收尾不触发任何模型请求。
        cancel_assert_no_request(&fixture);
        // 下一用户任务正常执行，且旧启动调用不重放。
        pty.input("background esc next\r");
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["background esc prompt", "background esc next"]
        );
        assert_eq!(cancel_called_ids(&body, protocol), ["bg-esc-start"]);
        let replayed = background_paired_snapshot(&body, protocol, "bg-esc-start");
        assert_eq!(replayed["task_id"], task_id.as_str());
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
        if protocol == "completion" {
            final_reply(request, "background esc recovered");
        } else {
            response_final(request, "background esc recovered");
        }
        pty.text("background esc recovered");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
        // 旧工具从未被重放：PID marker 保持取消后的字节，进程保持停止，无新 scratch。
        assert_eq!(
            std::fs::read(project.path().join("bg-esc-parent.pid")).unwrap(),
            parent_before
        );
        assert_eq!(
            std::fs::read(project.path().join("bg-esc-child.pid")).unwrap(),
            child_before
        );
        cancel_assert_no_scratch(project.path());
    }
}

// 后台任务存活且模型请求挂起（request_timeout_seconds=120）时按 Ctrl-C：PTY 必须
// 完成退出并恢复终端（finish 的 5 秒截止即证据，不等待整个 detached agent
// worker/HTTP），父子 PID 与 scratch 全部清理。
#[test]
fn background_ctrl_c_exits_and_cleans_tasks_while_model_request_is_pending() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background ctrl-c prompt",
            call_id: "bg-cc-start",
            markers: &["bg-cc-parent.pid", "bg-cc-child.pid"],
            log: None,
        };
        let (_task_id, _snapshot, held) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        pty.input("\x03");
        // 不杀掉 CLI 本身：退出由 Ctrl-C 键触发，finish 校验退出码、终端模式恢复。
        pty.finish();
        drop(held);
        cancel_wait_pid_dead(&project.path().join("bg-cc-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-cc-child.pid"));
        cancel_assert_no_scratch(project.path());
        cancel_assert_no_request(&fixture);
    }
}

// 同一后台生命周期的 status/stop 展示真实状态与累计日志；停止是正常路径，
// 不把 exit_code null 冒充为 Exit code 显示；前台卡片保留现有 Exit code/Wall。
#[test]
fn background_status_stop_lifecycle_displays_states_and_keeps_foreground_footer() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background lifecycle prompt",
            call_id: "bg-life-start",
            markers: &["bg-life-parent.pid", "bg-life-child.pid"],
            log: Some("bg-lifecycle-log"),
        };
        let (task_id, _snapshot, request) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        // status：轮询复用原始 Bash 卡片（不新增调用卡片），显示 running 与累计日志。
        background_reply_call(
            request,
            protocol,
            "bg-life-status",
            "bash_task",
            json!({"task_id": task_id.as_str(), "action": "status"}),
        );
        let (request, _) = pty.request_at(&fixture, path);
        pty.wait_for(
            "status 复用原始卡片显示 running 与累计日志",
            |screen| {
                let text = cancel_normalize(screen);
                !text.contains("bash_task·")
                    && text.contains("bg-lifecycle-log")
                    && text.contains(&format!("Task{task_id}:running"))
            },
        );
        // stop：同步等待进程/scratch 回收后返回，同一卡片显示真实 stopped 终态。
        background_reply_call(
            request,
            protocol,
            "bg-life-stop",
            "bash_task",
            json!({"task_id": task_id.as_str(), "action": "stop"}),
        );
        let (request, _) = pty.request_at(&fixture, path);
        cancel_wait_pid_dead(&project.path().join("bg-life-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-life-child.pid"));
        cancel_assert_no_scratch(project.path());
        pty.wait_for("stop 复用卡片显示 stopped 终态", |screen| {
            let text = cancel_normalize(screen);
            !text.contains("bash_task·") && text.contains(&format!("Task{task_id}:stopped"))
        });
        // 信号停止是 stop 的预期结果：不得显示 Exit: null 冒充已结束。
        assert!(
            !cancel_normalize(pty.parser.screen()).contains("Exit:null"),
            "stop 卡片不得显示 Exit: null"
        );
        // 前台成功卡片显示 Wall/Timeout footer。
        background_reply_call(
            request,
            protocol,
            "bg-life-foreground",
            "bash",
            json!({"command": "printf 'fg-visible-marker'", "mode": "foreground"}),
        );
        let (request, _) = pty.request_at(&fixture, path);
        pty.wait_for("前台卡片显示 Wall 与 Timeout footer", |screen| {
            screen.contents().lines().any(|line| {
                let line = line.split_whitespace().collect::<String>();
                line.contains("Wall:") && line.contains("Timeout:") && !line.contains("Exit:")
            }) && cancel_normalize(screen).contains("fg-visible-marker")
        });
        if protocol == "completion" {
            final_reply(request, "background lifecycle answer");
        } else {
            response_final(request, "background lifecycle answer");
        }
        pty.text("background lifecycle answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
    }
}

#[test]
fn request_retry_tui_stays_quiet_and_cancels_backoff_before_recovering() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        pty.input("retry cancelled prompt\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        request
            .respond(
                Response::builder()
                    .status(503)
                    .header("Retry-After", "30")
                    .body(Body::from("private-error-body fixture-key"))
                    .unwrap(),
            )
            .unwrap();
        // 日志作为已进入退避的栅栏；TUI 不再显示自动重试通知。
        pty.wait_for("请求已进入可取消的重试等待", |screen| {
            let retry_logged = std::fs::read_dir(home.path().join(".pp/logs"))
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .any(|text| text.contains("ai_request_retry"));
            retry_logged && !screen.contents().contains("Retrying in")
        });
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        cancel_assert_no_request(&fixture);
        assert!(!pty.parser.screen().contents().contains("fixture-key"));
        pty.input("after retry cancellation\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["retry cancelled prompt", "after retry cancellation"]
        );
        plan_final(request, protocol, "retry-recovery-complete");
        pty.text("retry-recovery-complete");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&fixture);
    }
}
