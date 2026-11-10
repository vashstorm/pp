//! Agent Hub：Agents 总览、完整 Activity 与独立 child 详情。
//!
//! 面板只持快照、投影与动作队列，不做直接 I/O；所有读取与控制在观察线程里
//! 完成。关闭 Hub 只停止观察，不取消任何 child。

mod detail;
mod projection;
mod worker;

use super::transcript_style::{
    ACCENT, ACTIVITY, ADDED, ASK_BORDER, DIM, MUTED, REMOVED, think_icon, waiting_clock,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
pub(in crate::tui) use detail::Anchor;
use detail::Detail;
pub(in crate::tui) use projection::now_ms as now;
use projection::{
    ActivityRow, AgentStatus, AgentView, Category, Kind, Projection, RowStatus, Scope, TreeRow,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use ratatui_textarea::TextArea;
use std::collections::BTreeMap;
pub(super) use worker::{Action as Control, Reply, Request, Worker};

const TITLE: &str = "Agent Hub";
const NARROW: u16 = 96;
const HUB_BG: Color = Color::Rgb(37, 40, 51);
const SELECTED_BG: Color = Color::Rgb(48, 53, 63);
const COST: Color = Color::Rgb(241, 96, 174);
const EFFORT: Color = Color::Rgb(183, 131, 207);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Agents,
    Activity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Roster,
    Inspector,
}

/// child 草稿：按 agent id 分组的 TextArea，只在当前根内存保留。
pub(super) struct Draft {
    pub editor: TextArea<'static>,
    pub revision: u64,
}

#[derive(Default)]
pub(super) struct Drafts {
    map: BTreeMap<String, Draft>,
    next: u64,
}

impl Drafts {
    pub(super) fn draft(&mut self, agent: &str) -> &mut Draft {
        self.map.entry(agent.to_owned()).or_insert_with(|| Draft {
            editor: detail::editor(),
            revision: 0,
        })
    }

    pub(super) fn touch(&mut self, agent: &str) {
        self.next += 1;
        let revision = self.next;
        if let Some(draft) = self.map.get_mut(agent) {
            draft.revision = revision;
        }
    }

    pub(super) fn revision(&self, agent: &str) -> u64 {
        self.map.get(agent).map_or(0, |draft| draft.revision)
    }

    /// 成功发送后只清掉同一份未修改的草稿。
    pub(super) fn clear_if_unchanged(&mut self, agent: &str, revision: u64, text: &str) -> bool {
        let Some(draft) = self.map.get_mut(agent) else {
            return false;
        };
        if draft.revision != revision || draft.editor.lines().join("\n") != text {
            return false;
        }
        draft.editor = detail::editor();
        self.next += 1;
        draft.revision = self.next;
        true
    }

    pub(super) fn clear(&mut self) {
        self.map.clear();
    }
}

/// 面板请求；根/view epoch 由 App 统一补齐。
pub(super) enum Command {
    Open {
        agent: String,
        anchor: Option<Anchor>,
    },
    CloseDetail,
    FocusEditor,
    Submit {
        agent: String,
        text: String,
        revision: u64,
    },
    Control {
        agent: String,
        action: Control,
        text: Option<String>,
    },
    Refresh,
}

struct ActivityView {
    category: Category,
    scope: Scope,
    query: String,
    editing: bool,
    follow: bool,
    selected: Option<String>,
    offset: usize,
}

impl Default for ActivityView {
    fn default() -> Self {
        Self {
            category: Category::All,
            scope: Scope::All,
            query: String::new(),
            editing: false,
            follow: true,
            selected: None,
            offset: 0,
        }
    }
}

pub(super) struct Panel {
    page: Page,
    focus: Focus,
    tree: bool,
    filter: String,
    filtering: bool,
    selected: Option<String>,
    inspector: u16,
    activity: ActivityView,
    detail: Option<Detail>,
    projection: Projection,
    notice: Option<String>,
    error: Option<String>,
    agent_notices: BTreeMap<String, String>,
    commands: Vec<Command>,
    roster_area: Rect,
    hits: Vec<(Rect, String)>,
    activity_hits: Vec<(Rect, usize)>,
    sending: Option<String>,
    clock: u64,
    rendered: u64,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            page: Page::Agents,
            focus: Focus::Roster,
            tree: false,
            filter: String::new(),
            filtering: false,
            selected: None,
            inspector: 0,
            activity: ActivityView::default(),
            detail: None,
            projection: Projection::default(),
            notice: None,
            error: None,
            agent_notices: BTreeMap::new(),
            commands: Vec::new(),
            roster_area: Rect::default(),
            hits: Vec::new(),
            activity_hits: Vec::new(),
            sending: None,
            clock: projection::now_ms(),
            rendered: 0,
        }
    }
}

impl Panel {
    pub(super) fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    pub(super) fn detail_view(&self) -> Option<u64> {
        self.detail.as_ref().map(|detail| detail.view)
    }

    pub(super) fn opened(
        &mut self,
        agent: &str,
        view: u64,
        anchor: Option<Anchor>,
        transcript: super::Transcript,
    ) {
        let mut detail = Detail::new(agent, view, transcript);
        if let Some(message) = self.agent_notices.remove(agent) {
            detail.notice = Some(message);
        }
        if let Some(anchor) = anchor {
            detail.anchor(anchor);
        }
        self.detail = Some(detail);
        self.notice = None;
    }

    pub(super) fn focus_editor(&mut self) {
        if let Some(detail) = &mut self.detail {
            detail.focus = true;
        }
    }

    /// 控制 ACK：只把 notice 放到对应 agent，不覆盖当前另一 agent 的详情。
    pub(super) fn send_settled(&mut self, agent: &str, result: Result<(), String>) {
        if self.sending.as_deref() == Some(agent) {
            self.sending = None;
        }
        match result {
            Ok(()) => {
                self.notice = Some(format!("Sent to {agent}."));
            }
            Err(error) => {
                let message = format!("Send failed; draft kept: {error}");
                self.agent_notices.insert(agent.to_owned(), message.clone());
                if let Some(detail) = self.detail.as_mut().filter(|detail| detail.agent == agent) {
                    detail.notice = Some(message);
                } else {
                    // 详情不在该 agent 时用短 notice 汇报，不覆盖其它详情。
                    self.notice = Some(format!("Control failed for {agent}: {error}"));
                }
            }
        }
    }

    pub(super) fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub(super) fn apply(&mut self, reply: Reply) {
        match reply {
            Reply::Snapshot { snapshot, .. } => {
                self.projection.apply_snapshot(*snapshot, self.clock);
                self.validate_selection();
            }
            Reply::Activity { agent, page, .. } => {
                if !page.warnings.is_empty() {
                    self.error = Some(page.warnings.join("; "));
                }
                self.projection.ingest(&agent, &page, self.clock);
                self.validate_selection();
            }
            Reply::Detail { agent, history, .. } => {
                let page = &history.page;
                if !page.warnings.is_empty() {
                    self.error = Some(page.warnings.join("; "));
                }
                if let Some(detail) = self.detail.as_mut().filter(|detail| detail.agent == agent) {
                    if page.reset {
                        // 文件替换后从头重建，保留草稿与 follow 偏好。
                        detail.rebuild();
                    }
                    if let Some(detail) = self.detail.as_mut() {
                        detail.ingest(page, &history.runs, &history.settled, projection::now_ms());
                    }
                }
            }
            Reply::Reset { agent, .. } => {
                self.projection.reset(&agent);
                self.validate_selection();
            }
            Reply::Control { agent, result, .. } => self.send_settled(&agent, result),
            Reply::Error { message, .. } => {
                self.error = Some(message);
            }
        }
        if let Some(detail) = &mut self.detail
            && let Some(snapshot) = self.projection.snapshot()
        {
            for preview in &snapshot.output_previews {
                if preview.agent_id == detail.agent {
                    detail.output_preview(preview);
                }
            }
            for preview in &snapshot.pipeline_previews {
                if preview.agent_id == detail.agent {
                    detail.pipeline_preview(preview);
                }
            }
        }
    }

    pub(super) fn tick(&mut self, now_ms: u64, instant: std::time::Instant) -> bool {
        let clock_changed = waiting_clock(u128::from(self.clock))
            != waiting_clock(u128::from(now_ms))
            && self.detail.is_none()
            && self
                .projection
                .agents()
                .iter()
                .any(|agent| agent.status == AgentStatus::Running);
        self.clock = now_ms;
        let detail_changed = self
            .detail
            .as_mut()
            .is_some_and(|detail| detail.tick(instant));
        // age/elapsed 每秒刷新一次，不为时钟重读全文件。
        if now_ms.saturating_sub(self.rendered) >= 1000 {
            self.rendered = now_ms;
            self.projection.refresh(now_ms);
            return true;
        }
        clock_changed || detail_changed
    }

