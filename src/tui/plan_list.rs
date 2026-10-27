//! 项目级 /plan-list 管理面板：只渲染 worker 发布的快照，磁盘/登记 I/O 全部留在 worker。
//! 键盘与绘制路径不做任何 IO；操作只携带 ID，worker 按 ID 重查登记并核对当前路径与精确正文。
use super::transcript::wrap_block;
use super::transcript_style::{ACCENT, DIM, MUTED, REMOVED, display_text};
use crate::agent::plan_management::{PlanListEntry, PlanListing, PlanOpenIntent};
use crate::plan::PlanLocation;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::time::SystemTime;

pub(super) const HINT: &str = "↑/↓ select · ←/→ or Tab category · Enter open · / search · F5 refresh · n new · c continue · e actions · r rename · d delete · Esc close";
/// Done 只读详情显示返回列表的提示。
pub(super) const DETAIL_HINT: &str = "Read-only completed plan · ↑/↓ scroll · Esc back to list";
const SEARCH_HINT: &str = "search: Enter apply · Esc exit · Ctrl+U clear";
const RENAME_HINT: &str = "rename: Enter submit · Esc cancel";
const CONFIRM_HINT: &str = "Tab/arrows select · Enter confirm · Esc cancel";
const DELETE_NOTICE: &str = "Delete the plan file only; code and sessions are not rolled back.";
/// 空列表仍提供新建入口；其余空态说明当前分类/过滤。
const EMPTY_NEW_PLAN: &str = "No saved plans. Press n to start a new plan.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Loading,
    Ready,
    Reading,
    Opening,
    Renaming,
    Deleting,
}

impl Phase {
    fn busy_label(self) -> Option<&'static str> {
        match self {
            Phase::Loading => Some("Loading plans…"),
            Phase::Reading => Some("Reading plan…"),
            Phase::Opening => Some("Opening plan…"),
            Phase::Renaming => Some("Renaming plan…"),
            Phase::Deleting => Some("Deleting plan…"),
            Phase::Ready => None,
        }
    }
}

pub(super) enum Action {
    None,
    Close,
    Refresh,
    /// Enter：读取 selection；Drafts/Plans 打开操作菜单，Done 只读展示。
    Preview(String),
    /// c/e：先读取 selection，再按意图打开只携带计划的新会话。
    Open(String, PlanOpenIntent),
    /// n：关闭列表并进入 Plan 待命；未落盘草稿由 App 复用防丢失门禁拒绝。
    NewPlan,
    /// r：先读取 selection，再提交重命名（新文件名已就绪）。
    Rename(String, String),
    /// d 确认后：先读取 selection，再删除。
    Delete(String),
    /// 面板内可见提示：禁用操作的原因等，不关闭列表。
    Message(String),
}

/// 读取 selection 后继续执行的列表动作；由 App 在 ListedPlanRead 后消费。
pub(super) enum PendingOp {
    Preview,
    Open(PlanOpenIntent),
    Rename(String),
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    All,
    Drafts,
    Plans,
    Done,
}

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::All => "All",
            Tab::Drafts => "Drafts",
            Tab::Plans => "Plans",
            Tab::Done => "Done",
        }
    }

    fn next(self) -> Self {
        match self {
            Tab::All => Tab::Drafts,
            Tab::Drafts => Tab::Plans,
            Tab::Plans => Tab::Done,
            Tab::Done => Tab::All,
        }
    }

    fn previous(self) -> Self {
        match self {
            Tab::All => Tab::Done,
            Tab::Drafts => Tab::All,
            Tab::Plans => Tab::Drafts,
            Tab::Done => Tab::Plans,
        }
    }

    fn matches(self, location: PlanLocation) -> bool {
        match self {
            Tab::All => true,
            Tab::Drafts => location == PlanLocation::Drafts,
            Tab::Plans => location == PlanLocation::Plans,
            Tab::Done => location == PlanLocation::Done,
        }
    }
}

