//! Review 页：汇总全部题目的答案、滚动与折行缓存。

use super::*;

impl Panel {
    pub(super) fn draw_review(&mut self, frame: &mut Frame, inner: Rect) {
        let error = self.error.clone();
        let hint_h = u16::from(inner.height > 1);
        let separator_h = u16::from(inner.height > 2);
        let error_h = u16::from(error.is_some() && inner.height > 2);
        let content_h = inner.height - hint_h - separator_h - error_h;
        self.layout_review(inner.width);
        let lines = &self.review.as_ref().expect("review layout initialized").1;
        let max_scroll = lines.len().saturating_sub(usize::from(content_h.max(1)));
        self.review_scroll = self.review_scroll.min(max_scroll);
        for (row, line) in lines
            .iter()
            .skip(self.review_scroll)
            .take(usize::from(content_h))
            .enumerate()
        {
            frame.render_widget(
                line,
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }
        if let Some(error) = error {
            frame.render_widget(
                Line::styled(error, Style::default().fg(REMOVED)),
                Rect {
                    y: inner.y + content_h,
                    height: error_h,
                    ..inner
                },
            );
        }
        if hint_h > 0 {
            if separator_h > 0 {
                self.draw_separator(frame, inner.y + content_h + error_h);
            }
            frame.render_widget(
                hint_line(REVIEW_HINT),
                Rect {
                    y: inner.y + content_h + error_h + separator_h,
                    height: hint_h,
                    ..inner
                },
            );
        }
    }

    pub(super) fn layout_review(&mut self, width: u16) {
        if self
            .review
            .as_ref()
            .is_none_or(|(cached_width, _)| *cached_width != width)
        {
            let lines = self
                .review_lines()
                .into_iter()
                .flat_map(|line| super::super::transcript::wrap_block(line, width, ""))
                .collect();
            self.review = Some((width, lines));
        }
    }

    fn review_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (index, question) in self.questions.questions.iter().enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            let chip = question
                .header
                .as_deref()
                .filter(|header| !header.trim().is_empty())
                .unwrap_or(&question.id);
            lines.push(Line::from(vec![Span::styled(
                display_text(chip).into_owned(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )]));
            let state = &self.states[index];
            let mut parts: Vec<String> = state
                .selected
                .iter()
                .filter_map(|&item| question.options.get(item))
                .map(|option| menu_text(&option.label))
                .collect();
            if !state.custom.is_empty() {
                parts.push(format!("custom: {}", display_text(&state.custom)));
            }
            let answered = !parts.is_empty();
            let summary = if answered {
                parts.join(", ")
            } else if question.multi {
                EMPTY_MULTI_MESSAGE.to_string()
            } else {
                NOT_ANSWERED_MESSAGE.to_string()
            };
            lines.push(Line::from(Span::styled(
                format!("  {summary}"),
                if answered {
                    Style::default()
                } else {
                    Style::default().fg(DIM)
                },
            )));
        }
        lines
    }
}
