//! Project-level /plan-list management panel: renders only worker-published snapshots, with all disk/registry I/O on the worker.
//! Key handling and drawing paths do no I/O; actions carry only IDs, and the worker re-reads the registry by ID and checks the current path and exact body.
use super::transcript::wrap_block;
use super::transcript_style::{accent, added, code, dim, link, muted, removed};
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
/// Read-only Done detail shows a hint for returning to the list.
pub(super) const DETAIL_HINT: &str = "Read-only completed plan · ↑/↓ scroll · Esc back to list";
const SEARCH_HINT: &str = "search: Enter apply · Esc exit · Ctrl+U clear";
const RENAME_HINT: &str = "rename: Enter submit · Esc cancel";
const CONFIRM_HINT: &str = "Tab/arrows select · Enter confirm · Esc cancel";
const DELETE_NOTICE: &str = "Delete the plan file only; code and sessions are not rolled back.";
/// An empty list still offers the create entry; other empty states describe the current category/filter.
const EMPTY_NEW_PLAN: &str = "No saved plans. Press n to start a new plan.";
fn row_bg() -> Color {
    super::themes::palette().panel_bg
}

fn alternate_row_bg() -> Color {
    super::themes::palette().panel_alt_bg
}

fn selected_bg() -> Color {
    super::themes::palette().panel_selected_bg
}

fn selected_fg() -> Color {
    super::themes::palette().panel_selected_fg
}

fn age_fg() -> Color {
    super::themes::palette().welcome_muted
}

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
    /// Enter: read the selection; Drafts/Plans open the action menu, Done is read-only.
    Preview(String),
    /// c/e: read the selection first, then open a new session carrying only the plan, per intent.
    Open(String, PlanOpenIntent),
    /// n: close the list and enter Plan standby; an unsaved draft is rejected by the App's existing loss-prevention gate.
    NewPlan,
    /// r: read the selection first, then submit the rename (the new file name is already available).
    Rename(String, String),
    /// After d is confirmed: read the selection first, then delete.
    Delete(String),
    /// Visible in-panel notice: why an action is disabled, and so on; the list stays open.
    Message(String),
}

