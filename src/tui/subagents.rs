//! Agent Hub：Agents 总览、完整 Activity 与独立 child 详情。
//!
//! 面板只持快照、投影与动作队列，不做直接 I/O；所有读取与控制在观察线程里
//! 完成。关闭 Hub 只停止观察，不取消任何 child。

mod detail;
mod projection;
mod worker;

use super::transcript_style::{ACCENT, ACTIVITY, ADDED, ASK_BG, DIM, MUTED, READ_BORDER, REMOVED};
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
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};
use ratatui_textarea::TextArea;
use std::collections::BTreeMap;
pub(super) use worker::{Action as Control, Reply, Request, Worker};

const TITLE: &str = "Agent Hub";
const NARROW: u16 = 96;
const ROSTER_MIN: u16 = 48;
const INSPECTOR_MIN: u16 = 34;

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
    Roles,
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
    /// `/agents` 角色定义页：只读展示。
    pub roles: bool,
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
    roles_rows: Vec<String>,
    roles_state: RolesState,
    agent_notices: BTreeMap<String, String>,
    commands: Vec<Command>,
    hits: Vec<(Rect, String)>,
    activity_hits: Vec<(Rect, usize)>,
    sending: Option<String>,
    clock: u64,
    rendered: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RolesState {
    Loading,
    Ready,
}

impl Default for Panel {
    fn default() -> Self {
        Self {
            roles: false,
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
            roles_rows: Vec::new(),
            roles_state: RolesState::Loading,
            agent_notices: BTreeMap::new(),
            commands: Vec::new(),
            hits: Vec::new(),
            activity_hits: Vec::new(),
            sending: None,
            clock: projection::now_ms(),
            rendered: 0,
        }
    }
}

impl Panel {
    pub(super) fn new(roles: bool) -> Self {
        let mut panel = Self {
            roles,
            ..Self::default()
        };
        if roles {
            panel.commands.push(Command::Roles);
        }
        panel
    }

    pub(super) fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    pub(super) fn detail_view(&self) -> Option<u64> {
        self.detail.as_ref().map(|detail| detail.view)
    }

