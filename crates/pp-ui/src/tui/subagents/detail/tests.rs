use super::*;
use pp_core::events::DiffHunk;
use pp_core::events::DiffLine;
use pp_core::events::DiffLineKind;
use pp_core::events::FileDiff;
use pp_core::events::OutputStream;
use pp_runtime::agent::persistence::ReplayRecord;
use pp_runtime::subagent::ObservedRecord;
use ratatui::{Terminal as TestTerminal, backend::TestBackend, buffer::Buffer};
use serde_json::json;
use std::time::{Duration, Instant};

#[test]
fn detail_speed_timing_ticks_and_clears_on_termination() {
    {
        let mut detail = Detail::new("a1", 1, Transcript::new());
        let runs = [run("r1", "running")];
        let timed = |offset, timestamp, value| {
            let mut entry = event(offset, "r1", value);
            entry.record.timestamp_ms = Some(timestamp);
            entry
        };
        let first = page(
            vec![
                timed(0, 1_000, AgentEvent::ModelRequestStarted),
                timed(
                    1,
                    2_000,
                    AgentEvent::ModelOutputProgress { output_bytes: 300 },
                ),
                timed(
                    2,
                    3_000,
                    AgentEvent::ModelOutputProgress { output_bytes: 600 },
                ),
            ],
            true,
        );
        detail.ingest(&first, &runs, &BTreeSet::new(), 10_000);
        let (rate, estimated) = detail.token_speed.value().unwrap();
        assert!(
            (rate - 200.0).abs() < 0.01,
            "speed estimated from the one-second output window in the log: {rate}"
        );
        assert!(estimated);
        // A repeated page must not accumulate twice or reset the sampling.
        detail.ingest(&first, &runs, &BTreeSet::new(), 10_000);
        assert_eq!(detail.token_speed.value(), Some((rate, true)));
        detail.ingest(
            &page(
                vec![timed(
                    3,
                    5_000,
                    AgentEvent::ModelRequestFinished {
                        elapsed_ms: 4_000,
                        usage: Some(pp_core::context::TokenUsage {
                            input_tokens: 100,
                            output_tokens: 200,
                            cached_tokens: None,
                            cache_write_tokens: None,
                            reasoning_tokens: None,
                        }),
                        success: true,
                    },
                )],
                false,
            ),
            &runs,
            &BTreeSet::new(),
            10_000,
        );
        assert_eq!(detail.token_speed.value(), Some((50.0, false)));
        detail.ingest(
            &page(vec![state(4, "r1", "completed", Some(5_000))], false),
            &[run("r1", "completed")],
            &BTreeSet::new(),
            10_000,
        );
        assert_eq!(detail.token_speed.value(), Some((50.0, false)));
        detail.rebuild();
        assert!(detail.token_speed.value().is_none());
    }

    for status in ["cancelled", "failed", "interrupted"] {
        let mut detail = Detail::new("a1", 1, Transcript::new());
        detail.ingest(
            &page(
                vec![
                    event(0, "r1", AgentEvent::ModelRequestStarted),
                    event(
                        1,
                        "r1",
                        AgentEvent::ModelOutputProgress { output_bytes: 300 },
                    ),
                ],
                false,
            ),
            &[run("r1", "running")],
            &BTreeSet::new(),
            10_000,
        );
        assert!(detail.tick(Instant::now() + Duration::from_millis(500)));
        assert!(
            detail
                .token_speed
                .value()
                .is_some_and(|(rate, estimated)| rate > 0.0 && estimated)
        );
        detail.ingest(
            &page(vec![state(2, "r1", status, None)], false),
            &[run("r1", status)],
            &BTreeSet::new(),
            10_000,
        );
        assert!(!detail.token_speed.is_active());
        assert!(detail.token_speed.value().is_none());
    }
}

