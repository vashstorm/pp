// 验证项 4（手动）：摘要 SSE 被服务端握住时屏幕可见 Compacting；压缩期间
// 保留新草稿；Esc 取消后等待真实终态回到 Ready，草稿不动，下一个任务请求
// 携带完整旧历史与新输入，供应商摘要输出永不进入可见对话。
#[test]
fn context_tui_esc_cancel_during_held_summary_preserves_draft_and_history() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        context_tui_fill_three_exchanges(&mut pty, &fixture, protocol);
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, "context-old-one", "context-recent-keep");
        // 服务端握住摘要 SSE：屏幕保持 Compacting 状态。
        let _held = request.start_sse().unwrap();
        pty.text("Compacting context...");
        // 压缩期间继续编辑草稿；Esc 取消并等待压缩终态。
        pty.input("context-draft-保留");
        pty.draft("context-draft-保留");
        pty.input("\x1b");
        pty.wait_for("压缩取消后回到输入状态", |screen| {
            let text = screen.contents();
            input_is_ready(screen)
                && !text.contains("Compacting context...")
                && !text.contains("Cancelling context...")
        });
        pty.ready();
        assert!(
            !pty.parser
                .screen()
                .contents()
                .contains("Compacting context...")
        );
        pty.draft("context-draft-保留");
        // 可继续任务：取消后的下一次普通请求包含完整旧历史与新输入。
        pty.input("吧\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert!(text.contains("context-draft-保留吧"));
        assert!(text.contains("context-old-one"), "取消不得丢弃旧历史");
        assert!(text.contains("context-recent-keep"));
        assert!(!text.contains("[Context summary]"));
        assert!(
            !text.contains(CONTEXT_TUI_INSTRUCTION),
            "取消后不得残留摘要指令"
        );
        assert_eq!(cancel_user_messages(&body, protocol).len(), 4);
        if protocol == "completion" {
            final_reply(request, "context recovered answer");
        } else {
            response_final(request, "context recovered answer");
        }
        pty.text("context recovered answer");
        pty.ready();
        assert!(!pty.parser.screen().contents().contains("SUMMARY"));
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 验证项 4（手动）：成功压缩不显示任务 Completed、不添加 You: /compact、
// 不清空 Transcript；状态栏用量下降；下一个普通请求只含一个摘要包装与
// 原生近期后缀；60/30 列 resize 上下文字段优先保留，Ctrl-C 恢复终端标志。
#[test]
fn context_tui_compact_success_keeps_dialogue_drops_usage_and_survives_narrow_resize() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        assert!(context_tui_metrics_hidden(pty.parser.screen()));
        context_tui_fill_three_exchanges(&mut pty, &fixture, protocol);
        let before = context_tui_ctx_tokens(pty.parser.screen());
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, "context-old-one", "context-recent-keep");
        pty.input("/model ");
        // 摘要请求尚未结束时即可补全模型；确认受忙碌检查约束。
        pty.model_search("");
        pty.text("fixture/primary");
        pty.input("\r");
        pty.model_search("");
        pty.text("Compacting context...");
        if protocol == "completion" {
            reply(
                request,
                json!({"role": "assistant", "content": CONTEXT_TUI_SUMMARY}),
            );
        } else {
            response_final(request, CONTEXT_TUI_SUMMARY);
        }
        pty.model_search("");
        pty.text("fixture/primary");
        pty.input("\x1b");
        pty.draft("");
        pty.text("Context compacted:");
        pty.wait_for("压缩后的用量快照已发布", |screen| {
            context_tui_ctx_tokens(screen) < before
        });
        let after = context_tui_ctx_tokens(pty.parser.screen());
        assert!(
            after < before,
            "压缩后估算必须下降：before {before} after {after}"
        );
        // 旧对话仍在 Transcript：向上翻页可见最早的旧标记。
        let mut paged = false;
        for _ in 0..80 {
            if pty.parser.screen().contents().contains("context-old-one") {
                paged = true;
                break;
            }
            if !resume_history_page(&mut pty, "\x1b[5~") {
                break;
            }
        }
        assert!(paged, "压缩不得清除旧对话：{}", pty.diagnostics());
        // 摘要文本与摘要指令永不进入可见对话，命令本身也不作为对话条目。
        let visible = pty.parser.screen().contents();
        assert!(!visible.contains(CONTEXT_TUI_SUMMARY), "{visible}");
        assert!(!visible.contains(CONTEXT_TUI_INSTRUCTION), "{visible}");
        assert!(!visible.contains("/compact"), "{visible}");
        for _ in 0..80 {
            if !resume_history_page(&mut pty, "\x1b[6~") {
                break;
            }
        }
        // 下一个普通请求：一个摘要包装 + 原生近期后缀，无旧大段原文。
        pty.input("context-after-question\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(text.matches("[Context summary]").count(), 1);
        assert!(text.contains(CONTEXT_TUI_SUMMARY));
        assert!(text.contains("context-recent-keep"));
        assert!(!text.contains("context-old-one"), "旧前缀原文不得复活");
        assert!(text.contains("context-after-question"));
        assert!(text.contains("noted context-recent-keep"));
        if protocol == "completion" {
            final_reply(request, "context after answer");
        } else {
            response_final(request, "context after answer");
        }
        pty.text("context after answer");
        pty.ready();
        // 窄屏 resize：上下文字段优先保留，任何宽度不溢出、不 panic。
        // resize 的首批字节可能来自上一帧，须以目标布局出现作为重绘栅栏。
        for width in [60, 30] {
            pty.resize(30, width);
            pty.wait_for("窄屏重绘保留上下文字段", |screen| {
                screen
                    .contents()
                    .lines()
                    .nth(26)
                    .is_some_and(|line| line.contains("\u{e62a} "))
            });
        }
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 验证项 4（自动取消）：两个完整只读批次把 32768 的用量推过 80% 后，服务端
// 握住自动摘要 SSE；屏幕可见 Compacting，草稿不被压缩抢占。Esc 取消等待真实
// 任务终态（Cancelled Note + Ready），取消后不得发普通请求；以自动压缩关闭
// 的配置恢复同一会话，下一次普通请求完整携带已接受输入与两批 tool results。
#[test]
fn context_tui_auto_compact_cancel_keeps_draft_and_recovers_accepted_history() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        // 窗口留出余量：保留的最近批次（40KB 只读结果）加系统与工具定义必须低于 80% 阈值。
        let (home, mut pty) = context_tui_start_auto(&fixture, protocol, 40960, 80);
        let first = "context-auto-cancel-first";
        let second = "context-auto-cancel-second";
        context_tui_write_batch(
            home.path(),
            "context-auto-cancel-one.txt",
            first,
            CONTEXT_TUI_AUTO_FIRST_BYTES,
        );
        context_tui_write_batch(
            home.path(),
            "context-auto-cancel-two.txt",
            second,
            CONTEXT_TUI_AUTO_SECOND_BYTES,
        );

        pty.input("context auto cancel prompt\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(body["tools"].as_array().is_some(), "首批批次前不得摘要");
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-cancel-one",
            "context-auto-cancel-one.txt",
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            body["tools"].as_array().is_some(),
            "首个批次不得提前触发自动摘要"
        );
        assert!(serde_json::to_string(&body).unwrap().contains(first));
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-cancel-two",
            "context-auto-cancel-two.txt",
        );

        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, first, second);
        let _held = request.start_sse().unwrap();
        pty.text("Compacting context");
        pty.wait_for("自动压缩触发后的用量超过 80%", |screen| {
            context_tui_ctx_percent(screen) >= 80
        });

        pty.input("context-auto-draft");
        pty.draft("context-auto-draft");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        // 取消后不发送普通请求，也不把摘要正文写入可见对话。
        no_more(&fixture);
        assert!(!pty.parser.screen().contents().contains(CONTEXT_TUI_SUMMARY));
        assert!(
            context_tui_page_to(&mut pty, "context auto cancel prompt"),
            "取消不得清除已接受输入：{}",
            pty.diagnostics()
        );
        context_tui_page_to_bottom(&mut pty);
        drop(_held);

        // 关闭自动压缩后恢复：此前已接受的输入与两个完整工具批次仍可用。
        let id = saved_session_id(home.path());
        pty.input("\x01\x0b");
        pty.draft("");
        pty.input("/quit\r");
        pty.finish();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            context_tui_live_settings(false, 80),
        )
        .unwrap();
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-c", &id]);
        resumed.wait_for("auto-continue keeps input editable", input_is_ready);
        let (request, body) = resumed.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert!(text.contains("context auto cancel prompt"));
        assert!(text.contains(first), "取消不得丢弃第一批 tool result");
        assert!(text.contains(second), "取消不得丢弃第二批 tool result");
        assert!(text.contains("context-auto-cancel-one"));
        assert!(text.contains("context-auto-cancel-two"));
        assert!(!text.contains("[Context summary]"));
        assert!(!text.contains(CONTEXT_TUI_INSTRUCTION));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["context auto cancel prompt"]
        );
        context_tui_final(request, protocol, "context auto cancel recovered");
        resumed.text("context auto cancel recovered");
        resumed.ready();
        resumed.input("/quit\r");
        resumed.finish();
        no_more(&fixture);
    }
}

