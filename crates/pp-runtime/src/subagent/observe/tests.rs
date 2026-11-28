use super::*;
use crate::subagent::{RootOptions, Spec, atomic_json};
use pp_core::model::ThinkLevel;
use std::fs;
use std::path::PathBuf;

fn spec() -> Spec {
    Spec {
        model_role: None,
        role_route: None,
        definition: "task".into(),
        prompt: String::new(),
        context: String::new(),
        model: "fixture-model".into(),
        thinking: ThinkLevel::None,
        tools: vec![],
        spawns: vec![],
        output_schema: None,
        cwd: PathBuf::from("."),
        depth: 1,
        readonly: false,
        isolated: false,
        cost: None,
    }
}

fn binding(dir: &std::path::Path, id: &str) -> Binding {
    let dir = dir.join("session.subagents");
    std::fs::create_dir_all(&dir).unwrap();
    atomic_json(
        &dir.join("registry.json"),
        &serde_json::json!({
            "version": 1,
            "agents": { id: {
                "id": id,
                "name": "probe",
                "parent": "Main",
                "generation": 1,
                "status": "idle",
                "spec": spec(),
                "session_file": null,
                "updated_ms": 1,
            }},
            "runs": {},
            "next_id": 1,
        }),
    )
    .unwrap();
    Binding::root(
        RootOptions {
            user: dir.clone(),
            dir,
            cwd: PathBuf::from("."),
            model: "fixture-model".into(),
            thinking: ThinkLevel::None,
            settings: Default::default(),
            plan_mode: false,
        },
        std::sync::Arc::new(|_, _| unreachable!("no launch in observe tests")),
        std::sync::Arc::new(|_| Ok(())),
    )
    .unwrap()
}

fn append(binding: &Binding, id: &str, record: &str) {
    use std::io::Write;
    let path = binding.directory().join(format!("{id}.events.jsonl"));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{record}").unwrap();
}

fn observe(
    binding: &Binding,
    id: &str,
    run: &str,
    generation: u64,
    sequence: u64,
    event: AgentEvent,
) {
    binding.inner.observe(
        id,
        Some(run),
        generation,
        Some(sequence),
        Some(&event),
        None,
    );
}

fn delta_text(entry: &ObservedEntry) -> String {
    match entry.record.event.clone().expect("event payload") {
        AgentEvent::ThinkingDelta { text }
        | AgentEvent::ReasoningSummaryDelta { text }
        | AgentEvent::AssistantDelta { text, .. } => text,
        other => panic!("expected delta, got {other:?}"),
    }
}

fn read_all(binding: &Binding, id: &str) -> Vec<ObservedEntry> {
    let mut cursor = None;
    let mut all = Vec::new();
    loop {
        let page = binding.read_events(id, cursor.as_ref()).unwrap();
        cursor = page.cursor;
        all.extend(page.entries);
        if !page.has_more {
            return all;
        }
    }
}

