// TUI regression for the temporary pipe stage panel: real `pp` + portable-pty + embedded shell.
// Local `!` commands produce no model requests; assertions use only the visible screen, latch files
// and real terminal states.
// Helpers use the `pipes_` prefix and tests use the `pipeline_` prefix to avoid clashing with sibling includes.

const PIPES_F8: &str = "\x1b[19~";
const PIPES_PGUP: &str = "\x1b[5~";
const PIPES_END: &str = "\x1b[4~";

fn pipes_open(pty: &mut Pty) {
    pty.input(PIPES_F8);
    pty.wait_for("pipe stage panel opens", |screen| {
        screen.alternate_screen() && screen.contents().contains("Pipe streams")
    });
}

fn pipes_close(pty: &mut Pty) {
    pty.input("\x1b");
    pty.wait_for("pipe stage panel closes and restores the main screen", |screen| {
        !screen.alternate_screen()
    });
}

/// `live-NN` line numbers in the visible body; the command echo only contains `live-%02d`, so it
/// never matches a number.
fn pipes_live_numbers(screen: &vt100::Screen) -> Vec<u32> {
    pipes_numbers(screen, "live-")
}

fn pipes_numbers(screen: &vt100::Screen, prefix: &str) -> Vec<u32> {
    let text = screen.contents();
    let mut numbers = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(index) = rest.find(prefix) {
            rest = &rest[index + prefix.len()..];
            let digits: String = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty()
                && let Ok(value) = digits.parse()
            {
                numbers.push(value);
            }
        }
    }
    numbers
}

fn pipes_max_live(screen: &vt100::Screen) -> Option<u32> {
    pipes_live_numbers(screen).into_iter().max()
}

/// The 32-character lowercase hex run ID in the panel header.
fn pipes_run_id(pty: &mut Pty) -> String {
    let text = pty.contents();
    text.split(|character: char| !character.is_ascii_hexdigit())
        .find(|token| token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("stage panel did not show a run ID: {text}"))
        .to_owned()
}

#[test]
fn pipeline_pipes_panel_switches_run_stream_freezes_and_restores_draft() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = bang_shell_start(&provider, home.path(), project.path(), "completion", "");

    // Earlier 3-stage run: the two edges carry different bodies, used for left/right and up/down switching.
    pty.input("!printf 'first-%s\n' edge | tr 'a-z' 'A-Z' | cat\r");
    pty.wait_for("earlier three-stage pipe run completes", |screen| {
        let text = screen.contents();
        text.contains("FIRST-EDGE") && text.contains("Pipes: /pipes (F8)")
    });
    assert!(
        !pty.contents().contains("Non-zero stage results"),
        "no warning may appear when all stages succeed"
    );

    // A running three-stage pipe: the compound stage and the downstream one run concurrently, and
    // bytes are visible on both edges while it runs.
    pty.input("!i=0; while :; do i=$((i+1)); printf 'live-%02d\\n' \"$i\"; sleep 0.02; done | cat | tail -n 1\r");
    pty.text("Elapsed:");
    pty.input("pipes-draft");
    pty.draft("pipes-draft");
    pipes_open(&mut pty);

    // By default the most recent running run (1/2) and its only edge are selected.
    pty.wait_for("latest pipe run and its first edge are shown by default", |screen| {
        let text = screen.contents();
        text.contains("run 1/2")
            && text.contains("follow")
            && !pipes_live_numbers(screen).is_empty()
    });
    assert!(pty.contents().contains("› s1"), "the selected stream must have a visible marker");
    // The edge after the compound stage must also be mirrored while running, not only at EOF.
    pty.wait_for("second edge receives bytes while running", |screen| {
        screen
            .contents()
            .lines()
            .any(|line| line.contains("s2 ") && !line.contains("kept 0..0 / 0 "))
    });
    pty.input("\x1b[B");
    pty.wait_for("Down selects the second edge of the running run", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s2"))
            && !pipes_live_numbers(screen).is_empty()
    });
    pty.input("\x1b[A");
    pty.wait_for("Up returns to the first edge of the running run", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s1"))
            && !pipes_live_numbers(screen).is_empty()
    });

    // Right/Left switch between the two runs.
    pty.input("\x1b[C");
    pty.wait_for("Right switches to the earlier completed three-stage run", |screen| {
        let text = screen.contents();
        text.contains("run 2/2") && text.contains("first-edge")
    });
    // Down/Up switch between adjacent edges of that run: the second edge holds the bytes after the tr
    // conversion.
    pty.input("\x1b[B");
    pty.wait_for("Down selects the second edge and shows the converted bytes", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s2"))
            && text.contains("FIRST-EDGE")
            && !text.lines().any(|line| line.trim() == "first-edge")
    });
    pty.input("\x1b[A");
    pty.wait_for("Up returns to the first edge", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s1"))
            && text.lines().any(|line| line.trim() == "first-edge")
    });
    pty.input("\x1b[D");
    pty.wait_for("Left returns to the latest running run", |screen| {
        screen.contents().contains("run 1/2")
    });

    // PgUp freezes: new bytes keep arriving, but the viewport does not return to the tail.
    pty.wait_for("first edge has produced enough lines to freeze scrolling", |screen| {
        pipes_max_live(screen).is_some_and(|last| last >= 30)
    });
    pty.input(PIPES_PGUP);
    pty.wait_for("PgUp freezes the body", |screen| screen.contents().contains("frozen"));
    let frozen = pipes_live_numbers(pty.parser.screen());
    assert!(!frozen.is_empty());
    // The contract allows two frozen hints: new bytes in the background, or the old cache overwritten.
    pty.wait_for("frozen view reports new background bytes or overwritten cache", |screen| {
        let text = screen.contents();
        text.contains("new +") || text.contains("cached bytes overwritten")
    });
    assert_eq!(
        pipes_live_numbers(pty.parser.screen()),
        frozen,
        "new output after freezing must not pull the viewport back to the bottom"
    );

    // Resize keeps the selected stream and the freeze anchor.
    pty.resize(30, 80);
    pty.wait_for("freeze and run selection survive resize", |screen| {
        screen.alternate_screen()
            && screen.contents().contains("run 1/2")
            && screen.contents().contains("frozen")
    });
    assert_eq!(
        pipes_live_numbers(pty.parser.screen()),
        frozen,
        "resize must not change the freeze anchor"
    );

    // End resumes following and shows the tail including the new bytes.
    let frozen_last = *frozen.last().unwrap();
    pty.input(PIPES_END);
    pty.wait_for("End resumes following and advances to new output", |screen| {
        let text = screen.contents();
        text.contains("follow")
            && pipes_live_numbers(screen)
                .last()
                .is_some_and(|last| *last > frozen_last)
    });

    // Esc only closes the panel: the shell keeps running, the draft is kept and no cancellation is triggered.
    pipes_close(&mut pty);
    pty.wait_for("Esc closes the panel without cancelling the shell", |screen| {
        let text = screen.contents();
        !screen.alternate_screen()
            && text.contains("Elapsed:")
            && !text.contains("Cancelling shell")
    });
    pty.draft("pipes-draft");

    // Only the second Esc cancels the shell, as cleanup. The cancelling hint may vanish between two
    // polls, so the barrier is the restored input and the terminal cancelled state, not transient copy.
    pty.input("\x1b");
    bang_shell_wait_done(&mut pty);
    pty.text("Command cancelled; side effects were not rolled back.");
    cancel_assert_no_scratch(project.path());
    cancel_assert_no_request(&provider);
    assert!(
        !bang_shell_sessions_dir(home.path()).exists(),
        "the local shell must not create a session"
    );
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}