    pub(super) fn opened(&mut self, agent: &str, view: u64, anchor: Option<Anchor>) {
        let mut detail = Detail::new(agent, view);
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
            Reply::Detail { agent, page, .. } => {
                if !page.warnings.is_empty() {
                    self.error = Some(page.warnings.join("; "));
                }
                if let Some(detail) = self.detail.as_mut().filter(|detail| detail.agent == agent) {
                    if page.reset {
                        // 文件替换后从头重建，保留草稿与 follow 偏好。
                        let view = detail.view;
                        let focus = detail.focus;
                        let mut fresh = Detail::new(&agent, view);
                        fresh.focus = focus;
                        fresh.notice = Some("Event log was replaced; transcript rebuilt.".into());
                        self.detail = Some(fresh);
                    }
                    if let Some(detail) = self.detail.as_mut() {
                        detail.ingest(&page, &self.projection);
                    }
                }
            }
            Reply::Reset { agent, .. } => {
                self.projection.reset(&agent);
                self.validate_selection();
            }
            Reply::Roles { roles, .. } => {
                self.roles_state = RolesState::Ready;
                match roles {
                    Ok(roles) => {
                        self.roles_rows = roles
                            .into_iter()
                            .map(|role| {
                                format!("{} — {} [{}]", role.name, role.description, role.source)
                            })
                            .collect();
                    }
                    Err(error) => self.error = Some(error),
                }
            }
            Reply::Control { agent, result, .. } => self.send_settled(&agent, result),
            Reply::Error { message, .. } => {
                self.error = Some(message);
            }
        }
    }

    pub(super) fn tick(&mut self, now_ms: u64, instant: std::time::Instant) {
        self.clock = now_ms;
        if let Some(detail) = &mut self.detail {
            detail.tick(instant);
        }
        // age/elapsed 每秒刷新一次，不为时钟重读全文件。
        if now_ms.saturating_sub(self.rendered) >= 1000 {
            self.rendered = now_ms;
            self.projection.refresh(now_ms);
        }
    }

    fn validate_selection(&mut self) {
        let list = self.projection.list(self.tree, &self.filter);
        let known = self.selected.as_deref().is_none_or(|selected| {
            list.iter()
                .any(|row| self.projection.agents()[row.agent].id == selected)
        });
        if !known {
            self.selected = None;
        }
        if self.selected.is_none() {
            self.selected = list
                .first()
                .map(|row| self.projection.agents()[row.agent].id.clone());
        }
        if let Some(agent) = self.selected.clone()
            && let Some(view) = self.projection.agent(&agent)
        {
            // 隐藏的选择不能继续发控制。
            let _ = view;
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
                    let in_inspector = self.hits.iter().all(|(rect, _)| !contains(*rect, point));
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

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect, drafts: &mut Drafts) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(Clear, area);
        self.hits.clear();
        self.activity_hits.clear();
        if self.roles {
            self.draw_roles(frame, area);
            return;
        }
        if self.detail.is_some() {
            let agent = self.detail.as_ref().map(|detail| detail.agent.clone());
            if let Some(view) = agent
                .as_deref()
                .and_then(|agent| self.projection.agent(agent))
                .cloned()
            {
                if let Some(detail) = &mut self.detail {
                    detail.draw(frame, area, &view, drafts, self.clock);
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
        let hints = match self.page {
            Page::Agents => "j/k select · Enter detail · t tree · / filter · c/x/r · Esc close",
            Page::Activity => "j/k move · Enter open · Space follow · f/s filter · Esc back",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(READ_BORDER))
            .title(self.title())
            .title_bottom(Line::from(Span::styled(
                format!(" {} ", super::transcript_style::display_text(hints)),
                Style::default().fg(DIM),
            )));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let lead = inner.height.min(1);
        let body = Rect::new(
            inner.x,
            inner.y + lead,
            inner.width,
            inner.height.saturating_sub(lead),
        );
        self.draw_lead(frame, Rect::new(inner.x, inner.y, inner.width, lead));
        match self.page {
            Page::Agents => self.draw_agents(frame, body),
            Page::Activity => self.draw_activity(frame, body),
        }
    }

    fn title(&self) -> Line<'static> {
        let tab = |label: &str, active: bool| {
            Span::styled(
                format!(" {label} "),
                if active {
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                },
            )
        };
        Line::from(vec![
            Span::styled(format!(" {TITLE} "), Style::default().fg(MUTED)),
            tab("1 Agents", self.page == Page::Agents),
            Span::styled("/", Style::default().fg(DIM)),
            tab("2 Activity", self.page == Page::Activity),
        ])
    }

    fn draw_lead(&mut self, frame: &mut Frame, area: Rect) {
        let agents = self.projection.agents();
        let running = agents
            .iter()
            .filter(|agent| agent.status == AgentStatus::Running)
            .count();
        let parked = agents
            .iter()
            .filter(|agent| agent.status == AgentStatus::Parked)
            .count();
        let aborted = agents
            .iter()
            .filter(|agent| agent.status == AgentStatus::Aborted)
            .count();
        let mut spans = vec![Span::styled(
            format!(
                "{} agents · {running} running · {parked} parked · {aborted} aborted",
                agents.len()
            ),
            Style::default().fg(ACTIVITY),
        )];
        if self.projection.loading() {
            spans.push(Span::styled(" · loading", Style::default().fg(ACCENT)));
        }
        if let Some(snapshot) = self.projection.snapshot() {
            for warning in &snapshot.warnings {
                spans.push(Span::styled(
                    format!(
                        " · {}",
                        super::transcript_style::display_text(warning).into_owned()
                    ),
                    Style::default().fg(REMOVED),
                ));
            }
        }
        if let Some(error) = &self.error {
            spans.push(Span::styled(
                format!(
                    " · {}",
                    super::transcript_style::display_text(error).into_owned()
                ),
                Style::default().fg(REMOVED),
            ));
        }
        if let Some(notice) = &self.notice {
            spans.push(Span::styled(
                format!(" · {notice}"),
                Style::default().fg(ACCENT),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans)).wrap(Wrap { trim: true }),
            area,
        );
    }

    fn draw_agents(&mut self, frame: &mut Frame, area: Rect) {
        let wide = area.width >= NARROW;
        let (roster, inspector) = if wide {
            let gap = 1;
            let target = (u32::from(area.width) * 58 / 100) as u16;
            let roster_width = target.clamp(ROSTER_MIN, area.width - gap - INSPECTOR_MIN);
            (
                Rect::new(area.x, area.y, roster_width, area.height),
                Some(Rect::new(
                    area.x + roster_width + gap,
                    area.y,
                    area.width - roster_width - gap,
                    area.height,
                )),
            )
        } else if self.focus == Focus::Inspector {
            (Rect::default(), Some(area))
        } else {
            (area, None)
        };
        if roster.width > 0 {
            self.draw_roster(frame, roster);
        }
        if let Some(inspector) = inspector {
            self.draw_inspector(frame, inspector);
        }
    }

    fn draw_roster(&mut self, frame: &mut Frame, area: Rect) {
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
                let lines = self.row_lines(row, area.width);
                lines_height(&lines, area.width)
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
            let lines = style_row(lines, is_selected);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
            let agent = self.projection.agents()[row.agent].id.clone();
            self.hits.push((rect, agent));
            y += height;
        }
    }

    fn row_lines(&self, row: &TreeRow, width: u16) -> Vec<Line<'static>> {
        let _ = width;
        let agent = &self.projection.agents()[row.agent];
        let status_color = status_color(agent.status);
        let marker = if self.selected.as_deref() == Some(agent.id.as_str()) {
            "›"
        } else {
            " "
        };
        let badge = match &agent.run_status {
            Some(run_status) => format!("{} ({run_status})", agent.status.label()),
            None => agent.status.label().to_owned(),
        };
        let mut first = vec![
            Span::styled(
                format!(
                    "{marker}{}{}",
                    row.prefix,
                    super::transcript_style::display_text(&agent.name).into_owned()
                ),
                Style::default().fg(if row.muted { MUTED } else { ACTIVITY }),
            ),
            Span::styled(format!("  {}", agent.id), Style::default().fg(DIM)),
            Span::styled(
                format!("  {badge}"),
                Style::default().fg(if row.muted { MUTED } else { status_color }),
            ),
            Span::styled(format!("  {}", agent.definition), Style::default().fg(DIM)),
        ];
        if let Some(change) = &agent.model_change {
            first.push(Span::styled(
                format!("  {change}"),
                Style::default().fg(ACCENT),
            ));
        }
        let secondary = agent
            .task
            .as_deref()
            .or(agent.current.as_deref())
            .unwrap_or("—");
        let tokens = agent
            .tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "—".into());
        let cost = agent
            .cost
            .map(|cost| format!("${cost:.4}"))
            .unwrap_or_else(|| "cost —".into());
        vec![
            Line::from(first),
            Line::from(Span::styled(
                format!(
                    "   {} · {}",
                    super::transcript_style::display_text(&agent.model).into_owned(),
                    summary_line(secondary)
                ),
                Style::default().fg(if row.muted { DIM } else { MUTED }),
            )),
            Line::from(Span::styled(
                format!(
                    "   {} req · {tokens} tok · {cost}{}",
                    agent.requests,
                    if agent.loading { " · loading" } else { "" }
                ),
                Style::default().fg(DIM),
            )),
        ]
    }

    fn draw_inspector(&mut self, frame: &mut Frame, area: Rect) {
        let Some(agent) = self.selected_agent().cloned() else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "Select an agent to inspect.",
                    Style::default().fg(MUTED),
                ))),
                area,
            );
            return;
        };
        let lines = self.inspector_lines(&agent, area.width);
        let height = area.height as usize;
        let max = lines.len().saturating_sub(height) as u16;
        if self.inspector > max {
            self.inspector = max;
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.inspector, 0)),
            area,
        );
    }

    fn inspector_lines(&self, agent: &AgentView, width: u16) -> Vec<Line<'static>> {
        let _ = width;
        let mut lines = Vec::new();
        let section = |label: &str, lines: &mut Vec<Line<'static>>| {
            lines.push(Line::from(Span::styled(
                label.to_owned(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
        };
        section("Identity", &mut lines);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} {}", agent.name, agent.id),
                Style::default().fg(ACTIVITY),
            ),
            Span::styled(
                format!("  {}", agent.status.label()),
                Style::default().fg(status_color(agent.status)),
            ),
        ]));
        let age = agent
            .last_activity
            .map(|timestamp| {
                format!(
                    "last activity {}",
                    detail::format_age(self.clock.saturating_sub(timestamp))
                )
            })
            .unwrap_or_else(|| "last activity —".into());
        lines.push(Line::from(Span::styled(age, Style::default().fg(DIM))));
        section("Model", &mut lines);
        let mut model = format!("{} · effort {}", agent.model, agent.thinking);
        if let Some(change) = &agent.model_change {
            model.push_str(&format!(" · {change}"));
        }
        lines.push(Line::from(Span::styled(
            super::transcript_style::display_text(&model).into_owned(),
            Style::default().fg(ACTIVITY),
        )));
        section("Task", &mut lines);
        lines.extend(wrapped_text(agent.task.as_deref().unwrap_or("—")));
        section("Current", &mut lines);
        lines.extend(wrapped_text(agent.current.as_deref().unwrap_or("—")));
        section("Usage / Context", &mut lines);
        let tokens = agent
            .tokens
            .map(|tokens| tokens.to_string())
            .unwrap_or_else(|| "—".into());
        let cost = match (agent.cost, agent.cost_measured, agent.runs) {
            (Some(cost), measured, runs) if measured == runs || runs == 0 => {
                format!("${cost:.4}")
            }
            (Some(cost), measured, runs) => {
                format!("${cost:.4} ({measured}/{runs} runs measured)")
            }
            (None, _, _) => "cost —".into(),
        };
        let time = agent
            .time_ms
            .map(format_duration)
            .unwrap_or_else(|| "—".into());
        lines.push(Line::from(Span::styled(
            format!(
                "{} req · {tokens} tok · {cost} · agent time {time}",
                agent.requests
            ),
            Style::default().fg(ACTIVITY),
        )));
        let context = match agent.context {
            Some(usage) => {
                let window = usage
                    .context_window
                    .map(|window| format!(" / {window}"))
                    .unwrap_or_default();
                match usage.percent() {
                    Some(percent) => format!(
                        "context ~{}{window} tokens · {percent}%",
                        usage.estimated_tokens
                    ),
                    None => format!("context ~{}{window} tokens", usage.estimated_tokens),
                }
            }
            None => "context —".into(),
        };
        lines.push(Line::from(Span::styled(
            context,
            Style::default().fg(MUTED),
        )));
        if agent.loading {
            lines.push(Line::from(Span::styled(
                "full-history metrics still loading",
                Style::default().fg(ACCENT),
            )));
        }
        section("Lineage", &mut lines);
        lines.push(Line::from(Span::styled(
            format!(
                "parent {} · {} · depth {}",
                agent.parent, agent.definition, agent.depth
            ),
            Style::default().fg(MUTED),
        )));
        section("Changes / artifacts", &mut lines);
        if agent.readonly {
            lines.push(Line::from(Span::styled(
                "Read-only agent · no workspace writes",
                Style::default().fg(MUTED),
            )));
        }
        if agent.isolated {
            lines.push(Line::from(Span::styled(
                "Isolated workspace · patches only",
                Style::default().fg(MUTED),
            )));
        }
        if !agent.readonly && !agent.isolated {
            lines.push(Line::from(Span::styled(
                "Shared workspace · per-agent LoC not attributable",
                Style::default().fg(MUTED),
            )));
        }
        if agent.artifacts.is_empty() {
            lines.push(Line::from(Span::styled(
                "No published result yet",
                Style::default().fg(DIM),
            )));
        } else {
            for artifact in &agent.artifacts {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        super::transcript_style::display_text(artifact).into_owned()
                    ),
                    Style::default().fg(MUTED),
                )));
            }
        }
        if let Some(error) = &agent.last_error {
            lines.push(Line::from(Span::styled(
                format!("last error: {error}"),
                Style::default().fg(REMOVED),
            )));
        }
        section("Recent activity", &mut lines);
        let recent = self.projection.recent(&agent.id, 20);
        if recent.is_empty() {
            lines.push(Line::from(Span::styled("—", Style::default().fg(DIM))));
        }
        for row in recent {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", detail::format_clock(row.timestamp)),
                    Style::default().fg(DIM),
                ),
                Span::styled(
                    format!("{} ", row.status.symbol()),
                    Style::default().fg(row_color(row.status)),
                ),
                Span::styled(
                    format!("{} {}", row.title, summary_line(&row.summary)),
                    Style::default().fg(ACTIVITY),
                ),
            ]));
        }
        lines
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
                format!("{} ", row.status.symbol()),
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

    fn draw_roles(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(READ_BORDER))
            .title(" Agent Hub · role definitions ")
            .title_bottom(
                Line::from(Span::styled(
                    "Project > User > Built-in · Esc close",
                    Style::default().fg(DIM),
                ))
                .alignment(Alignment::Right),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![Line::from(Span::styled(
            "Role definitions (Project > User > Built-in)",
            Style::default().fg(ACCENT),
        ))];
        match self.roles_state {
            RolesState::Loading => lines.push(Line::from(Span::styled(
                "Loading role definitions…",
                Style::default().fg(MUTED),
            ))),
            RolesState::Ready => {
                if self.roles_rows.is_empty() {
                    lines.push(Line::from(Span::styled(
                        "No role definitions found.",
                        Style::default().fg(MUTED),
                    )));
                }
                for row in &self.roles_rows {
                    lines.push(Line::from(Span::styled(
                        super::transcript_style::display_text(row).into_owned(),
                        Style::default().fg(ACTIVITY),
                    )));
                }
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::default().fg(REMOVED),
            )));
        }
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
    let collapsed = super::transcript_style::display_text(text)
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

