//! Agent Hub: the Agents overview, the full Activity view and the standalone child detail.
//!
//! The panel only holds snapshots, projections and an action queue. History reads and control run on the observer thread, and the speed probe reads the in-memory snapshot.
//! Closing the Hub only stops observation; it never cancels a child.

mod activity;
mod detail;
mod input;
mod inspector;
mod projection;
mod roster;
mod worker;

use super::transcript_style::{
    accent, activity, added, ask_border, dim, done_icon, fail_icon, muted, removed, think_icon,
    waiting_clock,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
pub(in crate::tui) use detail::Anchor;
use detail::Detail;
use inspector::status_icon;
use projection::{ActivityRow, AgentStatus, AgentView, Category, Kind, RowStatus, Scope, TreeRow};
pub(in crate::tui) use projection::{Projection, now_ms as now};
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
fn hub_bg() -> Color {
    super::themes::palette().ask_bg
}

fn selected_bg() -> Color {
    super::themes::palette().panel_alt_bg
}

fn cost_color() -> Color {
    super::themes::palette().status_cost
}

fn effort_color() -> Color {
    super::themes::palette().menu_default_model
}

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

/// Child draft: a TextArea per agent id, kept in memory only for the current root.
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

    /// Only the same unmodified draft is cleared after a successful send.
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

/// Panel request; the root/view epoch is filled in by App.
pub(super) enum Command {
    Open {
        agent: String,
        anchor: Option<Anchor>,
    },
    CloseDetail {
        view: u64,
    },
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
    pub(in crate::tui) fn with_projection(projection: Projection) -> Self {
        let mut panel = Self {
            projection,
            ..Self::default()
        };
        panel.validate_selection();
        panel
    }

    pub(in crate::tui) fn into_projection(self) -> Projection {
        self.projection
    }

    pub(in crate::tui) fn reset_projection(&mut self) {
        self.projection = Projection::default();
        self.detail = None;
        self.selected = None;
        self.inspector = 0;
        self.notice = None;
        self.error = None;
        self.agent_notices.clear();
        self.sending = None;
    }

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
        mut transcript: super::Transcript,
    ) {
        if let Some(agent) = self.projection.agent(agent) {
            transcript = transcript.with_cwd(&agent.cwd);
        }
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

    /// Control ACK: place the notice only on the matching agent and never overwrite the detail of another agent.
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
                    // Report with a short notice when the detail does not belong to that agent, without overwriting other details.
                    self.notice = Some(format!("Control failed for {agent}: {error}"));
                }
            }
        }
    }

    pub(super) fn set_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub(super) fn apply(&mut self, reply: Reply) {
        let update_previews = matches!(&reply, Reply::Snapshot { .. } | Reply::Detail { .. });
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
            Reply::ActivityBatch { pages, .. } => {
                let mut warnings = Vec::new();
                for (_, page) in &pages {
                    warnings.extend(page.warnings.iter().cloned());
                }
                if !warnings.is_empty() {
                    self.error = Some(warnings.join("; "));
                }
                self.projection.ingest_overview_batch(&pages, self.clock);
                self.validate_selection();
            }
            Reply::Detail { agent, history, .. } => {
                let page = &history.page;
                if !page.warnings.is_empty() {
                    self.error = Some(page.warnings.join("; "));
                }
                if let Some(detail) = self.detail.as_mut().filter(|detail| detail.agent == agent) {
                    if page.reset {
                        // Rebuild from scratch after a file replacement, keeping the draft and the follow preference.
                        detail.rebuild();
                    }
                    detail.ingest(page, &history.runs, &history.settled, projection::now_ms());
                }
            }
            Reply::Reset { agent, .. } => {
                self.projection.reset(&agent);
                self.validate_selection();
            }
            Reply::Control { agent, result, .. } => self.send_settled(&agent, result),
            Reply::Error { view, message, .. } => {
                if let Some(view) = view {
                    if let Some(detail) = self.detail.as_mut().filter(|detail| detail.view == view)
                    {
                        detail.history_failed(message);
                    }
                } else {
                    self.set_error(message);
                }
            }
        }
        if update_previews
            && let Some(detail) = &mut self.detail
            && let Some(snapshot) = self.projection.snapshot()
        {
            for preview in &snapshot.output_previews {
                if preview.agent_id == detail.agent {
                    detail.output_preview(preview);
                }
            }
            for preview in &snapshot.shell_previews {
                if preview.agent_id == detail.agent {
                    detail.shell_preview(preview);
                }
            }
        }
    }

    pub(super) fn tick(
        &mut self,
        now_ms: u64,
        instant: std::time::Instant,
        binding: Option<&pp_runtime::subagent::Binding>,
    ) -> bool {
        let frame_changed = |status| status_icon(status, self.clock) != status_icon(status, now_ms);
        let running_changed = frame_changed(AgentStatus::Running);
        let queued_changed = frame_changed(AgentStatus::Queued);
        let agent_frame_changed = |agent: &AgentView| match agent.status {
            AgentStatus::Running => running_changed,
            AgentStatus::Queued => queued_changed,
            _ => false,
        };
        let clock_changed = if let Some(detail) = &self.detail {
            self.projection
                .agent(&detail.agent)
                .is_some_and(agent_frame_changed)
        } else if self.page == Page::Activity {
            queued_changed
                && self
                    .projection
                    .rows()
                    .iter()
                    .any(|row| row.status == RowStatus::Pending)
        } else {
            (running_changed || queued_changed)
                && self.projection.agents().iter().any(agent_frame_changed)
        };
        self.clock = now_ms;
        let detail_changed = self.detail.as_mut().is_some_and(|detail| {
            if let Some(binding) = binding {
                detail.sample_model_progress(binding);
            }
            detail.tick(instant)
        });
        // age/elapsed refresh once per second instead of re-reading the whole file for the clock.
        if now_ms.saturating_sub(self.rendered) >= 1000 {
            self.rendered = now_ms;
            self.projection.refresh(now_ms);
            return true;
        }
        clock_changed || detail_changed
    }
}

