//! Structured rendering of approval reasons: highlight the paths needing authorization and split chained shell commands into a step list.
//!
//! `PermissionRequest::reason` is redacted line-oriented text. Only known line prefixes are parsed, best effort;
//! unrecognized lines are shown verbatim as quoted notes, so new reason wording never loses information.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::tui::{themes, transcript::wrap_block};

// Minimum readable body width next to the quote bar on notes; below it the quote bar is omitted.
const MIN_QUOTE_TEXT: u16 = 8;
// Width of the operator column left of a command step (`&& `, `|  `, `;  `).
const GUTTER: usize = 3;
// Maximum screen rows for one step; beyond that a hint suggests Ctrl+O for the full command.
const STEP_MAX_ROWS: usize = 4;
const COMMAND_PREFIX: &str = "Command: ";
const WARNING_PREFIX: &str = "[WARNING:";

/// Reason-line label followed by `: <path list>`.
const PATH_LABELS: &[&str] = &[
    "Read protected session path(s)",
    "Read outside workspace requires approval",
    "Modifies protected path(s)",
    "Extra writable directories",
    "Edit outside workspace requires approval",
    "Write requires approval in read-only mode",
];

/// Other reason-line prefixes that appear after a command and can mark the end of a multi-line command.
const OTHER_LINES: &[&str] = &[
    "Protected path write requires approval (",
    "Internet access (",
    "Allow rule",
    "Saved permissions unavailable",
    "Remembered approval",
    "Reviewer flagged",
    "Auto-review",
    "High-impact",
    "Requires outbound",
    "Dynamic or unanalyzable",
    "bash runs outside",
    "bash requires permission",
    "Command matched",
    COMMAND_PREFIX,
];

#[derive(Debug, PartialEq, Eq)]
enum Block {
    Paths {
        label: String,
        items: Vec<String>,
    },
    Command {
        text: String,
        warning: Option<String>,
    },
    Rules {
        label: String,
        items: Vec<String>,
    },
    Note(String),
}

fn is_known_line(line: &str) -> bool {
    PATH_LABELS
        .iter()
        .chain(OTHER_LINES)
        .any(|prefix| line.starts_with(prefix))
}

/// Path lists are joined with `, `; split only when the next segment still looks like the start of a path.
fn split_paths(list: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut start = 0;
    for (index, _) in list.match_indices(", ") {
        let next = list[index + 2..].chars().next();
        if matches!(next, Some('/' | '~')) {
            items.push(list[start..index].to_owned());
            start = index + 2;
        }
    }
    items.push(list[start..].to_owned());
    items.retain(|item| !item.is_empty());
    items
}

fn parse_paths(line: &str) -> Option<Block> {
    for label in PATH_LABELS {
        if let Some(rest) = line
            .strip_prefix(label)
            .and_then(|rest| rest.strip_prefix(": "))
        {
            return Some(Block::Paths {
                label: (*label).to_owned(),
                items: split_paths(rest),
            });
        }
    }
    if let Some(rest) = line.strip_prefix("Protected path write requires approval (")
        && let Some((scope, target)) = rest.split_once("): ")
    {
        return Some(Block::Paths {
            label: format!("Protected path write requires approval ({scope})"),
            items: vec![target.to_owned()],
        });
    }
    if line.starts_with("Internet access (")
        && let Some((head, target)) = line.split_once(" requires approval: ")
    {
        return Some(Block::Paths {
            label: format!("{head} requires approval"),
            items: vec![target.to_owned()],
        });
    }
    None
}

fn parse_rules(line: &str) -> Option<Block> {
    ["Allow rule (exact)", "Allow rules (exact)"]
        .into_iter()
        .find_map(|label| {
            line.strip_prefix(label)
                .and_then(|rest| rest.strip_prefix(": "))
                .map(|rest| Block::Rules {
                    label: label.to_owned(),
                    items: rest.split("; ").map(str::to_owned).collect(),
                })
        })
}

