//! Save-linkage regression for ordinary write/edit: final disk content, byte count, diff, undo and
//! the auto-diagnostics switch are all observed from a real fixture language server's protocol traffic, without echo simulation.

mod integration {
    use super::super::*;
    use pp_config::config::LspSettings;
    use std::time::{Duration, Instant};

    fn fixture_tools(dir: &tempfile::TempDir, args: &[&str], settings: LspSettings) -> Tools {
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.path().join("requests.jsonl");
        let mut server_args = vec![
            script.to_string_lossy().into_owned(),
            "--log".to_owned(),
            log.to_string_lossy().into_owned(),
        ];
        server_args.extend(args.iter().map(|arg| (*arg).to_owned()));
        fs::create_dir_all(dir.path().join(".pp")).unwrap();
        fs::write(
            dir.path().join(".pp/lsp.json"),
            json!({"servers": {"fixture": {
                "command": "python3",
                "args": server_args,
                "fileTypes": [".fixture"],
                "rootMarkers": [".pp"],
            }}})
            .to_string(),
        )
        .unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        tools.install_lsp(pp_lsp::Registry::new(dir.path().into(), None, settings));
        tools
    }

    fn save_settings() -> LspSettings {
        LspSettings {
            enabled: true,
            lazy: true,
            diagnostics_on_write: true,
            diagnostics_on_edit: false,
            diagnostics_deduplicate: true,
            request_timeout: 10,
        }
    }

    fn execute_events(
        tools: &Tools,
        name: &str,
        args: Value,
        id: &str,
    ) -> (Value, Vec<AgentEvent>) {
        let workflow = WorkflowState::default();
        let token = CancellationToken::new();
        let mut events = Vec::new();
        let execution = tools.execute(
            name,
            &args.to_string(),
            id,
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            &token,
            &mut |event| events.push(event),
        );
        assert!(
            execution.stop_error.is_none(),
            "unexpected cleanup failure: {:?}",
            execution.stop_error
        );
        (serde_json::from_str(&execution.output).unwrap(), events)
    }

