//! Pure layout helpers: tab/height computation, the title and the selection summary.

use super::*;
use crate::tui::transcript_style::tool_icon;

impl Panel {
    pub(super) fn has_review(&self) -> bool {
        !(self.questions.questions.len() == 1 && !self.questions.questions[0].multi)
    }

    pub(super) fn has_tabs(&self) -> bool {
        self.questions.questions.len() > 1 || self.has_review()
    }

    pub(super) fn title(&self) -> String {
        let title = match self.page {
            Page::Question(index) if self.questions.questions.len() > 1 => {
                format!("Ask · {}/{}", index + 1, self.questions.questions.len())
            }
            Page::Question(_) => "Ask".into(),
            Page::Review => "Ask · Submit".into(),
        };
        format!("{} {title}", tool_icon("ask"))
    }

    pub(super) fn selection_summary(&self) -> String {
        match self.page {
            Page::Review => "Review".into(),
            Page::Question(index) => {
                let question = &self.questions.questions[index];
                let state = &self.states[index];
                let mut parts: Vec<String> = state
                    .selected
                    .iter()
                    .filter_map(|&item| question.options.get(item))
                    .map(|option| menu_text(&option.label))
                    .collect();
                if !state.custom.is_empty() {
                    parts.push(menu_text(&state.custom));
                }
                if parts.is_empty() {
                    let focused = state.menu.selected_index().unwrap_or(0);
                    match focused.checked_sub(question.options.len()) {
                        Some(_) => OTHER_LABEL.to_string(),
                        None => question
                            .options
                            .get(focused)
                            .map(|option| menu_text(&option.label))
                            .unwrap_or_default(),
                    }
                } else {
                    parts.join(", ")
                }
            }
        }
    }

    // ---- Layout and drawing ----

    pub(super) fn draw_separator(&self, frame: &mut Frame, y: u16) {
        if let Some(area) = self.overlay {
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(area.width.saturating_sub(2))),
                    Style::default().fg(ask_border()),
                ),
                Rect::new(area.x + 1, y, area.width.saturating_sub(2), 1),
            );
        }
    }

    pub(in crate::tui) fn height(&mut self, available: Rect, screen_height: u16) -> u16 {
        if available.is_empty() {
            return 0;
        }
        let padding = if available.width > 6 { 4 } else { 2 };
        let content_height = self.max_content_height(available.width.saturating_sub(padding));
        // Two border rows, one row each for the bottom separator and the shortcut hints,
        // plus one more row when there are several tabs.
        let need = content_height
            .saturating_add(4 + u16::from(self.has_tabs()))
            .saturating_add(u16::from(self.error.is_some()));
        let cap = (u32::from(screen_height) * 7 / 10) as u16;
        need.min(cap).min(available.height).max(1)
    }

    /// Every tab shares the height of the tallest page, so switching tabs alone never
    /// resizes the panel.
    ///
    /// The custom editor belongs to the current page only and its rows count toward that
    /// page; multi-line input therefore still grows the panel, but no page shrinks just
    /// because another page is shorter.
    fn max_content_height(&mut self, width: u16) -> u16 {
        let mut height = 0;
        for index in 0..self.questions.questions.len() {
            let page = self
                .question_lines(index, width)
                .len()
                .min(usize::from(u16::MAX)) as u16;
            let choices = self.states[index]
                .menu
                .choices_height(width, self.editor_height(index));
            height = height.max(page.saturating_add(choices));
        }
        if self.has_review() {
            self.layout_review(width);
            let review = self
                .review
                .as_ref()
                .expect("review layout initialized")
                .1
                .len()
                .min(usize::from(u16::MAX)) as u16;
            height = height.max(review);
        }
        height
    }

    pub(super) fn editor_height(&self, index: usize) -> Option<u16> {
        self.editor
            .as_ref()
            .filter(|(owner, _)| *owner == index)
            .map(|(_, editor)| editor.lines().len().clamp(1, 5) as u16)
    }

    pub(super) fn question_lines(&self, index: usize, width: u16) -> Vec<Line<'static>> {
        let question = &self.questions.questions[index].question;
        let style = Style::default().fg(accent()).add_modifier(Modifier::BOLD);
        super::super::transcript::wrap_block(Line::styled(menu_text(question), style), width, "")
    }

    pub(super) fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let mut labels: Vec<_> = self
            .questions
            .questions
            .iter()
            .map(|question| {
                let label = question
                    .header
                    .as_deref()
                    .filter(|header| !header.trim().is_empty())
                    .unwrap_or(&question.id);
                format!(" {} ", menu_text(label).trim())
            })
            .collect();
        if self.has_review() {
            labels.push(" Review ".into());
        }
        let active = match self.page {
            Page::Question(index) => index,
            Page::Review => labels.len() - 1,
        };
        let mut start = 0;
        // On a narrow screen, drop earlier tabs from the left so the active tab stays
        // visible.
        while start < active
            && labels[start..=active]
                .iter()
                .map(|label| Line::raw(label).width() + 1)
                .sum::<usize>()
                .saturating_sub(1)
                > usize::from(area.width)
        {
            start += 1;
        }
        let mut spans = Vec::new();
        let mut remaining = usize::from(area.width);
        for (index, label) in labels.iter().enumerate().skip(start) {
            let width = Line::raw(label).width();
            if index > start {
                if remaining < width + 1 {
                    break;
                }
                // Each tab keeps one cell on either side; the single cell in between uses
                // the panel background.
                spans.push(Span::raw(" "));
                remaining -= 1;
            }
            let style = if index == active {
                Style::default().fg(activity()).bg(ask_tab_bg())
            } else {
                Style::default().fg(dim())
            };
            spans.push(Span::styled(label.as_str(), style));
            remaining = remaining.saturating_sub(width);
        }
        frame.render_widget(Line::from(spans), area);
    }
}
