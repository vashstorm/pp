use super::*;

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
