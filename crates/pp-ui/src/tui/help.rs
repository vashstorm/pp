use super::statusline::accent;
use crate::{
    commands::{
        CommandCategory, FileCommands, HELP_USAGE_COLUMN, SHELL_COMMAND_DESCRIPTION,
        SHELL_COMMAND_USAGE, category_specs,
    },
    shortcuts::platform_text,
};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

pub(super) struct HelpBlock {
    pub(super) entries: Vec<(String, String)>,
    pub(super) diagnostics: Vec<String>,
}

impl HelpBlock {
    pub(super) fn new(files: &FileCommands) -> Self {
        Self {
            entries: files
                .entries
                .iter()
                .map(|file| {
                    (
                        format!("{} [arguments]", file.name),
                        file.description.clone(),
                    )
                })
                .collect(),
            diagnostics: files.diagnostics.clone(),
        }
    }
}

fn first() -> Color {
    super::themes::palette().welcome_first
}

fn second() -> Color {
    super::themes::palette().welcome_second
}

fn separator() -> Color {
    super::themes::palette().welcome_separator
}

fn muted() -> Color {
    super::themes::palette().welcome_muted
}

fn error_color() -> Color {
    super::themes::palette().removed
}

const KEYBINDING_GROUPS: &[(&str, &[(&str, &str)])] = &[
    (
        "Queue",
        &[
            ("Enter", "steer"),
            (platform_text("⌥ + Enter", "Alt + Enter"), "queue"),
            (platform_text("⌥ + Q", "Ctrl + Q"), "pending messages"),
            (platform_text("⌥ + R", "Ctrl + R"), "resume queue"),
        ],
    ),
    (
        "Models",
        &[
            (platform_text("⌥ + P", "Ctrl + P"), "choose model"),
            ("Shift + Tab", "cycle thinking"),
            ("F6 / Alt + Shift + A", "cycle permissions"),
            ("Ctrl + F", "favorite model"),
            ("Ctrl + S", "save model default"),
            (platform_text("⌥ + S", "Ctrl + S"), "save effort default"),
        ],
    ),
    (
        "Panels",
        &[
            ("F1", "help"),
            ("F8", "pipe streams"),
            ("Esc Esc", "session tree"),
            ("Ctrl + O", "tool output"),
            (platform_text("⌥ + A", "Alt + A"), "Agent Hub"),
            (platform_text("⌥ + T", "Ctrl + T"), "TODO"),
            (
                platform_text("⌥ + PgUp/PgDn", "Ctrl + PgUp/PgDn"),
                "scroll TODO",
            ),
        ],
    ),
    (
        "Editing",
        &[
            ("Shift + Enter / Ctrl + J", "newline"),
            ("Tab", "complete"),
            ("@", "local resources"),
            ("Ctrl + V", "paste"),
            ("Ctrl + U", "undo"),
            ("Ctrl + A / Ctrl + E", "line start/end"),
            ("Ctrl + K", "delete to end"),
            ("↑ / ↓", "move/history/menu"),
            ("Wheel / PgUp/PgDn", "scroll"),
            ("Esc", "cancel/dismiss"),
            ("Ctrl + C", "quit"),
        ],
    ),
];

const SAFETY_NOTES: &[&str] = &[
    "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory.",
    "Foreground bash has an execution timeout; managed background tasks survive tool calls and stop at this user task's end or controlled exit.",
    "Ctrl + C stops managed background groups, but cleanup of other foreground children is not guaranteed. No shell/file side-effect rollback; deliberately detached descendants are not covered.",
];

fn push_section_header(
    lines: &mut Vec<Line<'static>>,
    title: &'static str,
    color: Color,
    width: u16,
) {
    lines.push(Line::default());
    lines.push(Line::styled(
        title,
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::styled(
        "─".repeat(usize::from(width)),
        Style::default().fg(separator()),
    ));
}

fn usage_spans(usage: &str) -> Vec<Span<'static>> {
    let key_style = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
    let arg_style = Style::default().fg(second());
    match usage.split_once(' ') {
        Some((name, args)) => vec![
            Span::styled(name.to_owned(), key_style),
            Span::styled(format!(" {args}"), arg_style),
        ],
        None => vec![Span::styled(usage.to_owned(), key_style)],
    }
}

fn description_spans(description: &str) -> Vec<Span<'static>> {
    let body_style = Style::default().fg(Color::Gray);
    if let Some(prefix) = description.strip_suffix(" (TUI)") {
        vec![
            Span::styled(prefix.to_owned(), body_style),
            Span::styled(" (TUI)", Style::default().fg(muted())),
        ]
    } else {
        vec![Span::styled(description.to_owned(), body_style)]
    }
}

fn push_command_row(lines: &mut Vec<Line<'static>>, usage: &str, description: &str, width: u16) {
    let usage_len = usage.chars().count();
    let desc_len = description.chars().count();
    let mut spans = usage_spans(usage);
    if width >= 56 && usage_len < HELP_USAGE_COLUMN {
        spans.push(Span::raw(" ".repeat(HELP_USAGE_COLUMN - usage_len)));
        spans.extend(description_spans(description));
        lines.push(Line::from(spans));
    } else if width < 56 || usage_len + 3 + desc_len <= usize::from(width) {
        spans.push(Span::styled(" — ", Style::default().fg(separator())));
        spans.extend(description_spans(description));
        lines.push(Line::from(spans));
    } else {
        lines.push(Line::from(spans));
        let mut indented = vec![Span::raw("    ")];
        indented.extend(description_spans(description));
        lines.push(Line::from(indented));
    }
}

