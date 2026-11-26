use super::*;
use crate::tui::themes;
use pp_config::config::{IconMode, Theme, ToolStyle};

#[test]
fn task_card_shows_dispatch_and_individual_outcomes_in_each_style() {
    for style in ToolStyle::ALL {
        themes::set_active_appearance(Theme::Default, IconMode::Ascii, style);
        let mut transcript = Transcript::new();
        tool(
            &mut transcript,
            "task1",
            "task",
            json!({
                "context": "Use workspace fixtures",
                "tasks": [
                    {"name": "查找入口", "role": "fast", "task": "Find the entry point"},
                    {"name": "Verifier", "role": "review", "task": "Check the output"},
                    {"role": "slow", "task": "Inspect the workspace"}
                ]
            }),
        );
        let pending = screen(&render(&mut transcript, 64, 18));
        for label in ["Dispatching", "3 agents", "查 找 入 口", "Preparing"] {
            assert!(pending.contains(label), "{style:?}\n{pending}");
        }
        transcript.progress(AgentEvent::ToolFinished {
            id: "task1".into(),
            result: json!({
                "ok": true,
                "tasks": [
                    {"agent_id": "a1", "run_id": "r1", "status": "queued"},
                    {
                        "agent_id": "a2", "run_id": "r2", "status": "queued", "blocking": true,
                        "results": {"results": [{"run": {"id": "r2", "status": "completed", "result": {"content":"Verified"}}}]}
                    },
                    {"agent_id": "a3", "run_id": "r3", "status": "failed", "error": "Missing workspace"}
                ]
            }),
        });
        for width in [24, 64] {
            let display = screen(&render(&mut transcript, width, 38));
            let normalized = display.split_whitespace().collect::<String>();
            for label in ["Task", "Partial", "Queued", "Completed", "Failed"] {
                assert!(normalized.contains(label), "{style:?} {width}\n{display}");
            }
            assert!(display.contains("Missing"), "{style:?} {width}\n{display}");
            assert!(!display.contains("agent_id") && !display.contains("Verified"));
        }
        transcript.toggle_tool_output();
        let expanded = screen(&render(&mut transcript, 64, 30));
        assert!(
            expanded.contains("Verified") && expanded.contains("Run r2"),
            "{expanded}"
        );
    }
    themes::set_active_appearance(Theme::Default, IconMode::Nerd, ToolStyle::Classic);
}

#[test]
fn task_card_expands_the_batch_and_replays_the_same_summary() {
    let tasks = (1..=6)
        .map(|i| {
            json!({
                "name": format!("review-{i}"),
                "role": "review",
                "task": format!("First instruction {i}\nSecond instruction {i}"),
            })
        })
        .collect::<Vec<_>>();
    let results = (1..=6)
        .map(|i| json!({"run_id": format!("r{i}"), "status": "queued", "output_uri": format!("agent://a{i}")}))
        .collect::<Vec<_>>();
    let events = [
        AgentEvent::ToolStarted {
            id: "batch".into(),
            name: "task".into(),
            arguments: json!({"context": "Shared fixture context", "tasks": tasks}).to_string(),
        },
        AgentEvent::ToolFinished {
            id: "batch".into(),
            result: json!({"ok": true, "tasks": results, "wait_args": {"run_ids":["r1","r2","r3","r4","r5","r6"]}}),
        },
    ];
    let mut live = Transcript::new();
    for event in &events {
        live.progress(event.clone());
    }
    let mut replay = Transcript::new();
    replay.replay(events.into_iter().map(ReplayRecord::Event).collect());
    let preview = screen(&render(&mut live, 76, 35));
    assert!(
        preview.contains("Dispatched") && preview.contains("2 more agents"),
        "{preview}"
    );
    assert!(!preview.contains("review-5") && !preview.contains("Shared fixture context"));
    assert_eq!(preview, screen(&render(&mut replay, 76, 35)));

    replay.toggle_tool_output();
    let expanded = screen(&render(&mut replay, 76, 35));
    for label in [
        "review-6",
        "Second instruction 6",
        "Run r6",
        "agent://a6",
        "Shared fixture context",
    ] {
        assert!(expanded.contains(label), "{expanded}");
    }
    assert!(!expanded.contains("2 more agents"));
}