struct ConfirmDelete {
    id: String,
    path: String,
    delete_selected: bool,
    width: Option<u16>,
    lines: Vec<Line<'static>>,
}

pub(super) struct Panel {
    phase: Phase,
    listing: PlanListing,
    current_id: Option<String>,
    tab: Tab,
    query: String,
    searching: bool,
    filtered: Vec<usize>,
    selected: usize,
    selected_id: Option<String>,
    offset: usize,
    page_size: usize,
    rename: Option<String>,
    confirm: Option<ConfirmDelete>,
    error: Option<String>,
}

impl Panel {
    pub(super) fn new(current_id: Option<String>) -> Self {
        Self {
            phase: Phase::Loading,
            listing: PlanListing::default(),
            current_id,
            tab: Tab::All,
            query: String::new(),
            searching: false,
            filtered: Vec::new(),
            selected: 0,
            selected_id: None,
            offset: 0,
            page_size: 1,
            rename: None,
            confirm: None,
            error: None,
        }
    }

    pub(super) fn set_loading(&mut self) {
        self.phase = Phase::Loading;
        self.error = None;
    }

    pub(super) fn set_reading(&mut self) {
        self.phase = Phase::Reading;
    }

    pub(super) fn set_opening(&mut self) {
        self.phase = Phase::Opening;
    }

