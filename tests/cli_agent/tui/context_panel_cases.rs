use super::*;

#[test]
fn context_panel_refreshes_during_main_run_and_preserves_cancellation() {
    let protocol = "response";
    let server = server();
    let (_home, mut pty) = context_tui_start(&server, protocol, 128_000, 80);
    pty.input("inspect running context\r");
    let (request, _) = pty.request_at(&server, api_path(protocol));

    // Hold the first model request: the panel must show the real context immediately.
    pty.input("/context\r");
    pty.wait_for(
        "the context panel opens while the task is running",
        |screen| {
            let text = screen.contents();
            text.contains("Context · fixture/primary")
                && text.contains("Built-in Tools")
                && text.contains("Auto-Compact Buffer")
                && !text.contains("Inspecting current session context")
        },
    );

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

    // The main task already finished a batch of tools and waits for the next model response; a refresh must pick up the new tool
    // calls and output and support expanding in place.
    pty.input("r");
    pty.wait_for(
        "Tool Output shows a non-zero value after the refresh",
        |screen| {
            let text = screen.contents();
            text.contains("Tool Output") && !text.contains("Tool Output  0  0.0%")
        },
    );
    // Move down to Tool Calls (index 8) and expand the visible bash(printf ctx-snap-ok) child.
    pty.input(&format!("{}\r", "\x1b[B".repeat(8)));
    pty.text("ctx-snap-ok");
    pty.input("\x7f");
    pty.wait_for("Backspace collapses the child", |screen| {
        !screen.contents().contains("ctx-snap-ok") && screen.contents().contains("Tool Calls")
    });
    pty.input("\x1b");
    pty.wait_for(
        "the main task is still running after the panel closes",
        |screen| {
            let text = screen.contents();
            !text.contains("Context · fixture/primary")
                && live_activity_labels(&text).next().is_some()
        },
    );

    // The alias opens while running too; the Esc that closes the panel must not cancel the task.
    pty.input("/ctx\r");
    pty.text("Context · fixture/primary");
    pty.input("\x1b");
    pty.wait_for("the panel opened by the alias closes", |screen| {
        !screen.contents().contains("Context · fixture/primary")
    });
    pty.input("\x1b");
    pty.wait_for(
        "the main task can still be cancelled after the panel closes",
        |screen| input_is_ready(screen) && screen.contents().contains("✻ Cancelled"),
    );
    drop(pending_request);
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
