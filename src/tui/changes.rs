//! Temporary latest-task view; filesystem access and undo remain on the worker.
use super::transcript::{diff_layout, wrap_block};
use super::transcript_style::{ACCENT, ADDED, ASK_BG, MUTED, REMOVED, display_text};
use crate::events::FileDiff;
use crate::tools::{ChangedFile, ChangesView, UndoToken};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Clear,
};

pub(super) const HINT: &str = "↑/↓ file  PgUp/PgDn/wheel diff  r refresh  u undo file  Esc close";
const SUBTITLE: &str = "edit/write only · memory only";
const PLAN_UNAVAILABLE: &str = "Undo is unavailable in Plan mode.";

pub(super) enum Action {
    None,
    Close,
    Refresh,
    Undo(UndoToken),
    Message(String),
}

struct Confirmation {
    token: UndoToken,
    created: bool,
    undo_selected: bool,
    width: Option<u16>,
    lines: Vec<Line<'static>>,
}

pub(super) struct Panel {
    plan_mode: bool,
    files: Vec<ChangedFile>,
    rows: Vec<Line<'static>>,
    subtitle: String,
    selected: usize,
    list_offset: usize,
    detail: Vec<Line<'static>>,
    detail_width: Option<u16>,
    detail_offset: usize,
    detail_height: usize,
    loading: bool,
    pending: bool,
    confirmation: Option<Confirmation>,
    error: Option<String>,
}

impl Panel {
    pub(super) fn new(plan_mode: bool) -> Self {
        Self {
            plan_mode,
            files: Vec::new(),
            rows: Vec::new(),
            subtitle: SUBTITLE.into(),
            selected: 0,
            list_offset: 0,
            detail: Vec::new(),
            detail_width: None,
            detail_offset: 0,
            detail_height: 0,
            loading: true,
            pending: false,
            confirmation: None,
            error: None,
        }
    }

