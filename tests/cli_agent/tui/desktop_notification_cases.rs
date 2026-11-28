use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn desktop_notifications_focus_reporting_preserves_ask_and_permission_roundtrip() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let notice_log = home.path().join("desktop-notices");
    let linux = cfg!(target_os = "linux");

    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pp"));
    command.cwd(cwd.path());
    command.env("HOME", home.path());
    command.args(["--sandbox", "off"]);
    let mode = if linux {
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let stub = bin.join("notify-send");
        std::fs::write(
            &stub,
            "#!/bin/sh\n[ \"$1 $2 $3\" = '--app-name=pp -- pp' ] || exit 2\nprintf '%s\\n' \"$4\" >> \"$PP_TEST_NOTICE_LOG\"\ncase \"$4\" in *answer*) exit 7;; esac\n",
        )
        .unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::var_os("PATH").unwrap();
        let path =
            std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&path))).unwrap();
        command.env("PATH", path);
        command.env("PP_TEST_NOTICE_LOG", &notice_log);
        "background"
    } else {
        "off"
    };
    write_settings(
        home.path(),
        format!("tui: {{desktop_notifications: {mode}}}\ntools: {{permission: {{write: prompt}}}}"),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let mut pty = Pty::spawn(command);
    pty.wait_for_raw(b"\x1b[?1004h");
    pty.ready();

    pty.input("\x1b[Ifocused task\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["messages"][1]["content"], "focused task");
    final_reply(request, "focused result");
    pty.completed_after("focused result");

    pty.input("\x1b[Obackground task\r");
    let (request, body) = pty.request(&server);
    assert_eq!(
        cancel_user_messages(&body, "completion").last().unwrap(),
        "background task"
    );
    calls_reply(
        request,
        json!([call("desktop-ask", "ask", ask_single_arguments())]),
    );
    ask_wait_panel(&mut pty, "Which database should the project use?");
    if linux {
        pty.wait_for("a notification while waiting for the answer", |_| {
            std::fs::read_to_string(&notice_log).is_ok_and(|text| text.contains("answer"))
        });
    }
    pty.input("\r");
    let (request, body) = pty.request(&server);
    let answers = cancel_tool_outputs(&body, "completion");
    let answer = answers.iter().find(|(id, _)| id == "desktop-ask").unwrap();
    assert_eq!(
        answer.1["answers"][0]["selected_options"],
        json!(["SQLite"])
    );

    calls_reply(
        request,
        json!([call(
            "desktop-write",
            "write",
            json!({
                "path": "approved.txt",
                "content": "approved"
            })
        )]),
    );
    pty.wait_for("the real permission approval dialog", |screen| {
        let text = screen.contents();
        text.contains("Permission required") && text.contains("Allow once")
    });
    let written = cwd.path().join("approved.txt");
    assert!(!written.exists());
    if linux {
        pty.wait_for("a notification while waiting for approval", |_| {
            std::fs::read_to_string(&notice_log).is_ok_and(|text| text.contains("approval"))
        });
    }
    pty.input("\r");
    let (request, _) = pty.request(&server);
    assert_eq!(std::fs::read_to_string(written).unwrap(), "approved");
    final_reply(request, "background result");
    pty.completed_after("background result");
    if linux {
        pty.wait_for("a background task completion notification", |_| {
            std::fs::read_to_string(&notice_log).is_ok_and(|text| text.contains("completed"))
        });
        assert_eq!(
            std::fs::read_to_string(&notice_log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            [
                "pp is waiting for your answer.",
                "pp is waiting for your approval.",
                "Task completed."
            ]
        );
    }

    pty.input("\x1b[I/quit\r");
    pty.wait_for_raw(b"\x1b[?1004l");
    pty.finish();
    no_more(&server);
}