fn status_color(status: AgentStatus) -> Color {
    let light = matches!(hub_bg(), Color::Rgb(r, g, b)
        if 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) >= 128_000);
    match (status, light) {
        (AgentStatus::Running, false) => Color::Rgb(140, 217, 123),
        (AgentStatus::Running, true) => Color::Rgb(37, 109, 52),
        (AgentStatus::Queued, false) => Color::Rgb(244, 176, 74),
        (AgentStatus::Queued, true) => Color::Rgb(139, 81, 0),
        (AgentStatus::Completed, false) => Color::Rgb(176, 183, 198),
        (AgentStatus::Completed, true) => Color::Rgb(86, 96, 113),
        (AgentStatus::Idle, _) => added(),
        (AgentStatus::Parked, _) => muted(),
        (AgentStatus::Aborted, _) => removed(),
    }
}

fn row_color(status: RowStatus) -> ratatui::style::Color {
    match status {
        RowStatus::Pending => accent(),
        RowStatus::Success => added(),
        RowStatus::Error => removed(),
        RowStatus::Aborted => muted(),
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
    let mut style = Style::default().fg(muted());
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
    lines.push(Line::from(colored(title, accent())));
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
        .map(|line| line.patch_style(Style::default().bg(selected_bg())))
        .collect()
}

fn visible_start(heights: &[u16], selected: Option<usize>, height: u16) -> usize {
    let Some(selected) = selected else {
        return 0;
    };
    let limit = usize::from(height);
    let mut start = 0;
    while start < selected {
        let total: usize = heights[start..=selected]
            .iter()
            .copied()
            .map(usize::from)
            .sum();
        if total <= limit {
            break;
        }
        start += 1;
    }
    start
}

fn tail_start(heights: &[u16], height: u16) -> usize {
    let limit = usize::from(height);
    let mut start = heights.len();
    let mut total = 0usize;
    while start > 0 {
        let next = usize::from(heights[start - 1]);
        if total + next > limit {
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
mod fixtures {
    use super::*;
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

    pub(super) fn spec(depth: usize) -> Spec {
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

    pub(super) fn record(id: &str, parent: &str, status: &str) -> Record {
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

    pub(super) fn run(id: &str, agent: &str, status: &str, task: &str) -> Run {
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

    pub(super) fn snapshot(agents: Vec<Record>, runs: Vec<Run>) -> HubSnapshot {
        HubSnapshot {
            output_previews: Vec::new(),
            shell_previews: Vec::new(),
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

    pub(super) fn entry(offset: u64, run: &str, event: AgentEvent) -> ObservedEntry {
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

    pub(super) fn panel(agents: Vec<Record>, runs: Vec<Run>) -> Panel {
        let mut panel = Panel::default();
        panel.apply(Reply::Snapshot {
            epoch: 1,
            snapshot: Box::new(snapshot(agents, runs)),
        });
        panel
    }

    pub(super) fn render(
        panel: &mut Panel,
        drafts: &mut Drafts,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        rows(
            &render_cells(panel, drafts, &Default::default(), width, height),
            width,
            height,
        )
    }

    pub(super) fn render_cells(
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

    pub(super) fn rows(buffer: &ratatui::buffer::Buffer, width: u16, height: u16) -> Vec<String> {
        (0..height.max(1))
            .map(|row| {
                (0..width.max(1))
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(|cell| cell.symbol().to_owned())
                    .collect::<String>()
            })
            .collect()
    }

    pub(super) fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::from(code))
    }

    /// Open detail carrying usage, model, working directory and cost; the cases that follow only change the size.
    pub(super) fn usage_detail() -> Panel {
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
                                reasoning_tokens: None,
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
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn geometry_keeps_selection_visible_and_never_underflows() {
        assert_eq!(visible_start(&[2, 2, 2], Some(2), 4), 1);
        assert_eq!(visible_start(&[2, 2, 2], Some(0), 4), 0);
        assert_eq!(visible_start(&[5], Some(0), 2), 0);
        assert_eq!(tail_start(&[2, 2, 2, 2], 5), 2);
        assert_eq!(tail_start(&[], 5), 0);
        // A total height above u16::MAX must not overflow and panic.
        let tall = vec![20_000u16; 5];
        assert_eq!(visible_start(&tall, Some(4), 25_000), 4);
        assert_eq!(tail_start(&tall, 25_000), 4);
        // A zero size draws nothing and never goes out of bounds.
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![], vec![]);
        render(&mut panel, &mut drafts, 0, 0);
        render(&mut panel, &mut drafts, 1, 1);
    }
}