#[test]
fn pagination_cursor_utf8_and_broken_lines() {
    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        for sequence in 1..=128 {
            append(
                &binding,
                "a1",
                &serde_json::json!({
                    "generation": 1,
                    "sequence": sequence,
                    "timestamp_ms": sequence,
                    "event": {"assistant_started": null},
                })
                .to_string(),
            );
        }
        // The coalesced single record far exceeds 4 KiB: the next newline falls outside the old fixed probe window.
        append(
            &binding,
            "a1",
            &serde_json::json!({
                "generation": 1,
                "sequence": 129,
                "timestamp_ms": 129,
                "event": {"thinking_delta": {"text": "x".repeat(10 * 1024)}},
            })
            .to_string(),
        );
        append(
            &binding,
            "a1",
            &serde_json::json!({
                "generation": 1,
                "sequence": 130,
                "timestamp_ms": 130,
                "event": {"assistant_finished": {"text": "done"}},
            })
            .to_string(),
        );
        let page = binding.read_events("a1", None).unwrap();
        assert_eq!(page.entries.len(), PAGE_ENTRIES);
        assert!(page.has_more);
        let next = binding.read_events("a1", page.cursor.as_ref()).unwrap();
        assert_eq!(next.entries.len(), 2);
        assert!(!next.has_more);
        assert_eq!(delta_text(&next.entries[0]).len(), 10 * 1024);
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        let first = binding.read_events("a1", None).unwrap();
        assert_eq!(first.entries.len(), 1);
        assert!(!first.reset);
        assert!(!first.has_more);
        let cursor = first.cursor.unwrap();
        // Partial line: writing advances the cursor without producing an entry and without reporting has_more.
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(binding.directory().join("a1.events.jsonl"))
                .unwrap();
            write!(file, r#"{{"agent_id":"a1","generation":1"#).unwrap();
        }
        let partial = binding.read_events("a1", Some(&cursor)).unwrap();
        assert_eq!(partial.entries.len(), 0);
        assert!(!partial.has_more);
        assert_eq!(partial.cursor.as_ref().unwrap().offset, cursor.offset);
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(binding.directory().join("a1.events.jsonl"))
                .unwrap();
            writeln!(
                file,
                r#","sequence":2,"event":{{"assistant_finished":{{"text":"done"}}}}}}"#
            )
            .unwrap();
        }
        let completed = binding
            .read_events("a1", Some(partial.cursor.as_ref().unwrap()))
            .unwrap();
        assert_eq!(completed.entries.len(), 1);
        assert_eq!(completed.entries[0].start_offset, cursor.offset);
        // Reading once more does not duplicate anything.
        let again = binding
            .read_events("a1", Some(completed.cursor.as_ref().unwrap()))
            .unwrap();
        assert!(again.entries.is_empty());
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let text = "中文输出".repeat(20_000);
        for sequence in 1..=200 {
            let record = serde_json::json!({
                "agent_id": "a1",
                "generation": 1,
                "sequence": sequence,
                "timestamp_ms": sequence,
                "event": {"assistant_delta": {"text": if sequence == 1 { &text } else { "x" }, "refusal": false}},
            });
            append(&binding, "a1", &record.to_string());
        }
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 200);
        assert_eq!(entries[0].start_offset, 0);
        // A large record spanning the soft budget is still read completely.
        let AgentEvent::AssistantDelta { text: first, .. } =
            entries[0].record.event.clone().unwrap()
        else {
            panic!("expected delta");
        };
        assert_eq!(first, text);
        assert!(
            entries
                .windows(2)
                .all(|pair| pair[0].start_offset < pair[1].start_offset)
        );
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        append(&binding, "a1", "{not json");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":2,"sequence":1,"hub":{"kind":"run_state","status":"queued"}}"#,
        );
        let page = binding.read_events("a1", None).unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[1].record.generation, 2);
        assert_eq!(page.warnings.len(), 1);
        assert!(page.warnings[0].contains("byte"));
        assert!(!page.warnings[0].contains("not json"));
    }
}

#[test]
fn replacement_truncation_missing_file_and_flat_snapshots() {
    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        let cursor = binding.read_events("a1", None).unwrap().cursor.unwrap();
        let path = binding.directory().join("a1.events.jsonl");
        // Replacing the file: an inode change must reset.
        let replacement = binding.directory().join("replacement.jsonl");
        fs::write(&replacement, "").unwrap();
        fs::rename(&replacement, &path).unwrap();
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        let page = binding.read_events("a1", Some(&cursor)).unwrap();
        assert!(page.reset);
        assert_eq!(page.entries.len(), 1);
        // Truncating below the cursor also resets.
        let cursor = page.cursor.unwrap();
        fs::write(&path, "").unwrap();
        let page = binding.read_events("a1", Some(&cursor)).unwrap();
        assert!(page.reset);
        assert!(page.entries.is_empty());
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let page = binding.read_events("a1", None).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.cursor.is_none());
        assert!(binding.read_events("a9", None).is_err());
        assert!(binding.same_root(&binding.clone()));

        let value = serde_json::to_value(binding.hub_snapshot()).unwrap();
        assert_eq!(value["agents"][0]["id"], "a1");
        assert_eq!(value["agents"][0]["parent"], "Main");
        assert!(value["agents"][0].get("spec").is_some());
        assert!(value["runs"].as_array().unwrap().is_empty());
        let legacy = binding.snapshots();
        assert_eq!(legacy["agents"][0]["id"], "a1");
    }
}

