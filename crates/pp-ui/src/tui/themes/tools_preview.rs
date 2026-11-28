use super::*;
use crate::tui::transcript::Transcript;
use pp_core::events::{AgentEvent, DiffHunk, DiffLine, DiffLineKind, FileDiff, OutputStream};
use ratatui::{Frame, layout::Rect, style::Style};
use serde_json::json;

struct AppearanceGuard(Appearance);

impl AppearanceGuard {
    fn push(appearance: Appearance) -> Self {
        let prev = current_appearance();
        set_active_appearance(appearance.theme, appearance.icons, appearance.tools);
        Self(prev)
    }
}

impl Drop for AppearanceGuard {
    fn drop(&mut self) {
        set_active_appearance(self.0.theme, self.0.icons, self.0.tools);
    }
}

pub(super) fn draw_edit_sample(
    frame: &mut Frame,
    area: Rect,
    theme: Theme,
    icons: IconMode,
    tools: ToolStyle,
) {
    if area.height < 3 || area.width < 10 {
        return;
    }
    let _guard = AppearanceGuard::push(Appearance {
        theme,
        icons,
        tools,
    });
    let mut transcript = Transcript::new();
    add_edit_sample(&mut transcript, theme, area.height < 4);
    transcript.draw(frame, area);
}

pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    p: &Palette,
    appearance: Appearance,
    ascii: bool,
) {
    if area.width < 12 || area.height < 5 {
        return;
    }
    super::preview::draw_header_bar(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        p,
        appearance.tools.label(),
        appearance.icons.label(),
        ascii,
    );

    let margin = u16::from(area.width >= 24);
    let viewport = Rect::new(
        area.x + margin,
        area.y + 1,
        area.width.saturating_sub(margin * 2),
        area.height.saturating_sub(1),
    );
    if viewport.width < 8 || viewport.height < 3 {
        return;
    }
    frame
        .buffer_mut()
        .set_style(viewport, Style::default().bg(p.md_bg).fg(p.md_text));

    let pad = u16::from(viewport.width >= 20);
    let body = Rect::new(
        viewport.x + pad,
        viewport.y,
        viewport.width.saturating_sub(pad * 2),
        viewport.height,
    );
    if body.width < 8 || body.height < 2 {
        return;
    }

    let _guard = AppearanceGuard::push(appearance);
    // Pick the sample card from richest to simplest for the available height so no card gets clipped.
    let presets: [&[SampleCard]; 4] = [
        &[
            SampleCard::ReadGroup,
            SampleCard::Bash,
            SampleCard::Rg,
            SampleCard::Edit,
            SampleCard::Mcp,
        ],
        &[
            SampleCard::ReadGroup,
            SampleCard::Bash,
            SampleCard::Edit,
            SampleCard::Mcp,
        ],
        &[SampleCard::ReadGroup, SampleCard::Bash, SampleCard::Edit],
        &[SampleCard::Bash, SampleCard::Edit],
    ];
    let mut chosen = build_sample(presets[3], appearance.theme);
    for preset in presets {
        let mut candidate = build_sample(preset, appearance.theme);
        if candidate.measure_height(body.width) <= usize::from(body.height) {
            chosen = candidate;
            break;
        }
    }
    chosen.draw(frame, body);
}

#[derive(Clone, Copy)]
enum SampleCard {
    ReadGroup,
    Bash,
    Rg,
    Edit,
    Mcp,
}

fn build_sample(cards: &[SampleCard], theme: Theme) -> Transcript {
    let mut t = Transcript::new();
    for card in cards {
        match card {
            SampleCard::ReadGroup => {
                for (id, path, lines) in [
                    ("r1", "crates/pp-config/src/config/theme.rs", 42),
                    ("r2", "crates/pp-ui/src/tui/themes.rs", 120),
                ] {
                    t.progress(AgentEvent::ToolStarted {
                        id: id.into(),
                        name: "read".into(),
                        arguments: json!({"path": path, "offset": 1, "limit": lines}).to_string(),
                    });
                    t.progress(AgentEvent::ToolFinished {
                        id: id.into(),
                        result: json!({"ok": true, "source": path, "lines_read": lines, "content": "ok"}),
                    });
                }
            }
            SampleCard::Bash => {
                t.progress(AgentEvent::ToolStarted {
                    id: "b1".into(),
                    name: "bash".into(),
                    arguments: json!({"command": "cargo nextest run -p pp-ui"}).to_string(),
                });
                t.progress(AgentEvent::ToolOutput {
                    id: "b1".into(),
                    stream: OutputStream::Stdout,
                    text: "running 12 tests\ntest result: ok. 12 passed\n".into(),
                });
                t.progress(AgentEvent::ToolFinished {
                    id: "b1".into(),
                    result: json!({
                        "ok": true,
                        "exit_code": 0,
                        "stdout": "running 12 tests\ntest result: ok. 12 passed\n",
                        "stderr": ""
                    }),
                });
            }
            SampleCard::Rg => {
                t.progress(AgentEvent::ToolStarted {
                    id: "g1".into(),
                    name: "rg".into(),
                    arguments: json!({"pattern": "ToolStyle::ALL", "path": "crates"}).to_string(),
                });
                t.progress(AgentEvent::ToolFinished {
                    id: "g1".into(),
                    result: json!({
                        "ok": true,
                        "content": "# crates/pp-config/src/config/tool_style.rs\n*16: pub const ALL: [Self; 6]",
                        "details": {
                            "outputMode": "content",
                            "matchCount": 1,
                            "fileCount": 1,
                            "fileMatches": [{"path": "crates/pp-config/src/config/tool_style.rs"}]
                        }
                    }),
                });
            }
            SampleCard::Edit => add_edit_sample(&mut t, theme, false),
            SampleCard::Mcp => {
                t.progress(AgentEvent::ToolStarted {
                    id: "m1".into(),
                    name: "mcp__memory__search".into(),
                    arguments: json!({"query": "themes"}).to_string(),
                });
                t.progress(AgentEvent::ToolFinished {
                    id: "m1".into(),
                    result: json!({
                        "ok": true,
                        "result": {"content": [{"type": "text", "text": "matched 3 nodes in pp-ui"}]}
                    }),
                });
            }
        }
    }
    t
}

fn add_edit_sample(t: &mut Transcript, theme: Theme, compact_one_line: bool) {
    t.progress(AgentEvent::ToolStarted {
        id: "e1".into(),
        name: "edit".into(),
        arguments: json!({"path": "src/tui/themes.rs"}).to_string(),
    });
    let mut lines = Vec::new();
    if !compact_one_line {
        lines.push(DiffLine {
            kind: DiffLineKind::Removed,
            old_line: Some(12),
            new_line: None,
            text: "const THEME: &str = \"old\";\n".into(),
        });
    }
    lines.push(DiffLine {
        kind: DiffLineKind::Added,
        old_line: None,
        new_line: Some(12),
        text: format!("let theme = \"{}\";\n", theme.as_str()),
    });
    t.progress(AgentEvent::FileChanged {
        id: "e1".into(),
        path: "src/tui/themes.rs".into(),
        diff: FileDiff::Text {
            created: false,
            deleted: false,
            added: 1,
            removed: usize::from(!compact_one_line),
            hunks: vec![DiffHunk { lines }],
        },
    });
    t.progress(AgentEvent::ToolFinished {
        id: "e1".into(),
        result: json!({"ok": true}),
    });
}
