// Real pp subprocesses, isolated HOME/workspace, and loopback providers only.
// HTTP replies, committed tool results, terminal frames, and disk bytes fence stages.

const CHANGES_ORIGINAL: &[u8] = b"user-local\r\nkeep";

fn changes_start(
    server: &Server,
    protocol: &str,
    mode: &str,
) -> (tempfile::TempDir, tempfile::TempDir, Pty) {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        diff_blocks_settings(mode),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    std::fs::write(project.path().join("demo.txt"), CHANGES_ORIGINAL).unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    (home, project, pty)
}

// Ask the running fixture for a real read before each hashline edit; never synthesize anchors.
fn changes_edit_arguments(
    pty: &mut Pty,
    server: &Server,
    protocol: &str,
    mode: &str,
    request: Request,
    step: usize,
) -> (Request, Value) {
    let old = if step == 1 { "user-local" } else { "agent-one" };
    let new = if step == 1 { "agent-one" } else { "agent-final" };
    let file = if step == 1 { "demo.txt" } else { "./demo.txt" };
    if mode == "replace" {
        return (
            request,
            json!({"path":file,"edits":[{"oldText":old,"newText":new}]}),
        );
    }
    let id = format!("changes-read-{step}");
    diff_blocks_calls_reply(
        request,
        protocol,
        json!([diff_blocks_call(protocol, &id, "read", json!({"path":file}))]),
    );
    let (request, body) = pty.request_at(server, api_path(protocol));
    let result = diff_blocks_tool_result(&body, protocol, &id);
    assert_eq!(result["ok"], true, "{result}");
    let header = result["content"].as_str().unwrap().lines().next().unwrap();
    assert!(header.starts_with('['), "{header}");
    (request, json!({"input":format!("{header}\nPUT 1.=1:\n+{new}")}))
}

fn changes_modify_twice(pty: &mut Pty, server: &Server, protocol: &str, mode: &str) {
    pty.input("changes task edits twice and creates a file\r");
    let (request, _) = pty.request_at(server, api_path(protocol));
    let (request, arguments) =
        changes_edit_arguments(pty, server, protocol, mode, request, 1);
    diff_blocks_calls_reply(
        request,
        protocol,
        json!([diff_blocks_call(protocol, "changes-edit-one", "edit", arguments)]),
    );
    let (request, body) = pty.request_at(server, api_path(protocol));
    assert_eq!(
        diff_blocks_tool_result(&body, protocol, "changes-edit-one")["ok"],
        true
    );
    let (request, arguments) =
        changes_edit_arguments(pty, server, protocol, mode, request, 2);
    diff_blocks_calls_reply(
        request,
        protocol,
        json!([
            diff_blocks_call(protocol, "changes-edit-final", "edit", arguments),
            diff_blocks_call(
                protocol,
                "changes-create",
                "write",
                json!({"path":"new.txt","content":"created\n"})
            ),
        ]),
    );
    let (request, body) = pty.request_at(server, api_path(protocol));
    for id in ["changes-edit-final", "changes-create"] {
        let result = diff_blocks_tool_result(&body, protocol, id);
        assert_eq!(result["ok"], true, "{id}: {result}");
    }
    diff_blocks_final(request, protocol, "changes task finished");
    pty.text("changes task finished");
    pty.completed();
}

fn changes_open(pty: &mut Pty) {
    pty.input("/changes\r");
    pty.wait_for("latest-task panel finished loading", |screen| {
        let text = screen.contents();
        text.contains("Latest task changes")
            && text.contains("edit/write only")
            && text.contains("memory only")
            && (text.contains("Ready")
                || text.contains("Blocked")
                || text.contains("No edit/write changes in the latest task."))
    });
}

fn changes_confirmation(pty: &mut Pty, file: &str, created: bool) {
    pty.input("u");
    pty.wait_for("per-file undo confirmation", |screen| {
        let text = screen.contents();
        text.contains("Undo file?")
            && text.contains(file)
            && text.contains("Cancel")
            && text.contains("Undo file")
            && text.contains(if created {
                "Delete this file created by this task."
            } else {
                "Restore content from before this task's first modification."
            })
    });
}