#[test]
fn progress_scoping_and_artifact_previews() {
    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(1),
            Some(&AgentEvent::ModelRequestStarted),
            None,
        );
        let revision = binding.hub_revision();
        let progress = |sequence| {
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(sequence),
                Some(&AgentEvent::ModelOutputProgress {
                    output_bytes: sequence,
                }),
                None,
            );
        };
        progress(2);
        let (bytes, first_output) = binding.model_output_progress("a1", "r1", 1, 1).unwrap();
        assert_eq!(bytes, 2);
        for sequence in 3..100 {
            progress(sequence);
        }
        assert_eq!(
            binding.model_output_progress("a1", "r1", 1, 1),
            Some((99, first_output))
        );
        assert_eq!(binding.hub_revision(), revision);
        assert!(binding.hub_snapshot().output_previews.is_empty());
        assert_eq!(read_all(&binding, "a1").len(), 1);
        for sequence in [100, 200] {
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(sequence),
                Some(&AgentEvent::ThinkingDelta {
                    text: "kept".into(),
                }),
                None,
            );
            // A non-delta boundary flushes the pending coalesce buffer first; gaps in sequence remain legal.
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(sequence + 50),
                Some(&AgentEvent::AssistantStarted),
                None,
            );
        }
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 5);
        assert_eq!(entries[1].record.sequence, Some(100));
        assert_eq!(entries[2].record.sequence, Some(150));
        assert_eq!(entries[3].record.sequence, Some(200));
        assert_eq!(entries[4].record.sequence, Some(250));
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(260),
            Some(&AgentEvent::ModelOutputProgress { output_bytes: 260 }),
            Some(HubEvent::RunState {
                status: "completed".into(),
            }),
        );
        let entries = read_all(&binding, "a1");
        assert!(entries[5].record.event.is_none());
        assert!(entries[5].record.hub.is_some());
        assert!(binding.model_output_progress("a1", "r1", 1, 1).is_none());
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let observe = |run, generation, sequence, event| {
            binding.inner.observe(
                "a1",
                Some(run),
                generation,
                Some(sequence),
                Some(&event),
                None,
            );
        };
        let progress = |bytes| AgentEvent::ModelOutputProgress {
            output_bytes: bytes,
        };
        let finished = AgentEvent::ModelRequestFinished {
            elapsed_ms: 100,
            usage: None,
            success: false,
        };
        observe("r1", 1, 1, AgentEvent::ModelRequestStarted);
        observe("r1", 1, 2, progress(300));
        observe("r1", 1, 3, finished.clone());
        assert!(binding.model_output_progress("a1", "r1", 1, 1).is_none());

        observe("r1", 1, 4, AgentEvent::ModelRequestStarted);
        observe("r1", 1, 5, progress(15));
        assert!(binding.model_output_progress("a1", "r1", 1, 1).is_none());
        assert_eq!(
            binding
                .model_output_progress("a1", "r1", 1, 4)
                .map(|sample| sample.0),
            Some(15)
        );

        observe("r2", 2, 1, AgentEvent::ModelRequestStarted);
        observe("r2", 2, 2, progress(30));
        observe("r1", 1, 6, progress(600));
        observe("r1", 1, 7, finished);
        assert!(binding.model_output_progress("a1", "r1", 1, 4).is_none());
        assert!(binding.model_output_progress("a1", "r2", 1, 1).is_none());
        assert_eq!(
            binding
                .model_output_progress("a1", "r2", 2, 1)
                .map(|sample| sample.0),
            Some(30)
        );
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(1),
            Some(&AgentEvent::ToolStarted {
                id: "bash".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }),
            None,
        );
        for i in 0..20 {
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(i + 2),
                Some(&AgentEvent::ToolOutputSnapshot {
                    id: "bash".into(),
                    output: format!("current-{i}"),
                }),
                None,
            );
        }
        let snapshot = binding.hub_snapshot();
        assert_eq!(snapshot.output_previews.len(), 1);
        assert!(
            matches!(&snapshot.output_previews[0].event, Some(AgentEvent::ToolOutputSnapshot { output, .. }) if output == "current-19")
        );
        assert_eq!(read_all(&binding, "a1").len(), 1);
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(22),
            Some(&AgentEvent::ToolFinished {
                id: "bash".into(),
                result: serde_json::json!({"ok":true,"output":"final"}),
            }),
            None,
        );
        let snapshot = binding.hub_snapshot();
        assert!(snapshot.output_previews.is_empty());
        assert_eq!(read_all(&binding, "a1").len(), 2);
    }
}

