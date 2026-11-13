//! 项目级 /plan-list 管理面板：只渲染 worker 发布的快照，磁盘/登记 I/O 全部留在 worker。
//! 键盘与绘制路径不做任何 IO；操作只携带 ID，worker 按 ID 重查登记并核对当前路径与精确正文。
use super::transcript::wrap_block;
use super::transcript_style::ACCENT;
use super::transcript_style::ADDED;
use super::transcript_style::CODE;
use super::transcript_style::DIM;
use super::transcript_style::LINK;
use super::transcript_style::MUTED;
use super::transcript_style::REMOVED;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pp_core::plan::PlanLocation;
use pp_core::text::display_text;
use pp_runtime::agent::plan_management::PlanListEntry;
use pp_runtime::agent::plan_management::PlanListing;
use pp_runtime::agent::plan_management::PlanOpenIntent;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::time::SystemTime;
use unicode_segmentation::UnicodeSegmentation;

mod input;
mod render;

use self::render::{category_color, relative_path};

pub(super) const HINT: &str = "↑/↓ select · ←/→ or Tab category · Enter open · / search · F5 refresh · n new · c continue · e actions · r rename · d delete · Esc close";
/// Done 只读详情显示返回列表的提示。
pub(super) const DETAIL_HINT: &str = "Read-only completed plan · ↑/↓ scroll · Esc back to list";
const SEARCH_HINT: &str = "search: Enter apply · Esc exit · Ctrl+U clear";
const RENAME_HINT: &str = "rename: Enter submit · Esc cancel";
const CONFIRM_HINT: &str = "Tab/arrows select · Enter confirm · Esc cancel";
const DELETE_NOTICE: &str = "Delete the plan file only; code and sessions are not rolled back.";
/// 空列表仍提供新建入口；其余空态说明当前分类/过滤。
const EMPTY_NEW_PLAN: &str = "No saved plans. Press n to start a new plan.";
const ROW_BG: Color = Color::Rgb(40, 42, 54);
const ALTERNATE_ROW_BG: Color = Color::Rgb(49, 51, 63);
const SELECTED_BG: Color = Color::Rgb(98, 164, 245);
const SELECTED_FG: Color = Color::Rgb(35, 38, 49);
const AGE_FG: Color = Color::Rgb(158, 161, 168);
const TITLE_COLORS: [Color; 8] = [
    Color::Rgb(137, 180, 250),
    Color::Rgb(137, 220, 235),
    Color::Rgb(166, 227, 161),
    Color::Rgb(249, 226, 175),
    Color::Rgb(245, 194, 231),
    Color::Rgb(180, 190, 254),
    Color::Rgb(242, 143, 173),
    Color::Rgb(250, 179, 135),
];

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
    fn color(self) -> Color {
        match self {
            Tab::All => MUTED,
            Tab::Drafts => category_color(PlanLocation::Drafts),
            Tab::Plans => category_color(PlanLocation::Plans),
            Tab::Done => category_color(PlanLocation::Done),
        }
    }

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
    pub(super) fn new() -> Self {
        Self {
            phase: Phase::Loading,
            listing: PlanListing::default(),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
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
        let mut panel = Panel::new();
        panel.set_listing(listing(entries));
        panel
    }

    fn selected_file(panel: &Panel) -> String {
        panel.selected_entry().expect("selection").file_name.clone()
    }

    fn draw(panel: &mut Panel, width: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        terminal
    }

    fn row_text(buffer: &Buffer, y: u16) -> String {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    }

    #[test]
    fn category_colors_match_tabs_and_badges_and_keep_selection_visible() {
        let mut panel = ready_panel(vec![
            entry("a", "d.md", PlanLocation::Drafts, "Draft", 0, 1),
            entry("b", "p.md", PlanLocation::Plans, "Ready", 0, 2),
            entry("c", "f.md", PlanLocation::Done, "Done", 2, 2),
        ]);
        let terminal = draw(&mut panel, 100);
        let buffer = terminal.backend().buffer();
        for (row, label, color) in [
            (3, "[Draft]", ACCENT),
            (4, "[Plan]", LINK),
            (5, "[Done]", ADDED),
        ] {
            assert!(row_text(buffer, row).contains(label));
            let cell = (0..100)
                .map(|x| &buffer[(x, row)])
                .find(|cell| cell.symbol() == "[")
                .expect("分类标记应可见");
            assert_eq!(cell.fg, if row == 3 { SELECTED_FG } else { color });
        }
        for selected in 0..4 {
            let terminal = draw(&mut panel, 100);
            let buffer = terminal.backend().buffer();
            for (index, (x, color)) in [(1, MUTED), (7, ACCENT), (16, LINK), (24, ADDED)]
                .into_iter()
                .enumerate()
            {
                let cell = &buffer[(x, 1)];
                assert_eq!(
                    cell.fg,
                    if index == 0 && selected == 0 {
                        CODE
                    } else {
                        color
                    }
                );
                assert_eq!(
                    cell.modifier.contains(Modifier::BOLD | Modifier::REVERSED),
                    index == selected,
                );
            }
            panel.key(key(KeyCode::Right));
        }
    }

    #[test]
    fn compact_rows_align_titles_fill_background_and_only_label_all_tab() {
        let now = SystemTime::now();
        let mut entries = vec![
            entry("a", "draft-file.md", PlanLocation::Drafts, "修复配置", 0, 2),
            entry("b", "plan-file.md", PlanLocation::Plans, "Ready", 1, 2),
            entry("c", "done-file.md", PlanLocation::Done, "Finished", 2, 2),
        ];
        for (entry, age) in entries.iter_mut().zip([1380, 3600, 86400]) {
            entry.modified = now - std::time::Duration::from_secs(age);
        }
        let mut panel = ready_panel(entries);
        let terminal = draw(&mut panel, 70);
        let buffer = terminal.backend().buffer();
        for (row, age, title, bg) in [
            (3, "23m ago", "修", SELECTED_BG),
            (4, "1h ago", "R", ALTERNATE_ROW_BG),
            (5, "1d ago", "F", ROW_BG),
        ] {
            let text = row_text(buffer, row);
            assert!(text.contains(age), "{text}");
            assert!(!text.contains(".md") && !text.contains("/2"), "{text}");
            assert_eq!(buffer[(14, row)].symbol(), title);
            for x in 0..70 {
                // 宽字符的后续单元由 Ratatui 重置，不会作为独立字符输出。
                if x > 0 && Span::raw(buffer[(x - 1, row)].symbol()).width() > 1 {
                    continue;
                }
                assert_eq!(buffer[(x, row)].bg, bg);
            }
        }
        panel.key(key(KeyCode::Down));
        let terminal = draw(&mut panel, 70);
        assert_eq!(terminal.backend().buffer()[(69, 3)].bg, ROW_BG);
        assert_eq!(terminal.backend().buffer()[(69, 4)].bg, SELECTED_BG);
        for title in ["修复配置", "Ready", "Finished"] {
            panel.key(key(KeyCode::Right));
            let terminal = draw(&mut panel, 70);
            let text = row_text(terminal.backend().buffer(), 3);
            assert!(text.replace(' ', "").contains(title), "{text}");
            assert!(!text.contains('['), "{text}");
        }
    }

    #[test]
    fn long_unicode_title_keeps_all_tab_badge_on_one_row() {
        let mut panel = ready_panel(vec![entry(
            "a",
            "long.md",
            PlanLocation::Drafts,
            "修复👩‍💻配置e\u{301}以及后续功能",
            0,
            1,
        )]);
        let terminal = draw(&mut panel, 34);
        let buffer = terminal.backend().buffer();
        let text = row_text(buffer, 3);
        assert!(
            text.replace(' ', "").contains("修复👩‍💻配置e\u{301}…[Draft]"),
            "{text}"
        );
        assert!(row_text(buffer, 4).trim().is_empty());
    }

    #[test]
    fn search_row_is_always_visible_and_rename_takes_priority() {
        let mut panel = ready_panel(vec![entry(
            "a",
            "fix.md",
            PlanLocation::Drafts,
            "修复配置",
            0,
            1,
        )]);
        let terminal = draw(&mut panel, 70);
        let buffer = terminal.backend().buffer();
        assert!(row_text(buffer, 2).starts_with("Search: Press / to search"));
        assert_eq!(buffer[(8, 2)].fg, DIM);
        assert!(!panel.searching);

        panel.key(key(KeyCode::Char('/')));
        panel.paste("修复");
        let terminal = draw(&mut panel, 70);
        assert_eq!(terminal.backend().cursor_position(), (12, 2).into());
        assert_eq!(terminal.backend().buffer()[(8, 2)].symbol(), "修");
        assert_eq!(terminal.backend().buffer()[(10, 2)].symbol(), "复");
        assert_eq!(selected_file(&panel), "fix.md");

        panel.key(key(KeyCode::Enter));
        let terminal = draw(&mut panel, 70);
        assert!(row_text(terminal.backend().buffer(), 2).starts_with("Search: 修"));
        assert_eq!(panel.query, "修复");
        panel.key(key(KeyCode::Char('r')));
        let terminal = draw(&mut panel, 70);
        assert!(row_text(terminal.backend().buffer(), 2).starts_with("rename> fix.md"));
        panel.key(key(KeyCode::Esc));
        panel.key(key(KeyCode::Char('/')));
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        panel.key(key(KeyCode::Esc));
        let terminal = draw(&mut panel, 70);
        assert!(row_text(terminal.backend().buffer(), 2).starts_with("Search: Press / to search"));
    }

    #[test]
    fn search_tail_and_cursor_fit_narrow_offset_areas_without_splitting_graphemes() {
        let mut panel = ready_panel(Vec::new());
        panel.key(key(KeyCode::Char('/')));
        for query in [
            "abcdefghijk",
            "前缀修复配置",
            "prefix-e\u{301}",
            "prefix-👩‍💻",
        ] {
            panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            panel.paste(query);
            for width in [1, 8, 9, 10, 12, 16, 50] {
                let mut terminal = Terminal::new(TestBackend::new(width + 4, 12)).unwrap();
                let area = Rect::new(2, 1, width, 8);
                terminal.draw(|frame| panel.draw(frame, area)).unwrap();
                let cursor = terminal.backend().cursor_position();
                assert_eq!(cursor.y, 3);
                assert!(cursor.x >= area.x && cursor.x < area.right());
                assert_eq!(
                    terminal.backend().buffer()[(cursor.x, cursor.y)].symbol(),
                    " "
                );
                assert_eq!(panel.query, query);
            }
        }
        panel.query = "前缀修复配置".into();
        let terminal = draw(&mut panel, 13);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(8, 2)].symbol(), "配");
        assert_eq!(buffer[(10, 2)].symbol(), "置");
        assert_eq!(terminal.backend().cursor_position(), (12, 2).into());
        panel.query = "prefix-e\u{301}".into();
        let terminal = draw(&mut panel, 10);
        assert_eq!(terminal.backend().buffer()[(8, 2)].symbol(), "e\u{301}");
        assert_eq!(terminal.backend().cursor_position(), (9, 2).into());
        panel.query = "prefix-👩‍💻".into();
        let terminal = draw(&mut panel, 11);
        assert_eq!(terminal.backend().buffer()[(8, 2)].symbol(), "👩‍💻");
        assert_eq!(terminal.backend().cursor_position(), (10, 2).into());
    }

    #[test]
    fn search_combines_with_category_and_shows_empty_results() {
        let mut panel = ready_panel(vec![
            entry("a", "draft.md", PlanLocation::Drafts, "修复配置", 0, 1),
            entry("b", "ready.md", PlanLocation::Plans, "修复配置", 0, 1),
        ]);
        panel.key(key(KeyCode::Right));
        panel.key(key(KeyCode::Char('/')));
        panel.paste("修复");
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "draft.md");
        panel.key(key(KeyCode::Enter));
        panel.key(key(KeyCode::Right));
        assert_eq!(panel.query, "修复");
        assert_eq!(selected_file(&panel), "ready.md");
        panel.key(key(KeyCode::Right));
        let terminal = draw(&mut panel, 70);
        assert!(row_text(terminal.backend().buffer(), 3).starts_with("No matching plans."));
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
        let mut loading = Panel::new();
        assert!(matches!(loading.key(key(KeyCode::Char('n'))), Action::None));
        assert!(matches!(loading.key(key(KeyCode::Esc)), Action::Close));
    }

    #[test]
    fn delete_confirmation_is_opaque_over_rows_behind_it() {
        // 100×12 的屏幕里列表行占 3..=10；确认框宽 min(96, 72) = 72、高 7、
        // 居中于 (14, 2)，正好压在行 3..=8 上，框内区域为 x 15..85、y 3..=7。
        // 标题用长串 Z 铺满 78 列，背后行只要有一格残留就会被抓到。
        let title = "Z".repeat(90);
        let mut panel = ready_panel(
            (0..5)
                .map(|index| {
                    entry(
                        &format!("id{index}"),
                        &format!("plan-{index}.md"),
                        PlanLocation::Drafts,
                        &title,
                        0,
                        1,
                    )
                })
                .collect(),
        );
        panel.key(key(KeyCode::Char('d')));
        assert!(panel.confirm.is_some());
        let terminal = draw(&mut panel, 100);
        let buffer = terminal.backend().buffer();
        let inside = |row: u16| -> String { (15..85).map(|x| buffer[(x, row)].symbol()).collect() };
        // 框内空白行不得留下列表行的时间、标题或分类标记。
        for row in 5..=6 {
            assert_eq!(inside(row), " ".repeat(70), "row {row}: {}", inside(row));
        }
        // 正文行与按钮行两侧也不得透出背后标题。
        for row in 3..=7 {
            assert!(!inside(row).contains('Z'), "row {row}: {}", inside(row));
        }
        // 清理范围只限对话框：同一行框外照常显示列表标题与分类标记。
        let behind: String = (86..100).map(|x| buffer[(x, 3)].symbol()).collect();
        assert!(behind.contains('Z'), "{behind}");
        // 对话框自身内容照常渲染。
        assert!(row_text(buffer, 3).contains("Path: .pp/drafts/plan-0.md"));
        assert!(row_text(buffer, 4).contains("Delete the plan file only"));
        let buttons = row_text(buffer, 7);
        assert!(
            buttons.contains(" Cancel ") && buttons.contains(" Delete "),
            "{buttons}"
        );
    }
}
