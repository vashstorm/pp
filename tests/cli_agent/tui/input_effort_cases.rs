use super::*;

#[test]
fn tui_effort_and_shift_tab_apply_to_each_continuation_while_busy() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("draft.txt"), "draft body").unwrap();
    std::fs::write(dir.path().join("draft2.txt"), "second body").unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("/effort ");
    pty.text("Highest level the model supports");
    pty.draft("/effort");
    assert!(server.try_recv().unwrap().is_none());
    pty.input("\u{1b}[B\u{1b}[B\u{1b}[B\u{1b}[B\r");
    pty.ready_effort("xhigh");
    pty.input("first\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "xhigh");
    // 不回复本轮请求：运行中提交的 `/effort` 只登记期望值，下一个请求必须已在
    // 边界领取新档位，且状态栏在边界前仍显示已应用的 xhi。
    pty.input("/effort none\r");
    pty.wait_for("运行中登记 /effort 待生效档位", |screen| {
        let text = screen.contents();
        text.contains("pending next request")
            && text
                .lines()
                .last()
                .is_some_and(|line| line.contains(" xhi ·"))
    });
    calls_reply(
        request,
        json!([call("effort-one", "read", json!({"path":"draft.txt"}))]),
    );
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "none");
    pty.wait_for("边界后状态栏切到 no think", |screen| {
        input_is_ready(screen)
            && screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.contains(" off ·"))
    });
    // Shift + Tab 在同一任务的下一轮输出期间同样只登记期望值。
    pty.input("\u{1b}[Z");
    pty.wait_for("运行中 Shift + Tab 登记待生效档位", |screen| {
        screen.contents().contains("pending next request")
    });
    calls_reply(
        request,
        json!([call("effort-two", "read", json!({"path":"draft2.txt"}))]),
    );
    let (request, body) = pty.request(&server);
    assert_eq!(body["reasoning_effort"], "low");
    final_reply(request, "busy effort done");
    pty.completed_after("busy effort done");
    pty.ready_effort("low");
    // 菜单的“当前”标记跟已应用档位走。
    pty.input("/effort ");
    pty.text("Light thinking (current)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("/effort max\r");
    pty.ready_effort("max");
    pty.input("\u{3}");
    pty.finish();
}