fn wrapped_text(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| {
            Line::from(Span::styled(
                super::transcript_style::display_text(line).into_owned(),
                Style::default().fg(ACTIVITY),
            ))
        })
        .collect()
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
        .map(|line| line.patch_style(Style::default().bg(ASK_BG).fg(ACCENT)))
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
    use crate::config::ThinkLevel;
    use crate::events::AgentEvent;
    use crate::subagent::{
        HubAgent, HubSnapshot, ObservedEntry, ObservedRecord, Record, Run, Spec,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use std::path::PathBuf;

    fn spec(depth: usize) -> Spec {
        Spec {
            model_role: None,
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
            requests: 0,
            tokens: 0,
            cost: None,
        }
    }

    fn snapshot(agents: Vec<Record>, runs: Vec<Run>) -> HubSnapshot {
        HubSnapshot {
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
        let mut panel = Panel::new(false);
        panel.apply(Reply::Snapshot {
            epoch: 1,
            snapshot: Box::new(snapshot(agents, runs)),
        });
        panel
    }

    fn render(panel: &mut Panel, drafts: &mut Drafts, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width.max(1), height.max(1))).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), drafts))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
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
        assert!(screen.contains("1 Agents"), "{screen}");
        assert!(screen.contains("2 Activity"));
        assert!(screen.contains("a1-name") && screen.contains("a2-name"));
        assert!(screen.contains("running"));
        // 选中项语义色：running 使用 ACCENT。
        let mut terminal = Terminal::new(TestBackend::new(140, 40)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area(), &mut drafts))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut found = None;
        for row in 0..40 {
            let line = (0..140)
                .filter_map(|column| buffer.cell((column, row)))
                .map(|cell| cell.symbol().to_owned())
                .collect::<String>();
            if line.contains("a2-name") {
                found = Some(row);
            }
        }
        let row = found.expect("selected agent visible");
        let color = (0..140)
            .find(|column| {
                buffer
                    .cell((*column, row))
                    .is_some_and(|cell| cell.symbol() == "r")
            })
            .and_then(|column| buffer.cell((column, row)))
            .map(|cell| cell.fg);
        assert!(color.is_some());
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
        assert!(!roster.contains("Usage / Context"));
        panel.handle(key(KeyCode::Tab), &mut drafts);
        let inspector = render(&mut panel, &mut drafts, 80, 24).join("\n");
        assert!(inspector.contains("Usage / Context"), "{inspector}");
        // Esc 先退出 inspector，再关闭 Hub。
        let close = panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(!close);
        let close = panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(close);
    }

    #[test]
    fn activity_paused_follow_keeps_viewport_and_escape_clears_search() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        for index in 0..6u64 {
            panel.apply(Reply::Activity {
                epoch: 1,
                agent: "a1".into(),
                page: crate::subagent::EventPage {
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
                page: crate::subagent::EventPage {
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
}