#[test]
fn detail_task_poll_and_live_output_pipeline_previews_update_cleanly() {
    {
        let mut detail = Detail::new("a1", 1, Transcript::new());
        let runs = [run("r1", "running")];
        detail.ingest(&page(vec![
            event(0, "r1", tool("start", "bash", json!({"command":"service","mode":"background"}))),
            event(1, "r1", AgentEvent::ToolFinished {
                id: "start".into(), result: json!({"ok":true,"task_id":"bg-1","status":"running","output":"before"}),
            }),
        ], true), &runs, &BTreeSet::new(), 10_000);
        let original = detail.by_tool[&("r1".into(), "start".into())];
        detail.ingest(&page(vec![
            event(2, "r1", tool("poll", "bash_task", json!({"action":"status","task_id":"bg-1"}))),
            event(3, "r1", AgentEvent::ToolFinished {
                id: "poll".into(), result: json!({"ok":true,"task_id":"bg-1","status":"exited","output":"after"}),
            }),
        ], false), &runs, &BTreeSet::new(), 10_000);
        assert_eq!(detail.by_tool[&("r1".into(), "poll".into())], original);
        assert_eq!(detail.by_offset[&2], original);
        assert_eq!(detail.transcript.entry_count(), original + 1);
        let screen = text(&mut detail.transcript);
        assert!(
            screen.contains("after") && !screen.contains("before"),
            "{screen}"
        );
    }

    {
        let mut detail = Detail::new("a1", 1, Transcript::new());
        let running = run("r1", "running");
        let start = page(
            vec![
                state(0, "r1", "running", None),
                event(1, "r1", tool("b", "bash", json!({"command":"work"}))),
            ],
            true,
        );
        detail.ingest(
            &start,
            std::slice::from_ref(&running),
            &BTreeSet::new(),
            10000,
        );
        let preview = event(
            2,
            "r1",
            AgentEvent::ToolOutputSnapshot {
                id: "b".into(),
                output: "old-preview".into(),
            },
        )
        .record;
        detail.output_preview(&preview);
        assert!(!text(&mut detail.transcript).contains("old-preview"));
        detail.ingest(
            &page(vec![], false),
            std::slice::from_ref(&running),
            &BTreeSet::new(),
            10000,
        );
        detail.output_preview(&preview);
        assert!(text(&mut detail.transcript).contains("old-preview"));
        let next = event(
            3,
            "r1",
            AgentEvent::ToolOutputSnapshot {
                id: "b".into(),
                output: "latest-preview".into(),
            },
        )
        .record;
        detail.output_preview(&next);
        let screen = text(&mut detail.transcript);
        assert!(screen.contains("latest-preview") && !screen.contains("old-preview"));
        detail.ingest(
            &page(
                vec![event(
                    4,
                    "r1",
                    AgentEvent::ToolFinished {
                        id: "b".into(),
                        result: json!({"ok":true,"output":"final-output"}),
                    },
                )],
                false,
            ),
            &[running],
            &BTreeSet::new(),
            10000,
        );
        detail.output_preview(&preview);
        let screen = text(&mut detail.transcript);
        assert!(screen.contains("final-output") && !screen.contains("old-preview"));
    }

    {
        const RUN: &str = "0123456789abcdef0123456789abcdef";
        let mut detail = Detail::new("a1", 1, Transcript::new());
        let running = run("r1", "running");
        detail.ingest(
            &page(
                vec![
                    state(0, "r1", "running", Some(1_000)),
                    event(1, "r1", tool("b", "bash", json!({"command":"work"}))),
                ],
                true,
            ),
            std::slice::from_ref(&running),
            &BTreeSet::new(),
            10_000,
        );
        let pipeline = |nonzero: u64, revision: u64| {
            event(
                2 + revision,
                "r1",
                AgentEvent::ToolPipelineSnapshot {
                    id: "b".into(),
                    preview: None,
                    pipes: json!({
                        "run_id": RUN,
                        "uri": format!("pipe://{RUN}"),
                        "pipelines": [{"id": 1, "stages": []}],
                        "streams": [],
                        "nonzero_stages": nonzero,
                        "incomplete": false,
                        "revision": revision,
                        "leak": "pipeline-secret",
                    }),
                },
            )
            .record
        };
        // The preview is dropped while the page is incomplete, so the card cannot change early.
        detail.pipeline_preview(&pipeline(1, 1));
        assert!(!text(&mut detail.transcript).contains("Pipes: /pipes"));
        detail.ingest(
            &page(vec![], false),
            std::slice::from_ref(&running),
            &BTreeSet::new(),
            10_000,
        );
        let main = event(
            5,
            "r1",
            AgentEvent::ToolOutputSnapshot {
                id: "b".into(),
                output: "main-preview".into(),
            },
        )
        .record;
        detail.output_preview(&main);
        detail.pipeline_preview(&pipeline(1, 1));
        let screen = text(&mut detail.transcript);
        // A stage snapshot only updates the bottom warning/entry; the main output preview remains the only body.
        assert!(screen.contains("main-preview"), "{screen}");
        assert!(screen.contains("Pipes: /pipes (F8)"), "{screen}");
        assert!(screen.contains("Non-zero stage results: 1"), "{screen}");
        assert!(!screen.contains("pipeline-secret"), "{screen}");
        let latest = event(
            6,
            "r1",
            AgentEvent::ToolOutputSnapshot {
                id: "b".into(),
                output: "latest-preview".into(),
            },
        )
        .record;
        detail.output_preview(&latest);
        detail.pipeline_preview(&pipeline(2, 2));
        let screen = text(&mut detail.transcript);
        assert!(
            screen.contains("latest-preview") && !screen.contains("main-preview"),
            "{screen}"
        );
        assert!(screen.contains("Non-zero stage results: 2"), "{screen}");
        // When the final result has no pipes, neither the transient entry nor the warning may linger.
        detail.ingest(
            &page(
                vec![event(
                    7,
                    "r1",
                    AgentEvent::ToolFinished {
                        id: "b".into(),
                        result: json!({"ok":true,"stdout":"final-output"}),
                    },
                )],
                false,
            ),
            std::slice::from_ref(&running),
            &BTreeSet::new(),
            10_000,
        );
        let screen = text(&mut detail.transcript);
        assert!(screen.contains("final-output"), "{screen}");
        assert!(!screen.contains("Pipes: /pipes"), "{screen}");
        assert!(!screen.contains("Non-zero stage results"), "{screen}");
    }
}

