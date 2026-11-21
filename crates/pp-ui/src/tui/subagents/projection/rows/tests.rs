//! rows.rs 的行为用例：Activity 行生成、legacy 合成幂等与 marker 短路。

use super::super::fixtures::*;
use super::*;
use pp_core::events::AgentEvent;
use pp_core::events::OutputStream;
use pp_runtime::subagent::HubEvent;

#[test]
fn response_and_tool_update_in_place_across_runs() {
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
        "一个 assistant turn 只产生一行"
    );
    let response = rows.iter().find(|row| row.kind == Kind::Response).unwrap();
    assert_eq!(response.summary, "hello world");
    assert_eq!(response.status, RowStatus::Success);
    let tools = rows
        .iter()
        .filter(|row| row.kind == Kind::Tool)
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 1, "ToolOutput 不产生新活动行");
    assert_eq!(tools[0].status, RowStatus::Error);
    assert_eq!(tools[0].summary, "missing file");
    assert_eq!(projection.recent("a1", 1)[0].summary, "missing file");
    assert_eq!(projection.agents()[0].tools, 1);
    // 新 Run 复用相同 call id 必须生成独立行。
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

#[test]
fn legacy_rows_keep_file_order_and_synthesize_once() {
    let mut projection = Projection::default();
    projection.apply_snapshot(
        snapshot(
            vec![record("a1", "Main", "idle")],
            vec![run("r1", "a1", "failed", "legacy work")],
        ),
        1,
    );
    // 旧日志：只有事件、无 timestamp、无 RunState。
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
        "旧日志只合成一次 task 关联"
    );
    assert_eq!(
        rows.iter().filter(|row| row.title == "Run").count(),
        1,
        "旧日志只合成一次终态"
    );
    let task = rows.iter().position(|row| row.title == "Task").unwrap();
    let first_event = rows
        .iter()
        .position(|row| row.kind == Kind::Response)
        .unwrap();
    assert!(task < first_event, "task 关联排在 Run 第一条事件之前");
    let terminal = rows.iter().find(|row| row.title == "Run").unwrap();
    assert_eq!(terminal.status, RowStatus::Error);
    assert_eq!(terminal.timestamp, None, "合成行不带伪造时间");
    let without = rows.iter().filter(|row| row.timestamp.is_none()).count();
    let with = rows.iter().filter(|row| row.timestamp.is_some()).count();
    assert!(with <= 1 && without >= 3);
    // 无时间记录排在有时间记录之前。
    let first_ts = rows.iter().position(|row| row.timestamp.is_some());
    if let Some(first_ts) = first_ts {
        assert!(rows[..first_ts].iter().all(|row| row.timestamp.is_none()));
    }
}

#[test]
fn activity_row_ids_are_stable_across_ingest() {
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

#[test]
fn legacy_synthesis_is_idempotent_across_repeated_snapshots() {
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
                // delta 原位更新 Response 行，不推进其 offset。
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
        .expect("首次扫描后应合成 Task 行");
    let terminal = projection
        .rows()
        .iter()
        .find(|row| row.title == "Run")
        .expect("首次扫描后应合成终态 Run 行");
    assert_eq!(task.offset, 0, "Task 行插在现存 rows 的 min(offset)");
    assert_eq!(
        terminal.offset, 61,
        "终态行插在现存 rows 的 max(offset) + 1，合成行自身不推进 bounds"
    );
    // 同一 legacy snapshot 连续发布 3 次（心跳）：不新增行、不重排、不改 offset。
    for now_ms in 2..=4u64 {
        projection.apply_snapshot(legacy(), now_ms);
        let rows = projection.rows();
        assert_eq!(
            rows.iter()
                .map(|row| (row.id(), row.offset))
                .collect::<Vec<_>>(),
            baseline,
            "重复快照不得新增、重排或推动任何行"
        );
        assert_eq!(
            rows.iter().filter(|row| row.title == "Task").count(),
            1,
            "重复快照只保留一条 Task 行"
        );
        assert_eq!(
            rows.iter().filter(|row| row.title == "Run").count(),
            1,
            "重复快照只保留一条终态 Run 行"
        );
    }
}

#[test]
fn legacy_offset_ignores_late_rows_after_terminal_synthesis() {
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
        .expect("终态合成后应存在 Run 行")
        .id();
    assert!(
        frozen.ends_with("|61"),
        "终态行落在 max(offset)+1：{frozen}"
    );
    // 合成后迟到一条更大 offset 的 legacy 行，已冻结的终态行不后移。
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
        .expect("迟到行不得移除终态行");
    assert_eq!(
        terminal.id(),
        frozen,
        "迟到行不移动已冻结的终态行（key 含 offset，相等即 offset 未变）"
    );
    let offsets = rows.iter().map(|row| row.offset).collect::<Vec<_>>();
    let terminal_at = offsets
        .iter()
        .position(|offset| *offset == 61)
        .expect("终态行 offset 仍为 61");
    let late_at = offsets
        .iter()
        .position(|offset| *offset == 900)
        .expect("迟到行应被摄入");
    assert!(
        terminal_at < late_at,
        "冻结终态行仍排在迟到行之前，不被更大 offset 推到尾部"
    );
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
    // 新日志：run 携带 hub.run_state marker，同时也有真实事件行。
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
        "带 marker 的 run 即使有 rows 也不合成 Task 行"
    );
    let run_rows = rows
        .iter()
        .filter(|row| row.title == "Run")
        .collect::<Vec<_>>();
    assert_eq!(run_rows.len(), 1, "只保留 marker 生成的真实 Run 行");
    assert_eq!(
        run_rows[0].status,
        RowStatus::Success,
        "marker 的真实终态直接生效"
    );
    assert_eq!(
        run_rows[0].timestamp,
        Some(10),
        "真实 Run 行来自 marker，不是无时间戳的合成行"
    );
    assert!(
        rows.iter().any(|row| row.kind == Kind::Response),
        "真实事件行仍被正常投影"
    );
}
