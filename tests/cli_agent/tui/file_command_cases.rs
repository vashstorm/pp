use super::*;

#[test]
fn tui_slash_file_command_menu_and_submission() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    install_command(
        dir.path(),
        "review.md",
        "---\ndescription: Review source\n---\nReview $1. Focus: $@[2:].\n",
    );
    install_command(
        dir.path(),
        "deploy.md",
        "---\ndescription: Deploy checklist\n---\nDeploy with care.\n",
    );
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();

    // 忽略大小写的模糊输入列出自定义命令及其说明。
    pty.input("/RVW");
    pty.wait_for("file command candidate with description", |screen| {
        let text = screen.contents();
        text.contains("review") && text.contains("Review source")
    });
    // Tab completes the name only; nothing is sent.
    pty.input("\t");
    pty.draft("/review");
    no_more(&server);
    // Adding an argument dismisses the menu; no parameter menu appears.
    pty.input(" auth \"error handling\" tests");
    pty.wait_for("argument dismisses the file command menu", |screen| {
        !screen.contents().contains("Review source")
    });
    pty.input("\r");
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    slash_assert_message(
        &messages[1],
        "user",
        "Review auth. Focus: error handling tests.\n",
    );
    // The transcript echoes the expanded prompt, not the slash input.
    pty.text("Review auth. Focus: error handling tests.");

    // Running Enter expands and queues the template once, without an early
    // transcript echo or request; the committed boundary applies it.
    pty.input("/review pending\r");
    pty.draft("");
    pty.text("Steering: 1");
    assert!(
        !pty.parser
            .screen()
            .contents()
            .contains("Review pending. Focus: .")
    );
    no_more(&server);
    final_reply(request, "review done");
    pty.text("review done");
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    slash_assert_message(
        &messages[1],
        "user",
        "Review auth. Focus: error handling tests.\n",
    );
    slash_assert_message(&messages[2], "assistant", "review done");
    slash_assert_message(&messages[3], "user", "Review pending. Focus: .\n");
    final_reply(request, "pending done");
    pty.text("pending done");
    pty.ready();

    // Input history recalls the original slash text, not the expansion.
    pty.input("\x1b[A");
    pty.draft("/review pending");
    pty.input("\x1b[A");
    pty.draft("/review auth \"error handling\" tests");
    pty.input("\x01\x0b");
    pty.draft("");

    // Enter on the menu of an argument-less template submits directly.
    pty.input("/DLY");
    pty.wait_for("second file command candidate", |screen| {
        let text = screen.contents();
        text.contains("deploy") && text.contains("Deploy checklist")
    });
    pty.input("\r");
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6);
    slash_assert_message(&messages[5], "user", "Deploy with care.\n");
    final_reply(request, "deploy done");
    pty.text("deploy done");
    pty.ready();

    // Esc cancels the menu and keeps the draft, without sending.
    pty.input("/rev");
    pty.wait_for("file command menu reopened", |screen| {
        screen.contents().contains("Review source")
    });
    pty.input("\x1b");
    pty.wait_for("Esc dismisses the file command menu", |screen| {
        !screen.contents().contains("Review source")
    });
    pty.draft("/rev");
    no_more(&server);
    pty.input("\x7f\x7f\x7f\x7f");
    pty.draft("");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
