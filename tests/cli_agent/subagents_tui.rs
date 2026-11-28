// Real PTY regression for the Agent Hub (Agents overview / Activity / standalone child detail).
// Messages are always sent verbatim; every barrier of the observer thread and supervisor uses screen
// content or HTTP requests, never a fixed sleep to assume the UI is ready.

/// A pending directory and a real session directory may both exist at startup; take the one with the most
/// registered agents.
fn hub_dir(home: &std::path::Path) -> std::path::PathBuf {
    let mut best: Option<(usize, std::path::PathBuf)> = None;
    for directory in [".pp/agent/sessions", ".pp/run/sessions"] {
        let dir = home.join(directory);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("subagents") {
                continue;
            }
            let agents = std::fs::read(path.join("registry.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|value| value["agents"].as_object().map(|agents| agents.len()))
                .unwrap_or(0);
            if best.as_ref().is_none_or(|(count, _)| agents >= *count) {
                best = Some((agents, path));
            }
        }
    }
    best.map(|(_, path)| path).expect("subagent root directory")
}

fn hub_registry(home: &std::path::Path) -> Value {
    let bytes = std::fs::read(hub_dir(home).join("registry.json")).expect("registry file");
    serde_json::from_slice(&bytes).expect("registry json")
}

fn hub_append(home: &std::path::Path, agent: &str, records: &[Value]) {
    use std::io::Write;
    let path = hub_dir(home).join(format!("{agent}.events.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("event log");
    for record in records {
        writeln!(file, "{record}").expect("append record");
    }
    file.flush().expect("flush record");
}

/// The tool call shapes differ between the two protocols, so dispatch and tool replies must both be built
/// per protocol.
fn hub_call(protocol: &str, id: &str, name: &str, args: Value) -> Value {
    if protocol == "completion" {
        call(id, name, args)
    } else {
        response_call(id, name, args)
    }
}

fn hub_body_text(body: &Value) -> String {
    body.to_string()
}

fn hub_is_child(body: &Value) -> bool {
    hub_body_text(body).contains("You are subagent")
}

/// Dispatch a batch of subagents and match each one's first model request by its task text.
fn hub_dispatch(
    pty: &mut Pty,
    fixture: &TestServer,
    protocol: &str,
    tasks: Value,
    needles: &[&str],
) -> Vec<CapturedRequest> {
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    ask_reply_calls(
        request,
        protocol,
        json!([hub_call(protocol, "task", "task", tasks)]),
    );
    let mut found: Vec<Option<CapturedRequest>> = needles.iter().map(|_| None).collect();
    // The parent proceeds automatically while the subagents are pending, so its immediately following
    // request must be consumed.
    let mut parent_answered = false;
    while found.iter().any(Option::is_none) || !parent_answered {
        let (request, body) = pty.request_at(fixture, api_path(protocol));
        if !hub_is_child(&body) {
            assert!(!parent_answered, "unexpected second parent request");
            ask_final_reply(request, protocol, "Waiting for hub children.");
            parent_answered = true;
            continue;
        }
        let text = hub_body_text(&body);
        let index = needles
            .iter()
            .position(|needle| text.contains(needle))
            .expect("known child task");
        found[index] = Some(request);
    }
    found.into_iter().map(Option::unwrap).collect()
}

/// Finish a subagent; the parent's continuation request is consumed only after the whole batch is done.
fn hub_finish_child(pty: &mut Pty, fixture: &TestServer, protocol: &str, child: CapturedRequest, text: &str) {
    ask_final_reply(child, protocol, text);
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    assert!(!hub_is_child(&body), "unexpected child request");
    ask_final_reply(request, protocol, "parent-ack");
    pty.text("parent-ack");
    pty.ready();
}

fn hub_last_user(body: &Value) -> Option<String> {
    let items = body
        .get("messages")
        .or_else(|| body.get("input"))?
        .as_array()?;
    let last = items
        .iter()
        .rev()
        .find(|item| item["role"] == "user")?;
    match &last["content"] {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => parts
            .iter()
            .find_map(|part| part["text"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

fn hub_count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

/// Find the agent observation log that contains the given text and return the agent id with all its
/// records.
/// In the new format every line is standalone JSON (no shared value table), so the test side can parse it
/// directly with serde_json.
fn hub_events_with(home: &std::path::Path, needle: &str) -> (String, Vec<Value>) {
    for entry in std::fs::read_dir(hub_dir(home))
        .expect("subagent directory")
        .flatten()
    {
        let Some(agent) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".events.jsonl"))
            .map(str::to_owned)
        else {
            continue;
        };
        let bytes = std::fs::read(entry.path()).expect("event log");
        if !String::from_utf8_lossy(&bytes).contains(needle) {
            continue;
        }
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("observed event json"))
            .collect();
        return (agent, records);
    }
    panic!("no observation log contains {needle}");
}

/// Kinds of observation records: unit variants are strings, while struct variants and hub records are
/// single-key objects.
fn hub_record_kind(record: &Value) -> String {
    if let Some(hub) = record.get("hub") {
        return format!(
            "hub:{}:{}",
            hub["kind"].as_str().unwrap_or("?"),
            hub["status"].as_str().unwrap_or("")
        );
    }
    match &record["event"] {
        Value::String(kind) => kind.clone(),
        Value::Object(map) => map.keys().next().cloned().unwrap_or_default(),
        other => panic!("unexpected observed record: {other}"),
    }
}

/// Send a lone Esc and wait for the target page, so it does not merge with the next key into an Alt
/// sequence.
fn hub_escape(pty: &mut Pty, expect: &str) {
    pty.input("\x1b");
    pty.text(expect);
}

/// Clear the Agents filter: Esc clears the filter first and then closes.
fn hub_clear_filter(pty: &mut Pty, expect: &str) {
    pty.input("\x1b");
    pty.text(expect);
}

/// Close the Hub: the main input prompt must come back after Esc.
fn hub_close(pty: &mut Pty) {
    pty.input("\x1b");
    pty.ready();
}

#[test]
fn agent_hub_streams_and_anchors_activity() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("probe.txt"), "hub-probe-body").unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"bash_timeout_seconds":37,"read":{"toolResultPreview":true},"model_roles":{"fast":{"model":"fixture/primary","effort":"none"}}}"#,
        ).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready();
        pty.input("delegate hub stream task\r");
        let mut children = hub_dispatch(
            &mut pty,
            &fixture,
            protocol,
            json!({"context":"hub context","tasks":[
                {"role":"fast","name":"LiveWriter","task":"stream the two hub chunks in order"},
                {"role":"fast","name":"ToolFailure","task":"read probe.txt then run the failing command"}
            ]}),
            &["the two hub chunks", "probe.txt"],
        );
        pty.wait_for("only the gold running count appears before either subagent finishes", |screen| {
            let Some((row, column)) = context_tui_locate(screen, "\u{f000e} 2") else {
                return false;
            };
            let gold = vt100::Color::Rgb(254, 188, 56);
            [0, 2].into_iter().all(|offset| {
                screen.cell(row, column + offset).is_some_and(|cell| cell.fgcolor() == gold)
            }) && context_tui_locate(screen, "\u{f000e} 2 · \u{f000e} 2").is_none()
        });
        // LiveWriter keeps its SSE open; ToolFailure goes through a failing tool + a later successful reply.
        let mut live = children.remove(0).start_sse().unwrap();
        ask_reply_calls(
            children.remove(0),
            protocol,
            json!([hub_call(
                protocol,
                "read-probe",
                "read",
                json!({"path":"probe.txt"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("hub-probe-body"));
        ask_reply_calls(
            request,
            protocol,
            json!([hub_call(
                protocol,
                "fail-bash",
                "bash",
                json!({"command":"printf hub-tool-fail; exit 3"})
            )]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_body_text(&body).contains("hub-tool-fail"));
        ask_final_reply(request, protocol, "hub-recovered");
        pty.wait_for("one subagent is running and one has completed", |screen| {
            let Some((row, column)) = context_tui_locate(screen, "\u{f007} 1 · \u{f007} 1") else {
                return false;
            };
            let gold = vt100::Color::Rgb(254, 188, 56);
            let gray = vt100::Color::Idx(8);
            [(0, gold), (2, gold), (6, gray), (8, gray)].into_iter().all(|(offset, color)| {
                screen.cell(row, column + offset).is_some_and(|cell| cell.fgcolor() == color)
            })
        });
        cancel_assert_no_request(&fixture);
        // Send the first chunk, then enter the detail view: it must be visible without any extra key.
        streaming_delta(&mut live, protocol, "hub-stream-first");
        pty.input("/agents\r");
        pty.wait_for("a spinner is shown before the running subagent name", |screen| {
            let text = screen.contents();
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
                .chars()
                .any(|frame| text.contains(&format!("{frame} LiveWriter")))
        });
        pty.text("ToolFailure");
        pty.input("\r");
        pty.text("hub-stream-first");
        pty.wait_for("the subagent shows a positive rate before the stream ends", context_tui_positive_speed);
        let (first_rate, _) = context_tui_speed(pty.parser.screen()).unwrap();
        pty.wait_for("the subagent detail shows a live clock", |screen| {
            let text = screen.contents();
            WAIT_CLOCKS
                .into_iter()
                .any(|clock| text.contains(&format!("{clock} Responding")))
        });
        let first_clock = {
            let text = pty.contents();
            WAIT_CLOCKS
                .into_iter()
                .find(|clock| text.contains(&format!("{clock} Responding")))
                .unwrap()
        };
        pty.wait_for("the subagent detail clock keeps ticking", |screen| {
            let text = screen.contents();
            WAIT_CLOCKS
                .into_iter()
                .any(|clock| clock != first_clock && text.contains(&format!("{clock} Responding")))
        });
        // The second chunk is appended exactly once.
        // The assembled fragments must match the final text, otherwise the Responses protocol reports a
        // conflict.
        let second = format!(" hub-stream-second{}", " more".repeat(100));
        streaming_delta(&mut live, protocol, &second);
        pty.text("hub-stream-second");
        assert_eq!(hub_count(&pty.contents(), "hub-stream-second"), 1);
        pty.wait_for("the subagent rate rises when later output arrives", |screen| {
            context_tui_speed(screen).is_some_and(|(rate, _)| rate > first_rate)
        });
        streaming_complete(&mut live, protocol, &format!("hub-stream-first{second}"));
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        ask_final_reply(request, protocol, "hub-parent-stream-done");
        pty.text("✻ Completed");
        // Activity: failures appear only under Errors.
        hub_escape(&mut pty, "Roster ·");
        pty.input("2");
        pty.text("All agents");
        pty.input("f");
        pty.text("Errors");
        let screen = pty.contents();
        assert_eq!(hub_count(&screen, "\u{f00d}"), 1, "{screen}");
        // Enter anchors on the failed tool card.
        pty.input("\r");
        pty.text("Agent Hub");
        pty.text("print(\"hub-tool-fail\")");
        // Tools restored from disk history use the shared configuration, so the replay time is not counted
        // as Wall.
        pty.text("Timeout: 37s");
        assert!(!pty.contents().contains("Wall:"));
        pty.input("\x1b[1;5H");
        pty.text("Read · probe.txt");
        assert!(!pty.contents().contains("hub-probe-body"));
        // Esc returns to the original scope/search/selected activity.
        hub_escape(&mut pty, "Errors");
        assert_eq!(hub_count(&pty.contents(), "\u{f00d}"), 1);
        hub_escape(&mut pty, "Roster ·");
        hub_close(&mut pty);
        pty.wait_for("only the gray completed count remains after both subagents finish", |screen| {
            context_tui_locate(screen, "\u{f000e} 2").is_some_and(|(row, column)| {
                screen
                    .cell(row, column)
                    .is_some_and(|cell| cell.fgcolor() == vt100::Color::Idx(8))
            })
        });
        pty.input("\x03");
        pty.finish();
}