    fn validate_selection(&mut self) {
        let list = self.projection.list(self.tree, &self.filter);
        let known = self.selected.as_deref().is_none_or(|selected| {
            list.iter()
                .any(|row| self.projection.agents()[row.agent].id == selected)
        });
        if !known {
            self.selected = None;
            self.inspector = 0;
        }
        if self.selected.is_none() {
            self.selected = list
                .first()
                .map(|row| self.projection.agents()[row.agent].id.clone());
        }
    }

    fn visible(&self) -> Vec<TreeRow> {
        self.projection.list(self.tree, &self.filter)
    }

    fn selected_index(&self, list: &[TreeRow]) -> Option<usize> {
        let selected = self.selected.as_deref()?;
        list.iter()
            .position(|row| self.projection.agents()[row.agent].id == selected)
    }

    fn selected_agent(&self) -> Option<&AgentView> {
        let list = self.visible();
        if list.is_empty() {
            return None;
        }
        // 过滤后旧选择可能不可见：控制只能作用于当前可见行。
        let index = self.selected_index(&list).unwrap_or(0);
        self.projection.agents().get(list[index].agent)
    }

    fn move_selection(&mut self, delta: isize) {
        let list = self.visible();
        if list.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index(&list).unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, list.len() as isize - 1) as usize;
        if current != next as isize {
            self.inspector = 0;
        }
        self.selected = Some(self.projection.agents()[list[next].agent].id.clone());
    }

    fn filter_editing(&mut self) -> bool {
        if self.page == Page::Agents {
            self.filtering
        } else {
            self.activity.editing
        }
    }

    fn type_filter(&mut self, key: KeyEvent) -> bool {
        let buffer = if self.page == Page::Agents {
            &mut self.filter
        } else {
            &mut self.activity.query
        };
        match key.code {
            KeyCode::Backspace => {
                buffer.pop();
                true
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(character);
                true
            }
            _ => false,
        }
    }

    /// 返回 true 表示关闭整个 Hub。
    pub(super) fn handle(&mut self, event: Event, drafts: &mut Drafts) -> bool {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Release
        {
            return false;
        }
        if self.detail.is_some() {
            return self.handle_detail(event, drafts);
        }
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let close = self.handle_key(key, drafts);
                // 过滤/树切换后让选择始终落在可见行上。
                self.validate_selection();
                close
            }
            Event::Paste(text) => {
                if self.filter_editing() {
                    if self.page == Page::Agents {
                        self.filter_push(&text);
                    } else {
                        self.query_push(&text);
                    }
                }
                false
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
                false
            }
            Event::Resize(..) => false,
            _ => false,
        }
    }

    fn filter_push(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.filter.push(character);
        }
    }

    fn query_push(&mut self, text: &str) {
        for character in text.chars().filter(|character| !character.is_control()) {
            self.activity.query.push(character);
        }
    }

    fn handle_detail(&mut self, event: Event, drafts: &mut Drafts) -> bool {
        let Some(detail) = self.detail.as_mut() else {
            return false;
        };
        match event {
            Event::Key(key) => {
                if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                    // Esc 保留 child 草稿，返回来源页。
                    self.commands.push(Command::CloseDetail);
                    self.detail = None;
                    return false;
                }
                if let Some(text) = detail.key(key, drafts) {
                    let agent = detail.agent.clone();
                    let revision = drafts.revision(&agent);
                    if self.sending.as_deref() != Some(agent.as_str()) {
                        self.sending = Some(agent.clone());
                        self.commands.push(Command::Submit {
                            agent,
                            text,
                            revision,
                        });
                    }
                }
                false
            }
            Event::Paste(text) => {
                detail.paste(&text, drafts);
                false
            }
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => detail.wheel(true, 3),
                    MouseEventKind::ScrollDown => detail.wheel(false, 3),
                    _ => {}
                }
                false
            }
            Event::Resize(..) => false,
            _ => false,
        }
    }

    fn handle_key(&mut self, key: KeyEvent, drafts: &mut Drafts) -> bool {
        let _ = drafts;
        let press = key.kind == KeyEventKind::Press;
        if self.filter_editing() {
            match key.code {
                KeyCode::Esc => {
                    if self.page == Page::Agents {
                        self.filter.clear();
                        self.filtering = false;
                    } else {
                        self.activity.query.clear();
                        self.activity.editing = false;
                    }
                    return false;
                }
                KeyCode::Enter => {
                    if self.page == Page::Agents {
                        self.filtering = false;
                    } else {
                        self.activity.editing = false;
                    }
                    return false;
                }
                _ => {
                    self.type_filter(key);
                    return false;
                }
            }
        }
        match key.code {
            KeyCode::Esc => {
                // 先清过滤，再退出 inspector，最后才关闭 Hub。
                if self.page == Page::Agents && !self.filter.is_empty() {
                    self.filter.clear();
                    return false;
                }
                if self.page == Page::Agents && self.focus == Focus::Inspector {
                    self.focus = Focus::Roster;
                    return false;
                }
                if self.page == Page::Activity {
                    self.page = Page::Agents;
                    return false;
                }
                return true;
            }
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Roster => Focus::Inspector,
                    Focus::Inspector => Focus::Roster,
                };
            }
            KeyCode::Char('1') => self.page = Page::Agents,
            KeyCode::Char('2') => self.page = Page::Activity,
            KeyCode::Char('/') => {
                if self.page == Page::Agents {
                    self.filtering = true;
                } else {
                    self.activity.editing = true;
                }
            }
            KeyCode::Char('t') if self.page == Page::Agents => self.tree = !self.tree,
            KeyCode::F(5) => {
                self.commands.push(Command::Refresh);
                self.notice = Some("Reloading agent hub…".into());
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.page == Page::Agents {
                    if self.focus == Focus::Inspector {
                        self.inspector = self.inspector.saturating_sub(1);
                    } else {
                        self.move_selection(-1);
                    }
                } else {
                    self.activity_step(-1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.page == Page::Agents {
                    if self.focus == Focus::Inspector {
                        self.inspector = self.inspector.saturating_add(1);
                    } else {
                        self.move_selection(1);
                    }
                } else {
                    self.activity_step(1);
                }
            }
            KeyCode::PageUp => {
                if self.page == Page::Agents {
                    self.inspector = self.inspector.saturating_sub(10);
                } else {
                    self.activity_step(-10);
                }
            }
            KeyCode::PageDown => {
                if self.page == Page::Agents {
                    self.inspector = self.inspector.saturating_add(10);
                } else {
                    self.activity_step(10);
                }
            }
            KeyCode::Char(' ') if self.page == Page::Activity => {
                self.activity.follow = true;
            }
            KeyCode::Char('f') if self.page == Page::Activity => {
                self.activity.category = self.activity.category.next();
            }
            KeyCode::Char('s') if self.page == Page::Activity => {
                self.activity.scope = self.activity.scope.next();
            }
            KeyCode::Enter => {
                if self.page == Page::Agents {
                    if let Some(agent) = self.selected_agent() {
                        let agent = agent.id.clone();
                        self.commands.push(Command::Open {
                            agent,
                            anchor: None,
                        });
                    }
                } else {
                    self.activity.follow = false;
                    let rows = self.activity_rows();
                    if let Some(index) = self.activity_index(&rows)
                        && let Some(row) = self.projection.rows().get(rows[index])
                    {
                        self.activity.selected = Some(row.id());
                        let agent = row.agent.clone();
                        let anchor = Anchor {
                            run: row.run.clone(),
                            offset: row.offset,
                            tool: row.tool.clone(),
                        };
                        self.commands.push(Command::Open {
                            agent,
                            anchor: Some(anchor),
                        });
                    }
                }
            }
            KeyCode::Char('e') if self.page == Page::Agents && press => {
                if let Some(agent) = self.selected_agent() {
                    let agent_id = agent.id.clone();
                    if agent.status == AgentStatus::Aborted && agent.run_status.is_none() {
                        self.notice =
                            Some("Aborted agents need r or a new message to resume.".into());
                    }
                    self.commands.push(Command::Open {
                        agent: agent_id,
                        anchor: None,
                    });
                    self.commands.push(Command::FocusEditor);
                }
            }
            KeyCode::Char('c') if press && self.page == Page::Agents => {
                self.control(Control::Cancel, None);
            }
            KeyCode::Char('x') if press && self.page == Page::Agents => {
                self.control(Control::Kill, None);
            }
            KeyCode::Char('r') if press && self.page == Page::Agents => {
                if let Some(agent) = self.selected_agent() {
                    // 有活动 Run 时恢复没有意义；parked/aborted 才真正重建实例。
                    if agent.run_status.is_some() {
                        self.notice =
                            Some("Agent is busy; cancel the run or send a message.".into());
                    } else {
                        self.control(Control::Revive, None);
                    }
                }
            }
            _ => {}
        }
        false
    }

    fn control(&mut self, action: Control, text: Option<String>) {
        let Some(agent) = self.selected_agent() else {
            return;
        };
        let available = match action {
            Control::Cancel => agent.run_status.is_some(),
            // 恢复是幂等的：空闲实例上为无操作，parked/aborted 才重建。
            Control::Revive => agent.run_status.is_none(),
            Control::Kill | Control::Send => true,
        };
        if !available {
            self.notice = Some(match action {
                Control::Cancel => "No active run to cancel.".into(),
                Control::Revive => "Agent is not parked or aborted.".into(),
                _ => String::new(),
            });
            return;
        }
        let id = agent.id.clone();
        self.commands.push(Command::Control {
            agent: id,
            action,
            text,
        });
    }

    fn activity_rows(&self) -> Vec<usize> {
        let anchor = self.selected_agent().map(|agent| agent.id.clone());
        self.projection.activity(
            self.activity.category,
            self.activity.scope,
            anchor.as_deref(),
            &self.activity.query,
        )
    }

    /// 当前可见列表中的选中位置；跟随模式下最后一行就是选中项。
    fn activity_index(&self, rows: &[usize]) -> Option<usize> {
        if self.activity.selected.is_none() {
            return rows.len().checked_sub(1);
        }
        let selected = self.activity.selected.as_ref()?;
        rows.iter()
            .position(|index| self.projection.rows()[*index].id() == *selected)
            .or_else(|| rows.len().checked_sub(1))
    }

    fn activity_step(&mut self, delta: isize) {
        let rows = self.activity_rows();
        if rows.is_empty() {
            return;
        }
        let current = self.activity_index(&rows).unwrap_or(rows.len() - 1) as isize;
        let next = (current + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.activity.selected = Some(self.projection.rows()[rows[next]].id());
        self.activity.follow = false;
        self.activity.offset = next;
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if self.page == Page::Agents {
                    let in_inspector = !contains(self.roster_area, point);
                    if in_inspector && self.detail.is_none() {
                        self.inspector = if up {
                            self.inspector.saturating_sub(1)
                        } else {
                            self.inspector.saturating_add(1)
                        };
                    } else {
                        self.move_selection(if up { -1 } else { 1 });
                    }
                } else {
                    self.activity_step(if up { -1 } else { 1 });
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.page == Page::Agents {
                    if let Some((_, agent)) = self
                        .hits
                        .iter()
                        .find(|(rect, _)| contains(*rect, point))
                        .cloned()
                    {
                        self.selected = Some(agent.clone());
                        self.inspector = 0;
                        self.commands.push(Command::Open {
                            agent,
                            anchor: None,
                        });
                    }
                } else if let Some((_, index)) = self
                    .activity_hits
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .copied()
                    && let Some(row) = self.projection.rows().get(index)
                {
                    self.activity.selected = Some(row.id());
                    self.activity.follow = false;
                }
            }
            _ => {}
        }
    }

    pub(super) fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        drafts: &mut Drafts,
        cost: &pp_config::config::CostDisplay,
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(Clear, area);
        self.roster_area = Rect::default();
        self.hits.clear();
        self.activity_hits.clear();
        if self.detail.is_some() {
            let agent = self.detail.as_ref().map(|detail| detail.agent.clone());
            if let Some(view) = agent
                .as_deref()
                .and_then(|agent| self.projection.agent(agent))
                .cloned()
            {
                if let Some(detail) = &mut self.detail {
                    detail.draw(frame, area, &view, drafts, cost, self.clock);
                }
                return;
            }
            let message = self
                .error
                .clone()
                .unwrap_or_else(|| "Waiting for agent data…".into());
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message,
                    Style::default().fg(MUTED),
                ))),
                area,
            );
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .style(Style::default().bg(HUB_BG))
            .border_style(Style::default().fg(ASK_BORDER))
            .title(Line::from(colored(format!(" {TITLE} "), ACCENT)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let footer = inner.height.saturating_sub(1).min(2);
        let body = Rect::new(inner.x, inner.y, inner.width, inner.height - footer);
        if footer == 2 {
            frame.render_widget(
                Paragraph::new(format!("├{}┤", "─".repeat(inner.width as usize)))
                    .style(Style::default().fg(ASK_BORDER)),
                Rect::new(area.x, body.bottom(), area.width, 1),
            );
        }
        if footer > 0 {
            let hint_area = inset_columns(Rect::new(inner.x, inner.bottom() - 1, inner.width, 1));
            self.draw_footer(frame, hint_area);
        }
        match self.page {
            Page::Agents => self.draw_agents(frame, body, footer == 2),
            Page::Activity => {
                let mut body = inset_columns(body);
                if let Some(alert) = self.alert() {
                    draw_line(frame, body, alert);
                    body = below(body, 1);
                }
                self.draw_activity(frame, body);
            }
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let hints: &[&str] = match self.page {
            Page::Agents => &[
                "1:agents",
                "2:activity",
                "↵/click:open",
                "Esc:close",
                "Tab:focus",
                "j/k/wheel:select",
                "PgUp/PgDn:details",
                "t:by parent",
                "/:filter",
                "c:cancel",
                "r:revive",
                "x:kill",
            ],
            Page::Activity => &[
                "1:agents",
                "2:activity",
                "↵:open",
                "Esc:back",
                "j/k:move",
                "Space:follow",
                "f:category",
                "s:scope",
                "/:search",
            ],
        };
        let mut text = String::new();
        for hint in hints {
            let separator = if text.is_empty() { "" } else { "  " };
            if Line::raw(&text).width() + separator.len() + Line::raw(*hint).width()
                <= area.width as usize
            {
                text.push_str(separator);
                text.push_str(hint);
            }
        }
        draw_line(frame, area, Line::from(colored(text, DIM)));
    }

    fn alert(&self) -> Option<Line<'static>> {
        if let Some(error) = self
            .error
            .as_ref()
            .or_else(|| self.projection.snapshot()?.warnings.first())
        {
            Some(Line::from(colored(projection::truncate(error), REMOVED)))
        } else {
            self.notice
                .as_ref()
                .map(|notice| Line::from(colored(projection::truncate(notice), ACCENT)))
        }
    }

    fn draw_roster_header(&self, frame: &mut Frame, mut area: Rect) -> Rect {
        let agents = self.projection.agents();
        let mut spans = vec![
            colored("Roster · ", ACTIVITY),
            colored("Flat", if self.tree { MUTED } else { ACCENT }),
            colored(" / ", DIM),
            colored("By parent", if self.tree { ACCENT } else { MUTED }),
        ];
        for status in [
            AgentStatus::Running,
            AgentStatus::Parked,
            AgentStatus::Idle,
            AgentStatus::Aborted,
        ] {
            let count = agents.iter().filter(|agent| agent.status == status).count();
            if count > 0 || matches!(status, AgentStatus::Running | AgentStatus::Parked) {
                spans.push(colored(" · ", DIM));
                spans.push(colored(
                    format!(
                        "{} {count} {}",
                        status_icon(status, self.clock),
                        status.label()
                    ),
                    status_color(status),
                ));
            }
        }
        draw_line(frame, area, Line::from(spans));
        area = below(area, 1);
        let totals = |value: fn(&AgentView) -> Option<u64>| {
            agents.iter().filter_map(value).reduce(u64::saturating_add)
        };
        let cost = agents
            .iter()
            .filter_map(|agent| agent.cost)
            .reduce(|a, b| a + b);
        let active = totals(|agent| agent.active_ms).map_or_else(|| "—".into(), format_duration);
        let tokens = totals(|agent| agent.tokens).map_or_else(|| "—".into(), compact);
        let requests: usize = agents.iter().map(|agent| agent.requests).sum();
        let tools: usize = agents.iter().map(|agent| agent.tools).sum();
        let timed = agents
            .iter()
            .filter(|agent| agent.active_ms.is_some())
            .count();
        let measured = agents.iter().filter(|agent| agent.cost.is_some()).count();
        let count = agents.len();
        let cost = format_cost(cost);
        let full = format!(
            " · {active} active agent time · {requests} req · {tools} tools · {tokens} tok · {timed}/{count} timed · {measured}/{count} measured"
        );
        let compact = format!(
            " {active} {requests}req {tools}tools {tokens}tok {timed}/{count} timed {measured}/{count} measured"
        );
        let metrics = if Line::raw(&full).width() + Line::raw(&cost).width() <= area.width as usize
        {
            full
        } else {
            compact
        };
        let mut stats = vec![colored(cost, COST), colored(metrics, DIM)];
        if agents.iter().any(|agent| agent.loading) {
            stats.push(colored(" · loading", ACCENT));
        }
        draw_line(frame, area, Line::from(stats));
        area = below(area, 2);
        if let Some(alert) = self.alert() {
            draw_line(frame, area, alert);
            area = below(area, 1);
        }
        area
    }

    fn draw_agents(&mut self, frame: &mut Frame, area: Rect, footer_separator: bool) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (roster, inspector) = if area.width >= NARROW {
            let roster_width = (u32::from(area.width - 1) * 60 / 100) as u16;
            let divider = area.x + roster_width;
            frame.render_widget(
                Block::default()
                    .borders(Borders::LEFT)
                    .border_style(Style::default().fg(ASK_BORDER)),
                Rect::new(divider, area.y, 1, area.height),
            );
            for (y, symbol) in [(area.y.saturating_sub(1), "┬"), (area.bottom(), "┴")] {
                if symbol == "┬" || footer_separator {
                    frame.render_widget(
                        Paragraph::new(symbol).style(Style::default().fg(ASK_BORDER)),
                        Rect::new(divider, y, 1, 1),
                    );
                }
            }
            (
                Rect::new(area.x, area.y, roster_width, area.height),
                Some(Rect::new(
                    divider + 1,
                    area.y,
                    area.width - roster_width - 1,
                    area.height,
                )),
            )
        } else if self.focus == Focus::Inspector {
            (Rect::default(), Some(area))
        } else {
            (area, None)
        };
        self.roster_area = roster;
        if roster.width > 0 {
            let list = self.draw_roster_header(frame, inset_columns(roster));
            self.draw_roster(frame, list);
        }
        if let Some(inspector) = inspector {
            self.draw_inspector(frame, inset_columns(inspector));
        }
    }

    fn draw_roster(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rows = self.visible();
        if rows.is_empty() {
            let message = if self.projection.loading() {
                "Loading agents…"
            } else if !self.filter.trim().is_empty() {
                "No matching agents"
            } else {
                "No subagents yet. Spawned agents will appear here."
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message,
                    Style::default().fg(MUTED),
                ))),
                area,
            );
            return;
        }
        let heights = rows
            .iter()
            .map(|row| {
                if self.projection.agents()[row.agent].status == AgentStatus::Running {
                    3
                } else {
                    2
                }
            })
            .collect::<Vec<u16>>();
        let selected = self.selected_index(&rows);
        let start = visible_start(&heights, selected, area.height);
        let mut y = area.y;
        for (index, row) in rows.iter().enumerate().skip(start) {
            let height = heights[index];
            if y >= area.bottom() {
                break;
            }
            let height = height.min(area.bottom() - y);
            let rect = Rect::new(area.x, y, area.width, height);
            let is_selected = selected == Some(index);
            let lines = self.row_lines(row, area.width);
            let mut content = Paragraph::new(lines);
            if is_selected {
                content = content.style(Style::default().bg(SELECTED_BG));
            }
            frame.render_widget(content, rect);
            let agent = self.projection.agents()[row.agent].id.clone();
            self.hits.push((rect, agent));
            y += height;
        }
    }

    fn row_lines(&self, row: &TreeRow, width: u16) -> Vec<Line<'static>> {
        let width = width as usize;
        let agent = &self.projection.agents()[row.agent];
        let marker = if self.selected.as_deref() == Some(agent.id.as_str()) {
            "› "
        } else {
            "  "
        };
        let icon = status_icon(agent.status, self.clock);
        let name = projection::truncate(&agent.name);
        let model_column = width.div_ceil(2);
        let model = model_line(agent, width - model_column);
        let left = vec![
            colored(marker, ACCENT),
            colored(row.prefix.clone(), MUTED),
            colored(format!("{icon} "), status_color(agent.status)),
            colored(name, if row.muted { MUTED } else { ACTIVITY }),
        ];
        let mut first = fit_line(Line::from(left), model_column.saturating_sub(1));
        first
            .spans
            .push(Span::raw(" ".repeat(model_column - first.width())));
        first.spans.extend(model.spans);
        let indent = 3 + Line::raw(&row.prefix).width() + Line::raw(icon).width();
        let mut lines = vec![first];
        if agent.status == AgentStatus::Running {
            lines.push(indented(
                agent.task.as_deref().unwrap_or("—"),
                indent,
                width,
                MUTED,
            ));
        }
        lines.push(indented(
            &format!("{}   {}", usage(agent, "   "), self.age(agent)),
            indent,
            width,
            DIM,
        ));
        lines
    }

    fn age(&self, agent: &AgentView) -> String {
        agent.last_activity.map_or_else(
            || "—".into(),
            |timestamp| detail::format_age(self.clock.saturating_sub(timestamp)),
        )
    }

    fn draw_inspector(&mut self, frame: &mut Frame, area: Rect) {
        let Some(agent) = self.selected_agent() else {
            draw_line(
                frame,
                area,
                Line::from(colored("Select an agent to inspect.", MUTED)),
            );
            return;
        };
        let lines = self.inspector_lines(agent, area);
        let max =
            u16::try_from(lines.len().saturating_sub(area.height as usize)).unwrap_or(u16::MAX);
        self.inspector = self.inspector.min(max);
        frame.render_widget(Paragraph::new(lines).scroll((self.inspector, 0)), area);
    }

    fn inspector_lines(&self, agent: &AgentView, area: Rect) -> Vec<Line<'static>> {
        let width = area.width as usize;
        let mut lines = vec![Line::from(vec![
            colored(
                format!("{} ", status_icon(agent.status, self.clock)),
                status_color(agent.status),
            ),
            colored(projection::truncate(&agent.definition), ACTIVITY),
        ])];
        if agent.name != agent.definition {
            lines.push(Line::from(colored(
                projection::truncate(&agent.name),
                MUTED,
            )));
        }
        let status = agent
            .run_status
            .as_deref()
            .unwrap_or_else(|| agent.status.label());
        let age = self.age(agent);
        lines.push(Line::from(vec![
            colored(status.to_owned(), status_color(agent.status)),
            colored(format!(" · {} · active {age}", agent_time(agent)), DIM),
        ]));
        lines.push(model_line(agent, width));
        if let Some(change) = &agent.model_change {
            lines.push(Line::from(colored(projection::truncate(change), MUTED)));
        }
        section(&mut lines, "Task");
        lines.push(Line::from(colored(
            projection::truncate(agent.task.as_deref().unwrap_or("—")),
            ACTIVITY,
        )));
        section(&mut lines, "Current");
        lines.push(Line::from(colored(
            projection::truncate(
                agent
                    .current
                    .as_deref()
                    .or(agent.task.as_deref())
                    .unwrap_or("—"),
            ),
            ACTIVITY,
        )));
        section(&mut lines, "Usage");
        lines.push(Line::from(colored(usage(agent, " · "), ACTIVITY)));
        if agent.run_status.as_deref() == Some("running")
            && let Some(context) = agent.context
            && let Some(window) = context.context_window.filter(|window| *window > 0)
            && let Some(percent) = context.percent()
        {
            let filled = (percent.min(100) as usize + 5) / 10;
            lines.push(Line::from(vec![
                colored("━".repeat(filled), ACCENT),
                colored("─".repeat(10 - filled), DIM),
                colored(
                    format!(
                        " {}/{} {percent}%",
                        compact(context.estimated_tokens),
                        compact(window)
                    ),
                    ACTIVITY,
                ),
            ]));
        }
        section(&mut lines, "Lineage");
        let parent = self
            .projection
            .agent(&agent.parent)
            .map_or(agent.parent.as_str(), |parent| parent.name.as_str());
        lines.push(Line::from(colored(
            format!("Spawned by {}", projection::truncate(parent)),
            ACTIVITY,
        )));
        if let Some(registered) = agent
            .registered_ms
            .and_then(|ms| i64::try_from(ms).ok())
            .and_then(chrono::DateTime::from_timestamp_millis)
        {
            lines.push(Line::from(colored(
                format!(
                    "Registered {}",
                    registered
                        .with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M %:z")
                ),
                DIM,
            )));
        }
        lines.push(Line::from(colored(format!("Agent ID {}", agent.id), DIM)));
        section(&mut lines, "Changes");
        let workspace = match (agent.readonly, agent.isolated) {
            (true, _) => "Read-only agent · no workspace writes",
            (_, true) => "Isolated workspace · patches only",
            _ => "Shared workspace · per-agent LoC not attributable",
        };
        lines.push(Line::from(colored(workspace, DIM)));
        if let Some((output, patches)) = agent.artifacts.split_first() {
            for patch in patches {
                lines.push(Line::from(colored(projection::truncate(patch), MUTED)));
            }
            lines.push(Line::from(colored("Output", ACTIVITY)));
            lines.push(Line::from(colored(output.clone(), ACTIVITY)));
        }
        if let Some(error) = &agent.last_error {
            lines.push(Line::from(colored(
                format!("last error: {}", projection::truncate(error)),
                REMOVED,
            )));
        }
        section(&mut lines, "Recent activity");
        // 概要始终保留最新几项，独立 Activity 页可查看更早事件。
        let limit = (area.height as usize)
            .saturating_sub(lines.len())
            .clamp(1, 20);
        let recent = self.projection.recent(&agent.id, limit);
        if recent.is_empty() {
            lines.push(Line::from(colored("—", DIM)));
        }
        for row in recent {
            let symbol = if row.kind == Kind::Response {
                "◆"
            } else {
                activity_icon(row.status, self.clock)
            };
            lines.push(Line::from(vec![
                colored(format!("{} ", detail::format_clock(row.timestamp)), DIM),
                colored(format!("{symbol} "), row_color(row.status)),
                colored(format!("{} ", projection::truncate(&row.title)), MUTED),
                colored(projection::truncate(&row.summary), ACTIVITY),
            ]));
        }
        lines
            .into_iter()
            .map(|line| fit_line(line, width))
            .collect()
    }

    fn draw_activity(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let header = Rect::new(area.x, area.y, area.width, 1);
        let body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let search = if self.activity.editing {
            format!("/{}", self.activity.query)
        } else if self.activity.query.is_empty() {
            "/search".to_owned()
        } else {
            format!("/{}", self.activity.query)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    self.activity.category.label().to_owned(),
                    Style::default().fg(ACCENT),
                ),
                Span::styled("  ·  ", Style::default().fg(DIM)),
                Span::styled(
                    self.activity.scope.label().to_owned(),
                    Style::default().fg(ACCENT),
                ),
                Span::styled("  ·  ", Style::default().fg(DIM)),
                Span::styled(
                    search,
                    Style::default().fg(if self.activity.editing { ACCENT } else { MUTED }),
                ),
                Span::styled("  ·  ", Style::default().fg(DIM)),
                Span::styled(
                    if self.activity.follow {
                        "follow latest"
                    } else {
                        "paused"
                    }
                    .to_owned(),
                    Style::default().fg(if self.activity.follow { ADDED } else { ACCENT }),
                ),
            ])),
            header,
        );
        let rows = self.activity_rows();
        if rows.is_empty() {
            let message = match self.activity.scope {
                Scope::Selected | Scope::Subtree if self.selected.is_none() => {
                    "No agent selected for this scope"
                }
                _ if !self.activity.query.is_empty() => "No matching activity",
                _ => "No activity yet",
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message,
                    Style::default().fg(MUTED),
                ))),
                body,
            );
            return;
        }
        let heights = rows
            .iter()
            .map(|index| {
                let lines = self.activity_lines(*index);
                lines_height(&lines, body.width)
            })
            .collect::<Vec<u16>>();
        let selected = self.activity_index(&rows);
        let start = if self.activity.follow {
            tail_start(&heights, body.height)
        } else {
            let offset = self.activity.offset.min(rows.len().saturating_sub(1));
            visible_start(&heights, Some(offset), body.height)
        };
        let mut y = body.y;
        for (slot, index) in rows.iter().enumerate().skip(start) {
            let height = heights[slot];
            if y >= body.bottom() {
                break;
            }
            let height = height.min(body.bottom() - y);
            let rect = Rect::new(body.x, y, body.width, height);
            let is_selected = selected == Some(slot);
            let lines = style_row(self.activity_lines(*index), is_selected);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
            self.activity_hits.push((rect, *index));
            y += height;
        }
    }

    fn activity_lines(&self, index: usize) -> Vec<Line<'static>> {
        let row: &ActivityRow = &self.projection.rows()[index];
        let name = self
            .projection
            .agent(&row.agent)
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| row.agent.clone());
        let category = match row.kind {
            Kind::Response => "response",
            Kind::Tool => "tool",
            Kind::Lifecycle => "lifecycle",
        };
        vec![Line::from(vec![
            Span::styled(
                format!("{} ", detail::format_clock(row.timestamp)),
                Style::default().fg(DIM),
            ),
            Span::styled(
                format!("{} ", activity_icon(row.status, self.clock)),
                Style::default().fg(row_color(row.status)),
            ),
            Span::styled(name, Style::default().fg(ACCENT)),
            Span::styled(
                format!(" {category} · {}", row.title),
                Style::default().fg(ACTIVITY),
            ),
            Span::styled(
                format!("  {}", summary_line(&row.summary)),
                Style::default().fg(MUTED),
            ),
        ])]
    }
}

