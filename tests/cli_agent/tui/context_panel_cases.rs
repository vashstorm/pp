use super::*;

#[test]
fn context_panel_refreshes_during_main_run_and_preserves_cancellation() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, mut pty) = context_tui_start(&server, protocol, 128_000, 80);
        pty.input("inspect running context\r");
        let (request, _) = pty.request_at(&server, api_path(protocol));

        // 握住首个模型请求，面板必须立即显示真实上下文。
        pty.input("/context\r");
        pty.wait_for("运行中打开上下文面板", |screen| {
            let text = screen.contents();
            text.contains("Context · fixture/primary")
                && text.contains("Built-in Tools")
                && text.contains("Auto-Compact Buffer")
                && !text.contains("Inspecting current session context")
        });

        let args = json!({"command":"printf ctx-snap-ok"});
        if protocol == "completion" {
            calls_reply(request, json!([call("context-call", "bash", args)]));
        } else {
            response_reply(
                request,
                json!([response_call("context-call", "bash", args)]),
            );
        }
        let (pending_request, _) = pty.request_at(&server, api_path(protocol));

        // 主任务已完成一批工具并等待下一次模型响应；刷新须读到新工具调用与输出，并支持原地展开。
        pty.input("r");
        pty.wait_for("刷新后可见 Tool Output 非零", |screen| {
            let text = screen.contents();
            text.contains("Tool Output") && !text.contains("Tool Output  0  0.0%")
        });
        // 向下移动到 Tool Calls (index 8) 展开可见 bash(printf ctx-snap-ok) 子项。
        pty.input(&format!("{}\r", "\x1b[B".repeat(8)));
        pty.text("ctx-snap-ok");
        pty.input("\x7f");
        pty.wait_for("Backspace 收起子项", |screen| {
            !screen.contents().contains("ctx-snap-ok") && screen.contents().contains("Tool Calls")
        });
        pty.input("\x1b");
        pty.wait_for("关闭面板后主任务仍在运行", |screen| {
            let text = screen.contents();
            !text.contains("Context · fixture/primary")
                && live_activity_labels(&text).next().is_some()
        });

        // 别名同样可在运行中打开；关闭面板的 Esc 不应取消任务。
        pty.input("/ctx\r");
        pty.text("Context · fixture/primary");
        pty.input("\x1b");
        pty.wait_for("关闭别名打开的面板", |screen| {
            !screen.contents().contains("Context · fixture/primary")
        });
        pty.input("\x1b");
        pty.wait_for("关闭面板后仍可取消主任务", |screen| {
            input_is_ready(screen) && screen.contents().contains("✻ Cancelled")
        });
        drop(pending_request);
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn context_panel_updates_free_space_after_changing_compact_threshold_in_settings() {
    let server = server();
    let (_home, mut pty) = context_tui_start_auto(&server, "response", 100_000, 80);

    // 打开 /context：80% 阈值下 Auto-Compact Buffer 为 20K (20.0%)，记录当前 Free Space。
    pty.input("/context\r");
    pty.wait_for("初始显示 20K 缓冲区", |screen| {
        let text = screen.contents();
        text.contains("Auto-Compact Buffer  20K  20.0%") && text.contains("Free Space")
    });
    let extract_free_tokens = |contents: &str| -> String {
        let line = contents
            .lines()
            .find(|l| l.contains("Free Space"))
            .expect("Free Space line");
        line.split("Free Space  ")
            .nth(1)
            .expect("Free Space value")
            .split("  ")
            .next()
            .expect("token value")
            .to_owned()
    };
    let initial_free = extract_free_tokens(&pty.parser.screen().contents());
    pty.input("\x1b");
    pty.wait_for("关闭初始 /context", |screen| {
        !screen.contents().contains("Context · fixture/primary") && input_is_ready(screen)
    });

    // 通过 /setting 将 Compact threshold (%) 从 80% 改为 90% 并保存。
    pty.input("/setting\r");
    pty.wait_for("打开通用设置页", |screen| {
        screen.contents().contains("Editing & Tools")
            && screen.contents().contains("Context & Model")
    });
    pty.input("\t\x1b[B\t\x1b[B");
    pty.wait_for("选中 Compact threshold (%)", |screen| {
        screen.contents().contains("Compact threshold (%)")
    });
    pty.input("\r");
    pty.wait_for("进入行内编辑", |screen| {
        screen.contents().contains("[80|]")
    });
    pty.input("\x7f\x7f90\r");
    pty.wait_for("保存 90% 阈值", |screen| {
        screen.contents().contains("Saved to settings.yml")
    });
    pty.input("\x1b");
    pty.wait_for("关闭设置面板", |screen| {
        !screen.contents().contains("Saved to settings.yml") && input_is_ready(screen)
    });

    // 再次打开 /context：Auto-Compact Buffer 变为 10K (10.0%)，Free Space 随之增加。
    pty.input("/context\r");
    pty.wait_for(
        "修改阈值后 Auto-Compact Buffer 与 Free Space 同步更新",
        |screen| {
            let text = screen.contents();
            text.contains("Auto-Compact Buffer  10K  10.0%")
                && text.contains("auto-compaction at 90%")
                && text.contains("Free Space")
                && extract_free_tokens(&text) != initial_free
        },
    );
    pty.input("\x1b");
    pty.wait_for("关闭更新后的 /context", |screen| {
        !screen.contents().contains("Context · fixture/primary") && input_is_ready(screen)
    });

    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