    fn logged(dir: &tempfile::TempDir) -> Vec<Value> {
        fs::read_to_string(dir.path().join("requests.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    fn wait_logged(dir: &tempfile::TempDir, mut ready: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = logged(dir);
            if ready(&events) || Instant::now() >= deadline {
                return events;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn start_fixture(tools: &Tools) {
        tools
            .lsp_registry()
            .expect("fixture registry")
            .start_client(
                "fixture",
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
    }

    fn watched(events: &[Value], path: &Path, kind: u64) -> usize {
        let uri = pp_lsp::uri::path_to_uri(path);
        events
            .iter()
            .filter(|event| {
                event["method"] == "workspace/didChangeWatchedFiles"
                    && event["params"]["changes"]
                        .as_array()
                        .is_some_and(|changes| {
                            changes
                                .iter()
                                .any(|change| change["uri"] == uri && change["type"] == kind)
                        })
            })
            .count()
    }

    fn fixture_server_from_events(events: &[AgentEvent]) -> Value {
        let finished = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolFinished { result, .. } => Some(result),
                _ => None,
            })
            .expect("ToolFinished event missing");
        finished["diagnostics"]["servers"]
            .as_array()
            .and_then(|servers| servers.iter().find(|server| server["server"] == "fixture"))
            .cloned()
            .unwrap_or_else(|| panic!("fixture server report missing: {finished}"))
    }

    #[test]
    fn lsp_save_write_preserves_ai_bytes_and_reports_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let tools = fixture_tools(&dir, &["--format", "edits"], save_settings());
        let path = dir.path().join("sample.fixture");
        let content = "wrong   \n\n";
        let (result, events) = execute_events(
            &tools,
            "write",
            json!({"path":"sample.fixture","content":content}),
            "write-1",
        );
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        // The model view keeps only the rendered content and does not resend the structured diagnostics object; UI events keep the full report.
        assert!(result.get("diagnostics").is_none(), "{result}");
        assert!(result["content"].as_str().unwrap().contains("wrong value"));
        let finished = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::ToolFinished { result, .. } => Some(result),
                _ => None,
            })
            .expect("ToolFinished event missing");
        let server = fixture_server_from_events(&events);
        assert_eq!(result["bytes_written"], content.len());
        assert_eq!(server["status"], "reported_issues");
        assert!(finished["diagnostics"]["formatter"].is_null());
        let changed = events
            .iter()
            .position(|e| matches!(e, AgentEvent::FileChanged { .. }))
            .unwrap();
        let diagnostic = events
            .iter()
            .position(|e| matches!(e, AgentEvent::LspDiagnostics { .. }))
            .unwrap();
        assert!(changed < diagnostic);
        let logged = logged(&dir);
        assert!(!logged.iter().any(|e| matches!(
            e["method"].as_str(),
            Some("textDocument/formatting" | "workspace/executeCommand")
        )));
    }
    #[test]
    fn lsp_save_edit_syncs_exact_text_and_version_without_formatting() {
        let dir = tempfile::tempdir().unwrap();
        let settings = LspSettings {
            diagnostics_on_write: false,
            ..save_settings()
        };
        let tools = fixture_tools(&dir, &["--format", "edits"], settings);
        let path = dir.path().join("sample.fixture");
        fs::write(&path, "one\n").unwrap();
        start_fixture(&tools);
        let (opened, _) = execute_events(
            &tools,
            "lsp",
            json!({"action":"hover","file":"sample.fixture"}),
            "open",
        );
        assert_eq!(opened["ok"], true, "{opened}");
        let (result, _) = execute_events(
            &tools,
            "edit",
            json!({"path":"sample.fixture","edits":[{"oldText":"one","newText":"two   "}]}),
            "edit-1",
        );
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "two   \n");
        assert_eq!(result["bytes_written"], 7);
        let first = tools
            .lsp_registry()
            .unwrap()
            .document_snapshot("fixture", &path)
            .unwrap();
        assert_eq!(first.1, "two   \n");
        let (result, _) = execute_events(
            &tools,
            "edit",
            json!({"path":"sample.fixture","edits":[{"oldText":"two","newText":"three"}]}),
            "edit-2",
        );
        assert_eq!(result["ok"], true);
        let second = tools
            .lsp_registry()
            .unwrap()
            .document_snapshot("fixture", &path)
            .unwrap();
        assert_eq!(second.1, "three   \n");
        assert!(second.0 > first.0);
        let events = wait_logged(&dir, |events| watched(events, &path, 2) >= 2);
        assert!(!events.iter().any(|e| matches!(
            e["method"].as_str(),
            Some(
                "textDocument/formatting" | "textDocument/diagnostic" | "workspace/executeCommand"
            )
        )));
    }
    #[test]
    fn lsp_save_failure_publishes_neither_diff_nor_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let tools = fixture_tools(&dir, &["--format", "edits"], save_settings());
        fs::create_dir(dir.path().join("blocked")).unwrap();
        let (result, events) = execute_events(
            &tools,
            "write",
            json!({"path":"blocked","content":"wrong\n"}),
            "write-blocked",
        );
        assert_eq!(result["ok"], false, "{result}");
        assert!(!dir.path().join("blocked").is_file());
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::FileChanged { .. })),
            "failed save must not publish a diff"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::LspDiagnostics { .. })),
            "failed save must not start diagnostics"
        );

        // A staging failure (old text missing) likewise has no commit, emits no diff and starts no diagnostics.
        let path = dir.path().join("sample.fixture");
        fs::write(&path, "one\n").unwrap();
        let (result, events) = execute_events(
            &tools,
            "edit",
            json!({"path":"sample.fixture","edits":[{"oldText":"missing","newText":"two"}]}),
            "edit-missing",
        );
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::FileChanged { .. })),
            "staging failure must not publish a diff"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::LspDiagnostics { .. })),
            "staging failure must not start diagnostics"
        );
    }
}