    pub(super) fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
    }

    /// worker 成功返回的新列表：保持分类/搜索，尽量保持同 ID 选中；
    /// 删除后 ID 消失则自动落到同位置的邻近项。
    pub(super) fn set_listing(&mut self, listing: PlanListing) {
        self.listing = listing;
        self.phase = Phase::Ready;
        self.error = None;
        self.confirm = None;
        self.rename = None;
        let previous_id = self.selected_id.take();
        self.refilter();
        if let Some(id) = previous_id
            && let Some(index) = self
                .filtered
                .iter()
                .position(|&index| self.listing.entries[index].id == id)
        {
            self.selected = index;
            self.selected_id = Some(id);
        } else {
            self.clamp_selection();
        }
        self.offset = self.offset.min(self.selected);
    }

    /// 失败保留旧列表与输入，只展示可见错误。
    pub(super) fn set_error(&mut self, error: String) {
        self.error = Some(display_text(&error).into_owned());
        self.phase = Phase::Ready;
        self.confirm = None;
        self.rename = None;
    }

    fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        self.filtered = self
            .listing
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| self.tab.matches(entry.location))
            .filter(|(_, entry)| {
                query.split_whitespace().all(|word| {
                    entry.title.to_lowercase().contains(word)
                        || entry.file_name.to_lowercase().contains(word)
                        || relative_path(entry).to_lowercase().contains(word)
                })
            })
            .map(|(index, _)| index)
            .collect();
    }

    fn reset_selection(&mut self) {
        self.selected = 0;
        self.offset = 0;
        self.selected_id = self
            .filtered
            .first()
            .map(|&index| self.listing.entries[index].id.clone());
    }

    fn clamp_selection(&mut self) {
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.selected_id = self
            .filtered
            .get(self.selected)
            .map(|&index| self.listing.entries[index].id.clone());
    }

    fn selected_entry(&self) -> Option<&PlanListEntry> {
        self.filtered
            .get(self.selected)
            .map(|&index| &self.listing.entries[index])
    }

    fn move_selection(&mut self, up: bool, count: usize) {
        self.selected = if up {
            self.selected.saturating_sub(count)
        } else {
            self.selected
                .saturating_add(count)
                .min(self.filtered.len().saturating_sub(1))
        };
        self.offset = self.offset.min(self.selected);
        self.selected_id = self
            .filtered
            .get(self.selected)
            .map(|&index| self.listing.entries[index].id.clone());
    }

    pub(super) fn paste(&mut self, text: &str) {
        if !matches!(self.phase, Phase::Ready) {
            return;
        }
        let cleaned: String = text
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        if let Some(name) = &mut self.rename {
            name.push_str(&cleaned);
        } else if self.searching {
            self.query.push_str(&cleaned);
            self.refilter();
            self.reset_selection();
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        match self.phase {
            Phase::Loading => {
                return if key.code == KeyCode::Esc {
                    Action::Close
                } else {
                    Action::None
                };
            }
            // 读取/打开/重命名/删除进行中：等待 worker 结果，不接收按键。
            Phase::Reading | Phase::Opening | Phase::Renaming | Phase::Deleting => {
                return Action::None;
            }
            Phase::Ready => {}
        }
        // 确认删除对话框优先：默认 Cancel，Tab/方向显式选择 Delete。
        if let Some(mut confirm) = self.confirm.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                    confirm.delete_selected = !confirm.delete_selected;
                    self.confirm = Some(confirm);
                }
                KeyCode::Enter if key.kind == KeyEventKind::Press => {
                    if confirm.delete_selected {
                        return Action::Delete(confirm.id);
                    }
                    // 默认 Cancel：关闭对话框，不删除。
                }
                _ => self.confirm = Some(confirm),
            }
            return Action::None;
        }
        // 重命名单行输入：预填文件名，Enter 提交，Esc 取消。
        if self.rename.is_some() {
            return self.rename_key(key);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // 搜索模式只接收查询字符/粘贴；r/d/n/c/e 不会成为搜索字符或触发动作。
        if self.searching {
            match key.code {
                KeyCode::Esc => self.searching = false,
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refilter();
                    self.reset_selection();
                }
                KeyCode::Char('u') if control => {
                    self.query.clear();
                    self.refilter();
                    self.reset_selection();
                }
                KeyCode::Char(character) if !control && !alt => {
                    self.query.push(character);
                    self.refilter();
                    self.reset_selection();
                }
                _ => {}
            }
            return Action::None;
        }
        // 快照选中项，避免动作分支中同时借用与修改面板状态。
        let selected = self.selected_entry().map(|entry| {
            (
                entry.id.clone(),
                entry.file_name.clone(),
                entry.error.clone(),
                entry.location,
                entry.total,
            )
        });
        match key.code {
            KeyCode::Esc => Action::Close,
            KeyCode::Enter => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((id, _, None, _, _)) if !id.is_empty() => Action::Preview(id),
                Some(_) => Action::Message("This file cannot be managed as a plan.".into()),
                None => Action::None,
            },
            KeyCode::Left | KeyCode::Right => {
                self.tab = if key.code == KeyCode::Left {
                    self.tab.previous()
                } else {
                    self.tab.next()
                };
                self.refilter();
                self.reset_selection();
                Action::None
            }
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.refilter();
                self.reset_selection();
                Action::None
            }
            KeyCode::BackTab => {
                self.tab = self.tab.previous();
                self.refilter();
                self.reset_selection();
                Action::None
            }
            KeyCode::Up => {
                self.move_selection(true, 1);
                Action::None
            }
            KeyCode::Down => {
                self.move_selection(false, 1);
                Action::None
            }
            KeyCode::PageUp => {
                self.move_selection(true, self.page_size);
                Action::None
            }
            KeyCode::PageDown => {
                self.move_selection(false, self.page_size);
                Action::None
            }
            KeyCode::Home => {
                self.move_selection(true, self.selected);
                Action::None
            }
            KeyCode::End => {
                self.move_selection(false, self.filtered.len());
                Action::None
            }
            KeyCode::F(5) => {
                self.error = None;
                self.set_loading();
                Action::Refresh
            }
            KeyCode::Char('/') if !control && !alt => {
                self.searching = true;
                Action::None
            }
            KeyCode::Char('n') if !control && !alt => Action::NewPlan,
            KeyCode::Char('c') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((id, _, None, PlanLocation::Drafts, _)) => {
                    Action::Open(id, PlanOpenIntent::ContinueDraft)
                }
                Some(_) => Action::Message("Continue is only available for drafts.".into()),
                None => Action::None,
            },
            KeyCode::Char('e') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((_, _, None, PlanLocation::Done, _)) => {
                    Action::Message("Completed plans are read-only.".into())
                }
                Some((id, _, None, _, _)) => Action::Open(id, PlanOpenIntent::Execute),
                None => Action::None,
            },
            KeyCode::Char('r') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((_, file_name, None, _, _)) => {
                    self.rename = Some(file_name);
                    Action::None
                }
                None => Action::None,
            },
            KeyCode::Char('d') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((id, file_name, None, location, _)) => {
                    self.confirm = Some(ConfirmDelete {
                        id,
                        path: format!(".pp/{}/{}", location.dir_name(), file_name),
                        delete_selected: false,
                        width: None,
                        lines: Vec::new(),
                    });
                    Action::None
                }
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn rename_key(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.rename = None;
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                let current = self
                    .selected_entry()
                    .map(|entry| (entry.id.clone(), entry.file_name.clone()));
                let Some((id, file_name)) = current else {
                    self.rename = None;
                    return Action::None;
                };
                let name = self
                    .rename
                    .take()
                    .expect("rename editor open")
                    .trim()
                    .to_owned();
                if name.is_empty() || name == file_name {
                    // 空名与同名都视为无操作：直接关闭编辑器。
                    return Action::None;
                }
                return Action::Rename(id, name);
            }
            KeyCode::Backspace => {
                if let Some(name) = &mut self.rename {
                    name.pop();
                }
            }
            KeyCode::Char('u') if control => {
                if let Some(name) = &mut self.rename {
                    name.clear();
                }
            }
            KeyCode::Char(character) if !control && !alt => {
                if let Some(name) = &mut self.rename {
                    name.push(character);
                }
            }
            _ => {}
        }
        Action::None
    }

    pub(super) fn wheel(&mut self, up: bool) {
        if matches!(self.phase, Phase::Ready) && self.confirm.is_none() && self.rename.is_none() {
            self.move_selection(up, 1);
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        if area.is_empty() {
            return;
        }
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(MUTED))
                .title(Line::styled(
                    " Plan list ",
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                )),
            area,
        );
        if area.height < 2 {
            return;
        }
        let mut y = area.y + 1;
        let bottom = area.bottom();
        // 分类标签行：All / Drafts / Plans / Done。
        if y < bottom {
            let mut spans = Vec::with_capacity(8);
            for tab in [Tab::All, Tab::Drafts, Tab::Plans, Tab::Done] {
                spans.push(Span::styled(
                    format!(" {} ", tab.label()),
                    if tab == self.tab {
                        Style::default()
                            .fg(ACCENT)
                            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                    } else {
                        Style::default().fg(MUTED)
                    },
                ));
                spans.push(Span::raw(" "));
            }
            frame.render_widget(Line::from(spans), Rect::new(area.x, y, area.width, 1));
            y += 1;
        }
        // 输入行：重命名编辑器优先，其次搜索；query 保留时始终可见。
        let mut cursor = None;
        if y < bottom {
            if let Some(name) = &self.rename {
                let line = format!("rename> {name}");
                frame.render_widget(
                    Line::styled(line.clone(), Style::default().fg(ACCENT)),
                    Rect::new(area.x, y, area.width, 1),
                );
                cursor = Some((area.x + 8 + Line::from(name.as_str()).width() as u16, y));
            } else if self.searching || !self.query.is_empty() {
                let line = format!("/{}", self.query);
                frame.render_widget(
                    Line::styled(line.clone(), Style::default().fg(ACCENT)),
                    Rect::new(area.x, y, area.width, 1),
                );
                if self.searching {
                    cursor = Some((
                        area.x + 1 + Line::from(self.query.as_str()).width() as u16,
                        y,
                    ));
                }
            }
            y += 1;
        }
        // 错误行：保留旧列表与输入，仅展示可见提示。
        if y < bottom
            && let Some(error) = &self.error
        {
            frame.render_widget(
                Line::styled(error.clone(), Style::default().fg(REMOVED)),
                Rect::new(area.x, y, area.width, 1),
            );
            y += 1;
        }
        // 底部提示与诊断；诊断最多占两行，超出截断。
        let diagnostics = self.listing.diagnostics.len().min(2) as u16;
        let hint_y = bottom.saturating_sub(1 + diagnostics);
        let list_bottom = hint_y;
        if y < list_bottom {
            let list_area = Rect::new(area.x, y, area.width, list_bottom - y);
            self.draw_entries(frame, list_area);
        }
        for (index, diagnostic) in self.listing.diagnostics.iter().take(2).enumerate() {
            let y = hint_y.saturating_sub(diagnostics) + index as u16;
            frame.render_widget(
                Line::styled(
                    display_text(diagnostic).into_owned(),
                    Style::default().fg(DIM),
                ),
                Rect::new(area.x, y, area.width, 1),
            );
        }
        let hint = if self.rename.is_some() {
            RENAME_HINT
        } else if self.confirm.is_some() {
            CONFIRM_HINT
        } else if self.searching {
            SEARCH_HINT
        } else {
            HINT
        };
        frame.render_widget(
            Line::styled(format!(" {hint}"), Style::default().fg(DIM)),
            Rect::new(area.x, bottom - 1, area.width, 1),
        );
        if self.confirm.is_some() {
            self.draw_confirmation(frame, area);
        }
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x, y));
        }
    }

    /// 居中确认删除框：完整相对路径、固定提示与默认 Cancel 按钮行。
    fn draw_confirmation(&mut self, frame: &mut Frame, area: Rect) {
        if area.width < 28 || area.height < 9 {
            return;
        }
        let width = area.width.saturating_sub(4).min(72);
        let height = 7.min(area.height.saturating_sub(2));
        let area = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(REMOVED))
            .title(Span::styled(
                " Delete plan? ",
                Style::default().fg(REMOVED).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let Some(confirm) = &mut self.confirm else {
            return;
        };
        if confirm.width != Some(inner.width) {
            let mut lines = wrap_block(
                Line::raw(format!("Path: {}", confirm.path)),
                inner.width,
                "",
            );
            lines.extend(wrap_block(Line::raw(DELETE_NOTICE), inner.width, ""));
            confirm.lines = lines;
            confirm.width = Some(inner.width);
        }
        if inner.height > 1 {
            for (row, line) in confirm
                .lines
                .iter()
                .take(usize::from(inner.height - 1))
                .enumerate()
            {
                frame.render_widget(
                    line.clone(),
                    Rect::new(inner.x, inner.y + row as u16, inner.width, 1),
                );
            }
        }
        // 说明可能折行；按钮行固定在框底，空间不足时优先保证可见。
        let button = |label: &str, selected: bool| {
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
        };
        frame.render_widget(
            Line::from(vec![
                button("Cancel", !confirm.delete_selected),
                Span::raw("  "),
                button("Delete", confirm.delete_selected),
            ]),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }

    fn draw_entries(&mut self, frame: &mut Frame, area: Rect) {
        let phase = self.phase;
        if let Some(label) = phase.busy_label() {
            frame.render_widget(
                Paragraph::new(label).style(Style::default().fg(ACCENT)),
                area,
            );
            return;
        }
        if self.filtered.is_empty() {
            let message = if !self.query.is_empty() {
                "No matching plans."
            } else if self.tab != Tab::All {
                "No plans in this category."
            } else {
                EMPTY_NEW_PLAN
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(MUTED)),
                area,
            );
            return;
        }
        self.page_size = usize::from(area.height.max(1));
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + self.page_size {
            self.offset = self.selected + 1 - self.page_size;
        }
        self.offset = self
            .offset
            .min(self.filtered.len().saturating_sub(self.page_size));
        let now = SystemTime::now();
        for (row, &index) in self
            .filtered
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.page_size)
        {
            let y = area.y + (row - self.offset) as u16;
            if y >= area.bottom() {
                break;
            }
            let entry = &self.listing.entries[index];
            let selected = row == self.selected;
            let marker = if selected { "❯ " } else { "  " };
            let marker_style = if selected {
                Style::default().fg(ACCENT)
            } else {
                Style::default()
            };
            let mut spans = vec![Span::styled(marker, marker_style)];
            if let Some(error) = &entry.error {
                spans.push(Span::styled(
                    display_text(&entry.file_name).into_owned(),
                    Style::default().fg(REMOVED),
                ));
                spans.push(Span::styled(
                    format!("  {}", display_text(error)),
                    Style::default().fg(REMOVED),
                ));
            } else {
                spans.push(Span::raw(display_text(&entry.file_name).into_owned()));
                spans.push(Span::styled(
                    format!("  {}", display_text(&entry.title)),
                    Style::default().fg(MUTED),
                ));
                spans.push(Span::styled(
                    format!("  [{}]", category_label(entry.location)),
                    Style::default().fg(MUTED),
                ));
                spans.push(Span::raw(format!("  {}", progress_label(entry))));
                spans.push(Span::styled(
                    format!("  {}", format_age(entry.modified, now)),
                    Style::default().fg(MUTED),
                ));
                if Some(entry.id.as_str()) == self.current_id.as_deref() {
                    spans.push(Span::styled("  Current", Style::default().fg(ACCENT)));
                }
            }
            let line = Line::from(spans);
            frame.render_widget(line, Rect::new(area.x, y, area.width, 1));
        }
    }
}