fn parse(reason: &str) -> Vec<Block> {
    let lines: Vec<&str> = reason.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        index += 1;
        if let Some(first) = line.strip_prefix(COMMAND_PREFIX) {
            let mut text = first.to_owned();
            let mut warning = None;
            // A multi-line command continues until a truncation warning or the next known reason line.
            while index < lines.len() && !is_known_line(lines[index]) {
                let next = lines[index];
                index += 1;
                if next.starts_with(WARNING_PREFIX) {
                    warning = Some(next.to_owned());
                    break;
                }
                text.push('\n');
                text.push_str(next);
            }
            if !text.trim().is_empty() {
                blocks.push(Block::Command { text, warning });
            }
        } else if let Some(block) = parse_paths(line).or_else(|| parse_rules(line)) {
            blocks.push(block);
        } else {
            blocks.push(Block::Note(line.to_owned()));
        }
    }
    blocks
}

/// Split a shell command at top-level `&&`, `||`, `|`, `;`, `&` and newlines into `(leading operator, step)`.
/// Never split inside quotes, backslash escapes, parentheses or backticks; commands with a heredoc stay whole.
fn split_steps(command: &str) -> Vec<(&'static str, String)> {
    #[derive(Default)]
    struct Splitter {
        steps: Vec<(&'static str, String)>,
        op: &'static str,
        current: String,
    }
    impl Splitter {
        fn cut(&mut self, next: &'static str) {
            let text = self.current.trim().to_owned();
            self.current.clear();
            if text.is_empty() {
                // An empty step may only be overridden by a stronger operator, so `a &&\n b` keeps `&&`.
                if matches!(next, "&&" | "||" | "|" | "|&") {
                    self.op = next;
                }
            } else {
                self.steps.push((self.op, text));
                self.op = next;
            }
        }
    }

    let whole = || vec![("", command.trim().to_owned())];
    if command.contains("<<") {
        return whole();
    }
    let chars: Vec<char> = command.chars().collect();
    let mut splitter = Splitter::default();
    let mut quote: Option<char> = None;
    let mut backtick = false;
    let mut depth = 0usize;
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        let previous = index.checked_sub(1).map(|i| chars[i]);
        let next = chars.get(index + 1).copied();
        if let Some(open) = quote {
            splitter.current.push(c);
            if c == '\\' && open == '"' {
                if let Some(escaped) = next {
                    splitter.current.push(escaped);
                    index += 1;
                }
            } else if c == open {
                quote = None;
            }
            index += 1;
            continue;
        }
        match c {
            '\\' => {
                splitter.current.push(c);
                if let Some(escaped) = next {
                    splitter.current.push(escaped);
                    index += 1;
                }
            }
            '\'' | '"' => {
                quote = Some(c);
                splitter.current.push(c);
            }
            '`' => {
                backtick = !backtick;
                splitter.current.push(c);
            }
            '(' => {
                depth += 1;
                splitter.current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                splitter.current.push(c);
            }
            _ if depth > 0 || backtick => splitter.current.push(c),
            ';' | '\n' => splitter.cut(";"),
            '|' if previous == Some('>') => splitter.current.push(c),
            '|' if next == Some('|') => {
                index += 1;
                splitter.cut("||");
            }
            '|' if next == Some('&') => {
                index += 1;
                splitter.cut("|&");
            }
            '|' => splitter.cut("|"),
            '&' if next == Some('&') => {
                index += 1;
                splitter.cut("&&");
            }
            '&' if matches!(previous, Some('>' | '<')) || next == Some('>') => {
                splitter.current.push(c);
            }
            '&' => splitter.cut("&"),
            _ => splitter.current.push(c),
        }
        index += 1;
    }
    let background = splitter.op == "&" && splitter.current.trim().is_empty();
    splitter.cut("");
    if background && let Some((_, step)) = splitter.steps.last_mut() {
        step.push_str(" &");
    }
    if splitter.steps.is_empty() {
        return whole();
    }
    splitter.steps
}

