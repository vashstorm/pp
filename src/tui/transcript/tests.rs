use super::*;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use serde_json::json;

fn native_rows(rows: &[Vec<ratatui::buffer::Cell>]) -> String {
    rows.iter()
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn retire_native(transcript: &mut Transcript, width: u16, height: u16, exiting: bool) -> String {
    let mut text = String::new();
    while let Some(batch) = transcript.next_native_batch(width, height, exiting) {
        assert!(batch.rows.len() <= 64);
        text.push_str(&native_rows(&batch.rows));
        text.push('\n');
        transcript.acknowledge_native_batch(batch);
    }
    text
}

fn entry_text(entry: &mut TranscriptEntry, width: u16) -> String {
    entry.ensure_layout(width);
    entry
        .layout
        .as_ref()
        .unwrap()
        .lines
        .iter()
        .map(|line| entry.line(line).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render(transcript: &mut Transcript, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| transcript.draw(frame, frame.area()))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn screen(buffer: &Buffer) -> String {
    (buffer.area.y..buffer.area.bottom())
        .map(|y| {
            (buffer.area.x..buffer.area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn delta(transcript: &mut Transcript, text: &str, refusal: bool) {
    transcript.progress(AgentEvent::AssistantDelta {
        text: text.into(),
        refusal,
    });
}

fn tree_render(
    transcript: &mut Transcript,
    blocks: &[TreeBlock],
    visible: &[usize],
    selected: Option<usize>,
    reveal: bool,
    width: u16,
    height: u16,
) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| transcript.draw_tree(frame, frame.area(), blocks, visible, selected, reveal))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn answer(text: &str) -> Vec<ReplayRecord> {
    vec![
        ReplayRecord::Event(AgentEvent::AssistantStarted),
        ReplayRecord::Event(AgentEvent::AssistantFinished { text: text.into() }),
    ]
}

fn tool(transcript: &mut Transcript, id: &str, name: &str, arguments: Value) {
    transcript.progress(AgentEvent::ToolStarted {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    });
}

fn todo_phases() -> Vec<crate::tools::todo::TodoPhase> {
    use crate::tools::todo::{TodoItem, TodoPhase, TodoStatus};
    vec![
        TodoPhase {
            name: "Write".into(),
            tasks: vec![
                TodoItem {
                    content: "Alpha".into(),
                    status: TodoStatus::Completed,
                    blocker: None,
                },
                TodoItem {
                    content: "Beta".into(),
                    status: TodoStatus::InProgress,
                    blocker: None,
                },
            ],
        },
        TodoPhase {
            name: "Verify".into(),
            tasks: vec![
                TodoItem {
                    content: "Check".into(),
                    status: TodoStatus::Pending,
                    blocker: None,
                },
                TodoItem {
                    content: "Check".into(),
                    status: TodoStatus::Blocked,
                    blocker: Some("wait up".into()),
                },
                TodoItem {
                    content: "Skipped".into(),
                    status: TodoStatus::Abandoned,
                    blocker: None,
                },
            ],
        },
    ]
}

fn todo_snapshot_event(
    revision: u64,
    phases: Vec<crate::tools::todo::TodoPhase>,
    status: crate::plan::PlanStatus,
) -> AgentEvent {
    AgentEvent::TodoSnapshot {
        plan_id: "plan-1".into(),
        revision,
        phases,
        status,
    }
}

mod diff_cases;
mod entry_cases;
mod instruction_cases;
mod layout_cases;
mod native_cases;
mod scroll_cases;
mod streaming_cases;
mod todo_cases;
mod tree_cases;
