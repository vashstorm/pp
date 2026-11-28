// 验证项 4（手动）：摘要 SSE 被服务端握住时屏幕可见 Compacting；压缩期间
// 保留新草稿；Esc 取消后等待真实终态回到 Ready，草稿不动，下一个任务请求
// 携带完整旧历史与新输入，供应商摘要输出永不进入可见对话。
// 验证项 4（手动）：成功压缩不显示任务 Completed、不添加 You: /compact、
// 不清空 Transcript；状态栏用量下降；下一个普通请求只含一个摘要包装与
// 原生近期后缀；60/30 列 resize 上下文字段优先保留，Ctrl-C 恢复终端标志。
#[test]
fn context_tui_compact_success_keeps_dialogue_drops_usage_and_survives_narrow_resize() {
    let protocol = "response";
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

