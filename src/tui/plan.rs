//! 计划审阅面板：只渲染 worker 发布的快照，文件/网络访问全部留在 worker。
//! 键盘与绘制路径不做任何 IO；审批必须携带面板看到的 ReviewToken。
use super::editor;
use super::markdown;
use super::transcript_style::{ACCENT, DIM, MUTED};
use crate::plan::{
    ExecutionContext, PlanDocument, PlanReview, PlanReviewReport, PlanStatus, ReviewToken,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear},
};
use ratatui_textarea::TextArea;

const ACTIONS: [&str; 6] = [
    "Execute in a new session",
    "Execute (keep context)",
    "Compact context and execute",
    "Review Plan",
    "Continue discussing Plan",
    "Save draft and leave Plan mode",
];
/// 打开完整审阅面板时直接聚焦操作区，默认选中第一项可用操作。
const DEFAULT_ACTION: usize = 0;
const HINT: &str = "↑↓ choose · Enter run/send · Tab actions/input/body · ←→ tabs · Esc close";
const READONLY_HINT: &str = "Read-only view · most recently synced version · Esc close";
const DISCUSS_HINT: &str = "Enter send · Ctrl + J newline · Esc discard";

/// 按字符数截断纯显示文本，超宽时以省略号结尾。
/// Highlighted choice/tab chip shared by the plan panel tabs.
fn choice_span(label: &str, selected: bool) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        if selected {
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(MUTED)
        },
    )
}

fn fit_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Close,
    Approve(ExecutionContext),
    Discuss(String),
    Review,
    SaveAndLeave,
    EditInput(KeyEvent),
    DiscussInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Plan,
    Review,
}

impl Tab {
    fn index(self) -> usize {
        match self {
            Tab::Plan => 0,
            Tab::Review => 1,
        }
    }

