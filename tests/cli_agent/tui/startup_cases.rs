use super::*;

#[test]
fn tui_notification_error_scrolls_into_native_history_and_preserves_draft() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&fixture),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"notification_timeout_seconds":1}"#,
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    pty.input("/help extra\r");
    pty.wait_for(
        "the error enters the message area inside a complete red frame",
        |screen| {
            notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                && screen.contents().contains("usage: /help")
        },
    );
    pty.input("\x01\x0b草稿🙂");
    pty.draft("草稿🙂");
    let cursor = pty.parser.screen().cursor_position();
    let since = Instant::now();
    pty.wait_for_timeout(
        "the error is kept past the old expiry duration",
        Duration::from_secs(4),
        |screen| {
            since.elapsed() >= Duration::from_millis(1100)
                && screen.contents().contains("usage: /help")
        },
    );
    assert_eq!(pty.parser.screen().cursor_position(), cursor);
    pty.resize(12, 35);
    pty.draft("草稿🙂");
    pty.resize(30, 100);
    pty.wait_for(
        "the error still shows completely after the window is restored",
        |screen| notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some(),
    );
    pty.draft("草稿🙂");
    pty.input("\x03");
    pty.wait_for("the terminal is restored after exit", |screen| {
        !screen.bracketed_paste() && !screen.hide_cursor()
    });
    let (history, _) = pty.native_history();
    assert_eq!(history.matches("usage: /help").count(), 1);
    assert!(!history.contains(" 1s "));
    pty.finish();
    no_more(&fixture);
}

#[test]
fn tui_startup_centers_brand_and_keeps_footer_colors() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut pty = Pty::start(&server, dir.path());
    pty.ready();
    pty.text("Session & workflow");
    let text = pty.parser.screen().contents();
    assert!(tips_headers_on_one_row(&text), "{text}");
    assert!(
        text.contains("/tips  All shortcuts   ·   /help  Commands"),
        "{text}"
    );
    assert!(text.contains("/compact"), "{text}");
    assert!(!text.contains("/skills"), "{text}");
    assert!(!input_bar_above_draft(&text), "{text}");

    pty.input("startup draft");
    pty.draft("startup draft");
    pty.resize(24, 80);
    pty.wait_for(
        "80 columns still show two columns and keep the draft",
        |screen| {
            let text = screen.contents();
            tips_headers_on_one_row(&text)
                && text.contains("startup draft")
                && !input_bar_above_draft(&text)
        },
    );
    pty.resize(30, 60);
    pty.wait_for(
        "the narrow screen switches tips to one column and keeps every entry",
        |screen| {
            let text = screen.contents();
            !tips_headers_on_one_row(&text)
                && tips_headers_stack(&text)
                && text.contains("/tree")
                && text.contains("Ctrl + O")
                && text.contains("startup draft")
                && text.contains("/tips  All shortcuts")
                && !input_bar_above_draft(&text)
        },
    );
    pty.resize(30, 38);
    pty.wait_for(
        "an insufficient width uses the compact brand and keeps the version",
        |screen| {
            let text = screen.contents();
            let mut lines = text.lines();
            lines
                .next()
                .is_some_and(|line| line.trim() == "\u{f1d9}  PP")
                && lines
                    .next()
                    .is_some_and(|line| line.trim() == concat!("v", env!("CARGO_PKG_VERSION")))
                && text.contains("startup draft")
                && text.contains("Shift + Tab")
                && text.contains("/help  Commands")
        },
    );
    pty.resize(12, 40);
    pty.wait_for(
        "a small window shows the compact welcome page and keeps the draft",
        |screen| {
            let text = screen.contents();
            text.lines()
                .next()
                .is_some_and(|line| line.trim() == "\u{f1d9}  PP")
                && text.contains("startup draft")
                && text.contains("/tips  All shortcuts")
                && text.contains("/help  Commands")
                && !text.contains("Session & workflow")
                && !text.contains("Keyboard")
                && !text.contains("\u{e62a} ")
        },
    );
    pty.resize(6, 20);
    pty.wait_for("a tiny window keeps the input draft", |screen| {
        let text = screen.contents();
        text.contains("startup draft") && text.lines().any(|line| line.starts_with('❯'))
    });
    pty.resize(30, 100);
    pty.text("▄▄▄██▀▀██");
    pty.draft("startup draft");
    pty.input("\r");
    let (request, body) = pty.request(&server);
    assert_eq!(body["messages"][1]["content"], "startup draft");
    final_reply(request, "startup answer");
    pty.text("startup answer");
    pty.completed();
    assert!(input_bar_above_draft(&pty.parser.screen().contents()));
    assert!(!pty.parser.screen().contents().contains("▄▄▄██▀▀██"));
    pty.input("/new\r");
    pty.text("▄▄▄██▀▀██");
    assert!(!pty.parser.screen().contents().contains("\u{e62a} "));
    assert!(!pty.parser.screen().contents().contains("Session reset."));
    pty.input("/help\r");
    pty.text("bash has no directory isolation");
    assert!(!pty.parser.screen().contents().contains("▄▄▄██▀▀██"));
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
}

