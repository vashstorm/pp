fn permission_fixture(home: &Path) {
    std::fs::write(
        config_path(home, "models.yml"),
        "providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:9/v1\n    models:\n      primary:\n        model: fixture-model\n",
    )
    .unwrap();
}

fn permission_label(screen: &vt100::Screen, label: &str, color: vt100::Color) -> bool {
    let (rows, cols) = screen.size();
    let row = rows - 1;
    (0..cols).any(|column| {
        label.chars().enumerate().all(|(offset, ch)| {
            screen.cell(row, column + offset as u16).is_some_and(|cell| {
                cell.contents() == ch.to_string() && cell.fgcolor() == color
            })
        })
    })
}

#[test]
fn permission_tui_cycles_modes_with_both_shortcuts_and_keeps_draft() {
    use vt100::Color::Rgb;
    let home = tempfile::tempdir().unwrap();
    permission_fixture(home.path());
    let default_footer = |screen: &vt100::Screen| {
        let (rows, cols) = screen.size();
        permission_label(screen, "Ask for approval", Rgb(125, 207, 255))
            && screen
                .rows(0, cols)
                .nth(usize::from(rows - 1))
                .is_some_and(|row| row.contains("off · \u{ed25} Ask for approval ·"))
    };
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.wait_for("default startup shows Ask for approval with a shield after effort", default_footer);
    pty.input("permission draft");
    pty.draft("permission draft");
    pty.input("\x1b[D");
    let (row, column) = pty.parser.screen().cursor_position();
    pty.wait_for("cursor inside the draft", |screen| screen.cursor_position() == (row, column.saturating_sub(1)));
    let cursor = pty.parser.screen().cursor_position();
    for (keys, label, color) in [
        ("\x1b[17~", "Approve for me", Rgb(254, 188, 56)),
        ("\x1b[17~", "Full Access", Rgb(224, 108, 117)),
        ("\x1b[17~", "Read Only", Rgb(254, 188, 56)),
        ("\x1bA", "Ask for approval", Rgb(125, 207, 255)),
        ("\x1b[97;4u", "Approve for me", Rgb(254, 188, 56)),
        ("\x1b[27;4;65~", "Full Access", Rgb(224, 108, 117)),
        ("\x1b[27;10;65~", "Read Only", Rgb(254, 188, 56)),
        ("\x1b[65;3u", "Ask for approval", Rgb(125, 207, 255)),
    ] {
        pty.input(keys);
        pty.wait_for(&format!("{keys:?} switches to {label}"), |screen| {
            permission_label(screen, label, color)
        });
        assert_eq!(pty.prompt(), "permission draft");
        assert_eq!(pty.parser.screen().cursor_position(), cursor);
    }
    pty.resize(24, 22);
    pty.wait_for("narrow screen Ask for approval", |screen| {
        permission_label(screen, "Ask for approval", Rgb(125, 207, 255))
    });
    pty.input("\x1b[17~\x1b[17~");
    pty.wait_for("narrow screen Full Access", |screen| {
        permission_label(screen, "Full Access", Rgb(224, 108, 117))
    });
    pty.input("\x03");
    pty.finish();

    let mut restarted = Pty::start_with_existing_files(home.path());
    restarted.wait_for("restart restores the default Ask for approval", default_footer);
    restarted.input("\x03");
    restarted.finish();
}

#[test]
fn permission_tui_approves_and_saves_an_exact_bash_request() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = home.path().canonicalize().unwrap();
    let sandbox = pp_sandbox::Sandbox::new(
        pp_config::config::SandboxSettings {
            mode: pp_config::config::SandboxMode::WorkspaceWrite,
            network: false,
            ..Default::default()
        },
        &cwd,
    );
    if sandbox.policy().is_err() {
        // The platform's isolation capability is verified by the pp-sandbox targeted cases.
        return;
    }
    write_settings(home.path(), "tools: { sandbox: { network: false } }").unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), fixture_models(&server)).unwrap();
    let mut pty = Pty::start_with_args(&cwd, home.path(), &[]);
    let command = "printf approved > marker";
    let request_arguments = json!({"command": command, "sandbox_permissions": {"network": true}});
    pty.input("request sandbox permission\r");
    let (request, body) = pty.request(&server);
    assert!(body["tools"].as_array().unwrap().iter().any(|tool| {
        tool["function"]["name"] == "bash"
            && tool["function"]["parameters"]["properties"].get("sandbox_permissions").is_some()
    }));
    calls_reply(request, json!([call("sandbox-test", "bash", request_arguments.clone())]));
    pty.wait_for("pre-execution approval and persistence options", |screen| {
        let contents = screen.contents();
        contents.contains("Permission required")
            && contents.contains("outbound network")
            && contents.contains("Always allow globally")
    });
    let marker = cwd.join("marker");
    let store = config_path(home.path(), "permission.toml");
    assert!(!marker.exists());
    assert!(!store.exists());
    pty.input("\x1b[B\x1b[B\r");
    let (request, body) = pty.request(&server);
    let result = body["messages"].as_array().unwrap().iter()
        .find(|message| message["role"] == "tool").unwrap();
    let output: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(output["ok"], true, "{output}");
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "approved");
    let rules = std::fs::read_to_string(&store).unwrap();
    assert!(rules.contains("[[rules]]"), "{rules}");
    assert!(
        rules.contains("pattern = [\"printf\", \"approved\"]"),
        "{rules}"
    );
    assert!(rules.contains("[[sandbox]]"), "{rules}");
    assert!(rules.contains("network = true"), "{rules}");
    assert!(rules.contains("command = \"printf approved\""), "{rules}");
    assert!(!rules.contains("permissions."), "{rules}");
    assert!(!rules.contains("version"), "{rules}");
    assert!(!rules.contains("[rules.grant]"), "{rules}");
    assert!(!rules.contains("id ="), "{rules}");
    assert!(!rules.contains("workspace ="), "{rules}");
    assert!(!rules.contains("cwd ="), "{rules}");
    assert!(!rules.contains("writable = []"), "{rules}");
    final_reply(request, "sandbox permission completed");
    pty.completed_after("sandbox permission completed");
    pty.input("/quit\r");
    pty.finish();
    no_more(&server);

    // Started in another project's working directory, the same command and scope hit the same global persistent
    // rule directly, so no confirmation pops up again.
    let other_project = tempfile::tempdir().unwrap();
    let other_cwd = other_project.path().canonicalize().unwrap();
    let mut second_pty = Pty::start_with_args(&other_cwd, home.path(), &[]);
    second_pty.input("run again in another project\r");
    let (request, _) = second_pty.request(&server);
    calls_reply(request, json!([call("sandbox-global", "bash", request_arguments)]));
    let (request, body) = second_pty.request(&server);
    let result = body["messages"].as_array().unwrap().iter()
        .find(|message| message["role"] == "tool").unwrap();
    let output: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    assert_eq!(output["ok"], true, "{output}");
    assert_eq!(std::fs::read_to_string(other_cwd.join("marker")).unwrap(), "approved");
    final_reply(request, "global permission reused");
    second_pty.completed_after("global permission reused");
    second_pty.input("/quit\r");
    second_pty.finish();
    no_more(&server);
}