    fn toggled(self) -> Self {
        match self {
            Tab::Plan => Tab::Review,
            Tab::Review => Tab::Plan,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Body,
    Actions,
    Input,
}

pub(super) struct Panel {
    document: PlanDocument,
    report: Option<PlanReviewReport>,
    token: Option<ReviewToken>,
    tab: Tab,
    focus: Focus,
    action: usize,
    scroll: [usize; 2],
    // (宽度, 布局缓存)；仅在宽度或正文变化时重建，不在每帧重排版。
    layouts: [Option<(u16, Vec<Line<'static>>)>; 2],
    discussion: Option<TextArea<'static>>,
    viewport: usize,
}

impl Panel {
    /// 完整审阅面板：携带 ReviewToken，Draft/Ready 提供操作区。
    pub(super) fn review(review: PlanReview) -> Self {
        let mut panel = Self::progress(review.document);
        panel.token = Some(review.token);
        panel.report = review.report;
        panel.focus = Focus::Actions;
        panel.ensure_enabled_action();
        panel
    }

    /// 只读进度视图（Execute 运行中查看 /plan view）：无 token、无操作区。
    pub(super) fn progress(document: PlanDocument) -> Self {
        Self {
            document,
            report: None,
            token: None,
            tab: Tab::Plan,
            focus: Focus::Body,
            action: DEFAULT_ACTION,
            scroll: [0; 2],
            layouts: [None, None],
            discussion: None,
            viewport: 0,
        }
    }

    /// Executed/Abandoned plans are read-only; the token presence is checked
    /// separately by `action_enabled`.
    fn readonly(&self) -> bool {
        !matches!(self.document.status, PlanStatus::Draft | PlanStatus::Ready)
    }

    /// 审查成功但安全读取失败时的降级视图：展示最终报告，无 token、无操作区，
    /// 旧执行 token 因此失效，直到下一次成功的安全读取（set_review）恢复。
    pub(super) fn degraded(document: PlanDocument, report: PlanReviewReport) -> Self {
        let mut panel = Self::progress(document);
        panel.report = Some(report);
        panel.tab = Tab::Review;
        panel
    }

    /// 审查完成后的刷新：焦点落在正文 Review 标签；操作区保留当前选项。
    pub(super) fn focus_review_body(&mut self) {
        if self.report.is_some() {
            self.tab = Tab::Review;
        }
        self.focus = Focus::Body;
    }

    /// 当前是否聚焦在有报告的正文 Review 标签上。
    #[cfg(test)]
    pub(super) fn on_review_body(&self) -> bool {
        self.report.is_some() && self.tab == Tab::Review && self.focus == Focus::Body
    }

    pub(super) fn token(&self) -> Option<&ReviewToken> {
        self.token.as_ref()
    }

    pub(super) fn input_focused(&self) -> bool {
        self.focus == Focus::Input && self.discussion.is_none()
    }

    fn border_color(&self, focus: Focus) -> Color {
        if self.focus == focus && self.discussion.is_none() {
            ACCENT
        } else {
            MUTED
        }
    }

    pub(super) fn document(&self) -> &PlanDocument {
        &self.document
    }

    /// 同一份计划的最新 workflow 快照：更新状态/名称/位置，保留滚动与
    /// 焦点；返回正文是否变化。名称/位置/状态变化只影响头部与操作区
    /// 显示，不失效正文布局缓存。
    pub(super) fn sync_document(&mut self, document: &PlanDocument) -> bool {
        if self.document.id != document.id {
            return false;
        }
        let changed = self.document.revision != document.revision
            || self.document.content != document.content;
        if changed {
            self.document.revision = document.revision;
            self.document.content = document.content.clone();
            self.layouts[Tab::Plan.index()] = None;
        }
        self.document.status = document.status;
        self.document.file_name = document.file_name.clone();
        self.document.location = document.location;
        self.document.approved_tasks = document.approved_tasks.clone();
        self.ensure_enabled_action();
        changed
    }

    /// 进度事件/进度视图：内容变化即重置正文缓存与滚动位置。
    pub(super) fn set_progress(&mut self, document: &PlanDocument) {
        if self.sync_document(document) {
            self.scroll[Tab::Plan.index()] = 0;
        }
    }

    /// 新一次 ReadPlan：修订变化重置正文滚动；报告变化重置报告排版缓存。
    pub(super) fn set_review(&mut self, review: PlanReview) {
        let same_revision = self.token.as_ref().is_some_and(|token| {
            token.plan_id == review.document.id && token.revision == review.document.revision
        });
        self.document = review.document;
        self.report = review.report;
        self.token = Some(review.token);
        self.layouts[Tab::Review.index()] = None;
        if !same_revision {
            self.layouts[Tab::Plan.index()] = None;
            self.scroll[Tab::Plan.index()] = 0;
            self.focus = Focus::Actions;
            self.action = DEFAULT_ACTION;
        }
        if self.report.is_none() {
            self.tab = Tab::Plan;
        }
        self.ensure_enabled_action();
    }

    fn ensure_enabled_action(&mut self) {
        if !self.action_enabled(self.action)
            && let Some(index) = (0..ACTIONS.len()).find(|&index| self.action_enabled(index))
        {
            self.action = index;
        }
    }

    pub(super) fn action_enabled(&self, index: usize) -> bool {
        if self.readonly() || self.token.is_none() {
            return false;
        }
        let ready = self.document.status == PlanStatus::Ready;
        match index {
            0..=3 => ready,
            4 | 5 => matches!(self.document.status, PlanStatus::Draft | PlanStatus::Ready),
            _ => false,
        }
    }

    /// 返回 false 时由主输入框接收粘贴；只读面板吞掉粘贴。
    pub(super) fn paste(&mut self, text: &str) -> bool {
        if let Some(editor) = &mut self.discussion {
            editor.insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
            true
        } else if self.token.is_some() && !self.readonly() {
            self.focus = Focus::Input;
            false
        } else {
            true
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if self.discussion.is_some() {
            return self.discussion_key(key);
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            // Esc 仅关闭面板：不降级、不执行，保留模式与正文。
            return Action::Close;
        }
        if self.focus != Focus::Input
            && self.report.is_some()
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            self.tab = self.tab.toggled();
            return Action::None;
        }
        if self.focus != Focus::Input
            && key.modifiers.is_empty()
            && key.kind == KeyEventKind::Press
            && let KeyCode::Char(number @ '1'..='6') = key.code
        {
            let index = (number as u8 - b'1') as usize;
            if self.action_enabled(index) {
                self.focus = Focus::Actions;
                self.action = index;
            }
            return Action::None;
        }
        if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            let with_input = self.token.is_some() && !self.readonly();
            self.focus = match (self.focus, key.code == KeyCode::BackTab, with_input) {
                (Focus::Actions, false, true) | (Focus::Body, true, true) => Focus::Input,
                (Focus::Actions, _, _) | (Focus::Input, false, _) => Focus::Body,
                _ => Focus::Actions,
            };
            return Action::None;
        }
        match self.focus {
            Focus::Body => self.body_key(key),
            Focus::Actions => self.action_key(key),
            Focus::Input => {
                if key.code == KeyCode::Enter && key.modifiers.is_empty() {
                    if key.kind == KeyEventKind::Press {
                        Action::DiscussInput
                    } else {
                        Action::None
                    }
                } else {
                    Action::EditInput(key)
                }
            }
        }
    }

    fn body_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::PageUp => {
                self.scroll_page(false);
            }
            KeyCode::PageDown => {
                self.scroll_page(true);
            }
            KeyCode::Up => self.scroll_by(false, 1),
            KeyCode::Down => self.scroll_by(true, 1),
            KeyCode::Left | KeyCode::Right if self.report.is_some() => {
                // 左右键切换 Plan/Review 标签；各自保留滚动位置。
                self.tab = self.tab.toggled();
            }
            _ => {}
        }
        Action::None
    }

    fn action_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Left => {
                if let Some(index) = (0..self.action)
                    .rev()
                    .find(|&index| self.action_enabled(index))
                {
                    self.action = index;
                }
            }
            KeyCode::Down | KeyCode::Right => {
                if let Some(index) =
                    (self.action + 1..ACTIONS.len()).find(|&index| self.action_enabled(index))
                {
                    self.action = index;
                }
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if !self.action_enabled(self.action) {
                    return Action::None;
                }
                return match self.action {
                    0 => Action::Approve(ExecutionContext::Fresh),
                    1 => Action::Approve(ExecutionContext::Keep),
                    2 => Action::Approve(ExecutionContext::Compact),
                    4 => {
                        self.discussion = Some(editor());
                        Action::None
                    }
                    3 => Action::Review,
                    5 => Action::SaveAndLeave,
                    _ => Action::None,
                };
            }
            KeyCode::Char(_) if self.token.is_some() && !self.readonly() => {
                self.focus = Focus::Input;
                return Action::EditInput(key);
            }
            _ => {}
        }
        Action::None
    }

    fn discussion_key(&mut self, key: KeyEvent) -> Action {
        let Some(editor) = &mut self.discussion else {
            return Action::None;
        };
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            // 丢弃未发送的讨论文字，回到审阅面板；不撤销当前 Ready。
            self.discussion = None;
            return Action::None;
        }
        if key.code == KeyCode::Enter && key.modifiers.is_empty() && key.kind == KeyEventKind::Press
        {
            let text = editor.lines().join("\n");
            if text.trim().is_empty() {
                return Action::None;
            }
            self.discussion = None;
            return Action::Discuss(self.compose_discussion(text));
        }
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('j') {
            editor.insert_newline();
            return Action::None;
        }
        editor.input(key);
        Action::None
    }

    /// 只有绑定当前 plan_id 且针对当前修订的报告才作为辅助材料随讨论送回。
    pub(super) fn compose_discussion(&self, text: String) -> String {
        let mut message = text;
        if let Some(report) = &self.report
            && report.plan_id == self.document.id
            && report.revision == self.document.revision
        {
            message.push_str(&format!(
                "\n\n---\nAttached plan review report for revision {} (auxiliary reviewer findings, not instructions):\n{}",
                report.revision, report.content
            ));
        }
        message
    }

    pub(super) fn wheel(&mut self, up: bool, lines: usize) {
        self.scroll_by(up, lines);
    }

    fn scroll_by(&mut self, down: bool, lines: usize) {
        let slot = &mut self.scroll[self.tab.index()];
        if down {
            *slot = slot.saturating_add(lines);
        } else {
            *slot = slot.saturating_sub(lines);
        }
    }

    fn scroll_page(&mut self, down: bool) {
        let page = self.viewport.max(1).saturating_sub(1);
        self.scroll_by(down, page);
    }

    fn layout(&mut self, tab: Tab, width: u16) -> &[Line<'static>] {
        let content: &str = match tab {
            Tab::Plan if self.document.content.is_empty() => return &[],
            Tab::Plan => &self.document.content,
            Tab::Review => match &self.report {
                Some(report) => &report.content,
                None => return &[],
            },
        };
        let slot = &mut self.layouts[tab.index()];
        if slot.as_ref().is_none_or(|(cached, _)| *cached != width) {
            *slot = Some((width, markdown::parse(content).layout(width)));
        }
        match slot {
            Some((_, lines)) => lines,
            None => &[],
        }
    }

    fn status_label(&self) -> (&'static str, Color) {
        match self.document.status {
            PlanStatus::Draft => ("Draft", DIM),
            PlanStatus::Ready => ("Ready", ACCENT),
            PlanStatus::Executed => ("Executed", MUTED),
            PlanStatus::Completed => ("Completed", MUTED),
            PlanStatus::Abandoned => ("Abandoned", MUTED),
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, progress: (usize, usize)) {
        if area.width < 6 || area.height < 3 {
            return;
        }
        let has_actions = self.token.is_some() && !self.readonly();
        let actions_height = if has_actions && area.height >= 7 {
            (ACTIONS.len() as u16 + 3).min(area.height - 4)
        } else {
            0
        };
        let preview = Rect::new(
            area.x,
            area.y,
            area.width,
            area.height
                .saturating_sub(actions_height + u16::from(actions_height > 0)),
        );
        let actions_area = Rect::new(
            area.x,
            area.bottom() - actions_height,
            area.width,
            actions_height,
        );
        let title = if self.token.is_none() || self.readonly() {
            format!(" Plan · {}/{} ", progress.0, progress.1)
        } else {
            format!(" Plan · r{} ", self.document.revision)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(self.border_color(Focus::Body)))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(self.border_color(Focus::Body))
                    .add_modifier(Modifier::BOLD),
            ));
        let mut body = block.inner(preview);
        frame.render_widget(block, preview);

        // 头部：Plan 标签显示相对路径 + 状态；Review 标签显示报告修订与
        // 模型来源，旧修订报告明确标注 outdated（宽度不足时截断模型引用）。
        if body.height > 0 {
            let (label, color) = self.status_label();
            let mut spans = Vec::new();
            match (&self.tab, &self.report) {
                (Tab::Review, Some(report)) => {
                    let prefix = format!(" Review r{}", report.revision);
                    let outdated = report.revision < self.document.revision;
                    let suffix_len = usize::from(outdated) * " · outdated".len();
                    let budget = (body.width as usize)
                        .saturating_sub(prefix.chars().count() + 3 + suffix_len);
                    spans.push(Span::styled(prefix, Style::default().fg(MUTED)));
                    spans.push(Span::raw(" · "));
                    spans.push(Span::styled(
                        fit_chars(&report.model_reference, budget),
                        Style::default().fg(MUTED),
                    ));
                    if outdated {
                        spans.push(Span::styled(
                            " · outdated",
                            Style::default().fg(DIM).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
                _ => {
                    // 未保存的新绑定不显示假路径。
                    let path = self
                        .document
                        .path()
                        .unwrap_or_else(|| "Not saved yet".to_owned());
                    spans.push(Span::styled(format!(" {path}"), Style::default().fg(MUTED)));
                }
            }
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                label,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
            let line = Line::from(spans);
            frame.render_widget(line, Rect::new(body.x, body.y, body.width, 1));
            body.y += 1;
            body.height -= 1;
        }

        // 报告标签行；没有报告时不占用行高。
        if self.report.is_some() && body.height > 0 {
            frame.render_widget(
                Line::from(vec![
                    choice_span("Plan", self.tab == Tab::Plan),
                    choice_span("Review", self.tab == Tab::Review),
                ]),
                Rect::new(body.x, body.y, body.width, 1),
            );
            body.y += 1;
            body.height -= 1;
        }

        // 只读提示留在预览内部；六项操作在独立外框中绘制。
        let footer_height = u16::from(!has_actions).min(body.height);
        let footer = Rect::new(
            body.x,
            body.y + body.height.saturating_sub(footer_height),
            body.width,
            footer_height,
        );
        body.height -= footer_height;

        // 正文：markdown 布局缓存 + 滚动。
        let viewport = body.height as usize;
        self.viewport = viewport;
        let tab = self.tab;
        let slot_scroll = self.scroll[tab.index()];
        let empty_plan = self.document.content.is_empty();
        let lines = self.layout(tab, body.width);
        let total = lines.len();
        let scroll = slot_scroll.min(total.saturating_sub(viewport));
        if body.height > 0 {
            if total == 0 && tab == Tab::Plan && empty_plan {
                frame.render_widget(
                    Line::styled("No plan content yet.", Style::default().fg(MUTED)),
                    body,
                );
            } else {
                for (row, line) in lines.iter().enumerate().skip(scroll).take(viewport) {
                    frame.render_widget(
                        line,
                        Rect::new(body.x, body.y + (row - scroll) as u16, body.width, 1),
                    );
                }
            }
        }
        // 布局借用结束后再落滚动位置，下一帧继续从此处显示。
        self.scroll[tab.index()] = scroll;

        if footer_height > 0 {
            frame.render_widget(
                Line::styled(format!(" {READONLY_HINT}"), Style::default().fg(MUTED)),
                footer,
            );
        }
        if actions_height > 0 {
            self.draw_actions(frame, actions_area);
        }

        if self.discussion.is_some() {
            self.draw_discussion(frame, area);
        }
    }

    fn draw_actions(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(self.border_color(Focus::Actions)))
            .title(Span::styled(
                " Actions ",
                Style::default()
                    .fg(self.border_color(Focus::Actions))
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let show_hint = inner.height > ACTIONS.len() as u16;
        let visible = usize::from(inner.height.saturating_sub(u16::from(show_hint)));
        // 短窗口只显示部分操作时，仍保证当前选项处于可见范围。
        let first = self.action.saturating_add(1).saturating_sub(visible);
        for (row, (index, label)) in ACTIONS
            .iter()
            .enumerate()
            .skip(first)
            .take(visible)
            .enumerate()
        {
            let selected = self.action == index && self.action_enabled(index);
            let style = if !self.action_enabled(index) {
                Style::default().fg(DIM)
            } else if selected {
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let marker = if selected { "❯ " } else { "  " };
            frame.render_widget(
                Line::styled(format!("{marker}{label}"), style),
                Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
            );
        }
        if show_hint {
            frame.render_widget(
                Line::styled(format!(" {HINT}"), Style::default().fg(DIM)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }

    fn draw_discussion(&self, frame: &mut Frame, area: Rect) {
        if area.width < 16 || area.height < 7 {
            return;
        }
        let width = area.width.saturating_sub(4).min(72);
        let height = area.height.saturating_sub(4).min(9);
        let area = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " Continue discussing Plan ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(Clear, area);
        frame.render_widget(block, area);
        let Some(editor) = &self.discussion else {
            return;
        };
        if inner.height > 1 {
            frame.render_widget(
                editor,
                Rect::new(inner.x, inner.y, inner.width, inner.height - 1),
            );
        }
        if inner.height > 0 {
            frame.render_widget(
                Line::styled(format!(" {DISCUSS_HINT}"), Style::default().fg(DIM)),
                Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::task_progress;
    use crossterm::event::KeyModifiers;

    fn document(status: PlanStatus, content: &str) -> PlanDocument {
        PlanDocument {
            id: "0123456789abcdef0123456789abcdef".into(),
            revision: 1,
            title: "Plan".into(),
            content: content.into(),
            status,
            origin_prompt: "original task".into(),
            file_name: Some("create-canary.md".into()),
            location: crate::plan::PlanLocation::Drafts,
            approved_tasks: None,
        }
    }

    fn token() -> ReviewToken {
        ReviewToken {
            session_id: "session".into(),
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            path: ".pp/drafts/create-canary.md".into(),
            revision: 1,
            content: "- [ ] step".into(),
        }
    }

    fn review_panel(status: PlanStatus) -> Panel {
        Panel::review(PlanReview {
            token: token(),
            document: document(status, "- [ ] step"),
            report: None,
        })
    }

    fn key_event(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render_panel(panel: &mut Panel, height: u16) -> Vec<String> {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), (0, 1)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn plan_preview_and_actions_have_separate_frames_and_spacing() {
        let mut panel = review_panel(PlanStatus::Ready);
        let rows = render_panel(&mut panel, 26);
        assert!(rows[0].starts_with("┌ Plan"));
        assert!(rows[15].starts_with('└'));
        assert!(rows[16].trim().is_empty());
        assert!(rows[17].starts_with("╭ Actions"));
        assert!(rows[25].starts_with('╰'));
        for (index, label) in ACTIONS.iter().enumerate() {
            assert!(rows[18 + index].contains(label));
        }
        let mut completed = Panel::progress(document(PlanStatus::Completed, "- [x] step"));
        let rows = render_panel(&mut completed, 26);
        assert!(!rows.iter().any(|row| row.contains("Actions")));
        assert!(rows[25].starts_with('└'));
    }

    #[test]
    fn plan_short_actions_frame_keeps_the_selected_option_visible() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Char('6')));
        let rows = render_panel(&mut panel, 10);
        assert!(
            rows.iter()
                .any(|row| row.contains("❯ Save draft and leave Plan mode"))
        );
        assert!(rows.last().unwrap().starts_with('╰'));
    }

    #[test]
    fn plan_actions_start_focused_and_tab_reaches_input_and_body() {
        let mut panel = review_panel(PlanStatus::Ready);
        assert_eq!(
            panel.key(key_event(KeyCode::Enter)),
            Action::Approve(ExecutionContext::Fresh)
        );
        panel.key(key_event(KeyCode::Tab));
        assert!(panel.input_focused());
        assert_eq!(
            panel.key(key_event(KeyCode::Char('1'))),
            Action::EditInput(key_event(KeyCode::Char('1')))
        );
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::DiscussInput);
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Body);
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Actions);
    }

    #[test]
    fn plan_number_keys_select_all_six_actions_without_executing() {
        for (number, expected) in [
            ('1', Action::Approve(ExecutionContext::Fresh)),
            ('2', Action::Approve(ExecutionContext::Keep)),
            ('3', Action::Approve(ExecutionContext::Compact)),
            ('4', Action::Review),
            ('6', Action::SaveAndLeave),
        ] {
            let mut panel = review_panel(PlanStatus::Ready);
            assert_eq!(panel.key(key_event(KeyCode::Char(number))), Action::None);
            assert_eq!(panel.key(key_event(KeyCode::Enter)), expected);
        }
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Char('5')));
        panel.key(key_event(KeyCode::Enter));
        assert!(panel.discussion.is_some());
    }

    #[test]
    fn draft_allows_discussion_and_save_but_requires_submission_to_execute() {
        let mut draft = review_panel(PlanStatus::Draft);
        for index in 0..4 {
            assert!(!draft.action_enabled(index));
        }
        assert!(draft.action_enabled(4));
        assert!(draft.action_enabled(5));
        draft.key(key_event(KeyCode::Char('1')));
        assert_eq!(draft.action, 4);
        assert!(draft.discussion.is_none());
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::None);
        assert!(draft.discussion.is_some());
    }

    #[test]
    fn plan_action_arrows_only_select_enabled_options_and_stop_at_boundaries() {
        for (status, first) in [(PlanStatus::Draft, 4), (PlanStatus::Ready, 0)] {
            let mut panel = review_panel(status);
            assert_eq!(panel.action, first);
            for up in [KeyCode::Up, KeyCode::Left] {
                panel.key(key_event(up));
                assert_eq!(panel.action, first);
            }
            for down in [KeyCode::Down, KeyCode::Right] {
                for expected in first + 1..ACTIONS.len() {
                    panel.key(key_event(down));
                    assert_eq!(panel.action, expected);
                    assert!(panel.action_enabled(panel.action));
                }
                panel.key(key_event(down));
                assert_eq!(panel.action, ACTIONS.len() - 1);
                for expected in (first..ACTIONS.len() - 1).rev() {
                    panel.key(key_event(KeyCode::Up));
                    assert_eq!(panel.action, expected);
                    assert!(panel.action_enabled(panel.action));
                }
            }
        }
    }

    #[test]
    fn plan_action_selection_stays_enabled_after_document_refresh() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Down));
        panel.sync_document(&document(PlanStatus::Draft, "- [ ] step"));
        assert_eq!(panel.action, 4);
        panel.key(key_event(KeyCode::Down));
        panel.set_review(PlanReview {
            token: token(),
            document: document(PlanStatus::Draft, "- [ ] step"),
            report: None,
        });
        assert_eq!(panel.action, 5);
        let mut updated = document(PlanStatus::Draft, "updated");
        updated.revision = 2;
        panel.set_review(PlanReview {
            token: token(),
            document: updated,
            report: None,
        });
        assert_eq!(panel.action, 4);
        let rows = render_panel(&mut panel, 26);
        assert!(
            rows.iter()
                .any(|row| row.contains("❯ Continue discussing Plan"))
        );
        assert!(!rows.iter().any(|row| row.contains("❯ Execute")));
    }

    #[test]
    fn executed_documents_are_read_only() {
        let mut panel = review_panel(PlanStatus::Executed);
        panel.key(key_event(KeyCode::Tab));
        for index in 0..ACTIONS.len() {
            assert!(!panel.action_enabled(index));
        }
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn esc_closes_and_discard_keeps_ready_state() {
        let mut panel = review_panel(PlanStatus::Ready);
        assert_eq!(panel.key(key_event(KeyCode::Esc)), Action::Close);
        panel.key(key_event(KeyCode::Char('5'))); // 讨论项
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
        assert!(panel.discussion.is_some());
        assert_eq!(panel.key(key_event(KeyCode::Esc)), Action::None);
        assert!(panel.discussion.is_none());
        assert_eq!(panel.document.status, PlanStatus::Ready);
    }

    #[test]
    fn discussion_injects_only_current_revision_report() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.report = Some(PlanReviewReport {
            plan_id: panel.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "report body".into(),
        });
        panel.discussion = Some(editor());
        assert!(matches!(
            panel.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Action::None
        ));
        let text = match panel.key(key_event(KeyCode::Enter)) {
            Action::Discuss(text) => text,
            other => panic!("expected discussion text, got {other:?}"),
        };
        assert!(text.starts_with('a'));
        assert!(text.contains("report body"));
        assert!(text.contains("revision 1"));

        // 过期报告不注入。
        let mut stale = review_panel(PlanStatus::Ready);
        stale.document.revision = 2;
        stale.report = Some(PlanReviewReport {
            plan_id: stale.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "stale".into(),
        });
        stale.discussion = Some(editor());
        stale
            .discussion
            .as_mut()
            .expect("discussion open")
            .insert_str("question");
        assert!(
            matches!(stale.key(key_event(KeyCode::Enter)), Action::Discuss(text) if !text.contains("stale"))
        );
    }

    #[test]
    fn progress_view_is_readonly_and_tracks_content() {
        let mut panel = Panel::progress(document(PlanStatus::Ready, "- [ ] step"));
        assert!(panel.token().is_none());
        for index in 0..ACTIONS.len() {
            assert!(!panel.action_enabled(index));
        }
        let mut updated = document(PlanStatus::Executed, "- [x] step");
        updated.revision = 2;
        panel.set_progress(&updated);
        assert_eq!(panel.document.revision, 2);
        assert_eq!(panel.document.content, "- [x] step");
        assert_eq!(task_progress(&panel.document.content), (1, 1));
    }

    #[test]
    fn review_action_dispatches_for_ready_documents_only() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Char('4')));
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::Review);
        let mut draft = review_panel(PlanStatus::Draft);
        draft.key(key_event(KeyCode::Char('4')));
        assert_eq!(draft.action, 4);
        assert!(draft.discussion.is_none());
    }

    #[test]
    fn degraded_view_hides_actions_until_safe_read_restores_token() {
        let report = PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 1,
            model_reference: "provider/model".into(),
            content: "findings".into(),
        };
        let mut panel = Panel::degraded(document(PlanStatus::Ready, "- [ ] step"), report);
        assert!(panel.token().is_none());
        for index in 0..ACTIONS.len() {
            assert!(!panel.action_enabled(index));
        }
        // 成功的安全读取恢复 token 与操作区。
        panel.set_review(PlanReview {
            token: token(),
            document: document(PlanStatus::Ready, "- [ ] step"),
            report: None,
        });
        assert!(panel.token().is_some());
        assert!(panel.action_enabled(1));
        assert!(!panel.on_review_body());
    }

    #[test]
    fn focus_review_body_requires_a_report() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.focus_review_body();
        assert!(!panel.on_review_body());
        panel.report = Some(PlanReviewReport {
            plan_id: panel.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "report".into(),
        });
        panel.focus_review_body();
        assert!(panel.on_review_body());
    }

    #[test]
    fn report_tabs_preserve_separate_scroll_positions() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.report = Some(PlanReviewReport {
            plan_id: panel.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "report".into(),
        });
        panel.focus = Focus::Body;
        panel.scroll[0] = 3;
        panel.scroll[1] = 5;
        panel.key(key_event(KeyCode::Right));
        assert_eq!(panel.tab, Tab::Review);
        assert_eq!(panel.scroll[1], 5);
        panel.key(key_event(KeyCode::Left));
        assert_eq!(panel.tab, Tab::Plan);
        assert_eq!(panel.scroll[0], 3);
    }

    #[test]
    fn plan_completed_view_never_enables_actions_or_accepts_input() {
        let mut panel = Panel::progress(document(PlanStatus::Completed, "- [x] step"));
        for index in 0..ACTIONS.len() {
            assert!(!panel.action_enabled(index));
        }
        panel.key(key_event(KeyCode::Char('1')));
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
        assert!(panel.paste("ignored"));
        assert!(!panel.input_focused());
        assert_eq!(panel.key(key_event(KeyCode::Esc)), Action::Close);
    }
}