fn push_keybinding_group(
    lines: &mut Vec<Line<'static>>,
    group: &'static str,
    bindings: &'static [(&'static str, &'static str)],
    width: u16,
) {
    const LABEL_WIDTH: usize = 10;
    let label_style = Style::default().fg(first()).add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(second()).add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::Gray);
    let sep_style = Style::default().fg(separator());
    let budget = usize::from(width).max(LABEL_WIDTH + 16);

    let mut current = vec![Span::styled(format!("{group:<LABEL_WIDTH$}"), label_style)];
    let mut current_width = LABEL_WIDTH;
    let mut first_in_row = true;

    for &(key, action) in bindings {
        let item_width = key.chars().count() + 1 + action.chars().count();
        let sep_width = if first_in_row { 0 } else { 3 };
        if !first_in_row && width >= 48 && current_width + sep_width + item_width > budget {
            lines.push(Line::from(current));
            current = vec![Span::raw(" ".repeat(LABEL_WIDTH))];
            current_width = LABEL_WIDTH;
            first_in_row = true;
        }
        if !first_in_row {
            current.push(Span::styled(" · ", sep_style));
            current_width += 3;
        }
        current.push(Span::styled(key, key_style));
        current.push(Span::styled(format!(" {action}"), desc_style));
        current_width += item_width;
        first_in_row = false;
    }
    lines.push(Line::from(current));
}

pub(super) fn lines(width: u16, block: &HelpBlock) -> Vec<Line<'static>> {
    let header = if width >= 56 {
        Line::from(vec![
            Span::styled("Help:", Style::default().fg(accent())),
            Span::styled(
                "  Commands, keybindings & safety notes · /tips for compact grid",
                Style::default().fg(muted()),
            ),
        ])
    } else {
        Line::styled("Help:", Style::default().fg(accent()))
    };
    let mut lines = vec![header];

    for &category in CommandCategory::ALL {
        let specs = category_specs(category);
        if specs.is_empty() {
            continue;
        }
        push_section_header(&mut lines, category.title(), first(), width);
        for spec in specs {
            push_command_row(&mut lines, spec.usage, spec.description, width);
        }
        if category == CommandCategory::Workflow {
            push_command_row(
                &mut lines,
                SHELL_COMMAND_USAGE,
                SHELL_COMMAND_DESCRIPTION,
                width,
            );
        }
    }

    if !block.entries.is_empty() {
        push_section_header(&mut lines, "Custom Commands", first(), width);
        for (usage, description) in &block.entries {
            push_command_row(&mut lines, usage, description, width);
        }
    }

    if !block.diagnostics.is_empty() {
        push_section_header(&mut lines, "Command load issues:", error_color(), width);
        for diagnostic in &block.diagnostics {
            lines.push(Line::from(vec![
                Span::styled("• ", Style::default().fg(error_color())),
                Span::styled(diagnostic.clone(), Style::default().fg(Color::Gray)),
            ]));
        }
    }

    push_section_header(&mut lines, "Keybindings", first(), width);
    for &(group, bindings) in KEYBINDING_GROUPS {
        push_keybinding_group(&mut lines, group, bindings, width);
    }

    push_section_header(&mut lines, "Safety & Execution", first(), width);
    for &note in SAFETY_NOTES {
        lines.push(Line::from(vec![
            Span::styled("• ", Style::default().fg(first())),
            Span::styled(note, Style::default().fg(Color::Gray)),
        ]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::FileCommand;

    #[test]
    fn help_lines_group_commands_keybindings_and_diagnostics() {
        let files = FileCommands {
            entries: vec![FileCommand {
                name: "/review".into(),
                description: "Review source".into(),
                content: "Review $1".into(),
            }],
            diagnostics: vec!["commands/broken.md: Invalid command metadata".into()],
        };
        let block = HelpBlock::new(&files);
        let wide = lines(100, &block);
        let wide_text: Vec<String> = wide
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(wide_text[0].starts_with("Help:  Commands, keybindings & safety notes"));
        for section in [
            "Session & History",
            "Models & Thinking",
            "Workflow & Execution",
            "Settings & Extensions",
            "Custom Commands",
            "Command load issues:",
            "Keybindings",
            "Safety & Execution",
        ] {
            assert!(
                wide_text.iter().any(|line| line == section),
                "missing section {section}"
            );
        }
        // 短命令对齐到固定列，长命令在 100 列下折叠为缩进说明行。
        assert!(wide_text.iter().any(|line| {
            line.starts_with("/model [provider/model]")
                && line.contains("Select a model at the next safe boundary")
        }));
        let mcp_row = wide_text
            .iter()
            .position(|line| line.starts_with("/mcp [list|"))
            .expect("mcp usage line");
        assert_eq!(
            wide_text[mcp_row + 1],
            "    Manage MCP servers and connection state"
        );
        assert!(
            wide_text
                .iter()
                .any(|line| line.starts_with("/review [arguments]")
                    && line.contains("Review source"))
        );
        assert!(
            wide_text
                .iter()
                .any(|line| line == "• commands/broken.md: Invalid command metadata")
        );

        // 窄屏省略顶栏副标题，并以单行连接符自然折行。
        let narrow = lines(40, &block);
        let narrow_text: Vec<String> = narrow
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert_eq!(narrow_text[0], "Help:");
        assert!(
            narrow_text
                .iter()
                .any(|line| line.starts_with("/new — Reset the session"))
        );
    }
}
