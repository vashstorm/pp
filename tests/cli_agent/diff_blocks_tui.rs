// PTY regression for diff block rendering: the real committed diff for both edit modes and both protocols
// (disk writes + red/green and line numbers on screen + stats), write creating/overwriting/identical
// content/empty file, the unavailable fallback for CRLF→LF / trailing-newline removal / BOM / non-UTF-8,
// write to an unreadable file not being blocked by a snapshot failure, failed edits not faking a diff, the
// default folding above 40 lines with Ctrl+O expand/collapse, the exit scrollback keeping the collapse
// state and colors it had, and /resume and startup restore replaying the same diff without re-running the
// old tools while logs missing FileChanged still load. The temporary argument preview cards produced while
// generating have their own streaming cases (both protocols × edit/write): the preview appears before the
// commit, the disk is unchanged, it disappears on cancellation, and after the commit it is replaced by the
// single real card without entering the journal. Everything is observed through the real PTY, loopback
// HTTP and disk files, never sleep to guess stages; helpers all use the `diff_blocks_` prefix and test
// functions use the `transcript_blocks_` / `*_preview_tui_` prefixes, adding no use declaration to avoid
// clashing with sibling include files.

const DIFF_BLOCKS_ADDED: vt100::Color = vt100::Color::Rgb(137, 210, 129);
const DIFF_BLOCKS_REMOVED: vt100::Color = vt100::Color::Rgb(252, 58, 75);

include!("diff_blocks_tui/preview_cases.rs");

fn diff_blocks_settings(mode: &str) -> String {
    format!(
        r#"{{"default_model":"fixture/primary","edit":"{mode}","notification_timeout_seconds":1,"read":{{"toolResultPreview":true}}}}"#
    )
}

// Match ASCII text and colors directly per terminal cell, so the title icon does not affect column math.
fn diff_blocks_screen_colored(screen: &vt100::Screen, needle: &str, color: vt100::Color) -> bool {
    // edit uses the reference palette, emphasizing partial changes through the background.
    let edit_color = match color {
        DIFF_BLOCKS_ADDED => vt100::Color::Rgb(153, 208, 137),
        DIFF_BLOCKS_REMOVED => vt100::Color::Rgb(232, 76, 82),
        other => other,
    };
    let (rows, columns) = screen.size();
    let width = needle.chars().count() as u16;
    for row in 0..rows {
        for start in 0..columns.saturating_sub(width).saturating_add(1) {
            if needle.chars().enumerate().all(|(offset, expected)| {
                screen
                    .cell(row, start + offset as u16)
                    .is_some_and(|cell| {
                        cell.contents() == expected.to_string()
                            && (cell.fgcolor() == color || cell.fgcolor() == edit_color || cell.bgcolor() == edit_color)
                    })
            }) {
                return true;
            }
        }
    }
    false
}

fn diff_blocks_call(protocol: &str, id: &str, name: &str, args: Value) -> Value {
    if protocol == "completion" {
        call(id, name, args)
    } else {
        response_call(id, name, args)
    }
}