fn status_color(status: AgentStatus) -> ratatui::style::Color {
    match status {
        AgentStatus::Running => ACCENT,
        AgentStatus::Idle => ADDED,
        AgentStatus::Parked => MUTED,
        AgentStatus::Aborted => REMOVED,
    }
}

fn row_color(status: RowStatus) -> ratatui::style::Color {
    match status {
        RowStatus::Pending => ACCENT,
        RowStatus::Success => ADDED,
        RowStatus::Error => REMOVED,
        RowStatus::Aborted => MUTED,
    }
}

fn summary_line(text: &str) -> String {
    let collapsed = pp_core::text::display_text(text)
        .into_owned()
        .replace(['\n', '\r', '\t'], " ");
    let collapsed = collapsed.trim();
    if collapsed.chars().count() > 160 {
        let mut out = collapsed.chars().take(159).collect::<String>();
        out.push('…');
        out
    } else {
        collapsed.to_owned()
    }
}

fn colored(text: impl Into<std::borrow::Cow<'static, str>>, color: Color) -> Span<'static> {
    let text = text.into();
    let safe = match pp_core::text::display_text(&text) {
        std::borrow::Cow::Owned(safe) => std::borrow::Cow::Owned(safe),
        std::borrow::Cow::Borrowed(_) => text,
    };
    Span::styled(safe, Style::default().fg(color))
}

fn fit_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let mut remaining = width.saturating_sub(1);
    let mut spans = Vec::new();
    let mut style = Style::default().fg(MUTED);
    for span in line.spans.drain(..) {
        style = span.style;
        let clipped = super::tool_display::clip(&span.content, remaining, false);
        remaining = remaining.saturating_sub(Line::raw(&clipped).width());
        let truncated = clipped.len() < span.content.len();
        spans.push(Span::styled(clipped, style));
        if truncated || remaining == 0 {
            break;
        }
    }
    if width > 0 {
        spans.push(Span::styled("…", style));
    }
    line.spans = spans;
    line
}