    pub(super) fn set_view(&mut self, view: ChangesView) {
        self.subtitle = format!(
            "{SUBTITLE} · task {} · revision {}",
            view.task_id, view.revision
        );
        self.files = view.files;
        self.rows = self.files.iter().map(file_row).collect();
        self.selected = self.selected.min(self.files.len().saturating_sub(1));
        self.list_offset = self.list_offset.min(self.selected);
        self.detail_width = None;
        self.loading = false;
        self.pending = false;
        self.confirmation = None;
        // Undo failures are followed by an authoritative worker refresh. Keep
        // their concrete error until the user explicitly refreshes or acts.
    }

    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(display_text(&error).into_owned());
        self.loading = false;
        self.pending = false;
        self.confirmation = None;
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || !key.modifiers.is_empty()
            || self.pending
        {
            return Action::None;
        }
        if let Some(confirmation) = &mut self.confirmation {
            match key.code {
                KeyCode::Esc => self.confirmation = None,
                KeyCode::Tab | KeyCode::Left | KeyCode::Right
                    if key.kind == KeyEventKind::Press =>
                {
                    confirmation.undo_selected = !confirmation.undo_selected;
                }
                KeyCode::Enter if key.kind == KeyEventKind::Press => {
                    let confirmation = self.confirmation.take().expect("confirmation is open");
                    if confirmation.undo_selected {
                        self.error = None;
                        self.pending = true;
                        return Action::Undo(confirmation.token);
                    }
                }
                _ => {}
            }
            return Action::None;
        }
        if key.code == KeyCode::Esc {
            return Action::Close;
        }
        if self.loading {
            return Action::None;
        }
        match key.code {
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
                self.error = None;
                self.loading = true;
                return Action::Refresh;
            }
            KeyCode::Char('u') if key.kind == KeyEventKind::Press => {
                if self.plan_mode {
                    return Action::Message(PLAN_UNAVAILABLE.into());
                }
                if let Some(file) = self.files.get(self.selected) {
                    match &file.undo {
                        Ok(token) => {
                            self.error = None;
                            self.confirmation = Some(Confirmation {
                                token: token.clone(),
                                created: matches!(file.diff, FileDiff::Text { created: true, .. }),
                                undo_selected: false,
                                width: None,
                                lines: Vec::new(),
                            });
                        }
                        Err(reason) => return Action::Message(reason.clone()),
                    }
                }
            }
            KeyCode::Up => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down => self.select(
                self.selected
                    .saturating_add(1)
                    .min(self.files.len().saturating_sub(1)),
            ),
            KeyCode::PageUp => self.scroll(true, self.detail_height.max(1)),
            KeyCode::PageDown => self.scroll(false, self.detail_height.max(1)),
            _ => {}
        }
        Action::None
    }

    fn select(&mut self, selected: usize) {
        if selected != self.selected {
            self.selected = selected;
            self.detail_width = None;
            self.detail_offset = 0;
        }
    }

    fn scroll(&mut self, up: bool, lines: usize) {
        self.detail_offset = if up {
            self.detail_offset.saturating_sub(lines)
        } else {
            self.detail_offset.saturating_add(lines)
        }
        .min(self.detail.len().saturating_sub(self.detail_height));
    }

    pub(super) fn wheel(&mut self, up: bool, lines: usize) {
        if !self.pending && !self.loading && self.confirmation.is_none() {
            self.scroll(up, lines);
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, mut area: Rect) {
        frame.render_widget(Clear, area);
        draw_line(
            frame,
            &mut area,
            Line::styled("Latest task changes", Style::default().fg(ACCENT)),
        );
        draw_line(
            frame,
            &mut area,
            Line::styled(self.subtitle.as_str(), Style::default().fg(MUTED)),
        );
        if let Some(error) = &self.error {
            draw_line(
                frame,
                &mut area,
                Line::styled(error.as_str(), Style::default().fg(REMOVED)),
            );
        }
        if self.loading || self.pending {
            draw_line(
                frame,
                &mut area,
                Line::styled(
                    if self.pending {
                        "Undoing file…"
                    } else {
                        "Loading changes…"
                    },
                    Style::default().fg(ACCENT),
                ),
            );
            return;
        }
        if self.files.is_empty() {
            draw_line(
                frame,
                &mut area,
                Line::styled(
                    "No edit/write changes in the latest task.",
                    Style::default().fg(MUTED),
                ),
            );
            return;
        }
        let list_height = usize::from(area.height / 3).min(8).min(self.files.len());
        if list_height > 0 {
            self.list_offset = self.list_offset.min(self.files.len() - list_height);
            if self.selected < self.list_offset {
                self.list_offset = self.selected;
            } else if self.selected >= self.list_offset + list_height {
                self.list_offset = self.selected + 1 - list_height;
            }
            for index in self.list_offset..self.list_offset + list_height {
                let target = take_line(&mut area);
                let selected = index == self.selected;
                let style = if selected {
                    Style::default().bg(ASK_BG).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                frame.render_widget(
                    Line::styled(if selected { "› " } else { "  " }, style),
                    target,
                );
                frame.render_widget(
                    &self.rows[index],
                    Rect::new(
                        target.x.saturating_add(2).min(target.right()),
                        target.y,
                        target.width.saturating_sub(2),
                        target.height,
                    ),
                );
            }
        }
        if let Some(confirmation) = &mut self.confirmation {
            draw_confirmation(frame, area, confirmation);
            return;
        }
        draw_line(
            frame,
            &mut area,
            Line::styled("Net diff", Style::default().fg(ACCENT)),
        );
        if let Err(reason) = &self.files[self.selected].undo {
            draw_line(
                frame,
                &mut area,
                Line::styled(display_text(reason), Style::default().fg(REMOVED)),
            );
        }
        self.detail_height = usize::from(area.height);
        if area.is_empty() {
            return;
        }
        if self.detail_width != Some(area.width) {
            self.detail = diff_layout(&self.files[self.selected].diff, area.width, true);
            self.detail_width = Some(area.width);
        }
        self.detail_offset = self
            .detail_offset
            .min(self.detail.len().saturating_sub(self.detail_height));
        for line in self
            .detail
            .iter()
            .skip(self.detail_offset)
            .take(self.detail_height)
        {
            frame.render_widget(line, take_line(&mut area));
        }
    }
}

fn file_row(file: &ChangedFile) -> Line<'static> {
    let mut spans = vec![Span::raw(
        display_text(&file.path.to_string_lossy()).into_owned(),
    )];
    match &file.diff {
        FileDiff::Text {
            created,
            added,
            removed,
            ..
        } => {
            spans.push(Span::raw(if *created {
                "  Created  "
            } else {
                "  Modified  "
            }));
            spans.push(Span::styled(
                format!("+{added}"),
                Style::default().fg(ADDED),
            ));
            spans.push(Span::raw("/"));
            spans.push(Span::styled(
                format!("-{removed}"),
                Style::default().fg(REMOVED),
            ));
        }
        FileDiff::Unavailable { reason } => {
            spans.push(Span::raw("  Modified  "));
            spans.push(Span::styled(
                display_text(reason).into_owned(),
                Style::default().fg(MUTED),
            ));
        }
    }
    spans.push(Span::styled(
        if file.undo.is_ok() {
            "  Ready"
        } else {
            "  Blocked"
        },
        Style::default().fg(if file.undo.is_ok() { ADDED } else { REMOVED }),
    ));
    Line::from(spans)
}

