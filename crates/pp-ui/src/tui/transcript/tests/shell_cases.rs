use super::*;
use pp_core::shell_display::{
    ShellPresentation, ShellRow, ShellSegment, ShellState, ShellStyle, VERSION,
};

fn segment(text: &str, style: ShellStyle, node: u32, state: ShellState) -> ShellSegment {
    ShellSegment {
        text: text.into(),
        style,
        node: Some(node),
        state,
        language: None,
    }
}

fn presentation(running: bool) -> ShellPresentation {
    let mut rows = Vec::new();
    for (index, (depth, prefix, command, state, meta)) in [
        (0, "", "ls -la", ShellState::Ok, "38ms"),
        (1, "and ", "print(\"---\")", ShellState::Ok, ""),
        (
            1,
            "and ",
            "git log --oneline -5",
            if running {
                ShellState::Running
            } else {
                ShellState::Fail
            },
            if running { "" } else { "exit 2 · 9ms" },
        ),
    ]
    .into_iter()
    .enumerate()
    {
        rows.push(ShellRow {
            depth,
            node: Some(index as u32),
            state,
            block: false,
            meta: meta.into(),
            effects: String::new(),
            segments: vec![
                segment(prefix, ShellStyle::Operator, index as u32, state),
                segment(command, ShellStyle::Command, index as u32, state),
            ],
        });
    }
    ShellPresentation {
        version: VERSION,
        revision: if running { 1 } else { 2 },
        running,
        rows,
    }
}

#[test]
fn shell_display_tui_live_final_and_replay_share_command_layout() {
    let mut transcript = Transcript::new();
    let start = AgentEvent::ToolStarted {
        id: "shell".into(),
        name: "bash".into(),
        arguments: json!({"command":"ls -la && echo '---' && git log --oneline -5"}).to_string(),
    };
    transcript.progress(start.clone());
    transcript.progress(AgentEvent::ToolShellSnapshot {
        id: "shell".into(),
        snapshot: presentation(true),
    });
    let buffer = render(&mut transcript, 100, 30);
    let live = screen(&buffer);
    assert!(live.contains("and print(\"---\")"), "{live}");
    assert!(live.contains("▶   and git log"), "{live}");
    let end = AgentEvent::ToolFinished {
        id: "shell".into(),
        result: json!({"ok":false,"exit_code":2,"output":"output","shell":presentation(false)}),
    };
    transcript.progress(end.clone());
    let final_snapshot = transcript.entries[0].shell.clone().unwrap();
    let mut restored = Transcript::new();
    restored.replay(vec![ReplayRecord::Event(start), ReplayRecord::Event(end)]);
    assert_eq!(restored.entries[0].shell, Some(final_snapshot));
    let buffer = render(&mut restored, 100, 30);
    let restored = screen(&buffer);
    assert!(restored.contains("exit 2 · 9ms"), "{restored}");
    assert_eq!(
        restored.matches('×').count(),
        1,
        "一个执行节点只能标记一次失败：{restored}"
    );
    let position = restored
        .lines()
        .position(|line| line.contains('×'))
        .unwrap();
    let column = restored
        .lines()
        .nth(position)
        .unwrap()
        .chars()
        .position(|c| c == '×')
        .unwrap();
    assert_eq!(
        buffer[(column as u16, position as u16)].fg,
        palette::REMOVED
    );
}

#[test]
fn shell_display_tui_wrapping_unicode_and_control_sequences_are_safe() {
    let mut snapshot = presentation(false);
    snapshot.rows.truncate(1);
    snapshot.rows[0].depth = 2;
    snapshot.rows[0].segments = vec![segment(
        "路径/中文/e\u{301}/really-long-token-without-spaces\tend\x1b[31m",
        ShellStyle::String,
        0,
        ShellState::Ok,
    )];
    for width in [1, 2, 4, 9, 18, 35, 100] {
        let rows = tool_display::shell_command(&snapshot, true, DisplayContext::default(), width);
        assert!(
            rows.iter().all(|line| line.width() <= usize::from(width)),
            "width {width}: {rows:?}"
        );
        assert!(rows.iter().all(|line| !line.to_string().contains('\x1b')));
        if width == 35 {
            assert!(
                rows.iter()
                    .skip(1)
                    .all(|line| line.to_string().starts_with("      ")),
                "{rows:?}"
            );
            assert!(
                !rows[0].to_string().trim().is_empty(),
                "长词不能挤出空白首行"
            );
        }
    }
}

#[test]
fn shell_display_tui_fold_background_wait_and_legacy_fallback() {
    let mut snapshot = presentation(false);
    snapshot.rows = (0..12).flat_map(|_| snapshot.rows.clone()).collect();
    let context = DisplayContext::default();
    let folded = tool_display::shell_command(&snapshot, false, context, 100);
    assert_eq!(folded.len(), context.window());
    assert!(folded[0].to_string().contains("earlier lines"));
    assert!(tool_display::shell_command(&snapshot, true, context, 100).len() > folded.len());
    let mut transcript = Transcript::new();
    transcript.begin_run();
    tool(
        &mut transcript,
        "start",
        "bash",
        json!({"command":"server","mode":"background"}),
    );
    transcript.progress(AgentEvent::ToolFinished { id: "start".into(), result: json!({"ok":true,"task_id":"bg-1","status":"running","output":"ready","shell":presentation(true)}) });
    transcript.apply_wait_background(&json!({"background_results":[{"ok":true,"task_id":"bg-1","exit_code":2,"output":"done","shell":presentation(false)}]}));
    assert_eq!(transcript.entries.len(), 1);
    assert_eq!(transcript.entries[0].shell, Some(presentation(false)));
    let mut legacy = Transcript::new();
    tool(
        &mut legacy,
        "old",
        "bash",
        json!({"command":"echo legacy && echo raw"}),
    );
    let text = entry_text(&mut legacy.entries[0], 80);
    assert!(text.contains("$ echo legacy && echo raw"), "{text}");
}

#[test]
fn shell_display_tui_embedded_document_keeps_multiline_highlight_state() {
    let rows = ["value = \"\"\"first", "inside", "last\"\"\""]
        .into_iter()
        .map(|text| {
            let mut code = segment(text, ShellStyle::Text, 0, ShellState::Pending);
            code.node = None;
            code.language = Some("python".into());
            ShellRow {
                depth: 1,
                segments: vec![code],
                node: None,
                state: ShellState::Pending,
                block: true,
                meta: String::new(),
                effects: String::new(),
            }
        })
        .collect();
    let presentation = ShellPresentation {
        version: VERSION,
        revision: 1,
        running: false,
        rows,
    };
    let lines = tool_display::shell_command(&presentation, true, DisplayContext::default(), 80);
    let span = lines[1]
        .spans
        .iter()
        .find(|span| span.content.contains("inside"))
        .unwrap();
    assert_eq!(span.style.fg, Some(palette::EDIT_STRING));
    assert!(
        span.style
            .add_modifier
            .contains(ratatui::style::Modifier::DIM)
    );
}
