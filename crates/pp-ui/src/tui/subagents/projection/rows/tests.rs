//! Behavioural cases for rows.rs: Activity row generation, legacy synthesis idempotence and the marker short circuit.

use super::super::fixtures::*;
use super::*;
use pp_core::events::AgentEvent;
use pp_core::events::OutputStream;
use pp_runtime::subagent::HubEvent;

#[test]
fn response_tool_and_row_id_invariants_across_ingest() {
    {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "running", "work")],
            ),
            1,
        );
        let entries = vec![
            entry(0, Some(10), "r1", AgentEvent::AssistantStarted),
            entry(60, Some(11), "r1", delta("hello")),
            entry(120, Some(12), "r1", delta(" world")),
            entry(
                180,
                Some(13),
                "r1",
                AgentEvent::AssistantFinished {
                    text: "hello world".into(),
                },
            ),
            entry(
                240,
                Some(14),
                "r1",
                AgentEvent::ToolStarted {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                },
            ),
            entry(
                300,
                Some(15),
                "r1",
                AgentEvent::ToolFinished {
                    id: "c1".into(),
                    result: serde_json::json!({"ok":false,"error":"missing file"}),
                },
            ),
            entry(
                360,
                Some(16),
                "r1",
                AgentEvent::ToolOutput {
                    id: "c1".into(),
                    stream: OutputStream::Stdout,
                    text: "ignored".into(),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 1_000);
        let rows = projection.rows();
        assert_eq!(
            rows.iter().filter(|row| row.kind == Kind::Response).count(),
            1,
            "one assistant turn produces exactly one row"
        );
        let response = rows.iter().find(|row| row.kind == Kind::Response).unwrap();
        assert_eq!(response.summary, "hello world");
        assert_eq!(response.status, RowStatus::Success);
        let tools = rows
            .iter()
            .filter(|row| row.kind == Kind::Tool)
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1, "ToolOutput produces no new Activity row");
        assert_eq!(tools[0].status, RowStatus::Error);
        assert_eq!(tools[0].summary, "missing file");
        assert_eq!(projection.recent("a1", 1)[0].summary, "missing file");
        assert_eq!(projection.agents()[0].tools, 1);
        // A new Run reusing the same call id must produce a separate row.
        let more = vec![entry(
            420,
            Some(20),
            "r2",
            AgentEvent::ToolStarted {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        )];
        projection.ingest("a1", &page(more, false), 2_000);
        assert_eq!(
            projection
                .rows()
                .iter()
                .filter(|row| row.kind == Kind::Tool)
                .count(),
            2
        );
    }

    {
        let mut projection = Projection::default();
        projection.apply_snapshot(snapshot(vec![record("a1", "Main", "idle")], vec![]), 1);
        projection.ingest(
            "a1",
            &page(
                vec![entry(0, Some(10), "r1", AgentEvent::AssistantStarted)],
                false,
            ),
            10,
        );
        let id = projection.rows()[0].id();
        projection.ingest(
            "a1",
            &page(vec![entry(30, Some(11), "r1", delta("more"))], false),
            20,
        );
        assert_eq!(projection.rows().len(), 1);
        assert_eq!(projection.rows()[0].id(), id);
    }
}

