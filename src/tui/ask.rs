//! 提问面板：多题单选／多选／自定义回答与 Review 一次提交。
//!
//! Panel 只维护交互状态与绘制；请求通道、答案回传与 transcript 记录块由
//! `App`（src/tui.rs）负责。所有展示文本经过 `display_text` 清洗，原始答案
//! 始终使用未经改写的 label／custom 文本。

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding},
};
use ratatui_textarea::TextArea;

use super::menu::{MenuEvent, MenuItem, SelectionMenu};
use super::transcript_style::{ACCENT, ASK_BG, ASK_BORDER, DIM, REMOVED, display_text};
use crate::interaction::{AskAnswer, AskArgs, AskRequest};

const OTHER_LABEL: &str = "Other (type your own)";
const REQUIRED_MESSAGE: &str = "Answer this question before submitting.";
const EMPTY_MULTI_MESSAGE: &str = "No options selected";
const NOT_ANSWERED_MESSAGE: &str = "(not answered)";
const EDITOR_HINT: &str = "Enter accept · Ctrl + J newline · Esc discard";
const SINGLE_HINT: &str = "Space select · ↵ confirm · ↑/↓ move · Tab/←/→ question · Esc cancel";
const MULTI_HINT: &str = "Space toggle · ↵ next · ↑/↓ move · Tab/←/→ question · Esc cancel";
const REVIEW_HINT: &str = "Enter submit · Tab questions · Esc cancel";
const COMPACT_HINT: &str = "Space select · Enter confirm · Tab next · Esc cancel";

#[derive(Debug)]
pub(super) enum Action {
    None,
    Submit(Vec<AskAnswer>),
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Question(usize),
    Review,
}

/// 每题独立的焦点、选集、custom 答案与列表位置。
struct QuestionState {
    selected: Vec<usize>,
    custom: String,
    menu: SelectionMenu<usize>,
}

impl QuestionState {
    fn new(question: &crate::interaction::AskQuestion) -> Self {
        let initial = question
            .recommended
            .filter(|index| *index < question.options.len())
            .unwrap_or(0);
        let menu =
            SelectionMenu::new(menu_items(question, &[], false), initial).with_accent(ACCENT);
        Self {
            selected: Vec::new(),
            custom: String::new(),
            menu,
        }
    }
}

pub(super) struct Panel {
    questions: Arc<AskArgs>,
    states: Vec<QuestionState>,
    page: Page,
    editor: Option<(usize, TextArea<'static>)>,
    error: Option<String>,
    submitted: bool,
    review_scroll: usize,
    overlay: Option<Rect>,
    review: Option<(u16, Vec<Line<'static>>)>,
}

impl Panel {
    pub(super) fn new(request: AskRequest) -> Self {
        let AskRequest { args, .. } = request;
        let states = args.questions.iter().map(QuestionState::new).collect();
        Self {
            questions: args,
            states,
            page: Page::Question(0),
            editor: None,
            error: None,
            submitted: false,
            review_scroll: 0,
            overlay: None,
            review: None,
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if self.submitted {
            return Action::None;
        }
        if self.questions.questions.is_empty() {
            return Action::Cancel;
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter {
            return Action::None;
        }
        if self.editor.is_some() {
            return self.editor_key(key);
        }
        match self.page {
            Page::Question(index) => self.question_key(index, key),
            Page::Review => self.review_key(key),
        }
    }

    pub(super) fn paste(&mut self, text: &str) -> Action {
        if self.editor.is_none()
            && let Page::Question(index) = self.page
            && self.states[index].menu.selected_index()
                == Some(self.questions.questions[index].options.len())
        {
            self.confirm(index, self.questions.questions[index].options.len());
        }
        if let Some((_, editor)) = self.editor.as_mut() {
            insert_custom_text(editor, text);
        }
        Action::None
    }

    /// 仅当滚轮命中面板 overlay 时导航选项列表；否则交还主界面滚动。
    pub(super) fn wheel(&mut self, up: bool, column: u16, row: u16) -> bool {
        let Some(area) = self.overlay else {
            return false;
        };
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return false;
        }
        if self.questions.questions.is_empty() || self.editor.is_some() {
            return true;
        }
        let key = KeyEvent::new(
            if up { KeyCode::Up } else { KeyCode::Down },
            KeyModifiers::NONE,
        );
        match self.page {
            Page::Question(index) => {
                self.states[index].menu.handle_key(key);
            }
            Page::Review => {
                if up {
                    self.review_scroll = self.review_scroll.saturating_sub(1);
                } else {
                    self.review_scroll = self.review_scroll.saturating_add(1);
                }
            }
        }
        true
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, available: Rect) {
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
        if height < 6 {
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
        self.draw_tabs(frame, Rect { height: 1, ..inner });
        inner.y += 1;
        inner.height -= 1;
        match self.page {
            Page::Question(index) => self.draw_question(frame, inner, index),
            Page::Review => self.draw_review(frame, inner),
        }
    }

    // ---- 键盘路由 ----

    fn editor_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                // Esc 只丢弃本次编辑，回到 picker。
                self.editor = None;
                self.error = None;
                Action::None
            }
            KeyCode::Enter => {
                let accepted = match self.editor.as_ref() {
                    Some((index, editor)) => {
                        let text = editor.lines().join("\n");
                        (!text.trim().is_empty()).then_some((*index, text))
                    }
                    None => None,
                };
                let Some((index, text)) = accepted else {
                    return Action::None;
                };
                let multi = self.questions.questions[index].multi;
                let state = &mut self.states[index];
                state.custom = text;
                if !multi {
                    state.selected.clear();
                }
                self.editor = None;
                self.rebuild_menu(index);
                self.error = None;
                self.advance(index)
            }
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                if let Some((_, editor)) = self.editor.as_mut() {
                    editor.insert_newline();
                }
                Action::None
            }
            _ => {
                if let Some((_, editor)) = self.editor.as_mut() {
                    editor.input(key);
                }
                Action::None
            }
        }
    }