pub(super) fn render(reason: &str, width: u16) -> Vec<Line<'static>> {
    let blocks = parse(reason);
    let marked: Vec<&str> = blocks
        .iter()
        .filter_map(|block| match block {
            Block::Paths { items, .. } => Some(items),
            _ => None,
        })
        .flatten()
        .map(String::as_str)
        .filter(|item| item.len() > 1)
        .collect();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut previous_note = true;
    for block in &blocks {
        let note = matches!(block, Block::Note(_));
        // Consecutive note lines stay compact; other blocks are separated by one blank row.
        if !lines.is_empty() && !(note && previous_note) {
            lines.push(Line::default());
        }
        previous_note = note;
        match block {
            Block::Note(text) => note_lines(&mut lines, text, width),
            Block::Paths { label, items } => path_lines(&mut lines, label, items, width),
            Block::Rules { label, items } => rule_lines(&mut lines, label, items, width),
            Block::Command { text, warning } => {
                command_lines(&mut lines, text, warning.as_deref(), &marked, width);
            }
        }
    }
    lines
}

fn note_lines(lines: &mut Vec<Line<'static>>, text: &str, width: u16) {
    let palette = themes::palette();
    let quote = if themes::is_ascii() { "| " } else { "▎ " };
    let quote_width = Span::raw(quote).width() as u16;
    let (quote, text_width) = if width >= quote_width + MIN_QUOTE_TEXT {
        (quote, width - quote_width)
    } else {
        ("", width)
    };
    let reason = Style::new().fg(palette.activity);
    let quote_style = Style::new().fg(palette.plan_mode);
    for row in wrap_block(Line::styled(text.to_owned(), reason), text_width, "") {
        let mut spans = vec![Span::styled(quote, quote_style)];
        spans.extend(row.spans);
        lines.push(Line::from(spans));
    }
}

/// Color path blocks by risk: protected/modify in red, write in orange, network in purple, read in blue.
fn path_color(label: &str) -> Color {
    let palette = themes::palette();
    if label.starts_with("Modifies protected") || label.starts_with("Protected path") {
        palette.removed
    } else if label.starts_with("Extra writable")
        || label.starts_with("Edit ")
        || label.starts_with("Write ")
    {
        palette.bash_title
    } else if label.starts_with("Internet access") {
        palette.mcp_title
    } else {
        palette.read_title
    }
}

fn path_lines(lines: &mut Vec<Line<'static>>, label: &str, items: &[String], width: u16) {
    let palette = themes::palette();
    let color = path_color(label);
    lines.extend(wrap_block(
        Line::styled(label.to_owned(), Style::new().fg(color)),
        width,
        "",
    ));
    let marker = if themes::is_ascii() { "> " } else { "▸ " };
    let strong = Style::new().fg(color).bold();
    let band = Style::new().bg(palette.panel_alt_bg);
    for item in items {
        let mut spans = vec![Span::styled(marker, strong)];
        match item.rsplit_once('/') {
            // The directory part stays plain while the last segment (file or directory name) is bold and colored; URLs are colored whole.
            Some((dir, name)) if !item.contains("://") && !name.is_empty() => {
                spans.push(Span::styled(
                    format!("{dir}/"),
                    Style::new().fg(palette.text),
                ));
                spans.push(Span::styled(name.to_owned(), strong));
            }
            _ => spans.push(Span::styled(item.clone(), strong)),
        }
        for mut row in wrap_block(Line::from(spans), width, "  ") {
            row.style = band;
            lines.push(row);
        }
    }
}

fn rule_lines(lines: &mut Vec<Line<'static>>, label: &str, items: &[String], width: u16) {
    let palette = themes::palette();
    lines.extend(wrap_block(
        Line::styled(label.to_owned(), Style::new().fg(palette.added)),
        width,
        "",
    ));
    for item in items {
        lines.extend(wrap_block(
            Line::from(vec![
                Span::raw("  "),
                Span::styled(item.clone(), Style::new().fg(palette.code)),
            ]),
            width,
            "    ",
        ));
    }
}