/// List action to run after reading the selection; consumed by the App after ListedPlanRead.
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
            Tab::All => muted(),
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

    /// New list returned by the worker on success: keep the category/search and, where possible, the same ID selected;
    /// if the ID disappeared after a delete, fall back to the neighbor at the same position.
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

    /// On failure keep the old list and input and show only a visible error.
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
    fn category_colors_compact_rows_and_long_unicode_badge() {
        let mut p1 = ready_panel(vec![
            entry("a", "d.md", PlanLocation::Drafts, "Draft", 0, 1),
            entry("b", "p.md", PlanLocation::Plans, "Ready", 0, 2),
            entry("c", "f.md", PlanLocation::Done, "Done", 2, 2),
        ]);
        let t1 = draw(&mut p1, 100);
        let b1 = t1.backend().buffer();
        for (row, label, color) in [
            (3, "[Draft]", accent()),
            (4, "[Plan]", link()),
            (5, "[Done]", added()),
        ] {
            assert!(row_text(b1, row).contains(label));
            let cell = (0..100)
                .map(|x| &b1[(x, row)])
                .find(|c| c.symbol() == "[")
                .unwrap();
            assert_eq!(cell.fg, if row == 3 { selected_fg() } else { color });
        }
        for selected in 0..4 {
            let t = draw(&mut p1, 100);
            let b = t.backend().buffer();
            for (index, (x, color)) in [(1, muted()), (7, accent()), (16, link()), (24, added())]
                .into_iter()
                .enumerate()
            {
                let cell = &b[(x, 1)];
                assert_eq!(
                    cell.fg,
                    if index == 0 && selected == 0 {
                        code()
                    } else {
                        color
                    }
                );
                assert_eq!(
                    cell.modifier.contains(Modifier::BOLD | Modifier::REVERSED),
                    index == selected
                );
            }
            p1.key(key(KeyCode::Right));
        }

        let now = SystemTime::now();
        let mut entries = vec![
            entry("a", "draft-file.md", PlanLocation::Drafts, "修复配置", 0, 2),
            entry("b", "plan-file.md", PlanLocation::Plans, "Ready", 1, 2),
            entry("c", "done-file.md", PlanLocation::Done, "Finished", 2, 2),
        ];
        for (entry, age) in entries.iter_mut().zip([1380, 3600, 86400]) {
            entry.modified = now - std::time::Duration::from_secs(age);
        }
        let mut p2 = ready_panel(entries);
        let t2 = draw(&mut p2, 70);
        let b2 = t2.backend().buffer();
        for (row, age, title, bg) in [
            (3, "23m ago", "修", selected_bg()),
            (4, "1h ago", "R", alternate_row_bg()),
            (5, "1d ago", "F", row_bg()),
        ] {
            let text = row_text(b2, row);
            assert!(
                text.contains(age) && !text.contains(".md") && !text.contains("/2"),
                "{text}"
            );
            assert_eq!(b2[(14, row)].symbol(), title);
            for x in 0..70 {
                if x > 0 && Span::raw(b2[(x - 1, row)].symbol()).width() > 1 {
                    continue;
                }
                assert_eq!(b2[(x, row)].bg, bg);
            }
        }
        p2.key(key(KeyCode::Down));
        let t2b = draw(&mut p2, 70);
        assert_eq!(t2b.backend().buffer()[(69, 3)].bg, row_bg());
        assert_eq!(t2b.backend().buffer()[(69, 4)].bg, selected_bg());
        for title in ["修复配置", "Ready", "Finished"] {
            p2.key(key(KeyCode::Right));
            let t = draw(&mut p2, 70);
            let text = row_text(t.backend().buffer(), 3);
            assert!(
                text.replace(' ', "").contains(title) && !text.contains('['),
                "{text}"
            );
        }

        let mut p3 = ready_panel(vec![entry(
            "a",
            "long.md",
            PlanLocation::Drafts,
            "修复👩‍💻配置e\u{301}以及后续功能",
            0,
            1,
        )]);
        let t3 = draw(&mut p3, 34);
        let b3 = t3.backend().buffer();
        assert!(
            row_text(b3, 3)
                .replace(' ', "")
                .contains("修复👩‍💻配置e\u{301}…[Draft]")
        );
        assert!(row_text(b3, 4).trim().is_empty());
    }

    #[test]
    fn search_bar_narrow_cursor_and_category_filtering() {
        let mut p1 = ready_panel(vec![entry(
            "a",
            "fix.md",
            PlanLocation::Drafts,
            "修复配置",
            0,
            1,
        )]);
        let t1 = draw(&mut p1, 70);
        assert!(row_text(t1.backend().buffer(), 2).starts_with("Search: Press / to search"));
        assert_eq!(t1.backend().buffer()[(8, 2)].fg, dim());
        p1.key(key(KeyCode::Char('/')));
        p1.paste("修复");
        let t1b = draw(&mut p1, 70);
        assert_eq!(t1b.backend().cursor_position(), (12, 2).into());
        assert_eq!(selected_file(&p1), "fix.md");
        p1.key(key(KeyCode::Enter));
        assert!(row_text(draw(&mut p1, 70).backend().buffer(), 2).starts_with("Search: 修"));
        p1.key(key(KeyCode::Char('r')));
        assert!(row_text(draw(&mut p1, 70).backend().buffer(), 2).starts_with("rename> fix.md"));
        p1.key(key(KeyCode::Esc));
        p1.key(key(KeyCode::Char('/')));
        p1.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        p1.key(key(KeyCode::Esc));
        assert!(
            row_text(draw(&mut p1, 70).backend().buffer(), 2)
                .starts_with("Search: Press / to search")
        );

        let mut p2 = ready_panel(Vec::new());
        p2.key(key(KeyCode::Char('/')));
        for query in [
            "abcdefghijk",
            "前缀修复配置",
            "prefix-e\u{301}",
            "prefix-👩‍💻",
        ] {
            p2.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
            p2.paste(query);
            for width in [1, 8, 9, 10, 12, 16, 50] {
                let mut terminal = Terminal::new(TestBackend::new(width + 4, 12)).unwrap();
                let area = Rect::new(2, 1, width, 8);
                terminal.draw(|frame| p2.draw(frame, area)).unwrap();
                let cursor = terminal.backend().cursor_position();
                assert!(cursor.y == 3 && cursor.x >= area.x && cursor.x < area.right());
                assert_eq!(
                    terminal.backend().buffer()[(cursor.x, cursor.y)].symbol(),
                    " "
                );
            }
        }
        p2.query = "prefix-👩‍💻".into();
        let t2 = draw(&mut p2, 11);
        assert_eq!(t2.backend().buffer()[(8, 2)].symbol(), "👩‍💻");
        assert_eq!(t2.backend().cursor_position(), (10, 2).into());

        let mut p3 = ready_panel(vec![
            entry("a", "draft.md", PlanLocation::Drafts, "修复配置", 0, 1),
            entry("b", "ready.md", PlanLocation::Plans, "修复配置", 0, 1),
        ]);
        p3.key(key(KeyCode::Right));
        p3.key(key(KeyCode::Char('/')));
        p3.paste("修复");
        assert_eq!(selected_file(&p3), "draft.md");
        p3.key(key(KeyCode::Enter));
        p3.key(key(KeyCode::Right));
        assert_eq!(selected_file(&p3), "ready.md");
        p3.key(key(KeyCode::Right));
        assert!(
            row_text(draw(&mut p3, 70).backend().buffer(), 3).starts_with("No matching plans.")
        );
    }

    #[test]
    fn search_multi_word_mode_guards_and_tab_cycling() {
        let mut panel = ready_panel(vec![
            entry("a", "fix-config.md", PlanLocation::Drafts, "修复配置", 0, 2),
            entry("b", "other.md", PlanLocation::Plans, "Login flow", 1, 3),
            entry("c", "done-plan.md", PlanLocation::Done, "Archive", 2, 2),
        ]);
        panel.key(key(KeyCode::Char('/')));
        for character in "LOGIN Flow".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        // Multi-word AND must match entirely within one entry (case-insensitive): only b has both words in its title.
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "other.md");
        // AND does not hold across entries: fix and login belong to different entries, so the result is empty.
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "fix login".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert!(panel.filtered.is_empty());
        // File-name match: only a contains fix.
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "FIX".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "fix-config.md");
        // Relative path match: the directory name and file name can both hit the same entry as separate words.
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "drafts fix".chars() {
            panel.key(key(KeyCode::Char(character)));
        }
        assert_eq!(panel.filtered.len(), 1);
        assert_eq!(selected_file(&panel), "fix-config.md");
        // Esc leaves search and keeps the query; re-entering search and pressing Ctrl+U clears it and restores everything.
        panel.key(key(KeyCode::Esc));
        assert!(!panel.searching);
        assert_eq!(panel.filtered.len(), 1);
        panel.key(key(KeyCode::Char('/')));
        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(panel.query.is_empty() && panel.filtered.len() == 3);

        let mut p2 = ready_panel(vec![entry(
            "a",
            "plan.md",
            PlanLocation::Drafts,
            "Title",
            0,
            1,
        )]);
        p2.key(key(KeyCode::Char('/')));
        for ch in "rdnce".chars() {
            assert!(matches!(p2.key(key(KeyCode::Char(ch))), Action::None));
        }
        assert!(p2.query == "rdnce" && p2.rename.is_none() && p2.confirm.is_none());
        p2.key(key(KeyCode::Enter));
        assert!(!p2.searching && p2.filtered.is_empty());
        assert!(matches!(p2.key(key(KeyCode::Enter)), Action::None));
        assert!(matches!(p2.key(key(KeyCode::Char('c'))), Action::None));

        let mut p3 = ready_panel(vec![
            entry("a", "d.md", PlanLocation::Drafts, "Draft", 0, 1),
            entry("b", "p.md", PlanLocation::Plans, "Ready", 0, 2),
            entry("c", "f.md", PlanLocation::Done, "Done", 2, 2),
        ]);
        p3.key(key(KeyCode::Right));
        assert_eq!(selected_file(&p3), "d.md");
        p3.key(key(KeyCode::Tab));
        assert_eq!(selected_file(&p3), "p.md");
        p3.key(key(KeyCode::Tab));
        assert_eq!(selected_file(&p3), "f.md");
        p3.key(key(KeyCode::Left));
        assert_eq!(selected_file(&p3), "p.md");
        p3.key(key(KeyCode::BackTab));
        p3.key(key(KeyCode::BackTab));
        assert_eq!(p3.filtered.len(), 3);
    }

    #[test]
    fn refresh_selection_rename_and_confirm_delete() {
        let mut panel = ready_panel(vec![
            entry("a", "first.md", PlanLocation::Drafts, "First", 0, 1),
            entry("b", "second.md", PlanLocation::Drafts, "Second", 0, 1),
            entry("c", "third.md", PlanLocation::Drafts, "Third", 0, 1),
        ]);
        panel.key(key(KeyCode::Down));
        assert_eq!(selected_file(&panel), "second.md");
        // After a rename the same ID stays selected.
        panel.set_listing(listing(vec![
            entry("a", "first.md", PlanLocation::Drafts, "First", 0, 1),
            entry("b", "renamed.md", PlanLocation::Drafts, "Second", 0, 1),
            entry("c", "third.md", PlanLocation::Drafts, "Third", 0, 1),
        ]));
        assert_eq!(selected_file(&panel), "renamed.md");
        // After a delete the ID is gone and the neighbor at the same position takes over.
        panel.set_listing(listing(vec![
            entry("a", "first.md", PlanLocation::Drafts, "First", 0, 1),
            entry("c", "third.md", PlanLocation::Drafts, "Third", 0, 1),
        ]));
        assert_eq!(selected_file(&panel), "third.md");
        // Category/search are preserved across a refresh.
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

        let mut p_del = ready_panel(vec![entry(
            "a",
            "plan.md",
            PlanLocation::Plans,
            "Title",
            0,
            1,
        )]);
        p_del.key(key(KeyCode::Char('d')));
        assert!(matches!(p_del.key(key(KeyCode::Enter)), Action::None) && p_del.confirm.is_none());
        p_del.key(key(KeyCode::Char('d')));
        p_del.key(key(KeyCode::Esc));
        assert!(p_del.confirm.is_none());
        p_del.key(key(KeyCode::Char('d')));
        p_del.key(key(KeyCode::Tab));
        assert!(matches!(p_del.key(key(KeyCode::Enter)), Action::Delete(id) if id == "a"));

        let mut p_ren = ready_panel(vec![entry(
            "a",
            "old-name.md",
            PlanLocation::Drafts,
            "Title",
            0,
            1,
        )]);
        p_ren.key(key(KeyCode::Char('r')));
        assert_eq!(p_ren.rename.as_deref(), Some("old-name.md"));
        assert!(matches!(p_ren.key(key(KeyCode::Esc)), Action::None) && p_ren.rename.is_none());
        p_ren.key(key(KeyCode::Char('r')));
        for ch in "v2".chars() {
            p_ren.key(key(KeyCode::Char(ch)));
        }
        assert!(
            matches!(p_ren.key(key(KeyCode::Enter)), Action::Rename(id, name) if id == "a" && name == "old-name.mdv2")
        );
        p_ren.key(key(KeyCode::Char('r')));
        assert!(matches!(p_ren.key(key(KeyCode::Enter)), Action::None) && p_ren.rename.is_none());
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
        // Done: preview works, c/e are rejected, r/d still work.
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
        // Zero tasks: the action menu still opens, and execution is rejected by the approval gate.
        panel.key(key(KeyCode::Down));
        assert!(matches!(
            panel.key(key(KeyCode::Char('e'))),
            Action::Open(_, PlanOpenIntent::Execute)
        ));
        // Error entry: every action only produces a visible notice.
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
        // While loading, no keys are accepted except Esc.
        let mut loading = Panel::new();
        assert!(matches!(loading.key(key(KeyCode::Char('n'))), Action::None));
        assert!(matches!(loading.key(key(KeyCode::Esc)), Action::Close));
    }

    #[test]
    fn delete_confirmation_diagnostics_and_rename_cursor() {
        let title = "Z".repeat(90);
        let mut panel = ready_panel(
            (0..5)
                .map(|i| {
                    entry(
                        &format!("id{i}"),
                        &format!("plan-{i}.md"),
                        PlanLocation::Drafts,
                        &title,
                        0,
                        1,
                    )
                })
                .collect(),
        );
        panel.key(key(KeyCode::Char('d')));
        let terminal = draw(&mut panel, 100);
        let buffer = terminal.backend().buffer();
        let inside = |row: u16| -> String { (15..85).map(|x| buffer[(x, row)].symbol()).collect() };
        for row in 5..=6 {
            assert_eq!(inside(row), " ".repeat(70), "row {row}");
        }
        for row in 3..=7 {
            assert!(!inside(row).contains('Z'), "row {row}");
        }
        assert!(
            (86..100)
                .map(|x| buffer[(x, 3)].symbol())
                .collect::<String>()
                .contains('Z')
        );
        assert!(row_text(buffer, 3).contains("Path: .pp/drafts/plan-0.md"));
        let buttons = row_text(buffer, 7);
        assert!(
            buttons.contains(" Cancel ") && buttons.contains(" Delete "),
            "{buttons}"
        );

        let mut list = listing(vec![
            entry("a", "alpha.md", PlanLocation::Drafts, "Alpha Plan", 0, 1),
            entry("b", "beta.md", PlanLocation::Drafts, "Beta Plan", 0, 1),
        ]);
        list.diagnostics = vec!["DIAG_ONE".into(), "DIAG_TWO".into()];
        let mut p_diag = Panel::new();
        p_diag.set_listing(list);
        let t_diag = draw(&mut p_diag, 100);
        let rows: Vec<String> = (0..12)
            .map(|y| row_text(t_diag.backend().buffer(), y))
            .collect();
        let alpha_row = rows.iter().position(|r| r.contains("Alpha Plan")).unwrap();
        let beta_row = rows.iter().position(|r| r.contains("Beta Plan")).unwrap();
        let diag1_row = rows.iter().position(|r| r.contains("DIAG_ONE")).unwrap();
        let diag2_row = rows.iter().position(|r| r.contains("DIAG_TWO")).unwrap();
        assert!(alpha_row < beta_row && beta_row < diag1_row && diag2_row == diag1_row + 1);

        let mut p_ren = ready_panel(vec![entry(
            "a",
            "alpha.md",
            PlanLocation::Drafts,
            "Alpha",
            0,
            1,
        )]);
        p_ren.rename = Some("a".repeat(60));
        let mut t_ren = Terminal::new(TestBackend::new(20, 12)).unwrap();
        t_ren.draw(|frame| p_ren.draw(frame, frame.area())).unwrap();
        let cursor = t_ren.get_cursor_position().unwrap();
        assert!(cursor.x < 20 && cursor.y < 12, "{cursor:?}");
    }
}