// Two-column welcome page: both header groups share one row.
fn tips_headers_on_one_row(text: &str) -> bool {
    text.lines()
        .any(|line| line.contains("Session & workflow") && line.contains("Keyboard"))
}

// One-column welcome page: `Keyboard` lands on a row after `Session & workflow`.
fn tips_headers_stack(text: &str) -> bool {
    let session = text
        .lines()
        .position(|line| line.contains("Session & workflow"));
    let keyboard = text.lines().position(|line| line.contains("Keyboard"));
    matches!((session, keyboard), (Some(session), Some(keyboard)) if session < keyboard)
}

// The Ctx field on the top edge of the input bar still sits next to the `❯` draft row.
fn input_bar_above_draft(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .position(|line| line.starts_with('❯'))
        .is_some_and(|row| row > 0 && lines[row - 1].contains("\u{e62a} "))
}

const TIPS_ACCENT: vt100::Color = vt100::Color::Rgb(127, 166, 106);
const TIPS_HEADER: vt100::Color = vt100::Color::Rgb(215, 175, 95);

// The `/tips` command group equals the command registry's visible entries, sorted case-insensitively by the key name users see.
const TIPS_COMMANDS: &[&str] = &[
    "/agents",
    "/compact",
    "/context",
    "/dump",
    "/effort",
    "/help",
    "/init",
    "/login",
    "/logout",
    "/mcp",
    "/model",
    "/new",
    "/permissions",
    "/pipes",
    "/plan",
    "/plan-list",
    "/quit",
    "/resume",
    "/roles",
    "/sandbox",
    "/setting",
    "/skills",
    "/stats",
    "/test-ask",
    "/test-bash",
    "/test-bash-task",
    "/test-edit",
    "/test-eval-js",
    "/test-eval-py",
    "/test-fail",
    "/test-glob",
    "/test-permission",
    "/test-read",
    "/test-rg",
    "/test-task",
    "/test-todo",
    "/test-tools",
    "/test-ui",
    "/test-wait",
    "/test-web-search",
    "/test-write",
    "/themes",
    "/tips",
    "/tree",
];