fn changes_assert_net(pty: &mut Pty) {
    pty.wait_for("deduplicated net diff with real line numbers and colors", |screen| {
        let text = screen.contents();
        text.lines()
            .filter(|line| line.contains("demo.txt") && line.contains("Modified"))
            .count()
            == 1
            && text.lines().any(|line| {
                line.contains("demo.txt") && line.contains("+1/-1") && line.contains("Ready")
            })
            && text.lines().any(|line| {
                line.contains("new.txt") && line.contains("Created") && line.contains("+1/-0")
            })
            && text.contains("  1 - user-local")
            && text.contains("  1 + agent-final")
            && text.contains("  2   keep")
            && !text.contains("agent-one")
            && diff_blocks_screen_colored(screen, "user-local", DIFF_BLOCKS_REMOVED)
            && diff_blocks_screen_colored(screen, "agent-final", DIFF_BLOCKS_ADDED)
    });
}

#[test]
fn changes_tui_net_diff_cancel_restore_and_delete_all_protocols_and_edit_modes() {
    for protocol in ["completion", "response"] {
        for mode in ["replace", "hashline"] {
            let server = server();
            let (_home, project, mut pty) = changes_start(&server, protocol, mode);
            changes_modify_twice(&mut pty, &server, protocol, mode);
            let demo = project.path().join("demo.txt");
            let new = project.path().join("new.txt");
            let latest = std::fs::read(&demo).unwrap();
            assert!(String::from_utf8_lossy(&latest).contains("agent-final"));
            assert_eq!(std::fs::read(&new).unwrap(), b"created\n");

            changes_open(&mut pty);
            changes_assert_net(&mut pty);
            changes_confirmation(&mut pty, "demo.txt", false);
            // Enter on the default Cancel must leave both files untouched.
            pty.input("\r");
            changes_assert_net(&mut pty);
            assert_eq!(std::fs::read(&demo).unwrap(), latest);
            assert_eq!(std::fs::read(&new).unwrap(), b"created\n");

            changes_confirmation(&mut pty, "demo.txt", false);
            pty.input("\x1b");
            changes_assert_net(&mut pty);
            assert_eq!(std::fs::read(&demo).unwrap(), latest);
            changes_confirmation(&mut pty, "demo.txt", false);
            // A second Enter must not turn one confirmation into two undo requests.
            pty.input("\t\r\r");
            pty.wait_for("restored file removed, next file selected", |screen| {
                let text = screen.contents();
                text.contains("Latest task changes")
                    && !text.contains("demo.txt")
                    && !text.contains("Undo file?")
                    && text.contains("new.txt")
                    && text.contains("Ready")
                    && text.contains("  1 + created")
            });
            assert_eq!(std::fs::read(&demo).unwrap(), CHANGES_ORIGINAL);
            assert_eq!(std::fs::read(&new).unwrap(), b"created\n");

            changes_confirmation(&mut pty, "new.txt", true);
            pty.input("\t\r");
            pty.text("No edit/write changes in the latest task.");
            assert!(!new.exists());
            assert_eq!(std::fs::read(&demo).unwrap(), CHANGES_ORIGINAL);
            // Empty lists cannot offer another undo.
            pty.input("u");
            pty.input("\x1b");
            pty.ready();
            pty.draft("");
            pty.input("/quit\r");
            pty.finish();
            no_more(&server);
        }
    }
}

