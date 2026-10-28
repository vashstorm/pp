use super::*;

#[test]
fn tui_input_history_and_control_keys_edit_the_submitted_buffer() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("old-中\r");
    let (request, _) = pty.request(&server);
    final_reply(request, "history-first-ok");
    pty.text("history-first-ok");
    pty.completed();

    pty.input("alpha\x0a中🙂trash\x01\x01\x1b[C\x1b[C\x0b-kept\x05\x05\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["messages"][3]["content"], "alpha\n中🙂-kept");
    pty.input("draft-中\x1b[D\x1b[A");
    pty.draft("中🙂-kept");
    pty.input("\x1b[A");
    pty.wait_for("历史输入中的上一行光标", |screen| {
        let (row, column) = screen.cursor_position();
        screen
            .cell(row, 2)
            .is_some_and(|cell| cell.contents() == "a")
            && column == 7
    });
    pty.input("\x1b[A");
    pty.draft("old-中");
    pty.input("\x1b[B");
    pty.draft("中🙂-kept");
    pty.input("\x1b[B!");
    pty.draft("draft-!中");
    no_more(&server);
    final_reply(request, "history-second-ok");
    pty.text("history-second-ok");
    pty.completed();

    pty.input("\x1b[A\x01\x0bcopied\r");
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.last().unwrap()["content"], "alpha\ncopied");
    assert_eq!(messages[3]["content"], "alpha\n中🙂-kept");
    final_reply(request, "history-edited-ok");
    pty.text("history-edited-ok");
    pty.ready();
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_native_cursor_and_multiline_arrows_edit_the_submitted_buffer() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.wait_for("空输入框的原生光标", |screen| {
        !screen.hide_cursor() && screen.cursor_position() == (27, 2)
    });

    pty.input("h");
    pty.wait_for("字母后的原生光标", |screen| {
        !screen.hide_cursor()
            && screen.cursor_position() == (27, 3)
            && screen
                .cell(27, 2)
                .is_some_and(|cell| cell.contents() == "h")
            && screen.cell(27, 3).is_some_and(|cell| !cell.inverse())
    });
    pty.input("\x01\x0b中🙂h\x0alast");
    pty.wait_for("多行输入末尾的原生光标", |screen| {
        !screen.hide_cursor()
            && screen.cursor_position() == (27, 6)
            && screen
                .cell(26, 2)
                .is_some_and(|cell| cell.contents() == "中")
            && screen
                .cell(26, 4)
                .is_some_and(|cell| cell.contents() == "🙂")
    });
    pty.input("\x1b[A!");
    pty.wait_for("向上移动后在首行插入", |screen| {
        !screen.hide_cursor()
            && screen.cursor_position() == (26, 7)
            && screen
                .cell(26, 6)
                .is_some_and(|cell| cell.contents() == "!")
    });
    pty.input("\x1b[B\x01>");
    pty.wait_for("向下移动后在末行插入", |screen| {
        !screen.hide_cursor()
            && screen.cursor_position() == (27, 3)
            && screen
                .cell(27, 2)
                .is_some_and(|cell| cell.contents() == ">")
    });
    pty.input("\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["messages"][1]["content"], "中🙂!h\n>last");
    final_reply(request, "cursor-edit-ok");
    pty.text("cursor-edit-ok");
    pty.ready();
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_real_task_keeps_draft_and_preserves_multiline() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.text("fixture/primary");
    pty.input("line one\x0aline two\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["model"], "fixture-model");
    assert_eq!(body["messages"][1]["content"], "line one\nline two");

    pty.input("next draft!");
    pty.draft("next draft!");
    pty.input("\x7f");
    pty.draft("next draft");
    assert!(input_is_ready(pty.parser.screen()));
    no_more(&server);
    pty.input("\x1b[200~\r\npasted line\x1b[201~");
    pty.text("pasted line");
    no_more(&server);

    let calls = json!([
        call(
            "write-proof",
            "write",
            json!({"path":"proof.txt","content":"tui-ok\n"})
        ),
        call("read-proof", "read", json!({"path":"proof.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[2]["tool_calls"], calls);
    let written = result(&messages[3], "write-proof");
    assert_eq!(written["ok"], true);
    assert_eq!(written["bytes_written"], 7);
    let read = result(&messages[4], "read-proof");
    assert_eq!(read["ok"], true);
    assert_eq!(
        std::fs::read(dir.path().join("proof.txt")).unwrap(),
        b"tui-ok\n"
    );
    let history = messages.clone();
    final_reply(request, "tui-ok");
    pty.wait_for("answer and preserved busy draft", |screen| {
        let text = screen.contents();
        text.contains("tui-ok")
            && text.contains("next draft")
            && text.contains("pasted line")
            && input_is_ready(screen)
    });
    no_more(&server);

    pty.input("\r");
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    assert_eq!(messages.len(), history.len() + 2);
    assert_eq!(messages[history.len()]["role"], "assistant");
    assert_eq!(messages[history.len()]["content"], "tui-ok");
    assert_eq!(messages.last().unwrap()["role"], "user");
    assert_eq!(
        messages.last().unwrap()["content"],
        "next draft\npasted line"
    );
    final_reply(request, "second-ok");
    pty.text("second-ok");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_reset_and_request_error_recover() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.input("first\r");
    let (request, _) = pty.request(&server);
    reply(request, json!({"role":"assistant","content":null}));
    pty.wait_for(
        "请求错误显示为红色弹窗，允许正文换行",
        |screen| {
            let text = screen.contents();
            notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                && text.contains("neither text nor")
                && text.contains("tool calls")
        },
    );
    pty.ready();

    pty.input("second\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["messages"][1]["content"], "first");
    assert_eq!(body["messages"][2]["content"], "second");
    final_reply(request, "recovered-answer");
    pty.text("recovered-answer");
    pty.ready();

    pty.input("/new\r");
    pty.wait_for("acknowledged reset", |screen| {
        let text = screen.contents();
        text.contains("▄▄▄██▀▀██")
            && input_is_ready(screen)
            && !text.contains("Session reset")
            && !text.contains("recovered-answer")
            && !text.contains("neither text nor tool calls")
    });
    no_more(&server);
    pty.input("fresh\r");
    let (request, body) = pty.request(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "fresh");
    final_reply(request, "fresh-answer");
    pty.text("fresh-answer");
    pty.ready();
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}