fn draw_line(frame: &mut Frame, area: Rect, line: Line<'static>) {
    if area.width > 0 && area.height > 0 {
        let row = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(Paragraph::new(fit_line(line, area.width as usize)), row);
    }
}

fn inset_columns(area: Rect) -> Rect {
    let margin = u16::from(area.width > 2);
    Rect::new(
        area.x + margin,
        area.y,
        area.width - 2 * margin,
        area.height,
    )
}

fn below(area: Rect, rows: u16) -> Rect {
    let rows = rows.min(area.height);
    Rect::new(area.x, area.y + rows, area.width, area.height - rows)
}

fn section(lines: &mut Vec<Line<'static>>, title: &'static str) {
    lines.push(Line::default());
    lines.push(Line::from(colored(title, ACCENT)));
}

fn indented(text: &str, indent: usize, width: usize, color: Color) -> Line<'static> {
    let indent = indent.min(width / 2);
    fit_line(
        Line::from(vec![
            Span::raw(" ".repeat(indent)),
            colored(projection::truncate(text), color),
        ]),
        width,
    )
}

fn model_line(agent: &AgentView, width: usize) -> Line<'static> {
    let role = agent.role.unwrap_or(&agent.definition);
    let prefix = colored(
        format!("{} · ", projection::truncate(role)),
        if agent.role.is_some() { ACCENT } else { MUTED },
    );
    let suffix = agent
        .thinking
        .parse::<pp_core::model::ThinkLevel>()
        .ok()
        .map(|level| colored(format!(" {} {}", think_icon(level), level.as_str()), EFFORT));
    let suffix_width = suffix.as_ref().map_or(0, Span::width);
    let model_width = width.saturating_sub(prefix.width() + suffix_width);
    if model_width < 3 {
        return fit_line(
            Line::from(colored(projection::truncate(&agent.model), MUTED)),
            width,
        );
    }
    let model = super::tool_display::clip(&projection::truncate(&agent.model), model_width, true);
    let mut spans = vec![prefix, colored(model, MUTED)];
    spans.extend(suffix);
    Line::from(spans)
}