    fn question_key(&mut self, index: usize, key: KeyEvent) -> Action {
        let multi = self.questions.questions[index].multi;
        let option_count = self.questions.questions[index].options.len();
        let focused = self.states[index].menu.selected_index().unwrap_or(0);
        match key.code {
            KeyCode::Esc => Action::Cancel,
            KeyCode::Up if key.modifiers == KeyModifiers::CONTROL => Action::None,
            KeyCode::Down if key.modifiers == KeyModifiers::CONTROL => Action::None,
            KeyCode::Left | KeyCode::BackTab => {
                self.step_page(-1);
                Action::None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.step_page(1);
                Action::None
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                if focused == option_count
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.confirm(index, focused);
                self.editor_key(key)
            }
            KeyCode::Char(' ')
                if key.kind == KeyEventKind::Press
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
                    && focused < option_count =>
            {
                let state = &mut self.states[index];
                if !multi {
                    state.selected = vec![focused];
                    state.custom.clear();
                } else if let Some(position) = state
                    .selected
                    .iter()
                    .position(|&selected| selected == focused)
                {
                    state.selected.remove(position);
                } else {
                    state.selected.push(focused);
                    state.selected.sort_unstable();
                }
                self.rebuild_menu(index);
                self.error = None;
                Action::None
            }
            _ => match self.states[index].menu.handle_key(key) {
                MenuEvent::Confirm(item) => self.confirm(index, item),
                _ => Action::None,
            },
        }
    }