fn command_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    warning: Option<&str>,
    marked: &[&str],
    width: u16,
) {
    let palette = themes::palette();
    let steps = split_steps(text);
    let mut header = vec![Span::styled(
        "Command",
        Style::new().fg(palette.bash_title).bold(),
    )];
    if steps.len() > 1 {
        header.push(Span::styled(
            format!(" · {} steps", steps.len()),
            Style::new().fg(palette.muted),
        ));
    }
    lines.push(Line::from(header));
    let indent = " ".repeat(GUTTER);
    for (index, (op, step)) in steps.iter().enumerate() {
        let (gutter, gutter_style) = if index == 0 {
            ("$", Style::new().fg(palette.added).bold())
        } else {
            (*op, Style::new().fg(operator_color(op)).bold())
        };
        let mut spans = vec![Span::styled(format!("{gutter:<GUTTER$}"), gutter_style)];
        spans.extend(step_spans(step, marked));
        let mut rows = wrap_block(Line::from(spans), width, &indent);
        if rows.len() > STEP_MAX_ROWS {
            let hidden = rows.len() - STEP_MAX_ROWS;
            rows.truncate(STEP_MAX_ROWS);
            rows.push(Line::styled(
                format!("{indent}… {hidden} more row(s); inspect the full command with Ctrl+O"),
                Style::new().fg(palette.dim),
            ));
        }
        lines.extend(rows);
    }
    if let Some(warning) = warning {
        lines.extend(wrap_block(
            Line::styled(warning.to_owned(), Style::new().fg(palette.removed)),
            width,
            "",
        ));
    }
}

/// Operator colors: `&&` green (continue only if the previous step succeeded), `||` yellow, pipe cyan, `;` gray, background purple.
fn operator_color(op: &str) -> Color {
    let palette = themes::palette();
    match op {
        "&&" => palette.added,
        "||" => palette.edit_type,
        "|" | "|&" => palette.search_title,
        "&" => palette.mcp_title,
        _ => palette.muted,
    }
}