#[test]
fn overview_events_coalescing_and_slim_backfill() {
    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let empty_dir = tempfile::tempdir().unwrap();
        let empty_binding = Binding::root(
            RootOptions {
                user: empty_dir.path().to_path_buf(),
                dir: empty_dir.path().to_path_buf(),
                cwd: PathBuf::from("."),
                model: "fixture-model".into(),
                thinking: ThinkLevel::None,
                settings: Default::default(),
                plan_mode: false,
            },
            std::sync::Arc::new(|_, _| unreachable!("no launch")),
            std::sync::Arc::new(|_| Ok(())),
        )
        .unwrap();
        assert!(!empty_binding.has_agents());
        assert!(binding.has_agents());

        let lines = [
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":100,"hub":{"kind":"run_state","status":"running"}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":105,"event":{"model_request_started":null}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":110,"event":{"assistant_started":null}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":111,"event":{"reasoning_summary_delta":{"text":"step 1 "}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":112,"event":{"reasoning_summary_delta":{"text":"step 2"}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":113,"event":{"assistant_delta":{"text":"discarded","refusal":false}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":114,"event":{"assistant_finished":{"text":""}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":115,"event":{"tool_started":{"id":"t1","name":"bash","arguments":"{}"}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":116,"event":{"tool_output":{"id":"t1","stream":"stdout","text":"huge output"}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":120,"event":{"tool_finished":{"id":"t1","result":{"ok":true}}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":121,"event":{"assistant_started":null}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":122,"event":{"assistant_delta":{"text":"replaced","refusal":false}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":125,"event":{"assistant_finished":{"text":"final answer"}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":130,"event":{"model_request_finished":{"elapsed_ms":25,"usage":{"input_tokens":10,"output_tokens":20},"success":true}}}"#,
            r#"{"agent_id":"a1","run_id":"r1","generation":1,"timestamp_ms":140,"hub":{"kind":"run_state","status":"completed"}}"#,
        ];
        for line in lines {
            append(&binding, "a1", line);
        }

        let page = binding.read_overview_events("a1", None).unwrap();
        assert!(!page.reset);
        assert!(!page.has_more);
        assert_eq!(page.last_activity_ms, Some(140));
        assert!(page.warnings.is_empty());
        // ToolOutput and ModelRequestStarted are skipped; the AssistantDelta before a non-empty AssistantFinished is discarded.
        assert!(page.entries.iter().all(|entry| !matches!(
            entry.record.event,
            Some(AgentEvent::ToolOutput { .. } | AgentEvent::ModelRequestStarted)
        )));
        assert!(page.entries.iter().any(|entry| matches!(
            &entry.record.event,
            Some(AgentEvent::AssistantDelta { text, .. }) if text == "discarded"
        )));
        assert!(!page.entries.iter().any(|entry| matches!(
            &entry.record.event,
            Some(AgentEvent::AssistantDelta { text, .. }) if text == "replaced"
        )));
        assert_eq!(
            page.entries
                .iter()
                .filter(|entry| matches!(
                    entry.record.event,
                    Some(AgentEvent::ReasoningSummaryDelta { .. })
                ))
                .count(),
            2
        );

        // The cursor catches up with newly appended events from the tail, including a trailing partial line and a file replacement reset.
        let cursor = page.cursor.unwrap();
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(binding.directory().join("a1.events.jsonl"))
                .unwrap();
            write!(file, r#"{{"agent_id":"a1","run_id":"r2""#).unwrap();
        }
        let partial = binding.read_overview_events("a1", Some(&cursor)).unwrap();
        assert!(partial.entries.is_empty());
        assert!(!partial.has_more);
        assert_eq!(partial.cursor.as_ref().unwrap().offset, cursor.offset);

        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(binding.directory().join("a1.events.jsonl"))
                .unwrap();
            writeln!(
                file,
                r#","generation":1,"timestamp_ms":200,"event":{{"tool_started":{{"id":"t2","name":"read","arguments":"{{}}"}}}}}}"#
            )
            .unwrap();
        }
        let incremental = binding
            .read_overview_events("a1", partial.cursor.as_ref())
            .unwrap();
        assert_eq!(incremental.entries.len(), 1);
        assert_eq!(incremental.last_activity_ms, Some(200));

        let path = binding.directory().join("a1.events.jsonl");
        fs::write(&path, "").unwrap();
        let truncated = binding
            .read_overview_events("a1", incremental.cursor.as_ref())
            .unwrap();
        assert!(truncated.reset);
        assert!(truncated.entries.is_empty());
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        observe(
            &binding,
            "a1",
            "r1",
            1,
            1,
            AgentEvent::ThinkingDelta { text: "思".into() },
        );
        observe(
            &binding,
            "a1",
            "r1",
            1,
            2,
            AgentEvent::ThinkingDelta { text: "考".into() },
        );
        observe(
            &binding,
            "a1",
            "r1",
            1,
            3,
            AgentEvent::ToolStarted {
                id: "t1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        );
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(4),
            None,
            Some(HubEvent::RunState {
                status: "running".into(),
            }),
        );
        let raw = fs::read_to_string(binding.directory().join("a1.events.jsonl")).unwrap();
        assert!(!raw.contains("\"agent_id\""), "{raw}");
        assert!(!raw.contains("\"root_id\""), "{raw}");
        // Explicit identity in legacy lines of the same file stays as is; missing identity is filled from the requested agent and directory name.
        append(
            &binding,
            "a1",
            r#"{"root_id":"other.subagents","agent_id":"a7","generation":1,"sequence":9,"event":{"assistant_started":null}}"#,
        );
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 4);
        for entry in &entries[..3] {
            assert_eq!(entry.record.agent_id, "a1");
            assert_eq!(entry.record.root_id.as_deref(), Some("session.subagents"));
        }
        assert_eq!(delta_text(&entries[0]), "思考");
        assert_eq!(entries[3].record.agent_id, "a7");
        assert_eq!(
            entries[3].record.root_id.as_deref(),
            Some("other.subagents")
        );

        let page = binding.read_overview_events("a1", None).unwrap();
        assert!(page.warnings.is_empty(), "{:?}", page.warnings);
        let legacy = page
            .entries
            .iter()
            .find(|entry| entry.record.sequence == Some(9))
            .expect("legacy line");
        assert_eq!(legacy.record.agent_id, "a7");
        assert_eq!(legacy.record.root_id.as_deref(), Some("other.subagents"));
        let tool = page
            .entries
            .iter()
            .find(|entry| matches!(entry.record.event, Some(AgentEvent::ToolStarted { .. })))
            .expect("tool record");
        assert_eq!(tool.record.agent_id, "a1");
        assert_eq!(tool.record.root_id.as_deref(), Some("session.subagents"));
        let hub = page
            .entries
            .iter()
            .find(|entry| entry.record.hub.is_some())
            .expect("hub record");
        assert_eq!(hub.record.agent_id, "a1");
        assert_eq!(hub.record.root_id.as_deref(), Some("session.subagents"));
    }
}

#[test]
fn delta_coalescing_boundaries_flush_and_failure_bounds() {
    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        for (sequence, text) in [(10, "思考"), (11, "片段"), (12, "继续")] {
            observe(
                &binding,
                "a1",
                "r1",
                1,
                sequence,
                AgentEvent::ThinkingDelta { text: text.into() },
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        observe(
            &binding,
            "a1",
            "r1",
            1,
            13,
            AgentEvent::AssistantFinished {
                text: "done".into(),
            },
        );
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 2);
        // Text is concatenated directly without separators; the merged record keeps the first fragment's sequence.
        assert_eq!(delta_text(&entries[0]), "思考片段继续");
        assert_eq!(entries[0].record.sequence, Some(10));
        // The merged record keeps the first fragment's time, not the last (still clearly earlier than the boundary record after the gap).
        let merged_ms = entries[0].record.timestamp_ms.unwrap();
        let finished_ms = entries[1].record.timestamp_ms.unwrap();
        assert!(
            finished_ms >= merged_ms + 90,
            "{merged_ms} vs {finished_ms}"
        );
        assert!(matches!(
            entries[1].record.event,
            Some(AgentEvent::AssistantFinished { .. })
        ));
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let thinking = |text: &str| AgentEvent::ThinkingDelta { text: text.into() };
        let assistant = |text: &str, refusal: bool| AgentEvent::AssistantDelta {
            text: text.into(),
            refusal,
        };
        observe(&binding, "a1", "r1", 1, 1, thinking("a"));
        observe(&binding, "a1", "r1", 1, 2, thinking("b"));
        observe(&binding, "a1", "r1", 1, 3, assistant("c", false));
        observe(&binding, "a1", "r1", 1, 4, assistant("d", false));
        observe(&binding, "a1", "r1", 1, 5, assistant("e", true));
        observe(&binding, "a1", "r2", 1, 6, assistant("f", true));
        observe(&binding, "a1", "r2", 2, 7, assistant("g", true));
        observe(
            &binding,
            "a1",
            "r2",
            2,
            8,
            AgentEvent::AssistantFinished { text: "end".into() },
        );
        let entries = read_all(&binding, "a1");
        let events: Vec<_> = entries
            .iter()
            .map(|entry| entry.record.event.clone().unwrap())
            .collect();
        assert_eq!(events.len(), 6);
        assert_eq!(delta_text(&entries[0]), "ab");
        assert_eq!(entries[0].record.sequence, Some(1));
        assert_eq!(delta_text(&entries[1]), "cd");
        assert_eq!(entries[1].record.sequence, Some(3));
        for (index, text) in [(2, "e"), (3, "f"), (4, "g")] {
            assert_eq!(delta_text(&entries[index]), text);
        }
        assert!(matches!(
            events[1],
            AgentEvent::AssistantDelta { refusal: false, .. }
        ));
        assert!(matches!(
            events[2],
            AgentEvent::AssistantDelta { refusal: true, .. }
        ));
        assert_eq!(entries[5].record.run_id.as_deref(), Some("r2"));
        assert_eq!(entries[5].record.generation, 2);
        assert!(matches!(events[5], AgentEvent::AssistantFinished { .. }));
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let chunk = "x".repeat(3000);
        for sequence in 1..=4 {
            observe(
                &binding,
                "a1",
                "r1",
                1,
                sequence,
                AgentEvent::ThinkingDelta {
                    text: chunk.clone(),
                },
            );
        }
        // After the third fragment the 8 KiB cap is reached and it is flushed at once, while the fourth stays in the in-memory buffer.
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 1);
        assert_eq!(delta_text(&entries[0]), chunk.repeat(3));
        assert_eq!(entries[0].record.sequence, Some(1));
        // A single fragment above the cap is written whole without splitting or joining the buffer.
        let big = "y".repeat(8197);
        observe(
            &binding,
            "a1",
            "r1",
            1,
            5,
            AgentEvent::ThinkingDelta { text: big.clone() },
        );
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 3);
        assert_eq!(delta_text(&entries[1]), chunk);
        assert_eq!(entries[1].record.sequence, Some(4));
        assert_eq!(delta_text(&entries[2]), big);
        assert_eq!(entries[2].record.sequence, Some(5));
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        observe(
            &binding,
            "a1",
            "r1",
            1,
            1,
            AgentEvent::ThinkingDelta {
                text: "tail".into(),
            },
        );
        std::thread::sleep(COALESCE_DELAY + std::time::Duration::from_millis(80));
        // The streaming tail advances purely through observation polling (flush before read), not via the next model event.
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 1);
        assert_eq!(delta_text(&entries[0]), "tail");
        assert_eq!(entries[0].record.sequence, Some(1));
        assert_eq!(
            binding
                .inner
                .append_gate("a1")
                .lock()
                .unwrap()
                .pending
                .as_ref()
                .map(|pending| pending.text.len()),
            None
        );
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        observe(
            &binding,
            "a1",
            "r1",
            1,
            1,
            AgentEvent::AssistantDelta {
                text: "buffer".into(),
                refusal: false,
            },
        );
        assert!(read_all(&binding, "a1").is_empty());
        binding.flush_observations();
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 1);
        assert_eq!(delta_text(&entries[0]), "buffer");
    }

    {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        observe(
            &binding,
            "a1",
            "r1",
            1,
            1,
            AgentEvent::ThinkingDelta {
                text: "before tool".into(),
            },
        );
        observe(
            &binding,
            "a1",
            "r1",
            1,
            2,
            AgentEvent::ToolStarted {
                id: "t1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
        );
        observe(
            &binding,
            "a1",
            "r1",
            1,
            3,
            AgentEvent::AssistantFinished {
                text: String::new(),
            },
        );
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(4),
            None,
            Some(HubEvent::RunState {
                status: "completed".into(),
            }),
        );
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 4);
        assert_eq!(delta_text(&entries[0]), "before tool");
        assert!(matches!(
            entries[1].record.event,
            Some(AgentEvent::ToolStarted { .. })
        ));
        assert!(matches!(
            entries[2].record.event,
            Some(AgentEvent::AssistantFinished { .. })
        ));
        assert!(entries[3].record.hub.is_some());

        // Persistently failing appends keep only a redacted health hint, and the failed buffer never accumulates without bound.
        let blocked = binding.directory().join("a2.events.jsonl");
        fs::create_dir(&blocked).unwrap();
        let chunk = "z".repeat(500);
        for sequence in 1..=40 {
            observe(
                &binding,
                "a2",
                "r1",
                1,
                sequence,
                AgentEvent::ThinkingDelta {
                    text: chunk.clone(),
                },
            );
        }
        assert!(
            binding
                .hub_snapshot()
                .warnings
                .iter()
                .any(|warning| warning.contains("possibly incomplete"))
        );
        let buffered = binding
            .inner
            .append_gate("a2")
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .map_or(0, |pending| pending.text.len());
        assert!(buffered < COALESCE_BYTES * 2, "buffered {buffered}");
        // One successful append after recovery clears the health hint.
        fs::remove_dir(&blocked).unwrap();
        observe(&binding, "a2", "r1", 1, 41, AgentEvent::AssistantStarted);
        assert!(binding.hub_snapshot().warnings.is_empty());
    }
}
