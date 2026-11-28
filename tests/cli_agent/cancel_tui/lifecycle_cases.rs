// 验证项 1：两协议各跑 headers 前与部分 delta 后两种阶段。Esc 取消后旧连接
// 必须在提交下一任务前真的关闭；无隐式重试；草稿/光标/alternate screen 保留；
// 新请求原生历史包含两条用户消息各一次、没有半成品 assistant。
#[test]
fn tui_cancel_idle_headers_and_sse_close_connection_before_next_request_and_recover() {
    for (protocol, phase) in [("completion", "headers"), ("response", "sse")] {
            let raw = CancelRawModel::start();
            let home = tempfile::tempdir().unwrap();
            // 原始 TCP fixture 只观察主请求；独立标题服务保持挂起，随任务取消。
            let title_server = server();
            let mut settings: Value = serde_json::from_str(CANCEL_SETTINGS).unwrap();
            settings["model_roles"] = json!({"fast":{"model":"title/fast","effort":"none"}});
            let mut models: Value = serde_json::from_str(&raw.models(protocol)).unwrap();
            models["providers"]["title"] = slash_provider(
                &title_server,
                "title",
                protocol,
                json!({"fast":{"model":"title-model"}}),
            );
            std::fs::write(
                config_path(home.path(), "settings.yml"),
                settings.to_string(),
            )
            .unwrap();
            std::fs::write(config_path(home.path(), "models.yml"), models.to_string()).unwrap();
            let mut pty = Pty::start_with_existing_files(home.path());
            pty.ready();
            pty.input("cancel idle prompt\r");
            let (path, body) = raw.expect_request();
            assert_eq!(path, api_path(protocol));
            let first_body = body.clone();
            assert_eq!(
                cancel_user_messages(&body, protocol),
                ["cancel idle prompt"]
            );
            if phase == "sse" {
                raw.delta(protocol, "partial-cancel-token");
                pty.text("partial-cancel-token");
            } else {
                raw.hold();
            }
            pty.input("保留草稿");
            pty.draft("保留草稿");
            pty.input("\x1b");
            cancel_wait_cancelled_ready(&mut pty);
            if phase == "sse" {
                assert_eq!(
                    cancel_count_normalized(pty.parser.screen(), "partial-cancel-token"),
                    1,
                    "已显示的 partial 必须保留"
                );
            }
            // 旧连接在提交下一任务前已经关闭，而不是等下一次 block_on 才清理。
            raw.expect_close();
            // 不先发送新用户消息就没有任何额外模型请求。
            raw.assert_silent();
            // 光标在草稿末尾：追加一个字符证明草稿与光标都被保留。
            pty.input("字");
            pty.draft("保留草稿字");
            pty.input("\r");
            let (path, body) = raw.expect_request();
            assert_eq!(path, api_path(protocol));
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
            assert!(
                !text.contains("partial-cancel-token"),
                "半成品 assistant 不得进入原生历史"
            );
            if protocol == "completion" {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(
                    messages
                        .iter()
                        .filter(|item| item["role"] == "user")
                        .count(),
                    2
                );
                assert_eq!(
                    messages
                        .iter()
                        .filter(|item| item["role"] == "assistant")
                        .count(),
                    0
                );
                assert_eq!(messages[1]["content"], "cancel idle prompt");
                assert_eq!(messages[2]["content"], "保留草稿字");
            } else {
                let items = body["input"].as_array().unwrap();
                assert_eq!(items.len(), 2);
                response_user(&items[0], "cancel idle prompt");
                response_user(&items[1], "保留草稿字");
            }
            // 新 runtime/client 真正可用：下一次请求返回正常最终回答。
            raw.final_answer(protocol, "idle recovered answer");
            pty.text("idle recovered answer");
            pty.ready();
            pty.input("/quit\r");
            pty.finish();
            raw.assert_silent();
            let database = super::stats::database(home.path());
            let statuses: Vec<String> = database
                .prepare("SELECT status FROM requests WHERE purpose!='task_title' ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(statuses, ["aborted", "ok"]);
            let contexts = super::stats::contexts(home.path());
            let captured: Vec<_> = contexts
                .iter()
                .filter_map(|context| context.get("request"))
                .collect();
            assert_eq!(captured, [&first_body]);
            let invalid_titles: i64 = database.query_row("SELECT COUNT(*) FROM requests WHERE purpose='task_title' AND status!='aborted'", [], |r| r.get(0)).unwrap();
            assert_eq!(invalid_titles, 0);
            let ttft: Option<f64> = database
                .query_row(
                    "SELECT ttft_ms FROM requests WHERE status='aborted' AND purpose!='task_title'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ttft.is_some(), phase == "sse");
    }
}

// 验证项 2：两协议 write→bash→write 批次取消。父 shell 忽略 TERM 也能被回收，
// 保留的文件与已捕获输出不回滚、跳过的调用未执行，下一次请求中三个 tool
// result 与原 call_id 按序完整配对且不重复执行；取消本身不触发任何 HTTP。
#[test]
fn tui_cancel_tool_batch_stops_process_group_and_pairs_results_on_next_request() {
    let protocol = "response";
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
        cancel_drive_batch(
            &mut pty,
            &fixture,
            protocol,
            "cancel batch prompt",
            project.path(),
        );
        pty.input("批后草稿");
        pty.draft("批后草稿");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        cancel_assert_batch_stopped(project.path());
        // 取消收尾不触发任何模型请求。
        cancel_assert_no_request(&fixture);
        // 主动提交下一条任务（保留的草稿），验证配对与不重放。
        pty.input("\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        cancel_assert_paired_results(&body, protocol);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["cancel batch prompt", "批后草稿"]
        );
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
        if protocol == "completion" {
            final_reply(request, "batch recovered answer");
        } else {
            response_final(request, "batch recovered answer");
        }
        pty.text("batch recovered answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
        // 旧工具从未被重放：文件状态保持取消后的样子。
        assert_eq!(
            std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
            "kept"
        );
        assert!(!project.path().join("skipped.txt").exists());
}

// 验证项 4：headers 前、SSE 空闲、工具批次三种取消的持久化与重启。取消只
// 显示状态行、不追加提示块、不误报 interrupted；/resume 列表为 aborted，完成新任务后可为 done；
// -r ID -p 与 -c ID 的原生请求只含已接受用户消息、完整旧历史和成对工具结果，
// 不执行旧工具，pp.cancelled 不进入请求。
#[derive(Clone, Copy, PartialEq)]
enum CancelPhase {
    Headers,
    Sse,
    Tools,
}

// 一个取消阶段 × 一种协议的完整往返：取消终态落盘、-r 恢复、/resume 标记、
// 行模式 -r/-c 复用历史。阶段之间互不依赖，按用例拆分并行执行。
fn cancel_terminal_state_case(protocol: &str, phase: CancelPhase) {
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
            let mut pid_before = Vec::new();
            let mut child_before = Vec::new();
            match phase {
                CancelPhase::Headers => {
                    pty.input("persist cancel prompt\r");
                    // 请求已到达但未回应；持有 CapturedRequest 防止对端提前 EOF。
                    let (_request, _) = pty.request_at(&fixture, path);
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                }
                CancelPhase::Sse => {
                    pty.input("persist cancel prompt\r");
                    let (request, _) = pty.request_at(&fixture, path);
                    let mut stream = request.start_sse().unwrap();
                    streaming_delta(&mut stream, protocol, "persist-partial-token");
                    pty.text("persist-partial-token");
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                }
                CancelPhase::Tools => {
                    cancel_drive_batch(
                        &mut pty,
                        &fixture,
                        protocol,
                        "persist cancel prompt",
                        project.path(),
                    );
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                    cancel_assert_batch_stopped(project.path());
                    pid_before = std::fs::read(project.path().join("bash.pid")).unwrap();
                    child_before = std::fs::read(project.path().join("bash-child.pid")).unwrap();
                }
            }
            if phase == CancelPhase::Sse {
                assert_eq!(
                    cancel_count_normalized(pty.parser.screen(), "persist-partial-token"),
                    1
                );
            }
            pty.input("/quit\r");
            pty.finish();
            cancel_assert_no_request(&fixture);

            let id = saved_session_id(home.path());
            let journal = std::fs::read_to_string(
                home.path()
                    .join(".pp/agent/sessions")
                    .join(format!("{id}.jsonl")),
            )
            .unwrap();
            assert!(journal.contains("pp.cancelled"), "取消终态必须落盘");

            // 交互式 -r 恢复：回放已接受的用户消息，不追加提示块，也不出现
            // 假 interrupted 横幅。
            let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
            resumed.wait_for("恢复取消会话：已接受的用户消息与空闲输入框", |screen| {
                input_is_ready(screen) && screen.contents().contains("persist cancel prompt")
            });
            assert!(
                !cancel_normalize(resumed.parser.screen())
                    .contains("Previousrequestwasinterrupted."),
                "主动取消不得误报为 interrupted"
            );
            assert_eq!(
                cancel_count_normalized(resumed.parser.screen(), "persist cancel prompt"),
                1
            );
            if phase == CancelPhase::Sse {
                assert_eq!(
                    cancel_count_normalized(resumed.parser.screen(), "persist-partial-token"),
                    1
                );
            }
            resumed.ready();
            if phase == CancelPhase::Tools {
                // 恢复不重放旧工具：PID marker 与文件保持不变。
                assert_eq!(
                    std::fs::read(project.path().join("bash.pid")).unwrap(),
                    pid_before
                );
                assert_eq!(
                    std::fs::read(project.path().join("bash-child.pid")).unwrap(),
                    child_before
                );
                assert_eq!(
                    std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
                    "kept"
                );
                assert!(!project.path().join("skipped.txt").exists());
            }
            cancel_assert_no_request(&fixture);

            // /resume 列表为 aborted 而不是 done；结果由 history worker 异步安装。
            resume_boundary_open(&mut resumed);
            resumed.wait_for("恢复列表把取消会话标记为 aborted", |screen| {
                screen.contents().contains("■ aborted")
            });
            assert!(!resumed.parser.screen().contents().contains("\u{f00c} done"));
            resume_boundary_close(&mut resumed);

            // 取消终态之后完成新任务，列表再次可为 done。
            resumed.input("persist recovery prompt\r");
            let (request, _) = resumed.request_at(&fixture, path);
            if protocol == "completion" {
                final_reply(request, "persist recovery answer");
            } else {
                response_final(request, "persist recovery answer");
            }
            resumed.text("persist recovery answer");
            // 输入框在运行中也可编辑；等待本次任务完成后才能执行空闲命令。
            resumed.completed();
            resume_boundary_open(&mut resumed);
            resumed.wait_for("完成新任务后列表标记为 done", |screen| {
                screen.contents().contains("\u{f00c} done")
            });
            resume_boundary_close(&mut resumed);
            resumed.input("/quit\r");
            resumed.finish();
            cancel_assert_no_request(&fixture);

            // 行模式 -r ID -p：请求只含已接受用户消息与完整旧历史，无 pp.cancelled。
            let home_path = home.path().to_str().unwrap();
            let cli = Cli::spawn(
                project.path(),
                &["-r", id.as_str(), "-p", "persist cli follow"],
                &[("HOME", home_path)],
            );
            let (request, body) = receive_at(&fixture, path);
            assert_eq!(
                cancel_user_messages(&body, protocol),
                [
                    "persist cancel prompt",
                    "persist recovery prompt",
                    "persist cli follow"
                ]
            );
            assert_eq!(
                cancel_assistant_texts(&body, protocol),
                ["persist recovery answer"]
            );
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
            assert!(
                !text.contains("persist-partial-token"),
                "半成品 assistant 不得进入原生历史"
            );
            if phase == CancelPhase::Tools {
                cancel_assert_paired_results(&body, protocol);
            } else {
                assert!(
                    cancel_tool_outputs(&body, protocol).is_empty(),
                    "非工具批次取消不得产生工具结果"
                );
            }
            if protocol == "completion" {
                final_reply(request, "persist cli answer");
            } else {
                response_final(request, "persist cli answer");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"persist cli answer\n");
            cancel_assert_no_request(&fixture);

            // 行模式 -c ID：发送保存的原生请求，历史仍合法、旧工具不执行。
            let cli = Cli::spawn(project.path(), &["-c", id.as_str()], &[("HOME", home_path)]);
            let (request, body) = receive_at(&fixture, path);
            assert_eq!(
                cancel_user_messages(&body, protocol),
                [
                    "persist cancel prompt",
                    "persist recovery prompt",
                    "persist cli follow"
                ]
            );
            assert_eq!(
                cancel_assistant_texts(&body, protocol),
                ["persist recovery answer", "persist cli answer"]
            );
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
            assert!(!text.contains("persist-partial-token"));
            if phase == CancelPhase::Tools {
                cancel_assert_paired_results(&body, protocol);
            }
            if protocol == "completion" {
                final_reply(request, "persist continue answer");
            } else {
                response_final(request, "persist continue answer");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"persist continue answer\n");
            cancel_assert_no_request(&fixture);
        if phase == CancelPhase::Tools {
            assert_eq!(
                std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
                "kept"
            );
            assert!(!project.path().join("skipped.txt").exists());
        }
}

// 验证项 4 的持久化故障模式：取消收尾的 commit/sync 失败必须优先显示
// persisting session，不声称成功取消、不写 pp.cancelled、不再发模型请求，
// 同时取消路径仍要停止真实 bash 进程。
#[test]
fn tui_cancel_journal_failure_reports_persisting_session_not_success() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        // ulimit -f 256：header/用户记录写得下，流式的大内容让取消收尾的
        // native commit 以 EFBIG 失败。
        let mut pty = Pty::start_with_file_limit(project.path(), home.path(), 256, &[]);
        pty.ready();
        pty.input("overflow cancel batch\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let big = "A".repeat(400_000);
        let command =
            "sh -c 'printf \"overflow-ready\\n\"; printf \"%s\" \"$$\" > overflow-bash.pid; sleep 30'";
        if protocol == "completion" {
            reply(
                request,
                json!({
                    "role":"assistant", "content":big,
                    "tool_calls":[call("overflow-bash", "bash", json!({"command":command}))]
                }),
            );
        } else {
            response_reply(
                request,
                json!([
                    response_message("msg-overflow", json!([response_text(&big)])),
                    response_call("overflow-bash", "bash", json!({"command":command}))
                ]),
            );
        }
        // PID marker 是进入 bash 执行阶段的栅栏。
        cancel_wait_pid_files(project.path(), &["overflow-bash.pid"]);
        pty.input("\x1b");
        pty.wait_for(
            "取消收尾提交失败显示 persisting session 红色弹窗",
            |screen| {
                notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                    && cancel_normalize(screen).contains("persistingsession")
            },
        );
        // 不声称成功取消。
        assert_eq!(
            cancel_count_normalized(pty.parser.screen(), CANCEL_DONE),
            0,
            "持久化失败不得显示取消完成状态行"
        );
        pty.ready();
        // 取消收尾失败后不再发模型请求。
        cancel_assert_no_request(&fixture);
        // 取消路径仍停止了真实 bash 进程。
        cancel_wait_pid_dead(&project.path().join("overflow-bash.pid"));
        pty.input("\x03");
        pty.finish();
        let id = saved_session_id(home.path());
        let journal = std::fs::read_to_string(
            home.path()
                .join(".pp/agent/sessions")
                .join(format!("{id}.jsonl")),
        )
        .unwrap();
        assert!(
            !journal.contains("pp.cancelled"),
            "取消终态提交失败时不得写 pp.cancelled"
        );
}

