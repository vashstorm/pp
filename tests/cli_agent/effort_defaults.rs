// A failed save stays in the message area; the temporary overlay is gone.
fn model_wait_saved(pty: &mut Pty, home: &std::path::Path, reference: &str) {
    pty.wait_for("default model is submitted and the menu marker updates", |screen| {
        read_agent_preferences(home).default_model.as_deref() == Some(reference)
            && screen.contents().contains(&format!("{reference} "))
    });
    assert!(!pty.parser.screen().contents().contains("Default model saved:"));
}

// After migration agent.db is the authority for preferences; a read-only connection checks the
// committed values, while runtime behavior is proven by the real PTY and restarts, without
// asserting where SQL or YAML landed.
struct DbPreferences {
    default_model: Option<String>,
    think_level: String,
    favorites: Vec<String>,
}

fn read_agent_preferences(home: &std::path::Path) -> DbPreferences {
    let connection = rusqlite::Connection::open_with_flags(
        config_path(home, "agent.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (default_model, think_level) = connection
        .query_row(
            "SELECT default_model, think_level FROM preferences WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let favorites = connection
        .prepare("SELECT model_reference FROM favorite_models ORDER BY position")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    DbPreferences {
        default_model,
        think_level,
        favorites,
    }
}

// Busy injection: an external write transaction holds agent.db past the 250ms busy_timeout, so
// the save must fail.
fn hold_agent_db_write(home: &std::path::Path) -> rusqlite::Connection {
    let connection = rusqlite::Connection::open(config_path(home, "agent.db")).unwrap();
    connection.busy_timeout(std::time::Duration::from_secs(2)).unwrap();
    connection
}

// Safety injection: an overly permissive database is rejected on open, without falling back and
// without rewriting the file.
fn restrict_agent_db(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        config_path(home, "agent.db"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
}

fn restore_agent_db(home: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        config_path(home, "agent.db"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}

// A failed save only shows the fixed domain hint: it must not leak paths, permissions or SQLite
// details.
fn assert_sanitized_database_error(screen: &vt100::Screen) {
    let contents = screen.contents();
    for leaked in ["agent.db", "denied", "os error", "SQL"] {
        assert!(
            !contents.contains(leaked),
            "{leaked} must not appear in the database error message"
        );
    }
}

// The model list is ordered by favorites and usage, and the order changes with session history;
// read the actual row order and highlighted item from the screen so navigation assertions depend
// only on visible behavior rather than a fixed ordering.
const PICKER_MODELS: [&str; 2] = ["fixture/primary", "gateway/team/secondary"];

fn picker_row_order(screen: &vt100::Screen) -> Vec<&'static str> {
    let text = screen.contents();
    let mut rows: Vec<(usize, &'static str)> = PICKER_MODELS
        .into_iter()
        .filter_map(|model| {
            text.lines()
                .position(|line| line.contains(model))
                .map(|row| (row, model))
        })
        .collect();
    rows.sort_unstable();
    rows.into_iter().map(|(_, model)| model).collect()
}

fn picker_selected(screen: &vt100::Screen) -> Option<&'static str> {
    let text = screen.contents();
    PICKER_MODELS
        .into_iter()
        .find(|model| text.lines().any(|line| line.contains('❯') && line.contains(model)))
}

#[test]
fn sqlite_default_model_picker_keeps_draft_and_persists_across_restart() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let path = config_path(home.path(), "settings.yml");
        let original = "# Settings beyond the default model are preserved\nthink_level: high\nedit: hashline\ndefault_model: fixture/primary # default model note\n";
        std::fs::write(&path, original).unwrap();
        let models = json!({"providers":{
            "fixture": slash_provider(&server, "fixture", protocol, json!({
                "primary": {"model":"first-model"}})),
            "gateway": slash_provider(&server, "gateway", protocol, json!({
                "team/secondary": {"model":"second-model"}}))}}).to_string();
        let models_path = config_path(home.path(), "models.yml");
        std::fs::write(&models_path, &models).unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready_model("fixture/primary");
        let migrated_settings = std::fs::read_to_string(&path).unwrap();
        pty.input("/model ");
        pty.text("fixture/primary  ");
        for (icon, color) in [
            ("", vt100::Color::Rgb(187, 154, 247)),
            ("", vt100::Color::Rgb(158, 206, 106)),
        ] {
            let screen = pty.parser.screen();
            let (rows, columns) = screen.size();
            assert!((0..rows).any(|row| (0..columns).any(|column| {
                screen.cell(row, column).is_some_and(|cell| {
                    cell.contents() == icon && cell.fgcolor() == color
                })
            })));
        }
        pty.text("Ctrl + F/Ctrl + S");
        pty.text(" favorite ·  default ·  current");
        assert!(!pty.parser.screen().contents().contains("gateway/team/secondary "));
        pty.input("\x01\x0bsecond");
        pty.text("gateway/team/secondary");
        pty.input("\x13");
        model_wait_saved(&mut pty, home.path(), "gateway/team/secondary");
        pty.ready_model("fixture/primary");
        pty.ready_effort("high");
        pty.model_search("second");
        pty.text("gateway/team/secondary ");
        assert!(!pty.parser.screen().contents().contains("fixture/primary "));
        pty.input("\x1b");
        pty.draft("");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved, migrated_settings);
        assert!(saved.contains("# Settings beyond the default model are preserved"));
        assert!(saved.contains("edit: hashline"));
        assert!(!saved.contains("default_model") && !saved.contains("think_level"));
        let preferences = read_agent_preferences(home.path());
        assert_eq!(
            preferences.default_model.as_deref(),
            Some("gateway/team/secondary")
        );
        // The old YAML's think_level: high became the authoritative database value on migration.
        assert_eq!(preferences.think_level, "high");
        assert_eq!(std::fs::read_to_string(&models_path).unwrap(), models);
        no_more(&server);
        pty.input("verify current model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "first-model");
        super::think::assert_level(&body, protocol, "high");
        super::think::finish_request(request, protocol);
        pty.ready_model("fixture/primary");
        pty.input("\x03");
        pty.finish();

        let mut restarted = Pty::start_with_existing_files(home.path());
        restarted.ready_model("gateway/team/secondary");
        restarted.input("/model ");
        restarted.text("gateway/team/secondary  ");
        restarted.input("\x1b");
        restarted.draft("");
        restarted.input("verify startup model\r");
        let (request, body) = receive_at(&server, api_path(protocol));
        assert_eq!(body["model"], "second-model");
        super::think::finish_request(request, protocol);
        restarted.completed();
        restarted.ready_model("gateway/team/secondary");
        restarted.input("草稿🙂");
        restarted.draft("草稿🙂");
        let cursor = restarted.parser.screen().cursor_position();
        restarted.function_key('p');
        restarted.text("fixture/primary");
        restarted.model_search("");
        restarted.input("fixture/primary");
        restarted.model_search("fixture/primary");
        restarted.input("\x13");
        model_wait_saved(&mut restarted, home.path(), "fixture/primary");
        restarted.ready_model("gateway/team/secondary");
        restarted.text("fixture/primary ");
        restarted.input("\x01\x0b");
        restarted.model_search("");
        restarted.input("\x1b[B");
        // Down moves from the current model to the end of the list; read the real highlighted model
        // before verifying Tab completion.
        restarted.wait_for("model list highlight moves to the last entry", |screen| {
            picker_selected(screen) == picker_row_order(screen).last().copied()
        });
        let tabbed = picker_selected(restarted.parser.screen()).expect("highlighted model");
        restarted.input("\t");
        restarted.model_search(tabbed);
        restarted.input("\x1b");
        restarted.draft("草稿🙂");
        assert_eq!(restarted.parser.screen().cursor_position(), cursor);
        assert_eq!(
            read_agent_preferences(home.path()).default_model.as_deref(),
            Some("fixture/primary")
        );
        restrict_agent_db(home.path());
        restarted.function_key('p');
        restarted.text("fixture/primary ");
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary "));
        restarted.input("\x13");
        restarted.text("Failed to save default model");
        assert_sanitized_database_error(restarted.parser.screen());
        restarted.model_search("");
        restarted.text("fixture/primary ");
        restarted.input("\x1b");
        restarted.draft("草稿🙂");
        restarted.function_key('p');
        restarted.text("fixture/primary ");
        assert!(!restarted.parser.screen().contents().contains("gateway/team/secondary "));
        // The failed save rewrote neither settings.yml nor any partial commit.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated_settings);
        restore_agent_db(home.path());
        // Once permissions are restored the same save succeeds immediately.
        restarted.input("fixture/primary");
        restarted.text("❯ fixture/primary");
        restarted.input("\x13");
        model_wait_saved(&mut restarted, home.path(), "fixture/primary");
        assert_eq!(
            read_agent_preferences(home.path()).default_model.as_deref(),
            Some("fixture/primary")
        );
        restarted.input("\x03");
        restarted.finish();
        no_more(&server);
}

// Real PTY regression for per-model thinking policy: A allows only low/high with an explicit
// default of high, B allows only medium/max with an explicit default of medium, C has the single
// level medium, and the global think_level is max.
fn model_thinking_models(server: &TestServer) -> String {
    json!({"providers":{"fixture": slash_provider(server, "fixture", "completion", json!({
        "a": {"model":"model-a","thinking":{"efforts":["low","high"],"defaultLevel":"high"}},
        "b": {"model":"model-b","thinking":{"efforts":["medium","max"],"defaultLevel":"medium"}},
        "c": {"model":"model-c","thinking":{"efforts":["medium"]}}}))}})
    .to_string()
}

fn model_thinking_home(server: &TestServer) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        "{\n  \"think_level\": \"max\"\n}\n",
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        model_thinking_models(server),
    )
    .unwrap();
    home
}

#[test]
fn model_thinking_tui_effort_menu_follows_model_policy_across_switches() {
    let server = server();
    let home = model_thinking_home(&server);
    let mut pty = Pty::start_with_existing_files(home.path());
    // The global level is max, so the startup level must be model A's explicit default high.
    pty.ready_effort("high");

    // A's menu lists only low/high, and the current and default markers both come from the model policy.
    pty.input("/effort ");
    pty.text("high (default)");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Light thinking"));
    assert!(screen.contains("Deep thinking (current)"));
    for absent in [
        "Moderate thinking",
        "Extra-deep thinking",
        "Highest level the model supports",
        "Disable thinking",
        "low (default)",
        "Light thinking (current)",
    ] {
        assert!(!screen.contains(absent), "{absent} must not appear in A's menu");
    }
    pty.input("\x01\x0b");
    pty.draft("");

    // Shift-Tab only cycles between the allowed levels and keeps the Chinese draft (wide characters
    // plus an emoji) intact.
    pty.input("草稿🙂\x1b[Z");
    pty.ready_effort("low");
    pty.draft("草稿🙂");
    pty.input("\x1b[Z");
    pty.ready_effort("high");
    pty.draft("草稿🙂");

    // Send the draft: the requested level matches the status line.
    pty.input("\r");
    let (request, body) = pty.request(&server);
    super::think::assert_level(&body, "completion", "high");
    assert_eq!(body["model"], "model-a");
    slash_assert_message(
        body["messages"].as_array().unwrap().last().unwrap(),
        "user",
        "草稿🙂",
    );
    super::think::finish_request(request, "completion");
    // Menus become actionable only after the worker's completion event;
    // an unchanged status label can still be visible while the task runs.
    pty.input("/effort ");
    pty.text("high (default)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.ready_effort("high");

    // Function key P switches to B: the explicit default medium overrides the current level.
    pty.function_key('p');
    pty.text("fixture/b");
    pty.input("\x1b[B\t");
    pty.model_search("fixture/b");
    pty.input("\r");
    pty.ready_model("fixture/b");
    pty.ready_effort("medium");

    // B's menu shows only medium/max.
    pty.input("/effort ");
    pty.text("medium (default)");
    let screen = pty.parser.screen().contents();
    assert!(screen.contains("Moderate thinking (current)"));
    assert!(screen.contains("Highest level the model supports"));
    for absent in [
        "Light thinking",
        "Deep thinking",
        "Extra-deep thinking",
        "Disable thinking",
        "low (default)",
        "high (default)",
        "max (default)",
    ] {
        assert!(!screen.contains(absent), "{absent} must not appear in B's menu");
    }

    // An unsupported prefix produces no candidate menu.
    pty.input("l");
    pty.wait_for("unsupported prefix has no candidate menu", |screen| {
        let text = screen.contents();
        text.contains("/effort l")
            && !text.contains("Moderate thinking")
            && !text.contains("Highest level the model supports")
            && !text.contains("Light thinking")
    });

    // Typing an unsupported level directly: a local error popup lists the allowed options and the
    // status line stays unchanged.
    pty.input("ow\r");
    pty.wait_for("typing an unsupported level directly reports an error", |screen| {
        notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
            && screen
                .contents()
                .contains("thinking level is not supported by the current model")
            && screen.contents().contains("options: medium, max")
    });
    pty.ready_effort("medium");
    no_more(&server);

    // A local rejection produces no level-switch target and the draft is kept as-is under the new
    // rule; clear it before typing an ordinary task.
    pty.draft("/effort low");
    pty.input("\u{1}");
    pty.input("\u{b}");
    pty.draft("");

    // The requested level on B matches the status line, and A's history is kept.
    pty.input("task B\r");
    let (request, body) = pty.request(&server);
    super::think::assert_level(&body, "completion", "medium");
    assert_eq!(body["model"], "model-b");
    let messages = body["messages"].as_array().unwrap();
    slash_assert_message(messages.last().unwrap(), "user", "task B");
    assert!(
        messages
            .iter()
            .any(|message| message["content"] == json!("草稿🙂")),
        "switching models must keep the previous session history"
    );
    super::think::finish_request(request, "completion");
    pty.input("/effort ");
    pty.text("medium (default)");
    pty.input("\x01\x0b");
    pty.draft("");
    pty.ready_effort("medium");

    // Switch back to A: the explicit default high overrides the current level (instead of falling
    // back to the lowest allowed level, low).
    pty.function_key('p');
    pty.text("fixture/a");
    pty.input("\x1b[A\t");
    pty.model_search("fixture/a");
    pty.input("\r");
    pty.ready_model("fixture/a");
    pty.ready_effort("high");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