fn draw_confirmation(frame: &mut Frame, mut area: Rect, confirmation: &mut Confirmation) {
    if area.is_empty() {
        return;
    }
    draw_line(
        frame,
        &mut area,
        Line::styled("Undo file?", Style::default().fg(ACCENT)),
    );
    if confirmation.width != Some(area.width) {
        let path = display_text(&confirmation.token.path.to_string_lossy()).into_owned();
        confirmation.lines = wrap_block(Line::raw(format!("File: {path}")), area.width, "");
        confirmation.lines.extend(wrap_block(
            Line::raw(if confirmation.created {
                "Delete this file created by this task."
            } else {
                "Restore content from before this task's first modification."
            }),
            area.width,
            "",
        ));
        confirmation.width = Some(area.width);
    }
    // Reserve a visible button row even when the explanation is clipped.
    for line in confirmation
        .lines
        .iter()
        .take(usize::from(area.height.saturating_sub(1)))
    {
        frame.render_widget(line, take_line(&mut area));
    }
    let button = |label, selected| {
        Span::styled(
            label,
            if selected {
                Style::default()
                    .fg(ACCENT)
                    .bg(ASK_BG)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            },
        )
    };
    draw_line(
        frame,
        &mut area,
        Line::from(vec![
            button("[ Cancel ]", !confirmation.undo_selected),
            Span::raw("  "),
            button("[ Undo file ]", confirmation.undo_selected),
        ]),
    );
}

fn take_line(area: &mut Rect) -> Rect {
    let row = Rect::new(area.x, area.y, area.width, area.height.min(1));
    area.y = area.y.saturating_add(row.height);
    area.height = area.height.saturating_sub(row.height);
    row
}

