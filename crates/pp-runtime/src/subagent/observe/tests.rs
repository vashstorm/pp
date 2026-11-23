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
fn cursor_resumes_without_rereading_and_completes_half_line() {
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
    // 半行：写入推进游标但不产生条目，也不报告 has_more。
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
    // 再读一次不会重复。
    let again = binding
        .read_events("a1", Some(completed.cursor.as_ref().unwrap()))
        .unwrap();
    assert!(again.entries.is_empty());
}

#[test]
fn utf8_split_across_reads_and_large_records_page() {
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
    // 大记录跨越软预算仍被完整读出。
    let AgentEvent::AssistantDelta { text: first, .. } = entries[0].record.event.clone().unwrap()
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

#[test]
fn generation_changes_and_broken_lines_keep_stream_readable() {
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

#[test]
fn replacement_and_truncation_request_reset() {
    let home = tempfile::tempdir().unwrap();
    let binding = binding(home.path(), "a1");
    append(
        &binding,
        "a1",
        r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
    );
    let cursor = binding.read_events("a1", None).unwrap().cursor.unwrap();
    let path = binding.directory().join("a1.events.jsonl");
    // 替换文件：inode 变化必须 reset。
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
    // 截断到游标之前同样 reset。
    let cursor = page.cursor.unwrap();
    fs::write(&path, "").unwrap();
    let page = binding.read_events("a1", Some(&cursor)).unwrap();
    assert!(page.reset);
    assert!(page.entries.is_empty());
}

#[test]
fn missing_file_and_unknown_agent_are_distinct() {
    let home = tempfile::tempdir().unwrap();
    let binding = binding(home.path(), "a1");
    let page = binding.read_events("a1", None).unwrap();
    assert!(page.entries.is_empty());
    assert!(page.cursor.is_none());
    assert!(binding.read_events("a9", None).is_err());
    assert!(!binding.same_root(&binding.clone()) || binding.same_root(&binding.clone()));
}

#[test]
fn snapshots_keep_flat_agent_shape() {
    let home = tempfile::tempdir().unwrap();
    let binding = binding(home.path(), "a1");
    let value = serde_json::to_value(binding.hub_snapshot()).unwrap();
    assert_eq!(value["agents"][0]["id"], "a1");
    assert_eq!(value["agents"][0]["parent"], "Main");
    assert!(value["agents"][0].get("spec").is_some());
    assert!(value["runs"].as_array().unwrap().is_empty());
    let legacy = binding.snapshots();
    assert_eq!(legacy["agents"][0]["id"], "a1");
}

#[test]
fn progress_does_not_write_or_bump_revision_and_sequence_gaps_are_valid() {
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
    }
    let entries = read_all(&binding, "a1");
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[1].record.sequence, Some(100));
    assert_eq!(entries[2].record.sequence, Some(200));
    binding.inner.observe(
        "a1",
        Some("r1"),
        1,
        Some(201),
        Some(&AgentEvent::ModelOutputProgress { output_bytes: 201 }),
        Some(HubEvent::RunState {
            status: "completed".into(),
        }),
    );
    let entries = read_all(&binding, "a1");
    assert!(entries[3].record.event.is_none());
    assert!(entries[3].record.hub.is_some());
    assert!(binding.model_output_progress("a1", "r1", 1, 1).is_none());
}

#[test]
fn model_progress_is_scoped_to_the_current_request() {
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

#[test]
fn artifact_output_previews_replace_in_memory_without_appending_event_log() {
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
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(i + 22),
            Some(&AgentEvent::ToolPipelineSnapshot {
                id: "bash".into(),
                pipes: serde_json::json!({"run_id":"r1","revision":i}),
                preview: None,
            }),
            None,
        );
    }
    let snapshot = binding.hub_snapshot();
    assert_eq!(snapshot.output_previews.len(), 1);
    assert!(
        matches!(&snapshot.output_previews[0].event,Some(AgentEvent::ToolOutputSnapshot { output, .. }) if output == "current-19")
    );
    // 阶段预览独立保留，不覆盖主输出预览。
    assert_eq!(snapshot.pipeline_previews.len(), 1);
    assert!(matches!(
        &snapshot.pipeline_previews[0].event,
        Some(AgentEvent::ToolPipelineSnapshot { pipes, .. })
            if pipes["revision"] == 19
    ));
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
    assert!(snapshot.pipeline_previews.is_empty());
    assert_eq!(read_all(&binding, "a1").len(), 2);
}

#[test]
fn overview_events_coalesce_deltas_skip_output_and_track_activity() {
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
    // ToolOutput 与 ModelRequestStarted 被跳过；非空 AssistantFinished 前的 AssistantDelta 被丢弃。
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

    // 游标从末尾继续追平新增事件，包括末尾半行和 file replacement reset。
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