fn run(id: &str, status: &str) -> Run {
    Run {
        id: id.into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: status.into(),
        task: format!("task-{id}"),
        result: None,
        consumed: false,
        duration_ms: 12_000,
        started_ms: 0,
        requests: 0,
        tokens: 0,
        cost: None,
    }
}

fn event(offset: u64, run: &str, event: AgentEvent) -> ObservedEntry {
    ObservedEntry {
        start_offset: offset,
        record: ObservedRecord {
            root_id: None,
            agent_id: "a1".into(),
            run_id: Some(run.into()),
            generation: 1,
            sequence: None,
            timestamp_ms: None,
            event: Some(event),
            hub: None,
        },
    }
}

fn state(offset: u64, run: &str, status: &str, timestamp_ms: Option<u64>) -> ObservedEntry {
    let mut entry = event(offset, run, AgentEvent::AssistantStarted);
    entry.record.event = None;
    entry.record.timestamp_ms = timestamp_ms;
    entry.record.hub = Some(HubEvent::RunState {
        status: status.into(),
    });
    entry
}

fn page(entries: Vec<ObservedEntry>, has_more: bool) -> EventPage {
    EventPage {
        entries,
        cursor: None,
        reset: false,
        has_more,
        warnings: vec![],
    }
}

fn render(transcript: &mut Transcript, width: u16, height: u16) -> Buffer {
    let mut terminal = TestTerminal::new(TestBackend::new(width, height)).unwrap();
    transcript.set_terminal_rows(height);
    terminal
        .draw(|frame| transcript.draw(frame, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn text(transcript: &mut Transcript) -> String {
    let buffer = render(transcript, 100, 200);
    buffer
        .content()
        .chunks(100)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool(id: &str, name: &str, arguments: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolStarted {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    }
}

fn finished(id: &str, result: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolFinished {
        id: id.into(),
        result,
    }
}

fn fixture() -> Vec<AgentEvent> {
    let lines = (0..16)
        .map(|i| format!("source-{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        AgentEvent::ProjectInstructionsLoaded {
            path: "/project/AGENTS.md".into(),
            content: lines.clone(),
        },
        AgentEvent::AssistantStarted,
        AgentEvent::ThinkingDelta {
            text: "**Reasoning**\n```rust\nlet answer = 42;\n```".into(),
        },
        AgentEvent::AssistantDelta {
            text: "# Answer\n- **item**\n```rust\nfn main() {}\n```".into(),
            refusal: false,
        },
        AgentEvent::AssistantFinished {
            text: "# Answer\n- **item**\n```rust\nfn main() {}\n```".into(),
        },
        tool("read-1", "read", json!({"path":"a.rs"})),
        finished("read-1", json!({"ok":true,"content":lines})),
        tool("read-2", "read", json!({"path":"b.rs"})),
        finished("read-2", json!({"ok":true,"content":"another file"})),
        tool("skill", "read", json!({"path":"skill://example"})),
        finished(
            "skill",
            json!({"ok":true,"source":"skill://example","path":"/skills/example/SKILL.md","content":lines}),
        ),
        tool("bash", "bash", json!({"command":"printf hello"})),
        AgentEvent::ToolOutput {
            id: "bash".into(),
            stream: OutputStream::Stderr,
            text: "live diagnostic".into(),
        },
        finished(
            "bash",
            json!({"ok":true,"stdout":lines,"stderr":"diagnostic","exit_code":0}),
        ),
        tool("rg", "rg", json!({"pattern":"needle","path":"."})),
        finished("rg", json!({"ok":true,"stdout":"a.rs:1:needle"})),
        tool("glob", "glob", json!({"pattern":"*.rs"})),
        finished("glob", json!({"ok":true,"paths":["a.rs","b.rs"]})),
        tool(
            "edit",
            "edit",
            json!({"path":"a.rs","old":"before","new":"after"}),
        ),
        AgentEvent::FileChanged {
            id: "edit".into(),
            path: "a.rs".into(),
            diff: FileDiff::Text {
                created: false,
                deleted: false,
                added: 1,
                removed: 1,
                hunks: vec![DiffHunk {
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Removed,
                            old_line: Some(1),
                            new_line: None,
                            text: "before".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Added,
                            old_line: None,
                            new_line: Some(1),
                            text: "after".into(),
                        },
                    ],
                }],
            },
        },
        finished("edit", json!({"ok":true})),
        tool(
            "write",
            "write",
            json!({"path":"new.rs","content":"new file"}),
        ),
        AgentEvent::FileChanged {
            id: "write".into(),
            path: "new.rs".into(),
            diff: FileDiff::Text {
                created: true,
                deleted: false,
                added: 1,
                removed: 0,
                hunks: vec![DiffHunk {
                    lines: vec![DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        text: "new file".into(),
                    }],
                }],
            },
        },
        finished("write", json!({"ok":true})),
    ]
}

#[test]
fn detail_messages_match_main_transcript_cells_and_folding() {
    for preview in [false, true] {
        for width in [28, 100] {
            let mut main = Transcript::with_read_preview(preview);
            main.set_bash_timeout(37);
            let mut detail = Detail::new("a1", 1, main.fresh());
            let events = fixture();
            let mut records = vec![ReplayRecord::User("task-r1".into())];
            records.extend(events.iter().cloned().map(ReplayRecord::Event));
            assert!(main.replay(records).is_none());
            main.finish_run_with_timing("Completed", Some(Duration::from_secs(12)), None);
            let entries = events
                .into_iter()
                .enumerate()
                .map(|(i, e)| event(i as u64, "r1", e))
                .collect::<Vec<_>>();
            let chunks = entries.chunks(3).collect::<Vec<_>>();
            for (i, chunk) in chunks.iter().enumerate() {
                detail.ingest(
                    &page(chunk.to_vec(), i + 1 < chunks.len()),
                    &[run("r1", "completed")],
                    &BTreeSet::from(["r1".into()]),
                    10_000,
                );
            }
            for _ in 0..2 {
                main.scroll_up(usize::MAX);
                detail.transcript.scroll_up(usize::MAX);
                assert_eq!(
                    render(&mut main, width, 240),
                    render(&mut detail.transcript, width, 240),
                    "preview={preview}, width={width}"
                );
                main.toggle_tool_output();
                detail.transcript.toggle_tool_output();
            }
            let screen = text(&mut detail.transcript);
            assert!(screen.contains("Timeout: 37s"));
            assert!(!screen.contains("Wall:"));
        }
    }
}

#[test]
fn detail_lifecycle_pages_queue_and_late_metadata() {
    let mut detail = Detail::new("a1", 1, Transcript::new());
    let runs = [run("r1", "completed"), run("r2", "cancelled")];
    let settled = BTreeSet::from(["r1".into(), "r2".into()]);
    detail.ingest(
        &page(
            vec![
                state(0, "r1", "running", Some(1_000)),
                event(1, "r1", AgentEvent::AssistantStarted),
                event(
                    2,
                    "r1",
                    AgentEvent::AssistantDelta {
                        text: "first".into(),
                        refusal: false,
                    },
                ),
            ],
            true,
        ),
        &runs,
        &settled,
        10_000,
    );
    assert!(detail.terminal.is_empty());
    detail.ingest(
        &page(
            vec![
                event(
                    3,
                    "r1",
                    AgentEvent::AssistantDelta {
                        text: " second".into(),
                        refusal: false,
                    },
                ),
                event(
                    4,
                    "r1",
                    AgentEvent::AssistantFinished {
                        text: "first second".into(),
                    },
                ),
            ],
            false,
        ),
        &runs,
        &settled,
        10_000,
    );
    assert_eq!(detail.terminal.len(), 1);
    let screen = text(&mut detail.transcript);
    assert_eq!(screen.matches("first second").count(), 1);
    assert_eq!(screen.matches("task-r1").count(), 1);
    assert!(screen.contains("Completed · —"));
    assert!(!screen.contains("task-r2") && !screen.contains("Cancelled"));

    let mut queued = Detail::new("a1", 1, Transcript::new());
    queued.ingest(
        &page(vec![state(0, "r1", "queued", None)], false),
        &[run("r1", "queued")],
        &BTreeSet::new(),
        10_000,
    );
    queued.ingest(
        &page(
            vec![
                state(1, "r1", "running", Some(1_000)),
                event(
                    2,
                    "r1",
                    AgentEvent::AssistantDelta {
                        text: "live".into(),
                        refusal: false,
                    },
                ),
            ],
            false,
        ),
        &[run("r1", "completed")],
        &BTreeSet::new(),
        10_000,
    );
    assert!(queued.terminal.is_empty());
    queued.ingest(
        &page(
            vec![
                event(
                    3,
                    "r1",
                    AgentEvent::AssistantFinished {
                        text: "live".into(),
                    },
                ),
                state(4, "r1", "completed", Some(u64::MAX)),
            ],
            false,
        ),
        &[run("r1", "completed")],
        &BTreeSet::new(),
        10_000,
    );
    assert!(text(&mut queued.transcript).contains("Completed · —"));

    let mut late = Detail::new("a1", 1, Transcript::new());
    late.ingest(
        &page(
            vec![
                event(0, "r1", AgentEvent::AssistantStarted),
                event(
                    1,
                    "r1",
                    AgentEvent::AssistantDelta {
                        text: "partial answer".into(),
                        refusal: false,
                    },
                ),
                state(2, "r1", "failed", Some(9_000)),
            ],
            false,
        ),
        &[run("r1", "running")],
        &BTreeSet::new(),
        10_000,
    );
    assert!(text(&mut late.transcript).contains("Failed ·"));
    assert_eq!(late.transcript.task_duration(), None);
    let mut failed = run("r1", "failed");
    failed.result = Some(json!({"error":"failure explanation"}));
    late.ingest(
        &page(vec![], false),
        &[failed],
        &BTreeSet::from(["r1".into()]),
        10_000,
    );
    let screen = text(&mut late.transcript);
    assert!(screen.contains("partial answer") && screen.contains("Failed ·"));
    assert_eq!(late.transcript.task_duration().as_deref(), Some("12s"));
    assert!(!screen.contains("failure explanation"));
    assert_eq!(late.notice.as_deref(), Some("failure explanation"));
}

#[test]
fn detail_unknown_timing_and_cancel_keep_partial_output_without_duplicates() {
    for timestamp in [None, Some(1_000)] {
        let mut detail = Detail::new("a1", 1, Transcript::new());
        let mut started = event(1, "r1", tool("b", "bash", json!({"command":"work"})));
        started.record.timestamp_ms = timestamp;
        let first = page(
            vec![
                state(0, "r1", "running", timestamp),
                started,
                event(
                    2,
                    "r1",
                    AgentEvent::ToolOutput {
                        id: "b".into(),
                        stream: OutputStream::Stdout,
                        text: "partial tool output".into(),
                    },
                ),
            ],
            false,
        );
        detail.ingest(&first, &[run("r1", "running")], &BTreeSet::new(), 10_000);
        let screen = text(&mut detail.transcript);
        if timestamp.is_some() {
            assert!(screen.contains("Elapsed: 9s"), "{screen}");
        } else {
            detail.tick(Instant::now() + Duration::from_secs(90));
            let screen = text(&mut detail.transcript);
            assert!(!screen.contains("Elapsed:"));
        }
        let cancelled = page(vec![state(3, "r1", "cancelled", None)], false);
        detail.ingest(
            &cancelled,
            &[run("r1", "cancelled")],
            &BTreeSet::new(),
            10_000,
        );
        detail.ingest(
            &cancelled,
            &[run("r1", "cancelled")],
            &BTreeSet::new(),
            10_000,
        );
        let screen = text(&mut detail.transcript);
        assert!(screen.contains("partial tool output") && screen.contains("Cancelled · —"));
        assert!(!screen.contains("Wall:") && !screen.contains("Elapsed:"));
        assert_eq!(detail.transcript.entry_count(), 2);
    }
}

#[test]
fn detail_run_scoped_anchors_wait_for_the_target_page_and_inputs_are_unique() {
    let mut detail = Detail::new("a1", 1, Transcript::new());
    let runs = [run("r1", "completed"), run("r2", "running")];
    detail.ingest(
        &page(
            vec![
                event(0, "r1", tool("same-id", "read", json!({"path":"first.rs"}))),
                event(
                    1,
                    "r1",
                    finished("same-id", json!({"ok":true,"content":"first"})),
                ),
            ],
            true,
        ),
        &runs,
        &BTreeSet::from(["r1".into()]),
        10_000,
    );
    detail.anchor(Anchor {
        run: Some("r2".into()),
        offset: 4,
        tool: Some("same-id".into()),
    });
    assert!(detail.pending.is_some());
    detail.ingest(
        &page(
            vec![
                event(
                    2,
                    "r2",
                    AgentEvent::UserInputAccepted {
                        id: 1,
                        text: "steering".into(),
                    },
                ),
                event(
                    3,
                    "r2",
                    AgentEvent::UserInputAccepted {
                        id: 1,
                        text: "steering".into(),
                    },
                ),
                event(
                    4,
                    "r2",
                    tool("same-id", "read", json!({"path":"second.rs"})),
                ),
            ],
            false,
        ),
        &runs,
        &BTreeSet::from(["r1".into()]),
        10_000,
    );
    let first = detail.by_tool[&("r1".into(), "same-id".into())];
    let second = detail.by_tool[&("r2".into(), "same-id".into())];
    assert!(first < second);
    assert_eq!(detail.by_run["r2"], 2);
    assert_eq!(text(&mut detail.transcript).matches("steering").count(), 1);
}

#[test]
fn detail_rebuild_and_editor_input_config() {
    let mut main = Transcript::with_read_preview(true);
    main.set_bash_timeout(37);
    let mut detail = Detail::new("a1", 1, main.fresh());
    detail.focus = true;
    detail.ingest(
        &page(
            vec![event(
                100,
                "r1",
                tool("b", "bash", json!({"command":"before"})),
            )],
            false,
        ),
        &[run("r1", "running")],
        &BTreeSet::new(),
        10_000,
    );
    detail.transcript.toggle_tool_output();
    assert!(!main.tools_expanded());
    detail.rebuild();
    assert!(detail.focus && detail.by_tool.is_empty() && detail.last_offset.is_none());
    assert!(!detail.transcript.tools_expanded());
    detail.ingest(
        &page(
            vec![event(
                0,
                "r1",
                tool("b", "bash", json!({"command":"after"})),
            )],
            false,
        ),
        &[run("r1", "running")],
        &BTreeSet::new(),
        10_000,
    );
    let screen = text(&mut detail.transcript);
    assert!(
        screen.contains("Timeout: 37s") && screen.contains("after") && !screen.contains("before")
    );

    let mut drafts = Drafts::default();
    drafts.draft("a1").editor.insert_str("first");
    for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT | KeyModifiers::SHIFT] {
        let before = drafts.draft("a1").editor.lines().len();
        let result = detail.key(KeyEvent::new(KeyCode::Enter, modifiers), &mut drafts);
        assert!(
            result.is_none(),
            "Shift + Enter must not submit: {modifiers:?}"
        );
        assert_eq!(
            drafts.draft("a1").editor.lines().len(),
            before + 1,
            "Shift + Enter only inserts a newline: {modifiers:?}"
        );
    }
    let draft = drafts.draft("a1");
    assert_eq!(draft.editor.lines().join("\n"), "first\n\n");
    assert!(draft.revision > 0);
    let submitted = detail.key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut drafts,
    );
    assert_eq!(submitted.as_deref(), Some("first\n\n"));
    let result = detail.key(
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        &mut drafts,
    );
    assert!(result.is_none());
    assert_eq!(drafts.draft("a1").editor.lines().join("\n"), "first\n\n\n");

    let child = editor();
    let main_ed = crate::tui::editor();
    assert_eq!(child.style(), main_ed.style());
    assert_eq!(child.wrap_mode(), main_ed.wrap_mode());
}