fn draw_line(frame: &mut Frame, area: &mut Rect, line: Line<'_>) {
    if !area.is_empty() {
        frame.render_widget(line, take_line(area));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{DiffHunk, DiffLine, DiffLineKind};
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn file(path: &str, created: bool, count: usize) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            diff: FileDiff::Text {
                created,
                added: count,
                removed: usize::from(!created),
                hunks: vec![DiffHunk {
                    lines: std::iter::once(DiffLine {
                        kind: if created {
                            DiffLineKind::Added
                        } else {
                            DiffLineKind::Removed
                        },
                        old_line: (!created).then_some(17),
                        new_line: created.then_some(1),
                        text: "original\r\n".into(),
                    })
                    .chain((0..count).map(|i| DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(i + 17),
                        text: format!("{path}-line-{i}\n"),
                    }))
                    .collect(),
                }],
            },
            undo: Ok(UndoToken {
                task_id: 4,
                revision: 9,
                path: path.into(),
            }),
        }
    }

    fn view(files: Vec<ChangedFile>) -> ChangesView {
        ChangesView {
            task_id: 4,
            revision: 9,
            files,
        }
    }

    fn draw(panel: &mut Panel, width: u16, height: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn text(buffer: &Buffer) -> String {
        buffer.content.iter().map(|cell| cell.symbol()).collect()
    }

    #[test]
    fn changes_selection_reveals_list_and_resets_physical_diff_scroll() {
        let mut panel = Panel::new(false);
        panel.set_view(view(
            (0..14)
                .map(|i| file(&format!("file-{i:02}"), false, 40))
                .collect(),
        ));
        let initial = draw(&mut panel, 80, 30);
        assert!(text(&initial).contains("file-00-line-0"));
        panel.key(key(KeyCode::PageDown));
        assert!(panel.detail_offset > 0);
        let paged = text(&draw(&mut panel, 80, 30));
        assert!(!paged.contains("file-00-line-0"));
        panel.wheel(true, usize::MAX);
        assert_eq!(panel.detail_offset, 0);
        panel.wheel(false, usize::MAX);
        assert_eq!(
            panel.detail_offset,
            panel.detail.len() - panel.detail_height
        );
        for _ in 0..12 {
            panel.key(key(KeyCode::Down));
        }
        let compact = text(&draw(&mut panel, 80, 12));
        assert!(compact.contains("› file-12"));
        let selected = text(&draw(&mut panel, 80, 30));
        assert!(selected.contains("› file-12"));
        assert!(!selected.contains("file-00  Modified"));
        assert!(selected.contains("file-12-line-0"));
        assert_eq!(panel.detail_offset, 0);
        panel.set_view(view(vec![file("remaining", false, 1)]));
        let refreshed = text(&draw(&mut panel, 80, 30));
        assert!(refreshed.contains("› remaining"));
        assert!(refreshed.contains("remaining-line-0"));
        assert_eq!(panel.selected, 0);
    }

    #[test]
    fn changes_confirmation_defaults_cancel_and_submits_only_one_press() {
        let mut panel = Panel::new(false);
        panel.set_view(view(vec![file("demo.txt", false, 1)]));
        panel.key(key(KeyCode::Char('u')));
        let confirmation = text(&draw(&mut panel, 80, 20));
        assert!(confirmation.contains("Undo file?"));
        assert!(confirmation.contains("File: demo.txt"));
        assert!(confirmation.contains("before this task's first modification"));
        assert!(!panel.confirmation.as_ref().unwrap().undo_selected);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.confirmation.is_none());
        panel.key(key(KeyCode::Char('u')));
        panel.key(key(KeyCode::Right));
        panel.key(key(KeyCode::Esc));
        assert!(panel.confirmation.is_none());
        panel.key(key(KeyCode::Char('u')));
        panel.key(key(KeyCode::Tab));
        let mut repeat = key(KeyCode::Enter);
        repeat.kind = KeyEventKind::Repeat;
        assert!(matches!(panel.key(repeat), Action::None));
        assert!(!panel.pending);
        let Action::Undo(token) = panel.key(key(KeyCode::Enter)) else {
            panic!("expected undo");
        };
        assert_eq!(token.path.to_str(), Some("demo.txt"));
        assert!(text(&draw(&mut panel, 80, 20)).contains("Undoing file…"));
        for code in [
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Down,
            KeyCode::Char('r'),
            KeyCode::Char('u'),
        ] {
            assert!(matches!(panel.key(key(code)), Action::None));
        }
        panel.wheel(false, 10);
        assert_eq!(panel.detail_offset, 0);
        panel.set_view(view(Vec::new()));
        assert!(
            text(&draw(&mut panel, 80, 20)).contains("No edit/write changes in the latest task.")
        );
    }

    #[test]
    fn changes_created_confirmation_and_plan_mode_never_restore_silently() {
        let mut panel = Panel::new(false);
        panel.set_view(view(vec![file("new.txt", true, 1)]));
        panel.key(key(KeyCode::Char('u')));
        assert!(text(&draw(&mut panel, 80, 20)).contains("Delete this file created by this task."));
        let mut plan = Panel::new(true);
        plan.set_view(view(vec![file("demo.txt", false, 1)]));
        let Action::Message(message) = plan.key(key(KeyCode::Char('u'))) else {
            panic!("expected Plan refusal");
        };
        assert_eq!(message, PLAN_UNAVAILABLE);
        assert!(plan.confirmation.is_none());
        assert!(!plan.pending);
    }

    #[test]
    fn changes_error_survives_worker_refresh_and_explicit_refresh_clears_it() {
        let mut panel = Panel::new(false);
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Close));
        panel.set_error("File changed outside this task; undo refused.".into());
        let mut blocked = file("demo.txt", false, 1);
        blocked.undo = Err("File changed outside this task; undo refused.".into());
        panel.set_view(view(vec![blocked]));
        let rendered = text(&draw(&mut panel, 100, 20));
        assert!(rendered.contains("Blocked"));
        assert!(rendered.contains("File changed outside this task; undo refused."));
        assert!(panel.error.is_some());
        assert!(matches!(
            panel.key(key(KeyCode::Char('u'))),
            Action::Message(_)
        ));
        assert!(panel.confirmation.is_none());
        assert!(matches!(
            panel.key(key(KeyCode::Char('r'))),
            Action::Refresh
        ));
        assert!(panel.error.is_none());
        assert!(text(&draw(&mut panel, 100, 20)).contains("Loading changes…"));
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Close));
    }

    #[test]
    fn changes_detail_reuses_colors_line_numbers_and_eol_annotations() {
        let mut panel = Panel::new(false);
        let mut changed = file("demo\u{1b}.txt", false, 1);
        if let FileDiff::Text { added, hunks, .. } = &mut changed.diff {
            *added += 2;
            hunks[0].lines.extend([
                DiffLine {
                    kind: DiffLineKind::Added,
                    old_line: None,
                    new_line: Some(18),
                    text: "original\n".into(),
                },
                DiffLine {
                    kind: DiffLineKind::Added,
                    old_line: None,
                    new_line: Some(19),
                    text: "\u{feff}last".into(),
                },
            ]);
        }
        panel.set_view(view(vec![changed]));
        let buffer = draw(&mut panel, 100, 20);
        let rendered = text(&buffer);
        assert!(rendered.contains("demo�.txt"));
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("17"));
        assert!(rendered.contains("CRLF"));
        assert!(rendered.contains("BOM"));
        assert!(rendered.contains("No newline at end of file"));
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "o" && cell.fg == REMOVED)
        );
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "d" && cell.fg == ADDED)
        );
        let layout = diff_layout(&panel.files[0].diff, 100, true);
        assert_eq!(panel.detail, layout);
        draw(&mut panel, 24, 20);
        assert_eq!(panel.detail, diff_layout(&panel.files[0].diff, 24, true));
    }

    #[test]
    fn changes_tiny_areas_clip_list_detail_and_confirmation_without_overflow() {
        for width in [0, 1, 2, 9, 80] {
            for height in [0, 1, 2, 3, 4, 8] {
                let mut panel = Panel::new(false);
                draw(&mut panel, width, height);
                panel.set_view(view(vec![file("long/path/to/demo.txt", false, 20)]));
                draw(&mut panel, width, height);
                panel.key(key(KeyCode::Char('u')));
                draw(&mut panel, width, height);
                panel.key(key(KeyCode::Esc));
                panel.set_view(view(Vec::new()));
                draw(&mut panel, width, height);
            }
        }
    }
}