    fn review_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => Action::Cancel,
            KeyCode::Enter => self.submit(),
            KeyCode::Up => {
                self.review_scroll = self.review_scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::Down => {
                self.review_scroll = self.review_scroll.saturating_add(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.review_scroll = self.review_scroll.saturating_sub(8);
                Action::None
            }
            KeyCode::PageDown => {
                self.review_scroll = self.review_scroll.saturating_add(8);
                Action::None
            }
            KeyCode::Home => {
                self.review_scroll = 0;
                Action::None
            }
            KeyCode::End => {
                self.review_scroll = usize::MAX;
                Action::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.step_page(-1);
                Action::None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.step_page(1);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn confirm(&mut self, index: usize, item: usize) -> Action {
        let option_count = self.questions.questions[index].options.len();
        if item == option_count {
            let mut editor = super::editor();
            editor.set_placeholder_text(OTHER_LABEL);
            editor.set_style(Style::default().bg(ASK_BG));
            insert_custom_text(&mut editor, &self.states[index].custom);
            self.editor = Some((index, editor));
            self.error = None;
            return Action::None;
        }
        if self.questions.questions[index].multi {
            // 多选 Enter 只前进，不额外勾选焦点项。
            self.error = None;
            return self.advance(index);
        }
        let state = &mut self.states[index];
        state.selected = vec![item];
        state.custom.clear();
        self.rebuild_menu(index);
        self.error = None;
        self.advance(index)
    }

    fn step_page(&mut self, delta: isize) {
        if !self.has_review() {
            return;
        }
        let pages = self.questions.questions.len() as isize + 1;
        let current = match self.page {
            Page::Question(index) => index as isize,
            Page::Review => pages - 1,
        };
        let next = (current + delta).rem_euclid(pages) as usize;
        self.error = None;
        if next == pages as usize - 1 {
            self.page = Page::Review;
            self.review_scroll = 0;
        } else {
            self.page = Page::Question(next);
        }
    }

    fn advance(&mut self, index: usize) -> Action {
        if index + 1 < self.questions.questions.len() {
            self.page = Page::Question(index + 1);
            return Action::None;
        }
        if self.has_review() {
            self.page = Page::Review;
            self.review_scroll = 0;
            return Action::None;
        }
        self.submit()
    }

    fn has_review(&self) -> bool {
        !(self.questions.questions.len() == 1 && !self.questions.questions[0].multi)
    }

    fn submit(&mut self) -> Action {
        for (index, question) in self.questions.questions.iter().enumerate() {
            if question.multi {
                continue;
            }
            let state = &self.states[index];
            if state.selected.is_empty() && state.custom.trim().is_empty() {
                self.error = Some(REQUIRED_MESSAGE.into());
                self.page = Page::Question(index);
                return Action::None;
            }
        }
        self.submitted = true;
        Action::Submit(self.collect_answers())
    }

    /// 按问题原顺序构造答案；多选 label 按原选项顺序，custom 保留原文。
    fn collect_answers(&self) -> Vec<AskAnswer> {
        self.questions
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let state = &self.states[index];
                let selected_options = state
                    .selected
                    .iter()
                    .filter_map(|&item| question.options.get(item))
                    .map(|option| option.label.clone())
                    .collect();
                let custom_input = (!state.custom.is_empty()).then(|| state.custom.clone());
                AskAnswer {
                    id: question.id.clone(),
                    selected_options,
                    custom_input,
                }
            })
            .collect()
    }

    fn rebuild_menu(&mut self, index: usize) {
        self.review = None;
        let question = &self.questions.questions[index];
        let state = &mut self.states[index];
        let custom = !state.custom.is_empty();
        for item in state.menu.items_mut() {
            let selected = if item.value == question.options.len() {
                custom
            } else {
                state.selected.contains(&item.value)
            };
            item.icon = Some(selection_icon(question.multi, selected));
            item.current = selected;
            if item.value == question.options.len() {
                item.description = menu_text(&state.custom);
            }
        }
    }

    fn title(&self) -> String {
        match self.page {
            Page::Question(index) if self.questions.questions.len() > 1 => {
                format!("Ask · {}/{}", index + 1, self.questions.questions.len())
            }
            Page::Question(_) => "Ask".into(),
            Page::Review => "Ask · Submit".into(),
        }
    }

    fn selection_summary(&self) -> String {
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

    fn draw_separator(&self, frame: &mut Frame, y: u16) {
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

    pub(super) fn height(&mut self, available: Rect, screen_height: u16) -> u16 {
        if available.is_empty() {
            return 0;
        }
        let padding = if available.width > 6 { 4 } else { 2 };
        let inner_width = available.width.saturating_sub(padding);
        let content_height = match self.page {
            Page::Question(index) => self.states[index]
                .menu
                .choices_height(inner_width, self.editor_height(index)),
            Page::Review => {
                self.layout_review(inner_width);
                self.review
                    .as_ref()
                    .expect("review layout initialized")
                    .1
                    .len()
                    .min(usize::from(u16::MAX)) as u16
            }
        };
        // 只计算当前页：边框两行、页签一行、底部分隔线和快捷键各一行。
        let need = content_height
            .saturating_add(5)
            .saturating_add(u16::from(self.error.is_some()));
        let cap = (u32::from(screen_height) * 7 / 10) as u16;
        need.min(cap).min(available.height).max(1)
    }

    fn editor_height(&self, index: usize) -> Option<u16> {
        self.editor
            .as_ref()
            .filter(|(owner, _)| *owner == index)
            .map(|(_, editor)| editor.lines().len().clamp(1, 5) as u16)
    }

    fn draw_tabs(&self, frame: &mut Frame, area: Rect) {
        let mut labels: Vec<_> = self
            .questions
            .questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let label = question
                    .header
                    .as_deref()
                    .filter(|header| !header.trim().is_empty());
                match label {
                    Some(label) => format!(" {}. {} ", index + 1, menu_text(label)),
                    None => format!(" Question {} ", index + 1),
                }
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
        let mut x = area.x;
        for (index, label) in labels.iter().enumerate().skip(start) {
            let width = Line::raw(label)
                .width()
                .min(usize::from(area.right().saturating_sub(x))) as u16;
            if width == 0 {
                break;
            }
            let style = if index == active {
                Style::default()
                    .fg(ASK_BG)
                    .bg(ACCENT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };
            frame.render_widget(
                Line::styled(label.as_str(), style),
                Rect::new(x, area.y, width, 1),
            );
            x = x.saturating_add(width).saturating_add(1);
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
            frame.render_widget(
                Line::styled(COMPACT_HINT, Style::default().fg(DIM)),
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

        let editor_height = self.editor_height(index);
        let editor_area = self.states[index]
            .menu
            .draw_choices(frame, content_area, editor_height);
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

    fn draw_review(&mut self, frame: &mut Frame, inner: Rect) {
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

    fn layout_review(&mut self, width: u16) {
        if self
            .review
            .as_ref()
            .is_none_or(|(cached_width, _)| *cached_width != width)
        {
            let lines = self
                .review_lines()
                .into_iter()
                .flat_map(|line| super::transcript::wrap_block(line, width, ""))
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

/// 选项列表：原选项加推荐徽标，末尾固定追加自定义回答入口。
fn insert_custom_text(editor: &mut TextArea<'static>, text: &str) {
    // TextArea::insert_str strips one CR before every LF and at EOF. Duplicate
    // only those CRs in its input so the stored editor text remains byte-exact.
    if !text.contains("\r\n") && !text.ends_with('\r') {
        editor.insert_str(text);
        return;
    }
    let mut insertion = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            insertion.push('\n');
        }
        insertion.push_str(line);
        if line.ends_with('\r') {
            insertion.push('\r');
        }
    }
    editor.insert_str(insertion);
}

fn menu_text(text: &str) -> String {
    let safe = display_text(text);
    let mut output = String::with_capacity(safe.len());
    let mut column = 0;
    for part in safe.split_inclusive(['\n', '\t']) {
        let (plain, separator) = if let Some(text) = part.strip_suffix('\n') {
            (text, Some('\n'))
        } else if let Some(text) = part.strip_suffix('\t') {
            (text, Some('\t'))
        } else {
            (part, None)
        };
        for grapheme in Span::raw(plain).styled_graphemes(Style::default()) {
            output.push_str(grapheme.symbol);
            column += Span::raw(grapheme.symbol).width();
        }
        if let Some(separator) = separator {
            let spaces = if separator == '\t' { 4 - column % 4 } else { 1 };
            output.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        }
    }
    output
}

fn menu_items(
    question: &crate::interaction::AskQuestion,
    selected: &[usize],
    custom: bool,
) -> Vec<MenuItem<usize>> {
    let count = question.options.len();
    let mut items = Vec::with_capacity(count + 1);
    for (index, option) in question.options.iter().enumerate() {
        let chosen = selected.contains(&index);
        let icon = selection_icon(question.multi, chosen);
        let mut label = menu_text(&option.label);
        if question.recommended == Some(index) {
            label.push_str(" (Recommended)");
        }
        items.push(MenuItem {
            value: index,
            label,
            description: option
                .description
                .as_deref()
                .map(menu_text)
                .unwrap_or_default(),
            icon: Some(icon),
            current: chosen,
        });
    }
    let other_icon = selection_icon(question.multi, custom);
    items.push(MenuItem {
        value: count,
        label: OTHER_LABEL.to_string(),
        description: String::new(),
        icon: Some(other_icon),
        current: custom,
    });
    items
}

pub(super) fn selection_icon(multi: bool, selected: bool) -> &'static str {
    // 使用同系列的大轮廓配对字形，避免选中后因字体回退而缩小。
    match (multi, selected) {
        (true, true) => "󰄲",
        (true, false) => "󰄱",
        (false, true) => "󰐾",
        (false, false) => "󰐽",
    }
}

fn hint_line(hint: &'static str) -> Line<'static> {
    Line::styled(hint, Style::default().fg(DIM))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction::{AskOption, AskQuestion};
    use ratatui::{Terminal, backend::TestBackend};

    fn option(label: &str) -> AskOption {
        AskOption {
            label: label.into(),
            description: None,
            preview: None,
        }
    }

    fn question(id: &str, multi: bool, recommended: Option<usize>, labels: &[&str]) -> AskQuestion {
        AskQuestion {
            id: id.into(),
            question: format!("Choose for {id}?"),
            options: labels.iter().map(|label| option(label)).collect(),
            header: None,
            multi,
            recommended,
        }
    }

    fn panel(questions: Vec<AskQuestion>) -> Panel {
        Panel::new(AskRequest {
            call_id: "call-1".into(),
            args: Arc::new(AskArgs { questions }),
        })
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(code), KeyModifiers::CONTROL)
    }

    fn render(panel: &mut Panel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, width, height)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn initial_focus_recommended_without_preselection() {
        let panel = panel(vec![question("q", false, Some(1), &["A", "B", "C"])]);
        assert_eq!(panel.states[0].menu.selected_index(), Some(1));
        assert!(panel.states[0].selected.is_empty());
        assert!(panel.states[0].custom.is_empty());
        let empty = self::panel(vec![question("q", true, None, &[])]);
        assert_eq!(empty.states[0].menu.selected_index(), Some(0));
    }

    #[test]
    fn recommended_badge_is_display_only_and_labels_sanitized() {
        let mut q = question("q", false, Some(0), &["Postgres", "Da\u{7f}ta"]);
        q.options[1].description = Some("fast\u{1b}[31m".into());
        let mut panel = panel(vec![q]);
        let items = panel.states[0].menu.items();
        assert!(!items[1].label.contains('\u{7f}'));
        assert!(!items[1].description.contains('\u{1b}'));
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("single question single select must submit immediately");
        };
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].id, "q");
        assert_eq!(answers[0].selected_options, vec!["Postgres".to_string()]);
        assert_eq!(answers[0].custom_input, None);
    }

    #[test]
    fn single_select_custom_editor_submit_with_multiline() {
        let mut panel = panel(vec![question("q", false, Some(0), &["A", "B"])]);
        panel.key(key(KeyCode::End));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.editor.is_some());
        panel.paste("  ");
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.editor.is_some());
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        assert!(panel.editor.is_none());
        assert!(panel.states[0].custom.is_empty());
        panel.key(key(KeyCode::Enter));
        assert!(panel.editor.is_some());
        panel.paste("第一行");
        panel.key(ctrl('j'));
        panel.paste("second");
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit after custom answer");
        };
        assert_eq!(answers[0].custom_input, Some("第一行\nsecond".to_string()));
        assert!(answers[0].selected_options.is_empty());
    }

    #[test]
    fn multi_toggle_order_then_required_answer_locates_question() {
        let mut panel = panel(vec![
            question("checks", true, Some(0), &["unit", "lint", "fmt"]),
            question("db", false, Some(0), &["SQLite", "Postgres"]),
        ]);
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Up));
        panel.key(key(KeyCode::Char(' ')));
        panel.key(key(KeyCode::Home));
        panel.key(key(KeyCode::Char(' ')));
        assert_eq!(panel.states[0].selected, vec![0, 2]);
        panel.key(key(KeyCode::Tab));
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert_eq!(panel.error.as_deref(), Some(REQUIRED_MESSAGE));
        assert_eq!(panel.page, Page::Question(1));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit after all questions answered");
        };
        assert_eq!(answers[0].id, "checks");
        assert_eq!(
            answers[0].selected_options,
            vec!["unit".to_string(), "fmt".to_string()]
        );
        assert_eq!(answers[0].custom_input, None);
        assert_eq!(answers[1].selected_options, vec!["SQLite".to_string()]);
    }

    #[test]
    fn multi_empty_selection_is_submittable() {
        let mut panel = panel(vec![question("checks", true, None, &["unit", "lint"])]);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("empty multi selection must submit");
        };
        assert!(answers[0].selected_options.is_empty());
        assert_eq!(answers[0].custom_input, None);
    }

    #[test]
    fn multi_custom_coexists_with_selection() {
        let mut panel = panel(vec![question("checks", true, Some(0), &["unit", "lint"])]);
        panel.key(key(KeyCode::Char(' ')));
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("再检查中文");
        panel.key(key(KeyCode::Enter));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit with selection and custom answer");
        };
        assert_eq!(answers[0].selected_options, vec!["unit".to_string()]);
        assert_eq!(answers[0].custom_input, Some("再检查中文".to_string()));
    }

    #[test]
    fn single_select_option_clears_previous_custom() {
        let mut panel = panel(vec![
            question("a", false, None, &["A", "B"]),
            question("b", true, None, &["x", "y"]),
        ]);
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("custom");
        panel.key(key(KeyCode::Enter));
        assert_eq!(panel.page, Page::Question(1));
        panel.key(key(KeyCode::BackTab));
        assert_eq!(panel.page, Page::Question(0));
        panel.key(key(KeyCode::Home));
        assert!(matches!(panel.key(key(KeyCode::Char(' '))), Action::None));
        assert_eq!(panel.page, Page::Question(0));
        assert_eq!(panel.states[0].selected, vec![0]);
        assert!(panel.states[0].custom.is_empty());
        panel.key(key(KeyCode::Enter));
        panel.key(key(KeyCode::Enter));
        assert_eq!(panel.page, Page::Review);
        let action = panel.key(key(KeyCode::Enter));
        let Action::Submit(answers) = action else {
            panic!("expected submit");
        };
        assert_eq!(answers[0].selected_options, vec!["A".to_string()]);
        assert_eq!(answers[0].custom_input, None);
    }

    #[test]
    fn esc_cancels_from_picker_and_review() {
        let mut picker = panel(vec![question("q", false, None, &["A"])]);
        assert!(matches!(picker.key(key(KeyCode::Esc)), Action::Cancel));
        let mut review = panel(vec![question("q", true, None, &["A"])]);
        review.key(key(KeyCode::Tab));
        assert!(matches!(review.key(key(KeyCode::Esc)), Action::Cancel));
    }

    #[test]
    fn tab_and_arrows_cycle_pages() {
        let mut panel = panel(vec![
            question("a", false, None, &["A"]),
            question("b", false, None, &["B"]),
        ]);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Question(1));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.page, Page::Review);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Question(0));
        panel.key(key(KeyCode::Left));
        assert_eq!(panel.page, Page::Review);
    }

    #[test]
    fn single_single_select_question_has_no_review_page() {
        let mut panel = panel(vec![question("a", false, None, &["A"])]);
        panel.key(key(KeyCode::Tab));
        assert_eq!(panel.page, Page::Question(0));
    }

    #[test]
    fn single_space_selects_without_submitting_and_details_key_is_ignored() {
        let mut panel = panel(vec![question("q", false, Some(1), &["A", "B"])]);
        assert!(matches!(panel.key(key(KeyCode::Char(' '))), Action::None));
        assert_eq!(panel.page, Page::Question(0));
        assert_eq!(panel.states[0].selected, [1]);
        assert!(!panel.submitted);
        let before = render(&mut panel, 100, 24);
        assert!(before.contains("󰐾 B (Recommended)"));
        assert!(!before.contains("details"));
        assert!(matches!(panel.key(ctrl('o')), Action::None));
        assert_eq!(render(&mut panel, 100, 24), before);
        panel.key(key(KeyCode::Home));
        panel.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL));
        panel.key(KeyEvent::new_with_kind(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        ));
        assert_eq!(panel.states[0].selected, [1]);
        panel.key(key(KeyCode::Char(' ')));
        assert_eq!(panel.states[0].selected, [0]);
        assert!(!panel.submitted);
        let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
            panic!("按 Enter 后应提交已选答案");
        };
        assert_eq!(answers[0].selected_options, ["A"]);
    }

    #[test]
    fn wheel_navigates_inside_overlay_only() {
        let mut panel = panel(vec![question("q", false, Some(0), &["A", "B", "C"])]);
        render(&mut panel, 40, 20);
        let area = panel.overlay.expect("overlay cached after draw");
        assert!(panel.wheel(false, area.x, area.bottom() - 1));
        assert_eq!(panel.states[0].menu.selected_index(), Some(1));
        assert!(!panel.wheel(true, area.right(), area.y));
        assert!(!panel.wheel(true, area.x, area.bottom()));
        assert_eq!(panel.states[0].menu.selected_index(), Some(1));
    }

    #[test]
    fn picker_shows_descriptions_and_keeps_previews_in_messages() {
        let mut q = question("q", false, Some(0), &["A"]);
        q.options[0].description = Some("Option description".into());
        q.options[0].preview = Some("# Head\n\ntext".into());
        let mut panel = panel(vec![q]);
        for (width, height) in [(40, 24), (24, 12), (40, 24)] {
            let screen = render(&mut panel, width, height);
            assert!(screen.contains("󰐽 A"), "应显示选项：{screen}");
            assert!(!screen.contains("Choose for q?"));
            if width >= 40 {
                assert!(screen.contains("Option description"));
            }
            assert!(!screen.contains("Head"));
            assert!(!screen.contains("text"));
        }
        panel.key(key(KeyCode::Char(' ')));
        let screen = render(&mut panel, 40, 24);
        assert!(screen.contains("󰐾 A"));
    }

    #[test]
    fn compact_mode_renders_without_border() {
        let mut panel = panel(vec![question("db", false, None, &["A", "B"])]);
        let screen = render(&mut panel, 30, 3);
        assert!(screen.contains("Ask"), "紧凑模式应显示 Ask 标题");
        assert!(!screen.contains("db"), "紧凑模式不应显示内部题目 ID");
        let screen = render(&mut panel, 30, 1);
        assert!(
            screen.starts_with('A'),
            "focused option lost on one-row screen: {screen}"
        );
    }

    #[test]
    fn option_display_expands_tabs_without_rewriting_answer_labels() {
        let mut panel = panel(vec![question("q", false, None, &["a\tb\nc"])]);
        let screen = render(&mut panel, 80, 24);
        assert!(
            screen.contains("a   b c"),
            "menu lost visible label whitespace: {screen}"
        );
        let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
            panic!("expected selected answer")
        };
        assert_eq!(answers[0].selected_options, ["a\tb\nc"]);
    }

    #[test]
    fn paste_without_editor_is_ignored() {
        let mut panel = panel(vec![question("q", false, None, &["A"])]);
        assert!(matches!(panel.paste("hello"), Action::None));
        assert!(panel.states[0].custom.is_empty());
    }

    #[test]
    fn custom_option_accepts_direct_typing_and_paste_without_leaving_list() {
        for paste in [false, true] {
            let mut panel = panel(vec![question("q", false, None, &["SQLite"])]);
            panel.key(key(KeyCode::End));
            render(&mut panel, 80, 24);
            let before = panel.overlay;
            if paste {
                panel.paste("answer");
            } else {
                for character in "answer".chars() {
                    panel.key(key(KeyCode::Char(character)));
                }
            }
            let screen = render(&mut panel, 80, 24);
            assert_eq!(panel.overlay, before, "进入 Other 不应改变外框位置和高度");
            assert!(screen.contains("󰐽 SQLite"));
            assert!(screen.contains("󰐽 answer"));
            assert!(!screen.contains("custom answer"));
            panel.key(ctrl('j'));
            panel.paste("second line");
            render(&mut panel, 80, 24);
            let after = panel.overlay.unwrap();
            let before = before.unwrap();
            assert_eq!(after.height, before.height + 1, "新增输入行应扩展面板");
            assert_eq!(after.bottom(), before.bottom(), "面板底部应保持锚定");
            let compact = render(&mut panel, 40, 8);
            assert!(
                compact.contains("second line"),
                "紧凑屏幕也应显示正在编辑的答案：{compact}"
            );
            panel.key(key(KeyCode::Esc));
            assert!(panel.states[0].custom.is_empty());
            assert!(render(&mut panel, 80, 24).contains(OTHER_LABEL));
            panel.paste("accepted");
            let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
                panic!("原位输入应可提交");
            };
            assert_eq!(answers[0].custom_input.as_deref(), Some("accepted"));
        }
    }

    #[test]
    fn picker_shows_header_tab_without_internal_id_or_repeated_question() {
        let mut q = question("confirm_commit", false, None, &["yes", "no"]);
        q.header = Some("Commit confirmation".into());
        let mut panel = panel(vec![q]);
        let screen = render(&mut panel, 80, 24);
        assert!(!screen.contains("confirm_commit"));
        assert!(screen.contains("1. Commit confirmation"));
        assert!(!screen.contains("Choose for"));
        let rows: Vec<_> = screen.lines().collect();
        let title = rows.iter().position(|row| row.contains("╭ Ask")).unwrap();
        assert!(rows[title + 1].contains("1. Commit confirmation"));
        assert!(rows[title + 2].contains("󰐽 yes"), "页签下方应直接显示选项");
    }

    #[test]
    fn tabs_highlight_current_question_and_review_including_narrow_layout() {
        let mut first = question("first", false, None, &["A"]);
        first.header = Some("范围".into());
        let mut second = question("second", true, None, &["B"]);
        second.header = Some("模式".into());
        let mut panel = panel(vec![first, second]);
        for (page, label) in [(0, "1. 范围"), (1, "2. 模式"), (2, "Review")] {
            for width in [80, 18] {
                let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                terminal
                    .draw(|frame| panel.draw(frame, frame.area()))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let row = panel.overlay.unwrap().y + 1;
                // 宽字符的后续单元格只是占位，不属于显示文本。
                let columns = (0..width)
                    .filter(|&x| x == 0 || Span::raw(buffer[(x - 1, row)].symbol()).width() <= 1);
                let highlighted: String = columns
                    .clone()
                    .filter(|&x| buffer[(x, row)].bg == ACCENT)
                    .map(|x| buffer[(x, row)].symbol())
                    .collect();
                assert!(
                    highlighted.contains(label),
                    "当前页签应可见且高亮：{highlighted}"
                );
                if width == 80 {
                    let text: String = columns.map(|x| buffer[(x, row)].symbol()).collect();
                    for expected in ["1. 范围", "2. 模式", "Review"] {
                        assert!(text.contains(expected), "应显示全部页签：{text}");
                    }
                }
            }
            assert_eq!(
                panel.page,
                if page < 2 {
                    Page::Question(page)
                } else {
                    Page::Review
                }
            );
            panel.key(key(KeyCode::Tab));
        }
    }

    #[test]
    fn height_tracks_current_page_wrapping_editor_and_validation() {
        let mut long = question("long", false, None, &["A", "B", "C"]);
        for option in &mut long.options {
            option.description =
                Some("A detailed option description that wraps at narrow widths".into());
        }
        let mut panel = panel(vec![long, question("short", false, None, &["D"])]);
        render(&mut panel, 100, 40);
        let long_height = panel.overlay.unwrap().height;
        assert_eq!(long_height, 12, "三项名称与说明、Other 和五行框架");
        render(&mut panel, 32, 40);
        assert_eq!(panel.overlay.unwrap().height, 15, "说明折行应计入实际高度");
        panel.key(key(KeyCode::Tab));
        render(&mut panel, 100, 40);
        assert_eq!(panel.overlay.unwrap().height, 7, "短题不应沿用长题的高度");
        panel.error = Some(REQUIRED_MESSAGE.into());
        let screen = render(&mut panel, 100, 40);
        assert_eq!(panel.overlay.unwrap().height, 8);
        assert!(screen.contains(REQUIRED_MESSAGE));
        panel.error = None;
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("one\ntwo\nthree");
        let screen = render(&mut panel, 100, 40);
        assert_eq!(panel.overlay.unwrap().height, 9);
        assert!(screen.contains("three"));
        panel.key(key(KeyCode::Esc));
        render(&mut panel, 100, 40);
        assert_eq!(panel.overlay.unwrap().height, 7, "丢弃编辑后应收回多行空间");
        panel.states[0].custom = "very long answer ".repeat(12);
        panel.page = Page::Review;
        render(&mut panel, 32, 40);
        let review_height = panel.overlay.unwrap().height;
        assert!(review_height > 10, "Review 高度应包含答案折行");
        assert_eq!(
            review_height,
            panel.review.as_ref().unwrap().1.len() as u16 + 5
        );
        render(&mut panel, 32, 12);
        assert!(
            panel.overlay.unwrap().height <= 8,
            "面板不应超过屏幕高度的 70%"
        );
    }

    #[test]
    fn custom_answer_keeps_raw_controls_but_terminal_cells_are_safe() {
        let mut panel = panel(vec![question("q", false, None, &["A"])]);
        panel.key(key(KeyCode::End));
        panel.key(key(KeyCode::Enter));
        panel.paste("bad\u{1b}[31mtail\r\nnext\rmiddle\r");
        let screen = render(&mut panel, 80, 24);
        assert!(
            !screen.contains('\u{1b}'),
            "unsafe control reached terminal cells"
        );
        let Action::Submit(answers) = panel.key(key(KeyCode::Enter)) else {
            panic!("expected custom answer")
        };
        assert_eq!(
            answers[0].custom_input.as_deref(),
            Some("bad\u{1b}[31mtail\r\nnext\rmiddle\r")
        );
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
    }
}