fn status_icon(status: AgentStatus, clock: u64) -> &'static str {
    match status {
        AgentStatus::Running => waiting_clock(u128::from(clock)),
        AgentStatus::Idle | AgentStatus::Parked => "⬤",
        AgentStatus::Aborted => "×",
    }
}

fn activity_icon(status: RowStatus, clock: u64) -> &'static str {
    match status {
        RowStatus::Pending => status_icon(AgentStatus::Running, clock),
        RowStatus::Success => "✓",
        RowStatus::Error => "×",
        RowStatus::Aborted => "■",
    }
}

fn format_cost(cost: Option<f64>) -> String {
    cost.map_or_else(|| "—".into(), |cost| format!("${cost:.3}"))
}

fn compact(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0).replace(".0M", "M")
    } else if value >= 1_000 {
        format!("{:.0}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn agent_time(agent: &AgentView) -> String {
    let (time, label) = if agent.status == AgentStatus::Running && agent.active_ms.is_some() {
        (agent.active_ms, "active")
    } else if agent.status != AgentStatus::Running && agent.span_ms.is_some() {
        (agent.span_ms, "span")
    } else {
        (agent.time_ms, "run time")
    };
    time.map_or_else(
        || "—".into(),
        |ms| format!("{} {label}", format_duration(ms)),
    )
}

fn usage(agent: &AgentView, separator: &str) -> String {
    let tokens = agent.tokens.map_or_else(|| "—".into(), compact);
    [
        format_cost(agent.cost),
        agent_time(agent),
        format!("{} req", agent.requests),
        format!("{} tools", agent.tools),
        format!("{tokens} tok"),
    ]
    .join(separator)
}

fn lines_height(lines: &[Line<'static>], width: u16) -> u16 {
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1))
        .max(1) as u16
}

