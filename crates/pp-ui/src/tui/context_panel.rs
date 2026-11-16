//! `/context` 上下文诊断独立 TUI 面板：分类导航、排序、正文预览与键鼠交互。

use std::cmp::Reverse;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use pp_core::text::display_text;
use pp_runtime::agent::context_diagnosis::{CompactionStatus, ContextReport, DiagnosticItem};
use ratatui::layout::Rect;

mod render;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tui) enum Category {
    #[default]
    Overview,
    Largest,
    System,
    AgentsMd,
    Tools,
    Skills,
    History,
}

impl Category {
    pub(super) const ALL: [Self; 7] = [
        Self::Overview,
        Self::Largest,
        Self::System,
        Self::AgentsMd,
        Self::Tools,
        Self::Skills,
        Self::History,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Largest => "Largest Items",
            Self::System => "System Prompt",
            Self::AgentsMd => "AGENTS.md",
            Self::Tools => "Tools & MCP",
            Self::Skills => "Skills",
            Self::History => "History & Results",
        }
    }

    fn target_from_overview_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::System),
            1 => Some(Self::AgentsMd),
            2 => Some(Self::Skills),
            3 => Some(Self::Tools),
            4 | 5 => Some(Self::History),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Focus {
    Sidebar,
    #[default]
    List,
    Preview,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum SortOrder {
    #[default]
    Default,
    TokensDesc,
}

impl SortOrder {
    fn toggle(self) -> Self {
        match self {
            Self::Default => Self::TokensDesc,
            Self::TokensDesc => Self::Default,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::TokensDesc => "Tokens ↓",
        }
    }
}

pub(in crate::tui) enum Action {
    None,
    Close,
    Refresh,
}

pub(in crate::tui) struct Panel {
    report: Option<ContextReport>,
    overview_items: Vec<DiagnosticItem>,
    category: Category,
    focus: Focus,
    sort: SortOrder,
    list_index: usize,
    list_scroll: usize,
    list_visible: usize,
    preview_scroll: usize,
    preview_visible: usize,
    preview_max_scroll: usize,
    loading: bool,
    error: Option<String>,
    pub(super) area: Rect,
    sidebar_area: Rect,
    list_area: Rect,
    preview_area: Rect,
}

impl Panel {
    pub(in crate::tui) fn new() -> Self {
        Self {
            report: None,
            overview_items: Vec::new(),
            category: Category::Overview,
            focus: Focus::List,
            sort: SortOrder::Default,
            list_index: 0,
            list_scroll: 0,
            list_visible: 1,
            preview_scroll: 0,
            preview_visible: 1,
            preview_max_scroll: 0,
            loading: true,
            error: None,
            area: Rect::default(),
            sidebar_area: Rect::default(),
            list_area: Rect::default(),
            preview_area: Rect::default(),
        }
    }

    pub(in crate::tui) fn set_loading(&mut self) {
        self.loading = true;
        self.error = None;
    }

    pub(in crate::tui) fn set_error(&mut self, message: String) {
        self.loading = false;
        self.error = Some(display_text(&message).into_owned());
    }

    pub(in crate::tui) fn set_report(&mut self, report: ContextReport) {
        self.overview_items = build_overview_items(&report);
        self.report = Some(report);
        self.loading = false;
        self.error = None;
        self.clamp_selection();
    }

    pub(in crate::tui) fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            if self.focus == Focus::Preview && self.report.is_some() {
                self.focus = Focus::List;
                return Action::None;
            }
            return Action::Close;
        }
        if key.kind == KeyEventKind::Press && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('r') => {
                    self.set_loading();
                    return Action::Refresh;
                }
                KeyCode::Char('s') => {
                    if !matches!(self.category, Category::Overview | Category::Largest) {
                        self.sort = self.sort.toggle();
                        self.list_index = 0;
                        self.list_scroll = 0;
                        self.preview_scroll = 0;
                    }
                    return Action::None;
                }
                _ => {}
            }
        }
        if key.code == KeyCode::Tab && key.modifiers.is_empty() {
            self.focus = match self.focus {
                Focus::Sidebar => Focus::List,
                Focus::List => Focus::Preview,
                Focus::Preview => {
                    if self.sidebar_area.is_empty() {
                        Focus::List
                    } else {
                        Focus::Sidebar
                    }
                }
            };
            return Action::None;
        }
        if (key.code == KeyCode::BackTab
            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT))
            || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT)
        {
            self.focus = match self.focus {
                Focus::Sidebar => Focus::Preview,
                Focus::List => {
                    if self.sidebar_area.is_empty() {
                        Focus::Preview
                    } else {
                        Focus::Sidebar
                    }
                }
                Focus::Preview => Focus::List,
            };
            return Action::None;
        }
        if !key.modifiers.is_empty() {
            return Action::None;
        }
        match key.code {
            KeyCode::Left => {
                if self.sidebar_area.is_empty() && self.focus != Focus::Preview {
                    self.step_category(-1);
                } else {
                    self.focus = match self.focus {
                        Focus::Preview => Focus::List,
                        Focus::List if !self.sidebar_area.is_empty() => Focus::Sidebar,
                        other => other,
                    };
                }
            }
            KeyCode::Right => {
                if self.sidebar_area.is_empty() && self.focus != Focus::Preview {
                    self.step_category(1);
                } else {
                    self.focus = match self.focus {
                        Focus::Sidebar => Focus::List,
                        Focus::List => Focus::Preview,
                        Focus::Preview => Focus::Preview,
                    };
                }
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => match self.focus {
                Focus::Sidebar => self.focus = Focus::List,
                Focus::List => {
                    if self.category == Category::Overview
                        && let Some(target) = Category::target_from_overview_index(self.list_index)
                    {
                        self.select_category(target);
                    } else {
                        self.focus = Focus::Preview;
                    }
                }
                Focus::Preview => {}
            },
            KeyCode::Up => match self.focus {
                Focus::Sidebar => self.step_category(-1),
                Focus::List => self.move_list(-1),
                Focus::Preview => self.scroll_preview(-1),
            },
            KeyCode::Down => match self.focus {
                Focus::Sidebar => self.step_category(1),
                Focus::List => self.move_list(1),
                Focus::Preview => self.scroll_preview(1),
            },
            KeyCode::PageUp => match self.focus {
                Focus::Sidebar => self.step_category(-1),
                Focus::List => self.move_list(-(self.list_visible.max(1) as isize)),
                Focus::Preview => self.scroll_preview(-(self.preview_visible.max(1) as isize)),
            },
            KeyCode::PageDown => match self.focus {
                Focus::Sidebar => self.step_category(1),
                Focus::List => self.move_list(self.list_visible.max(1) as isize),
                Focus::Preview => self.scroll_preview(self.preview_visible.max(1) as isize),
            },
            KeyCode::Home => match self.focus {
                Focus::Sidebar => self.select_category(Category::ALL[0]),
                Focus::List => {
                    self.list_index = 0;
                    self.list_scroll = 0;
                    self.preview_scroll = 0;
                }
                Focus::Preview => self.preview_scroll = 0,
            },
            KeyCode::End => match self.focus {
                Focus::Sidebar => self.select_category(*Category::ALL.last().unwrap()),
                Focus::List => {
                    let count = self.current_items().len();
                    if count > 0 {
                        self.list_index = count - 1;
                        self.preview_scroll = 0;
                    }
                }
                Focus::Preview => self.preview_scroll = self.preview_max_scroll,
            },
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.sidebar_area.contains(pos) {
                    self.focus = Focus::Sidebar;
                    let idx = usize::from(mouse.row.saturating_sub(self.sidebar_area.y));
                    if let Some(&category) = Category::ALL.get(idx) {
                        self.select_category(category);
                    }
                } else if self.list_area.contains(pos) {
                    self.focus = Focus::List;
                    let row = usize::from(mouse.row.saturating_sub(self.list_area.y));
                    let idx = self.list_scroll + row;
                    if idx < self.current_items().len() {
                        self.list_index = idx;
                        self.preview_scroll = 0;
                    }
                } else if self.preview_area.contains(pos) {
                    self.focus = Focus::Preview;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -3
                } else {
                    3
                };
                if self.preview_area.contains(pos) {
                    self.scroll_preview(delta);
                } else if self.list_area.contains(pos) {
                    self.move_list(if delta < 0 { -1 } else { 1 });
                } else if self.sidebar_area.contains(pos) {
                    self.step_category(if delta < 0 { -1 } else { 1 });
                } else if self.focus == Focus::Preview {
                    self.scroll_preview(delta);
                } else {
                    self.move_list(if delta < 0 { -1 } else { 1 });
                }
            }
            _ => {}
        }
    }

    fn select_category(&mut self, category: Category) {
        if self.category != category {
            self.category = category;
            self.list_index = 0;
            self.list_scroll = 0;
            self.preview_scroll = 0;
        }
    }

    fn step_category(&mut self, delta: isize) {
        let current = Category::ALL
            .iter()
            .position(|c| *c == self.category)
            .unwrap_or(0) as isize;
        let len = Category::ALL.len() as isize;
        let next = ((current + delta).rem_euclid(len)) as usize;
        self.select_category(Category::ALL[next]);
    }

    fn move_list(&mut self, delta: isize) {
        let len = self.current_items().len();
        if len == 0 {
            self.list_index = 0;
            return;
        }
        let next = (self.list_index as isize + delta).clamp(0, (len - 1) as isize) as usize;
        if next != self.list_index {
            self.list_index = next;
            self.preview_scroll = 0;
        }
    }

    fn scroll_preview(&mut self, delta: isize) {
        if delta < 0 {
            self.preview_scroll = self.preview_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.preview_scroll = self
                .preview_scroll
                .saturating_add(delta as usize)
                .min(self.preview_max_scroll);
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.current_items().len();
        if len == 0 {
            self.list_index = 0;
            self.list_scroll = 0;
        } else if self.list_index >= len {
            self.list_index = len - 1;
        }
        self.preview_scroll = self.preview_scroll.min(self.preview_max_scroll);
    }

    pub(super) fn current_items(&self) -> Vec<&DiagnosticItem> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        let raw: &[DiagnosticItem] = match self.category {
            Category::Overview => &self.overview_items,
            Category::Largest => &report.largest_items,
            Category::System => &report.system_items,
            Category::AgentsMd => &report.agent_md_items,
            Category::Tools => &report.tool_items,
            Category::Skills => &report.skill_items,
            Category::History => &report.history_items,
        };
        if self.sort == SortOrder::Default
            || matches!(self.category, Category::Overview | Category::Largest)
        {
            return raw.iter().collect();
        }
        let mut headers: Vec<&DiagnosticItem> = raw.iter().filter(|i| i.is_header).collect();
        let mut entries: Vec<&DiagnosticItem> = raw.iter().filter(|i| !i.is_header).collect();
        entries.sort_by_key(|item| (Reverse(item.tokens), Reverse(item.bytes)));
        headers.extend(entries);
        headers
    }

    pub(super) fn category_tokens(&self, category: Category) -> u64 {
        let Some(report) = &self.report else {
            return 0;
        };
        match category {
            Category::Overview => report.prepared_tokens,
            Category::Largest => report.largest_items.first().map_or(0, |i| i.tokens),
            Category::System => report.categories[0].tokens,
            Category::AgentsMd => report.categories[1].tokens,
            Category::Skills => report.categories[2].tokens,
            Category::Tools => report.categories[3].tokens,
            Category::History => report.categories[4].tokens + report.categories[5].tokens,
        }
    }
}

