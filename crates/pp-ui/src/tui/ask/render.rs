//! 面板外框、紧凑模式与题目页的绘制。

use super::*;

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, available: Rect) {
        if available.width == 0 || available.height == 0 {
            self.overlay = None;
            return;
        }
        let height = self.height(available, frame.area().height);
        let area = Rect {
            x: available.x,
            y: available.y + available.height - height,
            width: available.width,
            height,
        };
        self.overlay = Some(area);
        frame.render_widget(Clear, area);
        if height < 5 + u16::from(self.has_tabs()) {
            self.draw_compact(frame, area);
            return;
        }
        let title = Line::from(Span::styled(
            format!(" {} ", self.title()),
            Style::default().fg(ACCENT),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(ASK_BG))
            .border_style(Style::default().fg(ASK_BORDER))
            .padding(Padding::new(
                u16::from(area.width > 6),
                u16::from(area.width > 6),
                0,
                0,
            ))
            .title(title);
        let mut inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        if self.has_tabs() {
            self.draw_tabs(frame, Rect { height: 1, ..inner });
            inner.y += 1;
            inner.height -= 1;
        }
        match self.page {
            Page::Question(index) => self.draw_question(frame, inner, index),
            Page::Review => self.draw_review(frame, inner),
        }
    }

    fn draw_compact(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 1 {
            frame.render_widget(Line::from(self.selection_summary()), area);
            return;
        }
        frame.render_widget(
            Line::from(Span::styled(
                self.title(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Rect { height: 1, ..area },
        );
        if self.editor.is_some()
            && let Page::Question(index) = self.page
        {
            self.draw_question(
                frame,
                Rect {
                    y: area.y + 1,
                    height: area.height - 1,
                    ..area
                },
                index,
            );
            return;
        }
        frame.render_widget(
            Line::from(self.selection_summary()),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
        if area.height >= 3 {
            let hint = if self.has_tabs() {
                COMPACT_HINT
            } else {
                COMPACT_SINGLE_PAGE_HINT
            };
            frame.render_widget(
                Line::styled(hint, Style::default().fg(DIM)),
                Rect {
                    y: area.y + 2,
                    height: 1,
                    ..area
                },
            );
        }
    }

    fn draw_question(&mut self, frame: &mut Frame, inner: Rect, index: usize) {
        let multi = self.questions.questions[index].multi;
        let error = self.error.clone();
        let editing = matches!(&self.editor, Some((owner, _)) if *owner == index);
        let hint = if editing {
            EDITOR_HINT
        } else if multi {
            MULTI_HINT
        } else if !self.has_tabs() {
            SINGLE_PAGE_HINT
        } else {
            SINGLE_HINT
        };
        let hint_h = u16::from(inner.height > 1);
        let separator_h = u16::from(inner.height > 2);
        let error_h = u16::from(error.is_some() && inner.height > 2);
        let content_h = inner.height - hint_h - separator_h - error_h;
        let content_area = Rect {
            height: content_h,
            ..inner
        };
        let error_area = Rect {
            y: inner.y + content_h,
            height: error_h,
            ..inner
        };
        let hint_area = Rect {
            y: inner.y + content_h + error_h + separator_h,
            height: hint_h,
            ..inner
        };

        let question = self.question_lines(index, content_area.width);
        // 短屏至少为当前选项或编辑器保留一行。
        let question_h = question.len().min(usize::from(content_h.saturating_sub(1))) as u16;
        for (row, line) in question.iter().take(usize::from(question_h)).enumerate() {
            frame.render_widget(
                line,
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }
        let choices_area = Rect {
            y: content_area.y + question_h,
            height: content_area.height - question_h,
            ..content_area
        };
        let editor_height = self.editor_height(index);
        let editor_area = self.states[index]
            .menu
            .draw_choices(frame, choices_area, editor_height);
        if let Some((_, editor)) = self.editor.as_mut()
            && let Some(editor_area) = editor_area
            && !editor_area.is_empty()
        {
            frame.render_widget(&*editor, editor_area);
            for y in editor_area.y..editor_area.bottom() {
                for x in editor_area.x..editor_area.right() {
                    let cell = &mut frame.buffer_mut()[(x, y)];
                    if let std::borrow::Cow::Owned(safe) = display_text(cell.symbol()) {
                        cell.set_symbol(&safe);
                    }
                }
            }
            let cursor = editor.screen_cursor();
            let (top, left) = editor.scroll_offset();
            if let (Some(row), Some(column)) = (
                cursor.row.checked_sub(usize::from(top)),
                cursor.col.checked_sub(usize::from(left)),
            ) && row < usize::from(editor_area.height)
                && column < usize::from(editor_area.width)
            {
                frame.set_cursor_position((
                    editor_area.x + column as u16,
                    editor_area.y + row as u16,
                ));
            }
        }

        if let Some(error) = error {
            frame.render_widget(
                Line::styled(error, Style::default().fg(REMOVED)),
                error_area,
            );
        }
        if hint_h > 0 {
            if separator_h > 0 {
                self.draw_separator(frame, hint_area.y - 1);
            }
            frame.render_widget(hint_line(hint), hint_area);
        }
    }
}