fn style_row(lines: Vec<Line<'static>>, selected: bool) -> Vec<Line<'static>> {
    if !selected {
        return lines;
    }
    lines
        .into_iter()
        .map(|line| line.patch_style(Style::default().bg(SELECTED_BG)))
        .collect()
}

fn visible_start(heights: &[u16], selected: Option<usize>, height: u16) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    let mut start = 0;
    while start < selected {
        let total: u16 = heights[start..=selected].iter().copied().sum();
        if total <= height {
            break;
        }
        start += 1;
    }
    start
}

fn tail_start(heights: &[u16], height: u16) -> usize {
    let mut start = heights.len();
    let mut total = 0u16;
    while start > 0 {
        let next = heights[start - 1];
        if total + next > height {
            break;
        }
        total += next;
        start -= 1;
    }
    start
}

fn contains(rect: Rect, point: (u16, u16)) -> bool {
    point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
}

fn format_duration(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pp_config::config::ModelRole;
    use pp_core::context::{ContextUsage, TokenUsage};
    use pp_core::events::AgentEvent;
    use pp_core::model::ThinkLevel;
    use pp_runtime::subagent::HubAgent;
    use pp_runtime::subagent::HubSnapshot;
    use pp_runtime::subagent::ObservedEntry;
    use pp_runtime::subagent::ObservedRecord;
    use pp_runtime::subagent::Record;
    use pp_runtime::subagent::Run;
    use pp_runtime::subagent::Spec;
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn spec(depth: usize) -> Spec {
        Spec {
            model_role: None,
            role_route: None,
            definition: "task".into(),
            prompt: String::new(),
            context: String::new(),
            model: "fixture-model".into(),
            thinking: ThinkLevel::None,
            tools: vec![],
            spawns: vec![],
            output_schema: None,
            cwd: PathBuf::from("."),
            depth,
            readonly: false,
            isolated: false,
            cost: None,
        }
    }

    fn record(id: &str, parent: &str, status: &str) -> Record {
        Record {
            id: id.into(),
            name: format!("{id}-name"),
            parent: parent.into(),
            generation: 1,
            status: status.into(),
            spec: spec(1),
            session_file: None,
            registered_ms: None,
            updated_ms: 100,
        }
    }

    fn run(id: &str, agent: &str, status: &str, task: &str) -> Run {
        Run {
            id: id.into(),
            agent_id: agent.into(),
            owner: "Main".into(),
            status: status.into(),
            task: task.into(),
            result: None,
            consumed: false,
            duration_ms: 0,
            started_ms: 0,
            requests: 0,
            tokens: 0,
            cost: None,
        }
    }

    fn snapshot(agents: Vec<Record>, runs: Vec<Run>) -> HubSnapshot {
        HubSnapshot {
            output_previews: Vec::new(),
            pipeline_previews: Vec::new(),
            agents: agents
                .into_iter()
                .map(|record| HubAgent {
                    record,
                    model_change: None,
                })
                .collect(),
            runs,
            warnings: vec![],
        }
    }

    fn entry(offset: u64, run: &str, event: AgentEvent) -> ObservedEntry {
        ObservedEntry {
            start_offset: offset,
            record: ObservedRecord {
                root_id: None,
                agent_id: "a1".into(),
                run_id: Some(run.into()),
                generation: 1,
                sequence: None,
                timestamp_ms: Some(1_000 + offset),
                event: Some(event),
                hub: None,
            },
        }
    }

    fn panel(agents: Vec<Record>, runs: Vec<Run>) -> Panel {
        let mut panel = Panel::default();
        panel.apply(Reply::Snapshot {
            epoch: 1,
            snapshot: Box::new(snapshot(agents, runs)),
        });
        panel
    }

    fn render(panel: &mut Panel, drafts: &mut Drafts, width: u16, height: u16) -> Vec<String> {
        rows(
            &render_cells(panel, drafts, &Default::default(), width, height),
            width,
            height,
        )
    }

    fn render_cells(
        panel: &mut Panel,
        drafts: &mut Drafts,
        cost: &pp_config::config::CostDisplay,
        width: u16,
        height: u16,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width.max(1), height.max(1))).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), drafts, cost))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn rows(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height.max(1))
            .map(|row| {
                (0..width.max(1))
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(|cell| cell.symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::from(code))
    }

    #[test]
    fn geometry_keeps_selection_visible_and_never_underflows() {
        assert_eq!(visible_start(&[2, 2, 2], Some(2), 4), 1);
        assert_eq!(visible_start(&[2, 2, 2], Some(0), 4), 0);
        assert_eq!(visible_start(&[5], Some(0), 2), 0);
        assert_eq!(tail_start(&[2, 2, 2, 2], 5), 2);
        assert_eq!(tail_start(&[], 5), 0);
        // 0 尺寸不绘制、不越界。
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![], vec![]);
        render(&mut panel, &mut drafts, 0, 0);
        render(&mut panel, &mut drafts, 1, 1);
    }

    #[test]
    fn wide_layout_shows_selected_agent_and_semantic_colors() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![
                record("a1", "Main", "idle"),
                record("a2", "Main", "running"),
            ],
            vec![run("r1", "a2", "running", "child work")],
        );
        let screen = render(&mut panel, &mut drafts, 140, 40).join("\n");
        assert!(screen.contains("Roster · Flat / By parent"), "{screen}");
        assert!(screen.contains("1:agents"));
        assert!(screen.contains("2:activity"));
        assert!(screen.contains("a1-name") && screen.contains("a2-name"));
        assert!(screen.contains("running"));
        // 选中项语义色：running 使用 ACCENT。
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts, &Default::default()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        for (position, symbol) in [
            ((0, 0), "╭"),
            ((139, 39), "╯"),
            ((0, 37), "├"),
            ((83, 0), "┬"),
            ((83, 20), "│"),
            ((83, 37), "┴"),
            ((139, 37), "┤"),
        ] {
            let cell = &buffer[position];
            assert_eq!(cell.symbol(), symbol, "{position:?}");
            assert_eq!(cell.fg, ASK_BORDER, "{position:?}");
        }
        let marker = &buffer[(2, 4)];
        assert_eq!(marker.symbol(), "›");
        assert_eq!(marker.fg, ACCENT);
        assert_eq!(
            buffer[(4, 4)].symbol(),
            waiting_clock(u128::from(panel.clock))
        );
        assert_eq!(buffer[(6, 4)].symbol(), " ");
        assert_eq!(buffer[(7, 4)].fg, ACTIVITY);
        assert_eq!(buffer[(81, 5)].bg, SELECTED_BG);
    }

    #[test]
    fn roster_roles_align_with_different_model_widths() {
        let mut slow = record("a1", "Main", "running");
        slow.spec.model_role = Some(ModelRole::Slow);
        slow.spec.model = "provider/long-model-name".into();
        slow.spec.thinking = ThinkLevel::High;
        let mut fast = record("a2", "Main", "idle");
        fast.spec.model_role = Some(ModelRole::Fast);
        fast.spec.model = "provider/fast".into();
        let mut panel = panel(vec![slow, fast], vec![]);
        let mut drafts = Drafts::default();

        for width in [80, 140] {
            let screen = render(&mut panel, &mut drafts, width, 24);
            let role_column = |role| {
                screen
                    .iter()
                    .find_map(|line| line.find(role).map(|offset| line[..offset].chars().count()))
                    .unwrap()
            };
            assert_eq!(role_column("SLOW · "), role_column("FAST · "), "{width}");
        }
    }

    #[test]
    fn narrow_layout_tabs_between_roster_and_inspector() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![record("a1", "Main", "idle")],
            vec![run("r1", "a1", "completed", "narrow work")],
        );
        let roster = render(&mut panel, &mut drafts, 80, 24).join("\n");
        assert!(roster.contains("a1-name"));
        assert!(!roster.contains("Usage"));
        panel.handle(key(KeyCode::Tab), &mut drafts);
        let inspector = render(&mut panel, &mut drafts, 80, 24).join("\n");
        assert!(inspector.contains("Usage"), "{inspector}");
        // Esc 先退出 inspector，再关闭 Hub。
        let close = panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(!close);
        let close = panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(close);
    }

    #[test]
    fn hub_overview_preserves_summary_and_final_response_when_parked() {
        let mut drafts = Drafts::default();
        let mut agent = record("a1", "Main", "running");
        agent.registered_ms = Some(500);
        let task = "盘点工作区中的依赖和边界".repeat(12);
        let mut work = run("r1", "a1", "running", &task);
        let mut panel = panel(vec![agent.clone()], vec![work.clone()]);
        panel.clock = 5_000;
        let run_state = |offset, status: &str| {
            let mut state = entry(offset, "r1", AgentEvent::AssistantStarted);
            state.record.event = None;
            state.record.hub = Some(pp_runtime::subagent::HubEvent::RunState {
                status: status.to_owned(),
            });
            state
        };
        let delta = |offset, text: &str| {
            entry(
                offset,
                "r1",
                AgentEvent::ReasoningSummaryDelta { text: text.into() },
            )
        };
        let tool = entry(
            500,
            "r1",
            AgentEvent::ToolStarted {
                id: "bash-1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command":"cargo tree"}).to_string(),
            },
        );
        let reply = |entries| Reply::Activity {
            epoch: 1,
            agent: "a1".into(),
            page: pp_runtime::subagent::EventPage {
                entries,
                cursor: None,
                reset: false,
                has_more: false,
                warnings: vec![],
            },
        };
        panel.apply(reply(vec![
            run_state(0, "running"),
            entry(1, "r1", AgentEvent::AssistantStarted),
            delta(2, "检查"),
            delta(3, "依赖边界"),
            entry(
                4,
                "r1",
                AgentEvent::AssistantFinished {
                    text: "intermediate response".into(),
                },
            ),
            entry(
                6,
                "r1",
                AgentEvent::ContextUpdated {
                    usage: pp_core::context::ContextUsage {
                        estimated_tokens: 10_000,
                        context_window: Some(20_000),
                        last_request: None,
                        session_output_tokens: 0,
                        auto_compact: false,
                        threshold_percent: 80,
                    },
                },
            ),
            tool,
        ]));
        panel.handle(key(KeyCode::Tab), &mut drafts);
        let running = render(&mut panel, &mut drafts, 70, 40);
        let task_row = running
            .iter()
            .position(|line| line.contains("│ Task "))
            .unwrap();
        assert!(running[task_row + 1].contains('…'));
        assert!(running[task_row + 2].trim_matches(['│', ' ']).is_empty());
        let running = running.join("\n");
        assert!(
            running.replace(' ', "").contains("检查依赖边界") && running.contains("4s active"),
            "{running}"
        );
        assert!(running.contains("10K/20K 50%"));
        assert!(
            running.replace(' ', "").contains("bash检查依赖边界"),
            "{running}"
        );
        assert!(!running.contains("cargo tree"), "{running}");
        assert!(!running.contains("intermediate response"));

        panel.apply(reply(vec![
            entry(
                750,
                "r1",
                AgentEvent::ToolFinished {
                    id: "bash-1".into(),
                    result: serde_json::json!({"ok":true}),
                },
            ),
            entry(1_000, "r1", AgentEvent::AssistantStarted),
            entry(
                1_100,
                "r1",
                AgentEvent::AssistantFinished {
                    text: String::new(),
                },
            ),
            entry(
                1_200,
                "r1",
                AgentEvent::ToolStarted {
                    id: "read-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"Cargo.toml"}).to_string(),
                },
            ),
            entry(
                1_300,
                "r1",
                AgentEvent::ToolFinished {
                    id: "read-1".into(),
                    result: serde_json::json!({"ok":true}),
                },
            ),
            run_state(4_000, "completed"),
        ]));
        agent.status = "parked".into();
        work.status = "completed".into();
        work.result = Some(serde_json::json!({"ok":true, "output":"final answer"}));
        panel.apply(Reply::Snapshot {
            epoch: 1,
            snapshot: Box::new(snapshot(vec![agent], vec![work])),
        });
        let parked = render(&mut panel, &mut drafts, 70, 40);
        let current = parked
            .iter()
            .position(|line| line.contains("│ Current "))
            .unwrap();
        assert!(
            parked[current + 1]
                .replace(' ', "")
                .contains("检查依赖边界")
        );
        let parked = parked.join("\n");
        for expected in [
            "parked",
            "4s span",
            "Registered",
            "agent://a1",
            "✓ bash",
            "✓ read",
            "◆ Response final answer",
        ] {
            assert!(parked.contains(expected), "missing {expected}\n{parked}");
        }
        assert!(parked.replace(' ', "").contains("✓bash检查依赖边界"));
        assert!(!parked.replace(' ', "").contains("✓read检查依赖边界"));
        assert!(!parked.contains("Cargo.toml"));
        assert!(!parked.contains("10K/20K 50%"));
        assert!(!parked.contains("intermediate response"));
    }

    #[test]
    fn activity_paused_follow_keeps_viewport_and_escape_clears_search() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        for index in 0..6u64 {
            panel.apply(Reply::Activity {
                epoch: 1,
                agent: "a1".into(),
                page: pp_runtime::subagent::EventPage {
                    entries: vec![entry(index * 100, "r1", AgentEvent::AssistantStarted)],
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
            });
        }
        panel.handle(key(KeyCode::Char('2')), &mut drafts);
        // 默认跟随最新：最后一行的选中 id 就是末行。
        let rows = panel.activity_rows();
        let last = panel.projection.rows()[*rows.last().unwrap()].id();
        assert!(panel.activity.follow);
        // 手动上移暂停跟随并固定 id。
        panel.handle(key(KeyCode::Char('k')), &mut drafts);
        assert!(!panel.activity.follow);
        let pinned = panel.activity.selected.clone().unwrap();
        assert_ne!(pinned, last);
        for index in 6..12u64 {
            panel.apply(Reply::Activity {
                epoch: 1,
                agent: "a1".into(),
                page: pp_runtime::subagent::EventPage {
                    entries: vec![entry(index * 100, "r1", AgentEvent::AssistantStarted)],
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
            });
        }
        assert_eq!(panel.activity.selected.as_deref(), Some(pinned.as_str()));
        // Space 恢复跟随并跳最新。
        panel.handle(key(KeyCode::Char(' ')), &mut drafts);
        assert!(panel.activity.follow);
        // 搜索编辑优先于普通字符快捷键，Esc 先清空查询。
        panel.handle(key(KeyCode::Char('/')), &mut drafts);
        for character in "retry".chars() {
            panel.handle(key(KeyCode::Char(character)), &mut drafts);
        }
        assert_eq!(panel.activity.query, "retry");
        panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(panel.activity.query.is_empty());
        assert!(!panel.activity.editing);
        assert_eq!(panel.page, Page::Activity);
        // 再按 Esc 回到 Agents 页。
        panel.handle(key(KeyCode::Esc), &mut drafts);
        assert_eq!(panel.page, Page::Agents);
    }

    #[test]
    fn mouse_click_on_roster_row_opens_detail() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        let _ = render(&mut panel, &mut drafts, 140, 40);
        let (rect, agent) = panel.hits.first().cloned().expect("roster hit rect");
        assert_eq!(agent, "a1");
        panel.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 2,
                row: rect.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
            &mut drafts,
        );
        let commands = panel.take_commands();
        assert!(
            matches!(commands.as_slice(), [Command::Open { agent, .. }] if agent == "a1"),
            "roster click must open the mapped agent"
        );
    }

    #[test]
    fn detail_first_draw_resolves_run_scoped_anchor_and_reset_keeps_read_preview() {
        use super::worker::DetailHistory;
        use crate::tui::Transcript;
        use pp_runtime::subagent::EventPage;
        use serde_json::json;

        let runs = vec![
            run("r1", "a1", "completed", "first task"),
            run("r2", "a1", "completed", "second task"),
        ];
        let mut panel = panel(vec![record("a1", "Main", "idle")], runs.clone());
        panel.opened(
            "a1",
            1,
            Some(Anchor {
                run: Some("r1".into()),
                offset: 0,
                tool: Some("same".into()),
            }),
            Transcript::with_read_preview(true),
        );
        let mut entries = vec![];
        for (id, path) in [("r1", "first.rs"), ("r2", "second.rs")] {
            entries.push(entry(
                entries.len() as u64,
                id,
                AgentEvent::ToolStarted {
                    id: "same".into(),
                    name: "read".into(),
                    arguments: json!({"path":path}).to_string(),
                },
            ));
            entries.push(entry(
                entries.len() as u64,
                id,
                AgentEvent::ToolFinished {
                    id: "same".into(),
                    result: json!({"ok":true,"content":format!("body of {path}")}),
                },
            ));
        }
        entries.push(entry(
            4,
            "r2",
            AgentEvent::AssistantFinished {
                text: (0..40)
                    .map(|i| format!("tail-{i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            },
        ));
        panel.apply(Reply::Detail {
            epoch: 1,
            view: 1,
            agent: "a1".into(),
            history: DetailHistory {
                page: EventPage {
                    entries,
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
                runs: runs.clone(),
                settled: std::collections::BTreeSet::from(["r1".into(), "r2".into()]),
            },
        });
        // 历史可在首帧取得宽度前读完；此后没有新事件也必须定位。
        let mut drafts = Drafts::default();
        let screen = render(&mut panel, &mut drafts, 100, 12).join("\n");
        assert!(screen.contains("body of first.rs"), "{screen}");
        assert!(!screen.contains("tail-39"), "{screen}");
        panel.apply(Reply::Detail {
            epoch: 1,
            view: 1,
            agent: "a1".into(),
            history: DetailHistory {
                page: EventPage {
                    entries: vec![
                        entry(
                            0,
                            "r2",
                            AgentEvent::ToolStarted {
                                id: "same".into(),
                                name: "read".into(),
                                arguments: json!({"path":"replacement.rs"}).to_string(),
                            },
                        ),
                        entry(
                            1,
                            "r2",
                            AgentEvent::ToolFinished {
                                id: "same".into(),
                                result: json!({"ok":true,"content":"replacement body"}),
                            },
                        ),
                    ],
                    cursor: None,
                    reset: true,
                    has_more: false,
                    warnings: vec![],
                },
                runs,
                settled: std::collections::BTreeSet::from(["r2".into()]),
            },
        });
        let screen = render(&mut panel, &mut drafts, 100, 18).join("\n");
        assert!(screen.contains("replacement body"), "{screen}");
        assert!(
            !screen.contains("first.rs") && !screen.contains("tail-39"),
            "{screen}"
        );
    }

    #[test]
    fn filter_keeps_lineage_and_empty_state_blocks_controls() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![record("a1", "Main", "idle"), record("a2", "a1", "parked")],
            vec![],
        );
        panel.handle(key(KeyCode::Char('t')), &mut drafts);
        panel.handle(key(KeyCode::Char('/')), &mut drafts);
        for character in "a2".chars() {
            panel.handle(key(KeyCode::Char(character)), &mut drafts);
        }
        panel.handle(key(KeyCode::Enter), &mut drafts);
        let rows = panel.visible();
        assert_eq!(rows.len(), 2, "树中保留匹配项的祖先");
        assert!(rows[0].muted && !rows[1].muted);
        // 无匹配时不保留隐藏选择，也不能继续发控制。
        for character in "zzz".chars() {
            panel.handle(key(KeyCode::Char('/')), &mut drafts);
            panel.handle(key(KeyCode::Char(character)), &mut drafts);
            panel.handle(key(KeyCode::Enter), &mut drafts);
        }
        assert!(panel.visible().is_empty());
        assert!(panel.selected_agent().is_none());
    }

    /// 带 usage、模型、工作目录与费用的已打开详情；后续用例只调整尺寸。
    fn usage_detail() -> Panel {
        let mut agent = record("a1", "Main", "idle");
        agent.spec.model = "fixture/primary".into();
        agent.spec.cwd = PathBuf::from("/work/app");
        agent.spec.thinking = ThinkLevel::High;
        let mut work = run("r1", "a1", "completed", "child work");
        work.cost = Some(0.012);
        work.duration_ms = 85_000;
        let mut panel = panel(vec![agent], vec![work]);
        panel.opened("a1", 1, None, crate::tui::Transcript::new());
        panel.apply(Reply::Activity {
            epoch: 1,
            agent: "a1".into(),
            page: pp_runtime::subagent::EventPage {
                entries: vec![entry(
                    0,
                    "r1",
                    AgentEvent::ContextUpdated {
                        usage: ContextUsage {
                            estimated_tokens: 12_300,
                            context_window: Some(200_000),
                            last_request: Some(TokenUsage {
                                input_tokens: 12_300,
                                output_tokens: 45_678,
                                cached_tokens: Some(8_000),
                                cache_write_tokens: Some(0),
                            }),
                            session_output_tokens: 45_678,
                            auto_compact: false,
                            threshold_percent: 80,
                        },
                    },
                )],
                cursor: None,
                reset: false,
                has_more: false,
                warnings: vec![],
            },
        });
        panel
    }

    #[test]
    fn detail_header_is_one_line_without_model_effort_or_usage_summary() {
        let mut drafts = Drafts::default();
        let mut panel = panel(
            vec![record("a1", "Main", "idle")],
            vec![run("r1", "a1", "running", "child work")],
        );
        panel.opened("a1", 1, None, crate::tui::Transcript::new());
        let screen = render(&mut panel, &mut drafts, 120, 30);
        assert!(
            screen[0].contains("Agent Hub · a1-name / a1 · running · parent Main"),
            "{}",
            screen[0]
        );
        for absent in ["fixture-model", "effort", "tokens"] {
            assert!(!screen[0].contains(absent), "{absent}: {}", screen[0]);
        }
    }

    #[test]
    fn detail_input_metrics_and_status_bar_reuse_main_styles() {
        let mut drafts = Drafts::default();
        let mut panel = usage_detail();
        let buffer = render_cells(&mut panel, &mut drafts, &Default::default(), 120, 30);
        let screen = rows(&buffer, 120, 30);
        let banner = screen
            .iter()
            .position(|line| line.contains("subagent"))
            .unwrap_or_else(|| panic!("{screen:#?}"));
        // 顶边框行：标识、ctx、io 对与费用；左侧边框字符保留。
        assert!(screen[banner].starts_with('─'), "{}", screen[banner]);
        for expected in ["subagent", "12.3K/45.7K", "0.012"] {
            assert!(
                screen[banner].contains(expected),
                "{expected}: {}",
                screen[banner]
            );
        }
        let offset = screen[banner].find("subagent").unwrap();
        let column = screen[banner][..offset].chars().count();
        assert_eq!(
            buffer[(column as u16, banner as u16)].fg,
            Color::Rgb(254, 188, 56),
            "标识琥珀色"
        );
        // 提示行在输入框上方。
        assert!(
            screen[banner - 1].contains("Shift+Enter newline"),
            "{}",
            screen[banner - 1]
        );
        // 底边框整行是 ─。
        assert_eq!(screen[banner + 2], "─".repeat(120));
        // 末行状态栏复用主结构，不带旧摘要。
        for expected in ["fixture/primary", "hig", "/work/app"] {
            assert!(screen[29].contains(expected), "{expected}: {}", screen[29]);
        }
        let text = screen.join("\n");
        assert!(!text.contains(" tokens · "), "{text}");
    }

    #[test]
    fn detail_focus_requests_cursor_inside_editor() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        panel.opened("a1", 1, None, crate::tui::Transcript::new());
        let mut unfocused = Terminal::new(TestBackend::new(100, 30)).unwrap();
        unfocused
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts, &Default::default()))
            .unwrap();
        assert!(!unfocused.backend().cursor_visible(), "未聚焦不显示光标");
        panel.focus_editor();
        let mut focused = Terminal::new(TestBackend::new(100, 30)).unwrap();
        focused
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts, &Default::default()))
            .unwrap();
        assert!(focused.backend().cursor_visible(), "聚焦后显示光标");
        // 前 2 列为 ❯ 提示，下面依次是底边框与状态栏。
        assert_eq!(
            focused.backend().cursor_position(),
            ratatui::layout::Position::new(2, 30 - 3)
        );
    }

    #[test]
    fn detail_narrow_and_tiny_sizes_keep_badge_without_panicking() {
        let mut drafts = Drafts::default();
        let mut panel = usage_detail();
        let icons = [
            "\u{e62a}",
            "\u{f04e2}",
            "\u{f01bc}",
            "\u{f01c1}",
            "\u{f051b}",
        ];
        let counts = [120u16, 80, 60, 40, 30, 20]
            .into_iter()
            .map(|width| {
                let screen = render(&mut panel, &mut drafts, width, 24);
                let row = screen
                    .iter()
                    .find(|line| line.contains('\u{f007}'))
                    .unwrap_or_else(|| panic!("{width}: {screen:#?}"));
                icons.iter().filter(|icon| row.contains(**icon)).count()
            })
            .collect::<Vec<_>>();
        assert!(counts[0] > counts[counts.len() - 1], "{counts:?}");
        assert!(
            counts.windows(2).all(|pair| pair[0] >= pair[1]),
            "指标图标数量随宽度单调不增: {counts:?}"
        );
        for width in [40u16, 20] {
            let screen = render(&mut panel, &mut drafts, width, 24).join("\n");
            assert!(screen.contains("subagent"), "{width}: {screen}");
        }
        // 极矮或极窄的终端不得越界或 panic。
        for (width, height) in [
            (1u16, 1u16),
            (2, 2),
            (3, 3),
            (10, 3),
            (20, 1),
            (120, 2),
            (80, 1),
            (1, 12),
        ] {
            let _ = render(&mut panel, &mut drafts, width, height);
        }
    }
}