fn diff_blocks_calls_reply(request: CapturedRequest, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

fn diff_blocks_final(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn diff_blocks_tool_result(body: &Value, protocol: &str, id: &str) -> Value {
    cancel_tool_outputs(body, protocol)
        .into_iter()
        .find(|entry| entry.0 == id)
        .unwrap_or_else(|| panic!("missing tool result for {id}"))
        .1
}

// Read the native scrollback and the main screen after the Ctrl-C exit, keeping colors for assertions.
fn diff_blocks_exit_scrollback(pty: &mut Pty, last_answer: &str) -> (String, Vec<u8>) {
    pty.input("\x03");
    pty.wait_for("input mode is restored after exit", |screen| !screen.bracketed_paste());
    let (contents, raw) = pty.native_history();
    assert!(contents.contains(last_answer), "{contents}");
    (contents, raw)
}

// Copy a real journal and drop every FileChanged event line, simulating an old log without diff events.
fn diff_blocks_clone_journal_without_file_changed(home: &Path, source: &str, id: &str) {
    let journal = resume_history_journal_dir(home).join(format!("{source}.jsonl"));
    let text = session_text(&journal);
    let mut lines = text.lines();
    let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    header["id"] = json!(id);
    let mut cloned = format!("{header}\n");
    let mut parent = Value::Null;
    let mut event_map = Vec::new();
    let mut retained = 0;
    for line in lines {
        let mut entry: Value = serde_json::from_str(line).unwrap();
        if entry["data"]["event"] == "assistant_started" {
            event_map.clear(); retained = 0;
        } else if entry["customType"] == "pp.event" {
            event_map.push(Some(retained)); retained += 1;
        }
        if let Some(order) = entry["message"]["ppReplay"]["order"].as_array_mut() {
            order.retain_mut(|step| {
                if step["kind"] != "event" { return true; }
                let old = step["index"].as_u64().unwrap() as usize;
                match event_map[old] {
                    Some(index) => { step["index"] = json!(index); true }
                    None => false,
                }
            });
        }
        entry["parentId"] = parent;
        parent = entry["id"].clone();
        cloned.push_str(&entry.to_string());
        cloned.push('\n');
    }
    write_session_text(
        &resume_history_journal_dir(home).join(format!("{id}.jsonl")),
        &cloned,
    );
}

// A real edit old→new in replace mode plus write creating/overwriting/identical content/empty file: screen
// red/green, line numbers, the +N -N stats and the disk result all agree.
#[test]
fn transcript_blocks_diff_replace_edit_and_write_cards() {
    let protocol = "response";
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            diff_blocks_settings("replace"),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        std::fs::write(project.path().join("demo.txt"), "old\nkeep\n").unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("diff-blocks edit demo\r");

        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([diff_blocks_call(
                protocol,
                "e1",
                "edit",
                json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"new"}]})
            )]),
        );
        pty.wait_for("committed replace edit diff card", |screen| {
            let text = screen.contents();
            text.contains("Edit:") && text.contains("demo.txt")
                && text.contains("+1/-1")
                && text.contains("-  1│old")
                && text.contains("+  1│new")
                && text.contains("   2│keep")
                && diff_blocks_screen_colored(screen, "old", DIFF_BLOCKS_REMOVED)
                && diff_blocks_screen_colored(screen, "new", DIFF_BLOCKS_ADDED)
                && diff_blocks_screen_colored(screen, "Edit:", vt100::Color::Rgb(244, 191, 84))
                && diff_blocks_screen_colored(screen, "demo.txt", vt100::Color::Rgb(244, 191, 84))
        });
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\n"
        );
        let (request, body) = pty.request_at(&server, path);
        let edited = diff_blocks_tool_result(&body, protocol, "e1");
        assert_eq!(edited["ok"], json!(true), "{edited}");
        assert_eq!(edited["edits_applied"], json!(1), "{edited}");

        diff_blocks_calls_reply(
            request,
            protocol,
            json!([
                diff_blocks_call(
                    protocol,
                    "w1",
                    "write",
                    json!({"path":"fresh.txt","content":"alpha\n"})
                ),
                diff_blocks_call(protocol, "w2", "write", json!({"path":"empty.txt","content":""})),
            ]),
        );
        pty.wait_for("created and empty write cards", |screen| {
            let text = screen.contents();
            text.contains("Write · fresh.txt")
                && text.contains("alpha")
                && text.contains("Wrote 6 bytes")
                && text.contains("Write · empty.txt")
                && text.contains("Wrote 0 bytes")
        });
        assert_eq!(
            std::fs::read_to_string(project.path().join("fresh.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(std::fs::read(project.path().join("empty.txt")).unwrap().len(), 0);

        let (request, _) = pty.request_at(&server, path);
        diff_blocks_calls_reply(
            request,
            protocol,
            json!([
                diff_blocks_call(
                    protocol,
                    "w3",
                    "write",
                    json!({"path":"fresh.txt","content":"alpha\n"})
                ),
                diff_blocks_call(
                    protocol,
                    "w4",
                    "write",
                    json!({"path":"demo.txt","content":"new\nkeep\nbeta\n"})
                ),
            ]),
        );
        pty.wait_for("same-content and overwrite write cards", |screen| {
            let text = screen.contents();
            text.contains("Wrote 6 bytes")
                && text.contains("beta")
                && text.contains("Wrote 14 bytes")
        });
        let (request, _) = pty.request_at(&server, path);
        diff_blocks_final(request, protocol, "diff-blocks write round done");
        pty.text("diff-blocks write round done");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(project.path().join("demo.txt")).unwrap(),
            "new\nkeep\nbeta\n"
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join("fresh.txt")).unwrap(),
            "alpha\n"
        );
}

