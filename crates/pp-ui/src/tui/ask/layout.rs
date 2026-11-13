//! 页签／高度计算与标题、选中摘要等纯布局辅助。

use super::*;

impl Panel {
    pub(super) fn has_review(&self) -> bool {
        !(self.questions.questions.len() == 1 && !self.questions.questions[0].multi)
    }

    pub(super) fn has_tabs(&self) -> bool {
        self.questions.questions.len() > 1 || self.has_review()
    }

    pub(super) fn title(&self) -> String {
        match self.page {
            Page::Question(index) if self.questions.questions.len() > 1 => {
                format!("Ask · {}/{}", index + 1, self.questions.questions.len())
            }
            Page::Question(_) => "Ask".into(),
            Page::Review => "Ask · Submit".into(),
        }
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

    // ---- 布局与绘制 ----

    pub(super) fn draw_separator(&self, frame: &mut Frame, y: u16) {
        if let Some(area) = self.overlay {
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(area.width.saturating_sub(2))),
                    Style::default().fg(ASK_BORDER),
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
        // 边框两行、底部分隔线和快捷键各一行，多个页签时再占一行。
        let need = content_height
            .saturating_add(4 + u16::from(self.has_tabs()))
            .saturating_add(u16::from(self.error.is_some()));
        let cap = (u32::from(screen_height) * 7 / 10) as u16;
        need.min(cap).min(available.height).max(1)
    }

    /// 所有页签共用最高页的高度，只切换页签时面板不改变尺寸。
    ///
    /// 自定义编辑器只属于当前页，其行数计入该页；多行输入因此仍会撑高面板，
    /// 但任何一页都不会因为别的页更矮而收缩。
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
        super::super::transcript::wrap_block(
            Line::styled(
                menu_text(&self.questions.questions[index].question),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            width,
            "",
        )
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
        // 窄屏从左侧移出较早的页签，始终让当前页签可见。
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
                // 页签自身左右各留一格，中间的一格使用面板背景。
                spans.push(Span::raw(" "));
                remaining -= 1;
            }
            let style = if index == active {
                Style::default().fg(ACTIVITY).bg(ASK_TAB_BG)
            } else {
                Style::default().fg(DIM)
            };
            spans.push(Span::styled(label.as_str(), style));
            remaining = remaining.saturating_sub(width);
        }
        frame.render_widget(Line::from(spans), area);
    }
}