// The shortcut group ascends by displayed key name; non-ASCII keys such as `⌥` sort after ASCII, and the two `Ctrl + S`
// entries save the model and the thinking-effort defaults respectively.
fn tips_shortcuts() -> Vec<&'static str> {
    if cfg!(target_os = "macos") {
        vec![
            "Alt + Shift + A",
            "Ctrl + A",
            "Ctrl + C",
            "Ctrl + E",
            "Ctrl + F",
            "Ctrl + J",
            "Ctrl + K",
            "Ctrl + O",
            "Ctrl + S",
            "Enter",
            "Esc",
            "Esc Esc",
            "F1",
            "F6",
            "F7",
            "F8",
            "PgUp/PgDn",
            "Shift + Enter",
            "Shift + Tab",
            "Tab",
            "↑ / ↓",
            "⌥ + A",
            "⌥ + Enter",
            "⌥ + P",
            "⌥ + PgUp/PgDn",
            "⌥ + Q",
            "⌥ + R",
            "⌥ + S",
            "⌥ + T",
        ]
    } else {
        vec![
            "Alt + A",
            "Alt + Enter",
            "Alt + Shift + A",
            "Ctrl + A",
            "Ctrl + C",
            "Ctrl + E",
            "Ctrl + F",
            "Ctrl + J",
            "Ctrl + K",
            "Ctrl + O",
            "Ctrl + P",
            "Ctrl + PgUp/PgDn",
            "Ctrl + Q",
            "Ctrl + R",
            "Ctrl + S",
            "Ctrl + S",
            "Ctrl + T",
            "Enter",
            "Esc",
            "Esc Esc",
            "F1",
            "F6",
            "F7",
            "F8",
            "PgUp/PgDn",
            "Shift + Enter",
            "Shift + Tab",
            "Tab",
            "↑ / ↓",
        ]
    }
}

fn tips_commands() -> Vec<&'static str> {
    TIPS_COMMANDS
        .iter()
        .copied()
        .filter(|label| cfg!(debug_assertions) || !label.starts_with("/test-"))
        .collect()
}

// Key-name alignment width: the longest visible key name, with a floor of 12 columns.
fn tips_key_width() -> u16 {
    let widest = tips_commands()
        .into_iter()
        .chain(tips_shortcuts())
        .map(|label| label.chars().count())
        .max()
        .unwrap_or(0);
    u16::try_from(widest.max(12)).unwrap()
}

// Screen start column of the right column in two-column mode: two border columns on the left, left column width
// `(body width - 4) / 2`, and four blank columns between.
fn tips_right_column(cols: u16) -> u16 {
    2 + (cols - 2 - 4) / 2 + 4
}

// Read the key names of the given key column row by row, skipping header rows, separator rules and blank rows.
fn tips_visible_keys(screen: &vt100::Screen, column: u16, key_width: u16) -> Vec<String> {
    (0..screen.size().0)
        .filter(|&row| {
            screen
                .cell(row, 0)
                .is_some_and(|cell| cell.contents() == "│")
                && tip_columns(screen, row, 2..7).trim() != "Tips:"
        })
        .filter_map(|row| {
            let key = tip_columns(screen, row, column..column + key_width);
            let key = key.trim().to_owned();
            (!key.is_empty()
                && !key.chars().all(|ch| ch == '─')
                && !matches!(key.as_str(), "Tips:" | "Commands" | "Shortcuts"))
            .then_some(key)
        })
        .collect()
}

fn tips_key_row(screen: &vt100::Screen, column: u16, key_width: u16, label: &str) -> u16 {
    (0..screen.size().0)
        .find(|&row| {
            screen
                .cell(row, 0)
                .is_some_and(|cell| cell.contents() == "│")
                && if label == "Tips:" {
                    tip_columns(screen, row, column..column + 5).trim() == "Tips:"
                } else {
                    tip_columns(screen, row, column..column + key_width).trim() == label
                }
        })
        .unwrap_or_else(|| panic!("missing tips key {label}: {}", screen.contents()))
}

fn assert_tips_title(
    screen: &vt100::Screen,
    key_width: u16,
    title: &str,
    color: vt100::Color,
    bold: bool,
) {
    let row = tips_key_row(screen, 2, key_width, title);
    for (offset, character) in title.chars().enumerate() {
        let col = 2 + u16::try_from(offset).unwrap();
        let cell = screen.cell(row, col).unwrap();
        assert_eq!(
            cell.contents(),
            character.to_string(),
            "row {row}, col {col}"
        );
        assert_eq!(cell.fgcolor(), color, "row {row}, col {col}");
        assert_eq!(cell.bold(), bold, "row {row}, col {col}");
    }
}