// 验证项 4（自动成功）：摘要请求先于普通任务请求，成功后 Compact Note 在
// 任务 assistant 之前；旧对话仍在 Transcript，用量下降，摘要正文不产生
// assistant 气泡；普通请求只含摘要包装与原生近期批次。
#[test]
fn context_tui_auto_compact_success_keeps_dialogue_drops_usage_without_summary_bubble() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        // 窗口留出余量：保留的最近批次（40KB 只读结果）加系统与工具定义必须
        // 在压缩后落到 80% 阈值以下，否则自动压缩会拒绝候选并终止任务。
        let (home, mut pty) = context_tui_start_auto(&fixture, protocol, 40960, 80);
        let old = "context-auto-success-old";
        let recent = "context-auto-success-recent";
        context_tui_write_batch(
            home.path(),
            "context-auto-success-old.txt",
            old,
            CONTEXT_TUI_AUTO_FIRST_BYTES,
        );
        context_tui_write_batch(
            home.path(),
            "context-auto-success-new.txt",
            recent,
            CONTEXT_TUI_AUTO_SECOND_BYTES,
        );

        pty.input("context auto success prompt\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(body["tools"].as_array().is_some());
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-success-one",
            "context-auto-success-old.txt",
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            body["tools"].as_array().is_some(),
            "首个批次不得提前触发自动摘要"
        );
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-success-two",
            "context-auto-success-new.txt",
        );

        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, old, recent);
        pty.text("Compacting context");
        pty.wait_for("自动压缩触发后的用量超过 80%", |screen| {
            context_tui_ctx_percent(screen) >= 80
        });
        let before = context_tui_ctx_tokens(pty.parser.screen());
        context_tui_final(request, protocol, CONTEXT_TUI_SUMMARY);

        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        pty.text("Context compacted:");
        assert!(
            !pty.parser
                .screen()
                .contents()
                .contains("context auto compacted answer"),
            "Compact Note 必须在任务 assistant 之前出现"
        );
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(text.matches("[Context summary]").count(), 1);
        assert!(text.contains(CONTEXT_TUI_SUMMARY));
        assert!(text.contains(recent), "最近完整批次必须原生保留");
        assert!(!text.contains(old), "旧前缀原文不得复活");
        assert!(body["tools"].as_array().is_some());
        context_tui_final(request, protocol, "context auto compacted answer");

        pty.text("context auto compacted answer");
        pty.ready();
        let after = context_tui_ctx_tokens(pty.parser.screen());
        assert!(
            after < before,
            "自动压缩后估算必须下降：before {before} after {after}"
        );
        let visible = pty.parser.screen().contents();
        let note = visible
            .find("Context compacted:")
            .expect("自动压缩必须显示 Compact Note");
        let answer = visible.find("context auto compacted answer").unwrap();
        assert!(note < answer, "Compact Note 必须在任务 assistant 之前");
        assert!(!visible.contains(CONTEXT_TUI_SUMMARY));
        assert!(!visible.contains(CONTEXT_TUI_INSTRUCTION));
        assert!(
            context_tui_page_to(&mut pty, "context auto success prompt"),
            "自动压缩不得清除旧对话：{}",
            pty.diagnostics()
        );
        context_tui_page_to_bottom(&mut pty);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}