/// The program name is bold blue; options, strings, variables and redirections each get their own color;
/// arguments hitting an awaiting-authorization path are bold and underlined in the warning color.
fn step_spans(step: &str, marked: &[&str]) -> Vec<Span<'static>> {
    let palette = themes::palette();
    let mut spans = Vec::new();
    let mut program_seen = false;
    let mut rest = step;
    while !rest.is_empty() {
        let space = rest.starts_with(char::is_whitespace);
        let end = rest
            .find(|c: char| c.is_whitespace() != space)
            .unwrap_or(rest.len());
        let (token, tail) = rest.split_at(end);
        rest = tail;
        if space {
            spans.push(Span::raw(token.to_owned()));
            continue;
        }
        let style = if marked.iter().any(|path| token.contains(path)) {
            Style::new()
                .fg(palette.plan_mode)
                .bold()
                .add_modifier(Modifier::UNDERLINED)
        } else if !program_seen && !token.contains('=') {
            program_seen = true;
            Style::new().fg(palette.accent).bold()
        } else if token.starts_with('-') {
            Style::new().fg(palette.search_title)
        } else if token.starts_with(['\'', '"']) {
            Style::new().fg(palette.edit_string)
        } else if token.starts_with('$') || token.contains('=') {
            Style::new().fg(palette.edit_type)
        } else if token.starts_with(['>', '<'])
            || token.starts_with(|c: char| c.is_ascii_digit())
                && token
                    .trim_start_matches(|c: char| c.is_ascii_digit())
                    .starts_with(['>', '<'])
            || token.starts_with("&>")
        {
            Style::new().fg(palette.edit_keyword)
        } else {
            Style::new().fg(palette.text)
        };
        spans.push(Span::styled(token.to_owned(), style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn splits_chained_commands_only_at_top_level() {
        let steps = |command: &str| -> Vec<(&str, String)> { split_steps(command) };
        for (command, expected) in [
            ("ls", vec![("", "ls")]),
            (
                "cd /tmp && make -j4 || echo failed; ls | wc -l",
                vec![
                    ("", "cd /tmp"),
                    ("&&", "make -j4"),
                    ("||", "echo failed"),
                    (";", "ls"),
                    ("|", "wc -l"),
                ],
            ),
            (
                "echo 'a && b' \"c; d\" $(x | y) `p; q` && z",
                vec![("", "echo 'a && b' \"c; d\" $(x | y) `p; q`"), ("&&", "z")],
            ),
            (
                "cmd 2>&1 | tee out.log &> /dev/null",
                vec![("", "cmd 2>&1"), ("|", "tee out.log &> /dev/null")],
            ),
            ("a &&\n b\n\nc", vec![("", "a"), ("&&", "b"), (";", "c")]),
            ("sleep 1 &", vec![("", "sleep 1 &")]),
            (
                "cat <<EOF\na && b\nEOF",
                vec![("", "cat <<EOF\na && b\nEOF")],
            ),
        ] {
            let expected: Vec<(&str, String)> = expected
                .into_iter()
                .map(|(op, step)| (op, step.to_owned()))
                .collect();
            assert_eq!(steps(command), expected, "{command}");
        }
    }

    #[test]
    fn parses_paths_commands_rules_and_plain_notes() {
        let blocks = parse(
            "Read outside workspace requires approval: /a/b, /c d/e, ~/f\n\
             Protected path write requires approval (.git): /w/.git/config\n\
             Allow rules (exact): git add x; git commit\n\
             Command: cd /a &&\n  make\n\
             [WARNING: command truncated]\n\
             Saved permissions unavailable: boom\n\
             Review the requested permission.",
        );
        assert_eq!(
            blocks,
            vec![
                Block::Paths {
                    label: "Read outside workspace requires approval".into(),
                    items: vec!["/a/b".into(), "/c d/e".into(), "~/f".into()],
                },
                Block::Paths {
                    label: "Protected path write requires approval (.git)".into(),
                    items: vec!["/w/.git/config".into()],
                },
                Block::Rules {
                    label: "Allow rules (exact)".into(),
                    items: vec!["git add x".into(), "git commit".into()],
                },
                Block::Command {
                    text: "cd /a &&\n  make".into(),
                    warning: Some("[WARNING: command truncated]".into()),
                },
                Block::Note("Saved permissions unavailable: boom".into()),
                Block::Note("Review the requested permission.".into()),
            ]
        );
    }

    #[test]
    fn renders_one_row_per_path_and_one_row_per_command_step() {
        let rows: Vec<String> = render(
            "Read outside workspace requires approval: /etc/hosts, /var/log/system.log\n\
             Command: cd /etc && cat hosts | grep -v '#'",
            60,
        )
        .iter()
        .map(text)
        .collect();
        assert_eq!(
            rows,
            [
                "Read outside workspace requires approval",
                "▸ /etc/hosts",
                "▸ /var/log/system.log",
                "",
                "Command · 3 steps",
                "$  cd /etc",
                "&& cat hosts",
                "|  grep -v '#'",
            ]
        );
    }

    #[test]
    fn long_steps_wrap_with_a_hanging_indent_and_are_capped() {
        let command = format!("echo {}", "x".repeat(400));
        let rows: Vec<String> = render(&format!("Command: {command}"), 40)
            .iter()
            .map(text)
            .collect();
        assert!(rows[2].starts_with("   x"), "{rows:?}");
        assert!(rows.last().unwrap().contains("Ctrl+O"), "{rows:?}");
        assert_eq!(rows.len(), 1 + STEP_MAX_ROWS + 1, "{rows:?}");
    }

    #[test]
    fn plain_reason_lines_stay_adjacent() {
        let rows: Vec<String> = render("Runs outside the sandbox.\nReview it.", 60)
            .iter()
            .map(text)
            .collect();
        assert_eq!(rows, ["▎ Runs outside the sandbox.", "▎ Review it."]);
    }
}