#[test]
fn changes_tui_external_edit_during_confirmation_refuses_then_refresh_allows_undo() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, project, mut pty) = changes_start(&server, protocol, "replace");
        changes_modify_twice(&mut pty, &server, protocol, "replace");
        let demo = project.path().join("demo.txt");
        let latest = std::fs::read(&demo).unwrap();
        changes_open(&mut pty);
        changes_confirmation(&mut pty, "demo.txt", false);
        let external = b"outside\r\nuser bytes\0without newline";
        std::fs::write(&demo, external).unwrap();
        pty.input("\t\r");
        pty.wait_for("late disk conflict refused without closing panel", |screen| {
            let text = screen.contents();
            text.contains("Latest task changes")
                && text.contains("File changed outside this task; undo refused.")
                && text.lines().any(|line| line.contains("demo.txt") && line.contains("Blocked"))
        });
        assert_eq!(std::fs::read(&demo).unwrap(), external);
        assert_eq!(std::fs::read(project.path().join("new.txt")).unwrap(), b"created\n");
        // A blocked u must not open a confirmation or write through the conflict.
        pty.input("u");
        pty.input("r");
        pty.wait_for("refresh still reports the external conflict", |screen| {
            let text = screen.contents();
            !text.contains("Undo file?")
                && text.lines().any(|line| line.contains("demo.txt") && line.contains("Blocked"))
        });
        assert_eq!(std::fs::read(&demo).unwrap(), external);
        // Only the current mismatch is transient: putting latest back then refreshing recovers Ready.
        std::fs::write(&demo, &latest).unwrap();
        pty.input("r");
        changes_assert_net(&mut pty);
        changes_confirmation(&mut pty, "demo.txt", false);
        pty.input("\t\r");
        pty.wait_for("successful retry removes restored path", |screen| {
            let text = screen.contents();
            text.contains("Latest task changes") && text.contains("new.txt") && !text.contains("demo.txt")
        });
        assert_eq!(std::fs::read(&demo).unwrap(), CHANGES_ORIGINAL);
        assert_eq!(std::fs::read(project.path().join("new.txt")).unwrap(), b"created\n");
        pty.input("\x1b");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn changes_tui_detail_scroll_and_file_list_selection_survive_small_terminal() {
    let server = server();
    let (_home, project, mut pty) = changes_start(&server, "completion", "replace");
    let original: String = (0..90).map(|line| format!("base-{line:03}\n")).collect();
    let latest: String = (0..90).map(|line| format!("agent-{line:03}\n")).collect();
    std::fs::write(project.path().join("demo.txt"), &original).unwrap();
    pty.input("changes long detail and many files\r");
    let (request, _) = pty.request_at(&server, api_path("completion"));
    let mut calls = vec![diff_blocks_call(
        "completion",
        "changes-long",
        "write",
        json!({"path":"demo.txt","content":latest}),
    )];
    for index in 0..10 {
        calls.push(diff_blocks_call(
            "completion",
            &format!("changes-list-{index}"),
            "write",
            json!({"path":format!("z{index:02}.txt"),"content":format!("created-{index:02}\n")}),
        ));
    }
    diff_blocks_calls_reply(request, "completion", Value::Array(calls));
    let (request, body) = pty.request_at(&server, api_path("completion"));
    assert_eq!(diff_blocks_tool_result(&body, "completion", "changes-long")["ok"], true);
    assert_eq!(diff_blocks_tool_result(&body, "completion", "changes-list-9")["ok"], true);
    diff_blocks_final(request, "completion", "changes long task done");
    pty.text("changes long task done");
    pty.completed();
    changes_open(&mut pty);
    pty.wait_for("long diff begins at physical first line", |screen| {
        let text = screen.contents();
        text.contains("+90/-90")
            && text.contains("  1 - base-000")
            && diff_blocks_screen_colored(screen, "base-000", DIFF_BLOCKS_REMOVED)
    });
    pty.input("\x1b[6~");
    pty.wait_for("PgDn scrolls detail but retains selected file", |screen| {
        let text = screen.contents();
        text.contains("demo.txt") && text.contains("base-") && !text.contains("base-000")
    });
    let paged = pty.parser.screen().contents();
    pty.input("\x1b[<65;50;15M");
    pty.wait_for("mouse wheel scrolls physical detail lines", |screen| {
        let text = screen.contents();
        text.contains("demo.txt") && text.contains("base-") && text != paged
    });
    for _ in 0..20 {
        pty.input("\x1b[5~");
    }
    pty.text("  1 - base-000");
    for _ in 0..10 {
        pty.input("\x1b[B");
    }
    pty.wait_for("list scroll follows last selected file and replaces detail", |screen| {
        let text = screen.contents();
        text.contains("z09.txt") && text.contains("  1 + created-09") && !text.contains("base-000")
    });
    pty.resize(10, 40);
    pty.text("created-09");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.text("  1 + created-09");
    for _ in 0..10 {
        pty.input("\x1b[A");
    }
    pty.text("  1 - base-000");
    pty.input("\x1b");
    pty.ready();
    pty.draft("");
    assert_eq!(std::fs::read_to_string(project.path().join("demo.txt")).unwrap(), latest);
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn changes_tui_escape_resize_wheel_and_paste_preserve_conversation_viewport_and_empty_draft() {
    let server = server();
    let (_home, mut pty) = context_tui_start(&server, "completion", 100_000, 80);
    context_tui_fill_three_exchanges(&mut pty, &server, "completion");
    for _ in 0..30 {
        pty.input("\x1b[5~");
    }
    // Visible editor edits fence the preceding scroll keys before taking the viewport snapshot.
    context_tui_paste(&mut pty, "changes-viewport-fence");
    pty.draft("changes-viewport-fence");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.text("context-old-one-0000");
    let before = pty.parser.screen().contents();
    changes_open(&mut pty);
    pty.text("No edit/write changes in the latest task.");
    context_tui_paste(&mut pty, "DO-NOT-EDIT-THE-DRAFT\n/quit");
    pty.input("\x1b[A\x1b[B\x1b[5~\x1b[6~\x1b[<64;50;10M\x1b[<65;50;10M");
    pty.resize(10, 40);
    pty.text("Latest task changes");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.text("No edit/write changes in the latest task.");
    pty.input("\x1b");
    pty.wait_for("original conversation viewport restored", |screen| {
        !screen.contents().contains("Latest task changes")
            && screen.contents().contains("context-old-one-0000")
            && input_is_ready(screen)
    });
    pty.draft("");
    assert_eq!(
        pty.parser.screen().contents().lines().map(str::trim_end).collect::<Vec<_>>(),
        before.lines().map(str::trim_end).collect::<Vec<_>>()
    );
    // /changes is an explicitly submitted command in input history; panel paste is not.
    pty.input("\x1b[A");
    pty.draft("/changes");
    pty.input("\x01\x0b/quit\r");
    pty.finish();
    no_more(&server);
}

#[test]
fn changes_tui_running_command_is_rejected_and_never_enters_model_history() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, _project, mut pty) = changes_start(&server, protocol, "replace");
        let stream = runtime_input_live(&mut pty, &server, protocol, "changes-running-task");
        pty.input("/changes\r");
        pty.wait_for("busy command rejection notification", |screen| {
            let text = screen.contents();
            text.contains("Finish or cancel the current task before opening")
                && text.contains("/changes.")
        });
        pty.draft("/changes");
        assert!(!pty.parser.screen().contents().contains("Latest task changes"));
        runtime_input_end_live(stream, protocol);
        pty.completed();
        pty.draft("/changes");
        pty.input("\x01\x0bchanges-following-task\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["changes-running-task", "changes-following-task"]
        );
        diff_blocks_final(request, protocol, "changes following task done");
        pty.text("changes following task done");
        pty.completed();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn changes_tui_pending_followup_can_be_inspected_without_resuming_or_losing_it() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (_home, _project, mut pty) = changes_start(&server, protocol, "replace");
        let stream = runtime_input_live(&mut pty, &server, protocol, "changes-paused-task");
        pty.input("changes-queued-followup\x1b\r");
        runtime_input_counts(&mut pty, 0, 1, false);
        pty.input("\x1b");
        runtime_input_cancel_terminal(&mut pty, 1, 0, 1);
        drop(stream);
        changes_open(&mut pty);
        pty.text("No edit/write changes in the latest task.");
        // Pending-input and resume shortcuts belong to the covered panel while it is open.
        pty.function_key('r');
        pty.function_key('q');
        no_more(&server);
        pty.text("No edit/write changes in the latest task.");
        pty.input("\x1b");
        pty.ready();
        runtime_input_counts(&mut pty, 0, 1, true);
        pty.draft("");
        pty.function_key('r');
        let (request, body) = pty.request_at(&server, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["changes-paused-task", "changes-queued-followup"]
        );
        diff_blocks_final(request, protocol, "changes queued followup done");
        pty.text("changes queued followup done");
        runtime_input_queue_drained(&mut pty);
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn changes_tui_restart_and_resume_replay_diffs_but_never_restore_undo_snapshots() {
    for protocol in ["completion", "response"] {
        let server = server();
        let (home, project, mut pty) = changes_start(&server, protocol, "replace");
        changes_modify_twice(&mut pty, &server, protocol, "replace");
        changes_open(&mut pty);
        changes_assert_net(&mut pty);
        pty.input("\x1b");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        let latest = std::fs::read(project.path().join("demo.txt")).unwrap();
        let id = saved_session_id(home.path());
        for startup_resume in [false, true] {
            let args = if startup_resume { vec!["--resume", id.as_str()] } else { vec![] };
            let mut pty = Pty::start_with_args(project.path(), home.path(), &args);
            pty.ready();
            if !startup_resume {
                pty.input("/resume\r");
                pty.wait_for("saved task in resume picker", |screen| {
                    let text = screen.contents();
                    text.contains("Resume session")
                        && text.contains("changes task edits twice and creates a file")
                        && text.lines().any(|line| line.starts_with('>'))
                });
                pty.input("\r");
                resume_history_wait_resumed(&mut pty, &id, &["changes task finished"]);
            }
            // These are the historical per-call diffs, not a reconstructed undo baseline.
            pty.wait_for("historical committed diff replayed", |screen| {
                let text = screen.contents();
                text.contains("-  1│agent-one")
                    && text.contains("+  1│agent-final")
                    && text.contains("created")
                    && text.contains("Wrote 8 bytes")
                    && diff_blocks_screen_colored(screen, "agent-one", DIFF_BLOCKS_REMOVED)
                    && diff_blocks_screen_colored(screen, "agent-final", DIFF_BLOCKS_ADDED)
            });
            changes_open(&mut pty);
            pty.text("No edit/write changes in the latest task.");
            pty.input("u");
            pty.input("\x1b");
            pty.ready();
            assert_eq!(std::fs::read(project.path().join("demo.txt")).unwrap(), latest);
            assert_eq!(std::fs::read(project.path().join("new.txt")).unwrap(), b"created\n");
            pty.input("/quit\r");
            pty.finish();
            no_more(&server);
        }
    }
}

#[test]
fn changes_line_mode_requires_tui_and_single_prompt_keeps_literal_command() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let settings = diff_blocks_settings("replace");
        let models = response_models(&server, Some(protocol));
        let mut cli = super::streaming::LiveCli::new(Cli::start_with(
            home.path(),
            None,
            &settings,
            &models,
        ));
        cli.input("/changes\n");
        cli.wait_for(
            super::streaming::Pipe::Stderr,
            "/changes requires the TUI; run pp in a terminal.",
        );
        no_more(&server);
        cli.input("/changes extra\n");
        cli.wait_for(super::streaming::Pipe::Stderr, "usage: /changes");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("/changes requires the TUI; run pp in a terminal."), "{stderr}");
        assert!(stderr.contains("usage: /changes"), "{stderr}");
        no_more(&server);
        let cli = super::streaming::LiveCli::new(Cli::start_with_existing_files(
            home.path(),
            Some("/changes"),
        ));
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(cancel_user_messages(&body, protocol), ["/changes"]);
        diff_blocks_final(request, protocol, "literal changes prompt");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "literal changes prompt");
        no_more(&server);
    }
}
