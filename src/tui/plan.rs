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
    widgets::{Block, Borders},
};
use ratatui_textarea::TextArea;

const ACTIONS: [&str; 4] = [
    "1. Execute (plan only)",
    "2. Execute (keep context)",
    "3. Continue discussing Plan",
    "4. Review Plan",
];
/// 打开面板时焦点在正文；有意切到操作区后默认选第 2 项（keep context）。
const DEFAULT_ACTION: usize = 1;
const HINT: &str = "Tab focus actions · ↑↓ select · Enter run · ←/→ tab · Esc close";
const READONLY_HINT: &str = "Read-only view · most recently synced version · Esc close";
const DISCUSS_HINT: &str = "Enter send · Ctrl + J newline · Esc discard";

/// 按字符数截断纯显示文本，超宽时以省略号结尾。
/// Highlighted choice/tab chip shared by the confirm dialog and the panel.
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmKind {
    New,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfirmAction {
    None,
    Cancel,
    Proceed,
}

/// 本地确认框（/plan new、/plan off）；默认选中取消，Esc 也取消。
pub(super) struct Confirm {
    kind: ConfirmKind,
    selected: usize,
}

impl Confirm {
    pub(super) fn new(kind: ConfirmKind) -> Self {
        Self { kind, selected: 0 }
    }

    pub(super) fn kind(&self) -> ConfirmKind {
        self.kind
    }

    fn message(&self) -> &'static str {
        match self.kind {
            ConfirmKind::New => "Start a new plan? The current plan file is kept.",
            ConfirmKind::Off => "Leave Plan mode without executing this plan?",
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> ConfirmAction {
        if key.kind == KeyEventKind::Release {
            return ConfirmAction::None;
        }
        if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            self.selected = 1 - self.selected;
            return ConfirmAction::None;
        }
        if !key.modifiers.is_empty()
            || !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            return ConfirmAction::None;
        }
        match key.code {
            KeyCode::Esc => ConfirmAction::Cancel,
            KeyCode::Enter => {
                if self.selected == 0 {
                    ConfirmAction::Cancel
                } else {
                    ConfirmAction::Proceed
                }
            }
            KeyCode::Left | KeyCode::Up => {
                self.selected = 0;
                ConfirmAction::None
            }
            KeyCode::Right | KeyCode::Down => {
                self.selected = 1;
                ConfirmAction::None
            }
            _ => ConfirmAction::None,
        }
    }

    pub(super) fn draw(&self, frame: &mut Frame, area: Rect) {
        if area.width < 12 || area.height < 5 {
            return;
        }
        let width = area.width.saturating_sub(4).min(64);
        let height = area.height.saturating_sub(4).min(5);
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
                " Confirm ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        frame.render_widget(
            Line::styled(self.message(), Style::default().fg(Color::White)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        if inner.height < 2 {
            return;
        }
        frame.render_widget(
            Line::from(vec![
                choice_span("Cancel", self.selected == 0),
                Span::raw("  "),
                choice_span("Confirm", self.selected == 1),
            ]),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
        if inner.height > 2 {
            frame.render_widget(
                Line::styled(
                    "←/→ select · Enter confirm · Esc cancel",
                    Style::default().fg(DIM),
                ),
                Rect::new(inner.x, inner.y + 2, inner.width, 1),
            );
        }
    }
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

    /// 审查完成后的刷新：焦点落在正文 Review 标签；操作区默认项保持 keep 不变。
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

    pub(super) fn document(&self) -> &PlanDocument {
        &self.document
    }

    /// 同一份计划的最新 workflow 快照：更新状态/内容，保留滚动与焦点；
    /// 返回正文是否变化。
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
            self.focus = Focus::Body;
            self.action = DEFAULT_ACTION;
        }
        if self.report.is_none() {
            self.tab = Tab::Plan;
        }
    }

    pub(super) fn action_enabled(&self, index: usize) -> bool {
        if self.readonly() || self.token.is_none() {
            return false;
        }
        let ready = self.document.status == PlanStatus::Ready;
        match index {
            0 | 1 | 3 => ready,
            2 => matches!(self.document.status, PlanStatus::Draft | PlanStatus::Ready),
            _ => false,
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        if let Some(editor) = &mut self.discussion {
            editor.insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
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
        if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            self.focus = match self.focus {
                Focus::Body => {
                    self.action = DEFAULT_ACTION;
                    Focus::Actions
                }
                Focus::Actions => Focus::Body,
            };
            return Action::None;
        }
        match self.focus {
            Focus::Body => self.body_key(key),
            Focus::Actions => self.action_key(key),
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
                self.action = self.action.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Right => {
                self.action = (self.action + 1).min(ACTIONS.len() - 1);
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if !self.action_enabled(self.action) {
                    return Action::None;
                }
                return match self.action {
                    0 => Action::Approve(ExecutionContext::Fresh),
                    1 => Action::Approve(ExecutionContext::Keep),
                    2 => {
                        self.discussion = Some(editor());
                        Action::None
                    }
                    3 => Action::Review,
                    _ => Action::None,
                };
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
    fn compose_discussion(&self, text: String) -> String {
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
            PlanStatus::Abandoned => ("Abandoned", MUTED),
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, progress: (usize, usize)) {
        if area.width < 6 || area.height < 3 {
            return;
        }
        let title = if self.token.is_none() || self.document.status == PlanStatus::Executed {
            format!(" Plan · {}/{} ", progress.0, progress.1)
        } else {
            format!(" Plan · r{} ", self.document.revision)
        };
        let block = Block::default().borders(Borders::ALL).title(Span::styled(
            title,
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        let mut body = block.inner(area);
        frame.render_widget(block, area);

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
                    spans.push(Span::styled(
                        format!(" {}", self.document.path()),
                        Style::default().fg(MUTED),
                    ));
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

        // 底部操作区/只读提示。
        let footer_height = if self.readonly() || self.token.is_none() {
            1u16
        } else {
            ACTIONS.len() as u16 + 1
        };
        let footer_height = footer_height.min(body.height);
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
            if self.readonly() || self.token.is_none() {
                frame.render_widget(
                    Line::styled(format!(" {READONLY_HINT}"), Style::default().fg(MUTED)),
                    footer,
                );
            } else {
                let mut height = footer.height as usize;
                for (index, label) in ACTIONS.iter().enumerate() {
                    if height == 0 {
                        break;
                    }
                    height -= 1;
                    let selected = self.focus == Focus::Actions && self.action == index;
                    let style = if !self.action_enabled(index) {
                        Style::default().fg(DIM)
                    } else if selected {
                        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    let marker = if selected { "❯ " } else { "  " };
                    frame.render_widget(
                        Line::from(Span::styled(format!("{marker}{label}"), style)),
                        Rect::new(footer.x, footer.y + index as u16, footer.width, 1),
                    );
                }
                if height > 0 {
                    frame.render_widget(
                        Line::styled(format!(" {HINT}"), Style::default().fg(DIM)),
                        Rect::new(footer.x, footer.y + ACTIONS.len() as u16, footer.width, 1),
                    );
                }
            }
        }

        if self.discussion.is_some() {
            self.draw_discussion(frame, area);
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
        }
    }

    fn token() -> ReviewToken {
        ReviewToken {
            session_id: "session".into(),
            plan_id: "0123456789abcdef0123456789abcdef".into(),
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

    #[test]
    fn focus_starts_on_body_and_actions_default_to_keep() {
        let mut panel = review_panel(PlanStatus::Ready);
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None); // 正文焦点不落执行
        assert_eq!(panel.key(key_event(KeyCode::Tab)), Action::None);
        assert_eq!(panel.action, DEFAULT_ACTION);
        assert_eq!(
            panel.key(key_event(KeyCode::Enter)),
            Action::Approve(ExecutionContext::Keep)
        );
    }

    #[test]
    fn draft_allows_discussion_only_and_ready_enables_execution() {
        let mut draft = review_panel(PlanStatus::Draft);
        draft.key(key_event(KeyCode::Tab));
        assert_eq!(draft.key(key_event(KeyCode::Up)), Action::None);
        assert_eq!(draft.key(key_event(KeyCode::Up)), Action::None);
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::None); // Fresh 需 Ready
        assert_eq!(draft.key(key_event(KeyCode::Down)), Action::None);
        assert_eq!(draft.key(key_event(KeyCode::Down)), Action::None);
        assert_eq!(draft.key(key_event(KeyCode::Down)), Action::None);
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::None); // Review 禁用
        assert_eq!(draft.key(key_event(KeyCode::Up)), Action::None); // 回到讨论项
        match draft.key(key_event(KeyCode::Enter)) {
            Action::None => {}
            other => panic!("discussion opens the editor, got {other:?}"),
        }
        assert!(draft.discussion.is_some());
        assert!(!draft.action_enabled(0));

        let mut ready = review_panel(PlanStatus::Ready);
        ready.key(key_event(KeyCode::Tab));
        ready.key(key_event(KeyCode::Up));
        assert_eq!(
            ready.key(key_event(KeyCode::Enter)),
            Action::Approve(ExecutionContext::Fresh)
        );
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
        panel.key(key_event(KeyCode::Tab));
        panel.key(key_event(KeyCode::Down)); // 讨论项
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
    fn confirm_defaults_to_cancel() {
        let mut confirm = Confirm::new(ConfirmKind::Off);
        assert_eq!(
            confirm.key(key_event(KeyCode::Enter)),
            ConfirmAction::Cancel
        );
        assert_eq!(confirm.key(key_event(KeyCode::Esc)), ConfirmAction::Cancel);
        assert_eq!(confirm.key(key_event(KeyCode::Right)), ConfirmAction::None);
        assert_eq!(
            confirm.key(key_event(KeyCode::Enter)),
            ConfirmAction::Proceed
        );
    }

    #[test]
    fn review_action_dispatches_for_ready_documents_only() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Tab));
        for _ in 0..3 {
            panel.key(key_event(KeyCode::Down));
        }
        assert_eq!(panel.action, 3);
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::Review);

        let mut draft = review_panel(PlanStatus::Draft);
        draft.key(key_event(KeyCode::Tab));
        for _ in 0..3 {
            draft.key(key_event(KeyCode::Down));
        }
        assert!(!draft.action_enabled(3));
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::None);
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
        panel.scroll[0] = 3;
        panel.scroll[1] = 5;
        panel.key(key_event(KeyCode::Right));
        assert_eq!(panel.tab, Tab::Review);
        assert_eq!(panel.scroll[1], 5);
        panel.key(key_event(KeyCode::Left));
        assert_eq!(panel.tab, Tab::Plan);
        assert_eq!(panel.scroll[0], 3);
    }
}