fn relative_path(entry: &PlanListEntry) -> String {
    format!(".pp/{}/{}", entry.location.dir_name(), entry.file_name)
}

fn category_label(location: PlanLocation) -> &'static str {
    match location {
        PlanLocation::Drafts => "草稿",
        PlanLocation::Plans => "待执行",
        PlanLocation::Done => "已完成",
    }
}

fn progress_label(entry: &PlanListEntry) -> String {
    if entry.total == 0 {
        "—".to_owned()
    } else {
        format!("{}/{}", entry.completed, entry.total)
    }
}

fn format_age(time: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(time).unwrap_or_default().as_secs();
    let (value, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3600 => (seconds / 60, "minute"),
        3600..86_400 => (seconds / 3600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn entry(
        id: &str,
        file_name: &str,
        location: PlanLocation,
        title: &str,
        completed: usize,
        total: usize,
    ) -> PlanListEntry {
        PlanListEntry {
            id: id.into(),
            file_name: file_name.into(),
            location,
            title: title.into(),
            completed,
            total,
            modified: UNIX_EPOCH,
            error: None,
        }
    }

    fn listing(entries: Vec<PlanListEntry>) -> PlanListing {
        PlanListing {
            entries,
            diagnostics: Vec::new(),
        }
    }

    fn ready_panel(entries: Vec<PlanListEntry>) -> Panel {
        let mut panel = Panel::new(None);
        panel.set_listing(listing(entries));
        panel
    }

    fn selected_file(panel: &Panel) -> String {
        panel.selected_entry().expect("selection").file_name.clone()
    }

    #[test]
    fn search_filters_multi_word_case_insensitive_over_title_name_and_path() {
        let mut panel = ready_panel(vec![
            entry("a", "fix-config.md", PlanLocation::Drafts, "修复配置", 0, 2),
            entry("b", "other.md", PlanLocation::Plans, "Login flow", 1, 3),
            entry("c", "done-plan.md", PlanLocation::Done, "Archive", 2, 2),
        ]);
        panel.key(key(KeyCode::Char('/')));
        for character in "LOGIN Flow".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        // 多词 AND 必须在同一条目内全部命中（大小写无关）：仅标题含两词的 b。
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "other.md");
        // AND 跨条目不成立：fix 与 login 分属不同条目，结果为空。
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "fix login".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert!(panel.filtered.is_empty());
        // 文件名匹配：仅 a 含 fix。
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "FIX".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "fix-config.md");
        // 相对路径匹配：目录名与文件名可同时作为多词命中同一条目。
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "drafts fix".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "fix-config.md");
        // Esc 退出搜索保留 query；重新进入搜索后 Ctrl+U 清空恢复全部。
        panel.key(key(KeyCode::Esc));
        assert!(!panel.searching);
        assert_eq!(panel.filtered.len(), 1);
        panel.key(key(KeyCode::Char('/')));
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(panel.query.is_empty());
        assert_eq!(panel.filtered.len(), 3);
    }

    #[test]
    fn search_mode_never_triggers_actions_or_eats_navigation() {
        let mut panel = ready_panel(vec![entry(
            "a",
            "plan.md",
            PlanLocation::Drafts,
            "Title",
            0,
            1,
        )]);
        panel.key(key(KeyCode::Char('/')));
        // 搜索模式中 r/d/n/c/e 只进入 query，不触发动作。
        for character in "rdnce".chars() {
            assert!(matches!(
                panel.key(key(KeyCode::Char(character))),
                Action::None
            ));
        }
        assert_eq!(panel.query, "rdnce");
        assert!(panel.rename.is_none());
        assert!(panel.confirm.is_none());
        // Enter 回列表，query 继续过滤；无命中时不 panic 且动作被拒绝。
        panel.key(key(KeyCode::Enter));
        assert!(!panel.searching);
        assert!(panel.filtered.is_empty());
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(matches!(panel.key(key(KeyCode::Char('c'))), Action::None));
    }

    #[test]
    fn tabs_filter_categories_and_cycle_in_both_directions() {
        let mut panel = ready_panel(vec![
            entry("a", "d.md", PlanLocation::Drafts, "Draft", 0, 1),
            entry("b", "p.md", PlanLocation::Plans, "Ready", 0, 2),
            entry("c", "f.md", PlanLocation::Done, "Done", 2, 2),
        ]);
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "d.md");
        panel.key(key(KeyCode::Tab));
        assert_eq!(selected_file(&panel), "p.md");
        panel.key(key(KeyCode::Tab));
        assert_eq!(selected_file(&panel), "f.md");
        // 左方向回退：Done → Plans。
        panel.key(key(KeyCode::Left));
        assert_eq!(selected_file(&panel), "p.md");
        panel.key(key(KeyCode::BackTab));
        panel.key(key(KeyCode::BackTab));
        assert_eq!(panel.filtered.len(), 3);
    }

    #[test]
    fn refresh_keeps_same_id_selected_and_delete_falls_to_neighbor() {
        let mut panel = ready_panel(vec![
            entry("a", "first.md", PlanLocation::Drafts, "First", 0, 1),
            entry("b", "second.md", PlanLocation::Drafts, "Second", 0, 1),
            entry("c", "third.md", PlanLocation::Drafts, "Third", 0, 1),
        ]);
        panel.key(key(KeyCode::Down));
        assert_eq!(selected_file(&panel), "second.md");
        // 重命名后同 ID 保持在选中位置。
        panel.set_listing(listing(vec![
            entry("a", "first.md", PlanLocation::Drafts, "First", 0, 1),
            entry("b", "renamed.md", PlanLocation::Drafts, "Second", 0, 1),
            entry("c", "third.md", PlanLocation::Drafts, "Third", 0, 1),
        ]));
        assert_eq!(selected_file(&panel), "renamed.md");
        // 删除后同 ID 消失，落到同位置邻近项。
        panel.set_listing(listing(vec![
            entry("a", "first.md", PlanLocation::Drafts, "First", 0, 1),
            entry("c", "third.md", PlanLocation::Drafts, "Third", 0, 1),
        ]));
        assert_eq!(selected_file(&panel), "third.md");
        // 分类/搜索在刷新后保持。
        panel.key(key(KeyCode::Right));
        panel.key(key(KeyCode::Char('/')));
        for character in "third".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        panel.set_listing(listing(vec![entry(
            "c",
            "third.md",
            PlanLocation::Drafts,
            "Third",
            0,
            1,
        )]));
        assert!(matches!(panel.tab, Tab::Drafts));
        assert_eq!(panel.query, "third");
        assert_eq!(selected_file(&panel), "third.md");
    }

    #[test]
    fn confirm_delete_defaults_to_cancel_and_requires_explicit_delete() {
        let mut panel = ready_panel(vec![entry(
            "a",
            "plan.md",
            PlanLocation::Plans,
            "Title",
            0,
            1,
        )]);
        panel.key(key(KeyCode::Char('d')));
        assert!(panel.confirm.is_some());
        // 默认 Cancel：Enter 关闭对话框，不删除。
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.confirm.is_none());
        // Esc 取消。
        panel.key(key(KeyCode::Char('d')));
        panel.key(key(KeyCode::Esc));
        assert!(panel.confirm.is_none());
        // Tab 显式选择 Delete 后 Enter 才删除。
        panel.key(key(KeyCode::Char('d')));
        panel.key(key(KeyCode::Tab));
        assert!(matches!(
            panel.key(key(KeyCode::Enter)),
            Action::Delete(id) if id == "a"
        ));
        assert!(panel.confirm.is_none());
    }

    #[test]
    fn rename_prefills_name_and_submits_or_cancels() {
        let mut panel = ready_panel(vec![entry(
            "a",
            "old-name.md",
            PlanLocation::Drafts,
            "Title",
            0,
            1,
        )]);
        panel.key(key(KeyCode::Char('r')));
        assert_eq!(panel.rename.as_deref(), Some("old-name.md"));
        // Esc 取消，不产生动作。
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        assert!(panel.rename.is_none());
        // 追加编辑后 Enter 提交新名。
        panel.key(key(KeyCode::Char('r')));
        for character in "v2".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert!(matches!(
            panel.key(key(KeyCode::Enter)),
            Action::Rename(id, name) if id == "a" && name == "old-name.mdv2"
        ));
        // 同名无操作成功。
        panel.key(key(KeyCode::Char('r')));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(panel.rename.is_none());
    }

    #[test]
    fn done_and_error_entries_restrict_actions() {
        let mut panel = ready_panel(vec![
            entry("a", "done.md", PlanLocation::Done, "Done", 2, 2),
            entry("b", "empty.md", PlanLocation::Drafts, "Empty", 0, 0),
            PlanListEntry {
                id: String::new(),
                file_name: "broken.md".into(),
                location: PlanLocation::Drafts,
                title: "broken.md".into(),
                completed: 0,
                total: 0,
                modified: UNIX_EPOCH,
                error: Some("invalid utf-8".into()),
            },
        ]);
        // Done：预览可用，c/e 拒绝，r/d 仍可用。
        assert!(matches!(
            panel.key(key(KeyCode::Enter)),
            Action::Preview(id) if id == "a"
        ));
        assert!(matches!(
            panel.key(key(KeyCode::Char('c'))),
            Action::Message(_)
        ));
        assert!(matches!(
            panel.key(key(KeyCode::Char('e'))),
            Action::Message(_)
        ));
        // 零任务：仍能打开操作菜单，执行由审批门禁拒绝。
        panel.key(key(KeyCode::Down));
        assert!(matches!(
            panel.key(key(KeyCode::Char('e'))),
            Action::Open(_, PlanOpenIntent::Execute)
        ));
        // 错误条目：全部动作只产生可见提示。
        panel.key(key(KeyCode::Down));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Message(_)));
        assert!(matches!(
            panel.key(key(KeyCode::Char('r'))),
            Action::Message(_)
        ));
        assert!(matches!(
            panel.key(key(KeyCode::Char('d'))),
            Action::Message(_)
        ));
        assert!(matches!(
            panel.key(key(KeyCode::Char('c'))),
            Action::Message(_)
        ));
        // 加载中除 Esc 外不接收按键。
        let mut loading = Panel::new(None);
        assert!(matches!(loading.key(key(KeyCode::Char('n'))), Action::None));
        assert!(matches!(loading.key(key(KeyCode::Esc)), Action::Close));
    }
}
