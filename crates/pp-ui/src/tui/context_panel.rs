//! `/context` 上下文诊断独立 TUI 面板：先看总体指标，按分类钻取列表，点开再看具体详情。

use std::cmp::Reverse;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use pp_core::text::display_text;
use pp_runtime::agent::context_diagnosis::{
    CategoryKey, CompactionStatus, ContextReport, DiagnosticItem,
};
use ratatui::layout::Rect;

mod render;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tui) enum Category {
    #[default]
    Overview,
    System,
    AgentsMd,
    Skills,
    Tools,
    Mcp,
    Messages,
    Thinking,
    ToolResults,
}

impl Category {
    pub(super) const ALL: [Self; 9] = [
        Self::Overview,
        Self::System,
        Self::AgentsMd,
        Self::Skills,
        Self::Tools,
        Self::Mcp,
        Self::Messages,
        Self::Thinking,
        Self::ToolResults,
    ];

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::System => "System Prompt",
            Self::AgentsMd => "AGENTS.md",
            Self::Skills => "Skills",
            Self::Tools => "Tools",
            Self::Mcp => "MCP",
            Self::Messages => "Messages",
            Self::Thinking => "Thinking",
            Self::ToolResults => "Tool Results",
        }
    }

    fn from_key(key: CategoryKey) -> Self {
        match key {
            CategoryKey::System => Self::System,
            CategoryKey::AgentsMd => Self::AgentsMd,
            CategoryKey::Skills => Self::Skills,
            CategoryKey::Tools => Self::Tools,
            CategoryKey::Mcp => Self::Mcp,
            CategoryKey::Messages => Self::Messages,
            CategoryKey::Thinking => Self::Thinking,
            CategoryKey::ToolResults => Self::ToolResults,
        }
    }

    fn to_key(self) -> Option<CategoryKey> {
        match self {
            Self::Overview => None,
            Self::System => Some(CategoryKey::System),
            Self::AgentsMd => Some(CategoryKey::AgentsMd),
            Self::Skills => Some(CategoryKey::Skills),
            Self::Tools => Some(CategoryKey::Tools),
            Self::Mcp => Some(CategoryKey::Mcp),
            Self::Messages => Some(CategoryKey::Messages),
            Self::Thinking => Some(CategoryKey::Thinking),
            Self::ToolResults => Some(CategoryKey::ToolResults),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Focus {
    Sidebar,
    #[default]
    List,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ViewMode {
    #[default]
    List,
    Detail,
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

#[derive(Clone)]
struct OverviewRow {
    target: Option<Category>,
    in_context: bool,
    item: DiagnosticItem,
}

pub(in crate::tui) struct Panel {
    report: Option<ContextReport>,
    overview_rows: Vec<OverviewRow>,
    category: Category,
    focus: Focus,
    view: ViewMode,
    sort: SortOrder,
    list_index: usize,
    list_scroll: usize,
    list_visible: usize,
    detail_scroll: usize,
    detail_visible: usize,
    detail_max_scroll: usize,
    loading: bool,
    error: Option<String>,
    pub(super) area: Rect,
    sidebar_area: Rect,
    list_area: Rect,
    detail_area: Rect,
}

impl Panel {
    pub(in crate::tui) fn new() -> Self {
        Self {
            report: None,
            overview_rows: Vec::new(),
            category: Category::Overview,
            focus: Focus::List,
            view: ViewMode::List,
            sort: SortOrder::Default,
            list_index: 0,
            list_scroll: 0,
            list_visible: 1,
            detail_scroll: 0,
            detail_visible: 1,
            detail_max_scroll: 0,
            loading: true,
            error: None,
            area: Rect::default(),
            sidebar_area: Rect::default(),
            list_area: Rect::default(),
            detail_area: Rect::default(),
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
        self.overview_rows = build_overview_rows(&report);
        if report.empty_session {
            self.category = Category::Overview;
            self.view = ViewMode::List;
        }
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
            if self.view == ViewMode::Detail {
                self.view = ViewMode::List;
                self.detail_scroll = 0;
                return Action::None;
            }
            return Action::Close;
        }
        if key.code == KeyCode::Backspace && key.modifiers.is_empty() {
            if self.view == ViewMode::Detail {
                self.view = ViewMode::List;
                self.detail_scroll = 0;
            } else if self.category != Category::Overview {
                self.select_category(Category::Overview);
                self.focus = Focus::List;
            }
            return Action::None;
        }
        if key.kind == KeyEventKind::Press && key.modifiers.is_empty() {
            match key.code {
                KeyCode::Char('r') => {
                    self.set_loading();
                    return Action::Refresh;
                }
                KeyCode::Char('s') => {
                    if self.view == ViewMode::List
                        && self.report.as_ref().is_some_and(|r| !r.empty_session)
                    {
                        self.sort = self.sort.toggle();
                        self.list_index = 0;
                        self.list_scroll = 0;
                    }
                    return Action::None;
                }
                _ => {}
            }
        }
        if (key.code == KeyCode::Tab
            || key.code == KeyCode::BackTab
                && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            || key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT)
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
        {
            if self.view == ViewMode::Detail {
                self.view = ViewMode::List;
                self.detail_scroll = 0;
                self.focus = Focus::List;
            } else if !self.sidebar_area.is_empty() {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::List,
                    Focus::List => Focus::Sidebar,
                };
            }
            return Action::None;
        }
        if !key.modifiers.is_empty() {
            return Action::None;
        }
        if self.report.as_ref().is_some_and(|r| r.empty_session) {
            return Action::None;
        }
        match key.code {
            KeyCode::Left => {
                if self.view == ViewMode::Detail {
                    self.view = ViewMode::List;
                    self.detail_scroll = 0;
                } else if self.sidebar_area.is_empty() {
                    self.step_category(-1);
                } else {
                    self.focus = Focus::Sidebar;
                }
            }
            KeyCode::Right => {
                if self.view == ViewMode::List {
                    if self.sidebar_area.is_empty() {
                        self.step_category(1);
                    } else if self.focus == Focus::Sidebar {
                        self.focus = Focus::List;
                    } else {
                        self.activate_selected();
                    }
                }
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if self.view == ViewMode::Detail {
                    self.view = ViewMode::List;
                    self.detail_scroll = 0;
                } else if self.focus == Focus::Sidebar {
                    self.focus = Focus::List;
                } else {
                    self.activate_selected();
                }
            }
            KeyCode::Up => {
                if self.view == ViewMode::Detail {
                    self.scroll_detail(-1);
                } else {
                    match self.focus {
                        Focus::Sidebar => self.step_category(-1),
                        Focus::List => self.move_list(-1),
                    }
                }
            }
            KeyCode::Down => {
                if self.view == ViewMode::Detail {
                    self.scroll_detail(1);
                } else {
                    match self.focus {
                        Focus::Sidebar => self.step_category(1),
                        Focus::List => self.move_list(1),
                    }
                }
            }
            KeyCode::PageUp => {
                if self.view == ViewMode::Detail {
                    self.scroll_detail(-(self.detail_visible.max(1) as isize));
                } else {
                    match self.focus {
                        Focus::Sidebar => self.step_category(-1),
                        Focus::List => self.move_list(-(self.list_visible.max(1) as isize)),
                    }
                }
            }
            KeyCode::PageDown => {
                if self.view == ViewMode::Detail {
                    self.scroll_detail(self.detail_visible.max(1) as isize);
                } else {
                    match self.focus {
                        Focus::Sidebar => self.step_category(1),
                        Focus::List => self.move_list(self.list_visible.max(1) as isize),
                    }
                }
            }
            KeyCode::Home => {
                if self.view == ViewMode::Detail {
                    self.detail_scroll = 0;
                } else {
                    match self.focus {
                        Focus::Sidebar => self.select_category(Category::ALL[0]),
                        Focus::List => {
                            self.list_index = 0;
                            self.list_scroll = 0;
                        }
                    }
                }
            }
            KeyCode::End => {
                if self.view == ViewMode::Detail {
                    self.detail_scroll = self.detail_max_scroll;
                } else {
                    match self.focus {
                        Focus::Sidebar => self.select_category(*Category::ALL.last().unwrap()),
                        Focus::List => {
                            let count = self.current_items().len();
                            if count > 0 {
                                self.list_index = count - 1;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.report.as_ref().is_some_and(|r| r.empty_session) {
            return;
        }
        let pos = ratatui::layout::Position::new(mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.sidebar_area.contains(pos) {
                    self.focus = Focus::Sidebar;
                    let idx = usize::from(mouse.row.saturating_sub(self.sidebar_area.y));
                    if let Some(&category) = Category::ALL.get(idx) {
                        self.select_category(category);
                    }
                } else if self.view == ViewMode::List && self.list_area.contains(pos) {
                    self.focus = Focus::List;
                    let row = usize::from(mouse.row.saturating_sub(self.list_area.y));
                    let visual_idx = self.list_scroll + row;
                    if let Some(idx) = self.visual_row_to_item_index(visual_idx) {
                        if self.list_index == idx {
                            self.activate_selected();
                        } else {
                            self.list_index = idx;
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                    -3
                } else {
                    3
                };
                if self.view == ViewMode::Detail {
                    self.scroll_detail(delta);
                } else if self.sidebar_area.contains(pos) {
                    self.step_category(if delta < 0 { -1 } else { 1 });
                } else {
                    self.move_list(if delta < 0 { -1 } else { 1 });
                }
            }
            _ => {}
        }
    }

    fn activate_selected(&mut self) {
        if self.category == Category::Overview {
            let target = self
                .ordered_overview_rows()
                .get(self.list_index)
                .and_then(|row| row.target);
            if let Some(target) = target {
                self.select_category(target);
                self.focus = Focus::List;
                return;
            }
        }
        if self.current_items().get(self.list_index).is_some() {
            self.detail_scroll = 0;
            self.view = ViewMode::Detail;
        }
    }

    fn select_category(&mut self, category: Category) {
        if self.category != category || self.view != ViewMode::List {
            self.category = category;
            self.view = ViewMode::List;
            self.list_index = 0;
            self.list_scroll = 0;
            self.detail_scroll = 0;
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
        self.list_index = next;
    }

    fn scroll_detail(&mut self, delta: isize) {
        if delta < 0 {
            self.detail_scroll = self.detail_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.detail_scroll = self
                .detail_scroll
                .saturating_add(delta as usize)
                .min(self.detail_max_scroll);
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.current_items().len();
        if len == 0 {
            self.list_index = 0;
            self.list_scroll = 0;
            self.view = ViewMode::List;
        } else if self.list_index >= len {
            self.list_index = len - 1;
        }
        self.detail_scroll = self.detail_scroll.min(self.detail_max_scroll);
    }

    fn ordered_overview_rows(&self) -> Vec<&OverviewRow> {
        if self.sort == SortOrder::Default {
            return self.overview_rows.iter().collect();
        }
        let mut categories: Vec<&OverviewRow> = self
            .overview_rows
            .iter()
            .filter(|row| row.target.is_some())
            .collect();
        categories.sort_by_key(|row| (Reverse(row.item.tokens), Reverse(row.item.bytes)));
        categories.extend(self.overview_rows.iter().filter(|row| row.target.is_none()));
        categories
    }

    pub(super) fn overview_separator_index(&self) -> Option<usize> {
        if self.category != Category::Overview {
            return None;
        }
        let rows = self.ordered_overview_rows();
        let cat_count = rows.iter().take_while(|row| row.target.is_some()).count();
        (cat_count > 0 && cat_count < rows.len()).then_some(cat_count)
    }

    pub(super) fn visual_row_to_item_index(&self, visual_idx: usize) -> Option<usize> {
        let len = self.current_items().len();
        let item_idx = match self.overview_separator_index() {
            Some(sep) if visual_idx == sep => return None,
            Some(sep) if visual_idx > sep => visual_idx - 1,
            _ => visual_idx,
        };
        (item_idx < len).then_some(item_idx)
    }

    pub(super) fn current_items(&self) -> Vec<&DiagnosticItem> {
        let Some(report) = &self.report else {
            return Vec::new();
        };
        if report.empty_session {
            return Vec::new();
        }
        if self.category == Category::Overview {
            return self
                .ordered_overview_rows()
                .into_iter()
                .map(|r| &r.item)
                .collect();
        }
        let raw: &[DiagnosticItem] = match self.category {
            Category::Overview => unreachable!(),
            Category::System => &report.system_items,
            Category::AgentsMd => &report.agent_md_items,
            Category::Skills => &report.skill_items,
            Category::Tools => &report.tool_items,
            Category::Mcp => &report.mcp_items,
            Category::Messages => &report.message_items,
            Category::Thinking => &report.thinking_items,
            Category::ToolResults => &report.tool_result_items,
        };
        if self.sort == SortOrder::Default {
            return raw.iter().collect();
        }
        let mut headers: Vec<&DiagnosticItem> = raw.iter().filter(|i| i.is_header).collect();
        let mut entries: Vec<&DiagnosticItem> = raw.iter().filter(|i| !i.is_header).collect();
        entries.sort_by_key(|item| (Reverse(item.tokens), Reverse(item.bytes)));
        headers.extend(entries);
        headers
    }

    pub(super) fn current_row_in_context(&self, idx: usize) -> bool {
        if self.category == Category::Overview {
            self.ordered_overview_rows()
                .get(idx)
                .is_none_or(|row| row.in_context)
        } else if self.category == Category::Thinking {
            self.report.as_ref().is_none_or(|r| r.thinking_in_context)
        } else {
            true
        }
    }

    pub(super) fn category_tokens(&self, category: Category) -> u64 {
        let Some(report) = &self.report else {
            return 0;
        };
        if report.empty_session {
            return 0;
        }
        match category {
            Category::Overview => report.active_tokens,
            other => other
                .to_key()
                .and_then(|key| report.categories.iter().find(|c| c.key == key))
                .map_or(0, |c| c.tokens),
        }
    }
}

fn build_overview_rows(report: &ContextReport) -> Vec<OverviewRow> {
    if report.empty_session {
        return Vec::new();
    }
    let denom = report.active_tokens.max(1) as f64;
    let mut rows: Vec<OverviewRow> = report
        .categories
        .iter()
        .map(|cat| {
            let pct_line = if cat.in_context {
                let pct = cat.tokens as f64 * 100.0 / denom;
                format!("~{} ({pct:.1}% of active context)", cat.tokens)
            } else {
                format!("~{} (generated thinking output, not in next prompt)", cat.tokens)
            };
            OverviewRow {
                target: Some(Category::from_key(cat.key)),
                in_context: cat.in_context,
                item: DiagnosticItem {
                    kind_label: "Category".into(),
                    title: cat.name.to_owned(),
                    subtitle: format!("{} items · {}", cat.item_count, cat.description),
                    tokens: cat.tokens,
                    bytes: cat.bytes,
                    preview: format!(
                        "Category: {}\nItems: {}\nEstimated Tokens: {pct_line}\nEstimated Bytes: {}\n\n{}\n\nTip: Press Enter to inspect items in this category.",
                        cat.name, cat.item_count, cat.bytes, cat.description
                    ),
                    is_header: false,
                    rankable: false,
                },
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
            "Active Context (estimated): ~{} tokens",
            report.active_tokens
        ),
    ];
    if let Some(window) = report.context_window {
        let active_pct = report.active_tokens as f64 * 100.0 / window as f64;
        let threshold_tokens = window * u64::from(report.threshold_percent) / 100;
        let headroom = threshold_tokens.saturating_sub(report.active_tokens);
        diag_lines.push(format!(
            "Context Window: {window} tokens ({active_pct:.1}% used)"
        ));
        diag_lines.push(format!(
            "Auto-Compact: {} (threshold {}% = {threshold_tokens} tokens, headroom ~{headroom} tokens)",
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
        }
        None => diag_lines.push("Last Server Request Usage: unavailable yet".into()),
    }
    diag_lines.push(format!(
        "Session Total Output Tokens: {}",
        report.session_output_tokens
    ));
    diag_lines.push(format!(
        "LLM Thinking: ~{} tokens ({} turns, {})",
        report.thinking_tokens,
        report.thinking_turns,
        if report.thinking_in_context {
            "in context"
        } else {
            "generated output"
        }
    ));

    rows.push(OverviewRow {
        target: None,
        in_context: true,
        item: DiagnosticItem {
            kind_label: "Diagnostics".into(),
            title: "Server Usage & Capacity".into(),
            subtitle: match report.last_request {
                Some(u) => format!(
                    "in {} · out {} · cached {}",
                    u.input_tokens,
                    u.output_tokens,
                    u.cached_tokens
                        .map_or_else(|| "—".into(), |c| c.to_string())
                ),
                None => format!(
                    "session out {} · no server request recorded",
                    report.session_output_tokens
                ),
            },
            tokens: report
                .last_request
                .map_or(report.active_tokens, |u| u.input_tokens),
            bytes: 0,
            preview: diag_lines.join("\n"),
            is_header: false,
            rankable: false,
        },
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

    rows.push(OverviewRow {
        target: None,
        in_context: true,
        item: DiagnosticItem {
            kind_label: "Compaction".into(),
            title: "Compaction Boundary".into(),
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
        },
    });

    rows
}

#[cfg(test)]
mod tests;