#[test]
fn legacy_synthesis_idempotency_and_late_offsets() {
    {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "failed", "legacy work")],
            ),
            1,
        );
        // Legacy log: events only, no timestamp, no RunState.
        let entries = vec![
            entry(0, None, "r1", AgentEvent::AssistantStarted),
            entry(
                90,
                None,
                "r1",
                AgentEvent::AssistantFinished {
                    text: "legacy answer".into(),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 10);
        projection.ingest("a1", &page(Vec::new(), false), 10);
        let rows = projection.rows();
        assert_eq!(
            rows.iter().filter(|row| row.title == "Task").count(),
            1,
            "a legacy log synthesizes the task association only once"
        );
        assert_eq!(
            rows.iter().filter(|row| row.title == "Run").count(),
            1,
            "a legacy log synthesizes the terminal state only once"
        );
        let task = rows.iter().position(|row| row.title == "Task").unwrap();
        let first_event = rows
            .iter()
            .position(|row| row.kind == Kind::Response)
            .unwrap();
        assert!(
            task < first_event,
            "the task association sorts before the first event of the Run"
        );
        let terminal = rows.iter().find(|row| row.title == "Run").unwrap();
        assert_eq!(terminal.status, RowStatus::Error);
        assert_eq!(
            terminal.timestamp, None,
            "a synthesized row carries no forged time"
        );
        let without = rows.iter().filter(|row| row.timestamp.is_none()).count();
        let with = rows.iter().filter(|row| row.timestamp.is_some()).count();
        assert!(with <= 1 && without >= 3);
        // Rows without a time sort before rows with one.
        let first_ts = rows.iter().position(|row| row.timestamp.is_some());
        if let Some(first_ts) = first_ts {
            assert!(rows[..first_ts].iter().all(|row| row.timestamp.is_none()));
        }
    }

    {
        let legacy = || {
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "failed", "legacy work")],
            )
        };
        let mut projection = Projection::default();
        projection.apply_snapshot(legacy(), 1);
        projection.ingest(
            "a1",
            &page(
                vec![
                    entry(0, None, "r1", AgentEvent::AssistantStarted),
                    entry(
                        60,
                        None,
                        "r1",
                        AgentEvent::ToolStarted {
                            id: "c1".into(),
                            name: "read".into(),
                            arguments: "{}".into(),
                        },
                    ),
                    // A delta updates the Response row in place and does not advance its offset.
                    entry(120, None, "r1", delta("legacy body")),
                ],
                false,
            ),
            10,
        );
        let baseline = projection
            .rows()
            .iter()
            .map(|row| (row.id(), row.offset))
            .collect::<Vec<_>>();
        let task = projection
            .rows()
            .iter()
            .find(|row| row.title == "Task")
            .expect("a Task row must be synthesized after the first scan");
        let terminal = projection
            .rows()
            .iter()
            .find(|row| row.title == "Run")
            .expect("a terminal Run row must be synthesized after the first scan");
        assert_eq!(
            task.offset, 0,
            "the Task row is inserted at min(offset) of the existing rows"
        );
        assert_eq!(
            terminal.offset, 61,
            "the terminal row is inserted at max(offset) + 1 of the existing rows and the synthesized row never advances the bounds"
        );
        // The same legacy snapshot published three times in a row (heartbeat): no new rows, no re-sort, no offset change.
        for now_ms in 2..=4u64 {
            projection.apply_snapshot(legacy(), now_ms);
            let rows = projection.rows();
            assert_eq!(
                rows.iter()
                    .map(|row| (row.id(), row.offset))
                    .collect::<Vec<_>>(),
                baseline,
                "a repeated snapshot must not add, re-sort or push any row"
            );
            assert_eq!(
                rows.iter().filter(|row| row.title == "Task").count(),
                1,
                "a repeated snapshot keeps a single Task row"
            );
            assert_eq!(
                rows.iter().filter(|row| row.title == "Run").count(),
                1,
                "a repeated snapshot keeps a single terminal Run row"
            );
        }
    }

    {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "completed", "legacy work")],
            ),
            1,
        );
        projection.ingest(
            "a1",
            &page(
                vec![
                    entry(0, None, "r1", AgentEvent::AssistantStarted),
                    entry(
                        60,
                        None,
                        "r1",
                        AgentEvent::ToolStarted {
                            id: "c1".into(),
                            name: "read".into(),
                            arguments: "{}".into(),
                        },
                    ),
                ],
                false,
            ),
            10,
        );
        let frozen = projection
            .rows()
            .iter()
            .find(|row| row.title == "Run")
            .expect("a Run row must exist after the terminal state is synthesized")
            .id();
        assert!(
            frozen.ends_with("|61"),
            "the terminal row lands at max(offset)+1: {frozen}"
        );
        // A late legacy row with a larger offset arrives after synthesis; the frozen terminal row does not move back.
        projection.ingest(
            "a1",
            &page(
                vec![entry(
                    900,
                    None,
                    "r1",
                    AgentEvent::ToolStarted {
                        id: "c9".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                )],
                false,
            ),
            20,
        );
        let rows = projection.rows();
        let terminal = rows
            .iter()
            .find(|row| row.title == "Run")
            .expect("a late row must not remove the terminal row");
        assert_eq!(
            terminal.id(),
            frozen,
            "a late row does not move the frozen terminal row (the key contains offset, so equality means the offset did not change)"
        );
        let offsets = rows.iter().map(|row| row.offset).collect::<Vec<_>>();
        let terminal_at = offsets
            .iter()
            .position(|offset| *offset == 61)
            .expect("the terminal row offset is still 61");
        let late_at = offsets
            .iter()
            .position(|offset| *offset == 900)
            .expect("the late row must be ingested");
        assert!(
            terminal_at < late_at,
            "the frozen terminal row still sorts before the late row and is not pushed to the tail by a larger offset"
        );
    }
}

#[test]
fn run_state_markers_prevent_legacy_task_and_terminal_rows() {
    let mut projection = Projection::default();
    projection.apply_snapshot(
        snapshot(
            vec![record("a1", "Main", "idle")],
            vec![run("r1", "a1", "completed", "new work")],
        ),
        1,
    );
    // New log: the run carries a hub.run_state marker and also has real event rows.
    let mut marker = entry(0, Some(10), "r1", AgentEvent::AssistantStarted);
    marker.record.event = None;
    marker.record.hub = Some(HubEvent::RunState {
        status: "completed".into(),
    });
    let entries = vec![
        marker,
        entry(40, Some(11), "r1", AgentEvent::AssistantStarted),
        entry(80, Some(12), "r1", delta("live body")),
    ];
    projection.ingest("a1", &page(entries, false), 10);
    let rows = projection.rows();
    assert_eq!(
        rows.iter().filter(|row| row.title == "Task").count(),
        0,
        "a run with a marker synthesizes no Task row even when it has rows"
    );
    let run_rows = rows
        .iter()
        .filter(|row| row.title == "Run")
        .collect::<Vec<_>>();
    assert_eq!(
        run_rows.len(),
        1,
        "only the real Run row produced by the marker is kept"
    );
    assert_eq!(
        run_rows[0].status,
        RowStatus::Success,
        "the real terminal state of the marker takes effect directly"
    );
    assert_eq!(
        run_rows[0].timestamp,
        Some(10),
        "the real Run row comes from the marker, not from a synthesized row without a timestamp"
    );
    assert!(
        rows.iter().any(|row| row.kind == Kind::Response),
        "real event rows are still projected normally"
    );
}
