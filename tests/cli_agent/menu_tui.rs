#[test]
fn tui_streaming_completion_menus_remain_usable_both_protocols() {
    let protocol = "response";
        let server = server();
        let home = runtime_input_home(&server, protocol, "");
        install_command(home.path(), "qcmd.md", "Review $ARGUMENTS");
        std::fs::write(home.path().join("picker-file.rs"), "fn main() {}\n").unwrap();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
        pty.ready();
        let mut stream = runtime_input_live(&mut pty, &server, protocol, "completion task");

        pty.input("/mo");
        pty.text("Select a model at the next safe boundary");
        streaming_delta(&mut stream, protocol, "completion-menu-stream-progress");
        pty.text("completion-menu-stream-progress");
        pty.text("Select a model at the next safe boundary");
        pty.input("\x1b");
        pty.wait_for("Esc collapses completion and keeps the running input", |screen| {
            !screen
                .contents()
                .contains("Select a model at the next safe boundary")
                && screen.contents().contains("❯ /mo")
        });
        // Without changing the draft, press Tab to re-open and complete it.
        pty.input("\t");
        pty.draft("/model");
        pty.input(" ");
        pty.text("");
        pty.text("fixture/primary");
        pty.input("\x1b");
        // The main input box is always visible below the panel; the search focus must really be handed back, otherwise
        // the immediately following Esc merges with the next character into an Alt sequence.
        pty.draft("");

        pty.input("/think h");
        pty.text("Deep thinking");
        pty.input("\t");
        pty.draft("/effort high");
        // Enter now submits the completed `/effort high` directly and clears the draft, so a following Esc would cancel
        // the task; this only verifies the completion menu during streaming, so Esc closes the menu after completing.
        pty.input("\x1b");
        pty.wait_for("closing the thinking completion keeps the running draft", |screen| {
            !screen.contents().contains("Deep thinking")
                && screen.contents().contains("❯ /effort high")
        });

        pty.input("\x01\x0b/qc");
        pty.wait_for("template command completion is shown during output", |screen| {
            screen
                .contents()
                .lines()
                .any(|line| line.starts_with("❯ /qc"))
                && screen.contents().contains("/qcmd")
        });
        pty.input("\t @picker");
        pty.text("picker-file.rs");
        pty.input("\t");
        pty.draft("/qcmd @picker-file.rs");
        streaming_delta(&mut stream, protocol, "completion-file-stream-progress");
        pty.text("completion-file-stream-progress");
        pty.draft("/qcmd @picker-file.rs");
        no_more(&server);

        streaming_complete(
            &mut stream,
            protocol,
            "runtime-live-markercompletion-menu-stream-progresscompletion-file-stream-progress",
        );
        stream.finish();
        pty.completed();
        pty.draft("/qcmd @picker-file.rs");
        pty.input("\x03");
        pty.finish();
        no_more(&server);
}