fn build_overview_items(report: &ContextReport) -> Vec<DiagnosticItem> {
    let denom = report.prepared_tokens.max(1) as f64;
    let mut items: Vec<DiagnosticItem> = report
        .categories
        .iter()
        .map(|cat| {
            let pct = cat.tokens as f64 * 100.0 / denom;
            DiagnosticItem {
                kind_label: "Category".into(),
                title: cat.name.to_owned(),
                subtitle: format!("{} items · {}", cat.item_count, cat.description),
                tokens: cat.tokens,
                bytes: cat.bytes,
                preview: format!(
                    "Category: {}\nItems: {}\nEstimated Tokens: ~{} ({pct:.1}% of prepared request)\nEstimated Bytes: {}\n\n{}\n\nTip: Press Enter to inspect items in this category.",
                    cat.name, cat.item_count, cat.tokens, cat.bytes, cat.description
                ),
                is_header: false,
                rankable: false,
            }
        })
        .collect();

    let mut diag_lines = vec![
        format!("Model: {} ({})", report.model_reference, report.model_name),
        format!(
            "Protocol: {} · Mode: {} · Thinking: {}",
            report.protocol,
            report.work_mode,
            report.think_level.label()
        ),
        format!(
            "Active Context (status bar): {}",
            if report.empty_session {
                "0 tokens (empty session — nothing sent yet)".into()
            } else {
                format!("~{} tokens", report.active_tokens)
            }
        ),
        format!(
            "Prepared Request Baseline: ~{} tokens",
            report.prepared_tokens
        ),
    ];
    if let Some(window) = report.context_window {
        let active_pct = report.active_tokens as f64 * 100.0 / window as f64;
        let prep_pct = report.prepared_tokens as f64 * 100.0 / window as f64;
        let threshold_tokens = window * u64::from(report.threshold_percent) / 100;
        let headroom = threshold_tokens.saturating_sub(report.active_tokens);
        diag_lines.push(format!(
            "Context Window: {window} tokens (active {active_pct:.1}% · prepared {prep_pct:.1}%)"
        ));
        diag_lines.push(format!(
            "Auto-Compact: {} (threshold {}% = {threshold_tokens} tokens, remaining headroom ~{headroom} tokens)",
            if report.auto_compact { "on" } else { "off" },
            report.threshold_percent
        ));
    } else {
        diag_lines.push("Context Window: unconfigured (auto-compact inactive)".into());
    }
    diag_lines.push(String::new());
    match report.last_request {
        Some(usage) => {
            diag_lines.push("Last Server Request Usage:".into());
            diag_lines.push(format!("  Input tokens:       {}", usage.input_tokens));
            diag_lines.push(format!("  Output tokens:      {}", usage.output_tokens));
            if let Some(cached) = usage.cached_tokens {
                let hit = if usage.input_tokens > 0 {
                    format!(
                        " ({:.1}% hit)",
                        cached as f64 * 100.0 / usage.input_tokens as f64
                    )
                } else {
                    String::new()
                };
                diag_lines.push(format!("  Cache read tokens:  {cached}{hit}"));
            }
            if let Some(write) = usage.cache_write_tokens {
                diag_lines.push(format!("  Cache write tokens: {write}"));
            }
            let diff = usage.input_tokens as i128 - report.active_tokens as i128;
            diag_lines.push(format!(
                "  Delta vs local estimate: {:+} tokens (server vs ~{})",
                diff, report.active_tokens
            ));
        }
        None => diag_lines
            .push("Last Server Request Usage: unavailable (no completed request yet)".into()),
    }
    diag_lines.push(format!(
        "Session Total Output Tokens: {}",
        report.session_output_tokens
    ));

    items.push(DiagnosticItem {
        kind_label: "Diagnostics".into(),
        title: "Capacity, Cache & Server Usage".into(),
        subtitle: match report.last_request {
            Some(u) => format!(
                "server in {} · cached {}",
                u.input_tokens,
                u.cached_tokens
                    .map_or_else(|| "—".into(), |c| c.to_string())
            ),
            None => "no server usage yet".into(),
        },
        tokens: report
            .last_request
            .map_or(report.prepared_tokens, |u| u.input_tokens),
        bytes: 0,
        preview: diag_lines.join("\n"),
        is_header: false,
        rankable: false,
    });

    let comp = &report.compaction;
    let mut comp_lines = vec![
        format!("Total complete exchanges: {}", comp.total_exchanges),
        format!(
            "Retained suffix budget:   ~{} tokens",
            comp.retained_budget_tokens
        ),
        format!(
            "Target summary budget:    ~{} tokens",
            comp.summary_budget_tokens
        ),
        format!(
            "Existing [Context summary]: {}",
            if comp.has_existing_summary {
                "yes"
            } else {
                "no"
            }
        ),
        String::new(),
    ];
    let comp_subtitle = match &comp.status {
        CompactionStatus::NotEnoughHistory => {
            comp_lines.push(
                "Status: Not enough complete exchanges to compact (requires at least 2 exchanges)."
                    .into(),
            );
            "not enough exchanges (< 2)".into()
        }
        CompactionStatus::UnsupportedHistory => {
            comp_lines.push(
                "Status: History contains unsupported or incomplete tool call/result pairs.".into(),
            );
            "unsupported or incomplete history".into()
        }
        CompactionStatus::Ready {
            compressible_exchanges,
            compressible_tokens,
            retained_exchanges,
            retained_tokens,
        } => {
            comp_lines.push("If /compact runs now:".into());
            comp_lines.push(format!(
                "  Will summarize: {compressible_exchanges} older exchange(s) (~{compressible_tokens} tokens)"
            ));
            comp_lines.push(format!(
                "  Will preserve:  {retained_exchanges} recent exchange(s) (~{retained_tokens} tokens)"
            ));
            format!(
                "would summarize {compressible_exchanges} (~{compressible_tokens} tok) · keep {retained_exchanges}"
            )
        }
    };

    items.push(DiagnosticItem {
        kind_label: "Compaction".into(),
        title: "Compaction Boundary Preview".into(),
        subtitle: comp_subtitle,
        tokens: match comp.status {
            CompactionStatus::Ready {
                compressible_tokens,
                ..
            } => compressible_tokens,
            _ => 0,
        },
        bytes: 0,
        preview: comp_lines.join("\n"),
        is_header: false,
        rankable: false,
    });

    items
}

#[cfg(test)]
mod tests;
