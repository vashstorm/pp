// TUI regression for the local `!command` shell: real binary + PTY + loopback provider. Covers the output
// card/Ctrl + O expansion/input history and isolation from model requests and sessions, the three
// rejections (busy/Plan/pending), Esc cancellation reclaiming the whole process group, Ctrl + J multi-line
// input and the bash timeout. Every barrier is the visible screen, HTTP or a PID/file marker, never sleep to
// guess UI stages. Helpers are all named with the `bang_shell_` prefix and no use declaration is added, to
// avoid clashing with sibling include files.

fn bang_shell_start(
    server: &TestServer,
    home: &Path,
    project: &Path,
    protocol: &str,
    settings_extra: &str,
) -> Pty {
    std::fs::write(
        config_path(home, "settings.yml"),
        format!(
            r#"{{"default_model":"fixture/primary","notification_timeout_seconds":2{settings_extra}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project, home, &[]);
    pty.ready();
    pty
}

fn bang_shell_sessions_dir(home: &Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

// Bash card completion barrier: the footer line must contain Wall together with Timeout or the failure
// status; an idle input box alone does not prove the local shell finished.
fn bang_shell_wait_footer(pty: &mut Pty, description: &str, status: &str) {
    let compact = status.replace(' ', "");
    pty.wait_for(description, |screen| {
        screen
            .contents()
            .lines()
            .rfind(|line| {
                line.contains("╰") && (line.contains("Wall:") || line.contains("Elapsed:"))
            })
            .is_some_and(|line| {
                let line = line.split_whitespace().collect::<String>();
                line.contains("Wall:")
                    && line.contains(&compact)
                    && (status != "Timeout:"
                        || (!line.contains("Exit:")
                            && !line.contains("cancelled")
                            && !line.contains("timedout")))
            })
    });
}

// Controlled termination of the local shell: the status hint disappears and the input area is restored.
fn bang_shell_wait_done(pty: &mut Pty) {
    pty.wait_for("local shell finished and the input area is restored", |screen| {
        let text = screen.contents();
        input_is_ready(screen)
            && !text.contains("Running shell...")
            && !text.contains("Cancelling shell...")
    });
}

#[test]
fn bash_task_polls_reuse_one_card_and_keep_bounded_tail_both_protocols() {
    let protocol = "response";
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = ask_tui_start(&provider, home.path(), project.path(), protocol);
        pty.input("background output\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let args = json!({"command":"for i in {0..19}; do printf 'task-line-%02d\\n' \"$i\"; done; touch task-output-ready", "mode":"background"});
        let make_call = |id: &str, name: &str, args: Value| {
            if protocol == "completion" {
                call(id, name, args)
            } else {
                response_call(id, name, args)
            }
        };
        ask_reply_calls(request, protocol, json!([make_call("start", "bash", args)]));
        let (mut request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        let task_id = outputs.last().unwrap().1["task_id"].as_str().unwrap().to_owned();
        pty.wait_for("background command has flushed its output", |_| project.path().join("task-output-ready").is_file());
        for poll in 0..5 {
            ask_reply_calls(request, protocol, json!([make_call(
                &format!("poll-{poll}"), "bash_task", json!({"task_id":task_id,"action":"status"})
            )]));
            let (next, body) = pty.request_at(&provider, api_path(protocol));
            request = next;
            let outputs = cancel_tool_outputs(&body, protocol);
            assert_eq!(outputs.last().unwrap().1["task_id"], task_id);
            pty.text(&format!("Task {task_id}:"));
            let history = pty.native_history().0;
            assert_eq!(history.lines().filter(|line| line.contains('╭') && line.contains("Bash")).count(), 1, "{history}");
            assert!(!history.contains("Bash task ·"), "{history}");
        }
        let args = json!({"task_id":task_id, "action":"stop"});
        ask_reply_calls(request, protocol, json!([make_call("stop", "bash_task", args)]));
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        let stdout = outputs.last().unwrap().1["output"].as_str().unwrap();
        assert!(stdout.contains("task-line-00") && stdout.contains("task-line-19"));
        pty.wait_for("Bash task snapshot shows only the latest ten lines", |screen| {
            let text = screen.contents();
            text.contains("task-line-19")
                && text.contains("10 earlier lines, showing 10 of 20")
                && !text.contains("task-line-09")
        });
        pty.input("kept snapshot draft");
        pty.draft("kept snapshot draft");
        pty.function_key('o');
        pty.text("task-line-00");
        pty.draft("kept snapshot draft");
        ask_final_reply(request, protocol, "snapshot done");
        pty.text("snapshot done");
        pty.completed();
        // Ctrl + O browses on the alternate screen; the native history commit can only be checked after
        // returning to the main screen.
        pty.input("\x1b");
        pty.wait_for("exiting the expanded tool view", |screen| !screen.alternate_screen());
        let deadline = Instant::now() + DEADLINE;
        let history = loop {
            let history = pty.native_history().0;
            if history.lines().any(|line| line.contains('╭') && line.contains("Bash")) {
                break history;
            }
            assert!(Instant::now() < deadline, "Bash card was not committed to native history: {history}");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(history.lines().filter(|line| line.contains('╭') && line.contains("Bash")).count(), 1, "{history}");
        pty.input("\x03");
        pty.finish();
        no_more(&provider);
}

