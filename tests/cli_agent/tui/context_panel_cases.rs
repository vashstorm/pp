use super::*;

#[test]
fn context_panel_refreshes_during_main_run_and_preserves_cancellation() {
    let protocol = "response";
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
        !text.contains("Context · fixture/primary") && live_activity_labels(&text).next().is_some()
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
