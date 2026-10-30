use crate::agent::persistence::{SessionListing, SessionSummary};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};
use std::{path::PathBuf, time::SystemTime};

const ACCENT: Color = Color::Rgb(229, 177, 64);
const BORDER: Color = Color::Rgb(57, 151, 192);
const MUTED: Color = Color::Rgb(99, 105, 119);
const SUCCESS: Color = Color::Rgb(133, 192, 117);

pub(super) enum Phase {
    Loading,
    Ready,
    Restoring,
    ConfirmDelete(usize),
    Deleting,
}

pub(super) enum Action {
    None,
    Cancel,
    Resume(usize),
    Delete(usize),
}

/// 全屏会话选择器。目录扫描与检索由独立 history worker 完成；面板只安装
/// 与当前代次和最新请求对应的结果，绘制和按键从不读取数据库。
pub(super) struct Panel {
    phase: Phase,
    sessions: Vec<SessionSummary>,
    current_id: Option<String>,
    cwd: PathBuf,
    all_projects: bool,
    query: String,
    selected: usize,
    offset: usize,
    page_size: usize,
    generation: u64,
    request_id: u64,
    // 最新请求尚未回复：保留旧行仅在内存，UI 不展示也不允许对旧行操作。
    pending: bool,
    // 查询/范围变化后安装结果时重置选中；删除后的同步保留选中并夹取。
    reset_selection: bool,
    dirty: bool,
    error: Option<String>,
}

impl Panel {
    pub(super) fn new(generation: u64, current_id: Option<String>, cwd: PathBuf) -> Self {
        Self {
            phase: Phase::Loading,
            sessions: Vec::new(),
            current_id,
            cwd,
            all_projects: false,
            query: String::new(),
            selected: 0,
            offset: 0,
            page_size: 1,
            generation,
            request_id: 0,
            pending: false,
            reset_selection: true,
            dirty: false,
            error: None,
        }
    }

    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    fn scope(&self) -> Option<PathBuf> {
        (!self.all_projects).then(|| self.cwd.clone())
    }

    fn issue(&mut self) -> (u64, Option<PathBuf>, String) {
        self.request_id = self.request_id.wrapping_add(1);
        self.pending = true;
        self.error = None;
        (self.request_id, self.scope(), self.query.clone())
    }

    /// 打开面板和删除成功后的同步：重新扫描会话目录并按空/当前查询检索。
    pub(super) fn sync_request(&mut self) -> (u64, Option<PathBuf>, String) {
        self.dirty = false;
        self.issue()
    }

    /// 删除成功后的刷新保留当前选中位置；旧行在结果返回前不可操作。
    pub(super) fn refresh_request(&mut self) -> (u64, Option<PathBuf>, String) {
        self.reset_selection = false;
        self.dirty = false;
        self.issue()
    }

    /// 查询或范围变化后待发送的 Search 请求；没有变化时返回 None。
    pub(super) fn take_search(&mut self) -> Option<(u64, Option<PathBuf>, String)> {
        if !self.dirty {
            return None;
        }
        self.reset_selection = true;
        self.dirty = false;
        Some(self.issue())
    }

    /// 只安装与最新请求匹配的结果；迟到的旧请求和旧面板代次由调用方丢弃。
    pub(super) fn install(&mut self, request_id: u64, listing: SessionListing) -> bool {
        if request_id != self.request_id {
            return false;
        }
        self.sessions = listing.sessions;
        self.pending = false;
        self.error = None;
        if matches!(self.phase, Phase::Loading | Phase::Ready) {
            self.phase = Phase::Ready;
        }
        if self.reset_selection {
            self.selected = 0;
            self.offset = 0;
        } else {
            self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
            self.offset = self.offset.min(self.sessions.len().saturating_sub(1));
        }
        true
    }

    /// 检索失败：清空旧行避免对过期结果执行恢复/删除，保留查询与 Esc。
    pub(super) fn fail(&mut self, request_id: u64, message: String) -> bool {
        if request_id != self.request_id {
            return false;
        }
        self.sessions.clear();
        self.selected = 0;
        self.offset = 0;
        self.pending = false;
        self.error = Some(message);
        if matches!(self.phase, Phase::Loading) {
            self.phase = Phase::Ready;
        }
        true
    }

    pub(super) fn restore_started(&mut self) {
        self.phase = Phase::Restoring;
    }
    pub(super) fn restore_failed(&mut self) {
        self.phase = Phase::Ready;
    }

    /// 删除结束：成功时由调用方紧接一次同步；失败保留当前行与选中。
    pub(super) fn delete_finished(&mut self, succeeded: bool) {
        self.phase = Phase::Ready;
        if succeeded {
            self.pending = true;
            self.error = None;
        }
    }

    pub(super) fn id(&self, index: usize) -> Option<&str> {
        self.sessions.get(index).map(|session| session.id.as_str())
    }

    pub(super) fn paste(&mut self, text: &str) {
        if matches!(self.phase, Phase::Loading | Phase::Ready) {
            self.query
                .extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
            self.dirty = true;
        }
    }

    fn actionable(&self) -> bool {
        !self.pending && self.error.is_none()
    }

    fn confirm_delete(&mut self) {
        if !self.actionable() {
            return;
        }
        if let Some(session) = self.sessions.get(self.selected)
            && Some(session.id.as_str()) != self.current_id.as_deref()
        {
            self.phase = Phase::ConfirmDelete(self.selected);
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        match self.phase {
            Phase::Restoring | Phase::Deleting => return Action::None,
            Phase::ConfirmDelete(index) => {
                return match key.code {
                    KeyCode::Enter | KeyCode::Char('y' | 'Y') => {
                        self.phase = Phase::Deleting;
                        Action::Delete(index)
                    }
                    KeyCode::Esc | KeyCode::Char('n' | 'N') => {
                        self.phase = Phase::Ready;
                        Action::None
                    }
                    _ => Action::None,
                };
            }
            Phase::Loading | Phase::Ready => {}
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Enter => {
                if self.actionable() && self.sessions.get(self.selected).is_some() {
                    return Action::Resume(self.selected);
                }
            }
            KeyCode::Right | KeyCode::Left | KeyCode::Tab => {
                self.all_projects = !self.all_projects;
                self.dirty = true;
            }
            KeyCode::Up if self.actionable() => self.move_selection(true, 1),
            KeyCode::Down if self.actionable() => self.move_selection(false, 1),
            KeyCode::PageUp if self.actionable() => self.move_selection(true, self.page_size),
            KeyCode::PageDown if self.actionable() => self.move_selection(false, self.page_size),
            KeyCode::Home if self.actionable() => self.selected = 0,
            KeyCode::End if self.actionable() => {
                self.selected = self.sessions.len().saturating_sub(1);
            }
            KeyCode::Delete => self.confirm_delete(),
            KeyCode::Backspace if control || self.query.is_empty() => self.confirm_delete(),
            KeyCode::Backspace => {
                self.query.pop();
                self.dirty = true;
            }
            KeyCode::Char('u') if control => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.dirty = true;
                }
            }
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.query.push(c);
                self.dirty = true;
            }
            _ => {}
        }
        Action::None
    }

    fn move_selection(&mut self, up: bool, count: usize) {
        self.selected = if up {
            self.selected.saturating_sub(count)
        } else {
            self.selected
                .saturating_add(count)
                .min(self.sessions.len().saturating_sub(1))
        };
    }

    pub(super) fn wheel(&mut self, up: bool) {
        if matches!(self.phase, Phase::Ready) && self.actionable() {
            self.move_selection(up, 1);
        }
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        if area.is_empty() {
            return;
        }
        let scope = if self.all_projects {
            "all projects"
        } else {
            "current folder"
        };
        frame.render_widget(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(BORDER))
                .title(Line::styled(
                    format!(" Resume session ({scope}) "),
                    Style::default().fg(ACCENT),
                )),
            area,
        );
        if area.height < 3 {
            return;
        }
        // 长查询保留尾部，光标始终留在可见输入区，按字符裁剪避免截断 UTF-8。
        let mut query = self.query.as_str();
        let query_width = usize::from(area.width.saturating_sub(3));
        while Line::from(query).width() > query_width {
            query = &query[query.chars().next().map_or(0, char::len_utf8)..];
        }
        frame.render_widget(
            Line::from(format!("> {query}")),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );
        if matches!(self.phase, Phase::Loading | Phase::Ready) && area.width > 2 {
            frame.set_cursor_position((area.x + 2 + Line::from(query).width() as u16, area.y + 2));
        }
        let footer_y = area.bottom() - 1;
        let hint = match self.phase {
            Phase::ConfirmDelete(_) => "Delete selected session?  Enter/y delete · Esc/n cancel",
            Phase::Deleting => "Deleting session…",
            Phase::Restoring => "Restoring session…",
            _ if self.pending => "Searching sessions… · Esc cancel",
            _ if self.error.is_some() => "[type to retry · Esc cancel]",
            _ if self.all_projects => {
                "[⌫/Delete delete · ↵ select · ← current folder · Esc cancel]"
            }
            _ => "[⌫/Delete delete · ↵ select · → all projects · Esc cancel]",
        };
        frame.render_widget(
            Line::styled(
                hint,
                Style::default().fg(if matches!(self.phase, Phase::ConfirmDelete(_)) {
                    ACCENT
                } else {
                    MUTED
                }),
            ),
            Rect::new(area.x, footer_y, area.width, 1),
        );
        let top = area.y.saturating_add(4);
        let height = footer_y.saturating_sub(top);
        if height == 0 {
            return;
        }
        let list_area = Rect::new(area.x, top, area.width, height);
        let message = if let Some(error) = &self.error {
            Some(error.clone())
        } else if self.pending {
            Some("Searching sessions…".to_owned())
        } else if self.sessions.is_empty() {
            let text = if !self.query.is_empty() {
                "No matching sessions."
            } else if self.all_projects {
                "No saved sessions found."
            } else {
                "No sessions in current folder. Press → for all projects."
            };
            Some(text.to_owned())
        } else {
            None
        };
        if let Some(message) = message {
            frame.render_widget(
                Paragraph::new(message).style(Style::default().fg(MUTED)),
                list_area,
            );
            return;
        }
        self.page_size = usize::from((height.saturating_add(1) / 4).max(1));
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + self.page_size {
            self.offset = self.selected + 1 - self.page_size;
        }
        self.offset = self
            .offset
            .min(self.sessions.len().saturating_sub(self.page_size));
        let now = SystemTime::now();
        for (row, session) in self
            .sessions
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.page_size)
        {
            let current = Some(session.id.as_str()) == self.current_id.as_deref();
            let title = if session.summary.is_empty() {
                "Untitled session"
            } else {
                &session.summary
            };
            let preview = if session.preview.is_empty() {
                "(no messages)"
            } else {
                &session.preview
            };
            let mut metadata = vec![Span::styled(
                format!(
                    "{}  ·  {}  ·  ",
                    format_age(session.updated_at, now),
                    format_size(session.bytes)
                ),
                Style::default().fg(MUTED),
            )];
            if current {
                metadata.push(Span::styled("current  ·  ", Style::default().fg(ACCENT)));
            }
            metadata.push(Span::styled(
                if session.done {
                    "✓ done"
                } else {
                    "■ aborted"
                },
                Style::default().fg(if session.done { SUCCESS } else { MUTED }),
            ));
            let lines = [
                Line::from(vec![
                    Span::styled(
                        if row == self.selected { "❯ " } else { "  " },
                        Style::default().fg(ACCENT),
                    ),
                    Span::raw(title),
                ]),
                Line::styled(format!("  {preview}"), Style::default().fg(MUTED)),
                Line::from(
                    std::iter::once(Span::raw("  "))
                        .chain(metadata)
                        .collect::<Vec<_>>(),
                ),
            ];
            let y = top + ((row - self.offset) * 4) as u16;
            for (line, text) in lines.into_iter().enumerate() {
                if y + (line as u16) < footer_y {
                    frame.render_widget(text, Rect::new(area.x, y + line as u16, area.width, 1));
                }
            }
        }
    }
}

fn format_age(time: SystemTime, now: SystemTime) -> String {
    let seconds = now.duration_since(time).unwrap_or_default().as_secs();
    let (value, unit) = match seconds {
        0..60 => return "just now".into(),
        60..3600 => (seconds / 60, "minute"),
        3600..86400 => (seconds / 3600, "hour"),
        86400..604800 => (seconds / 86400, "day"),
        604800..2592000 => (seconds / 604800, "week"),
        2592000..31536000 => (seconds / 2592000, "month"),
        _ => (seconds / 31536000, "year"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

fn format_size(bytes: u64) -> String {
    match bytes {
        0..1024 => format!("{bytes}B"),
        1024..1048576 => format!("{:.1}KB", bytes as f64 / 1024.0),
        1048576..1073741824 => format!("{:.1}MB", bytes as f64 / 1048576.0),
        _ => format!("{:.1}GB", bytes as f64 / 1073741824.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::Duration;

    fn summary(id: &str, cwd: &str, text: &str) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            cwd: cwd.into(),
            summary: text.into(),
            preview: format!("answer for {text}"),
            bytes: 1536,
            updated_at: SystemTime::now() - Duration::from_secs(120),
            done: true,
        }
    }

    fn listing(sessions: Vec<SessionSummary>) -> SessionListing {
        SessionListing {
            sessions,
            diagnostics: vec![],
        }
    }

    fn project_listing() -> SessionListing {
        listing(vec![
            summary("current", "/project", "当前会话"),
            summary("alpha", "/project", "修复 Rust 测试"),
        ])
    }

    fn all_listing() -> SessionListing {
        listing(vec![
            summary("current", "/project", "当前会话"),
            summary("beta", "/other", "其他目录"),
            summary("alpha", "/project", "修复 Rust 测试"),
        ])
    }

    /// 打开面板并安装服务端返回的首个结果（调用方负责检索）。
    fn panel() -> Panel {
        let mut panel = Panel::new(1, Some("current".into()), "/project".into());
        let (request_id, cwd, query) = panel.sync_request();
        assert_eq!(cwd, Some(PathBuf::from("/project")));
        assert!(query.is_empty());
        assert!(panel.install(request_id, project_listing()));
        panel
    }

    fn search(panel: &mut Panel) -> (u64, Option<PathBuf>, String) {
        panel.take_search().expect("query or scope changed")
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render(panel: &mut Panel, width: u16, height: u16) -> (String, Terminal<TestBackend>) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let text = (0..height)
            .map(|y| {
                let mut line = String::new();
                let mut x = 0;
                while x < width {
                    let symbol = terminal.backend().buffer()[(x, y)].symbol();
                    line.push_str(symbol);
                    x += Line::from(symbol).width().max(1) as u16;
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, terminal)
    }

    #[test]
    fn resume_panel_matches_three_line_layout_and_searches_scopes() {
        let mut panel = panel();
        let (screen, terminal) = render(&mut panel, 100, 30);
        let lines: Vec<_> = screen.lines().collect();
        assert!(lines[0].contains("Resume session (current folder)"));
        assert!(lines[2].starts_with("> "));
        assert!(lines[4].starts_with("❯ 当前会话"));
        assert!(lines[5].contains("answer for 当前会话"));
        assert!(lines[6].contains("2 minutes ago  ·  1.5KB  ·  current  ·  ✓ done"));
        assert!(lines[8].contains("修复 Rust 测试"));
        assert_eq!(terminal.backend().buffer()[(0, 4)].fg, ACCENT);
        assert_eq!(terminal.backend().buffer()[(2, 5)].fg, MUTED);
        assert_eq!(terminal.backend().cursor_position(), (2, 2).into());

        // 范围切换只发送新检索；等待期间旧行不可见也不能选择。
        panel.key(key(KeyCode::Right));
        let (scope_request, cwd, query) = search(&mut panel);
        assert_eq!(cwd, None);
        assert!(query.is_empty());
        let screen = render(&mut panel, 100, 30).0;
        assert!(screen.contains("Searching sessions…"));
        assert!(!screen.contains("其他目录"));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        panel.key(key(KeyCode::Delete));
        assert!(matches!(panel.phase, Phase::Ready));
        assert!(panel.install(scope_request, all_listing()));
        assert!(render(&mut panel, 100, 30).0.contains("其他目录"));

        // 服务端已过滤：粘贴换行转空格、检索词下发、结果直接安装。
        panel.paste("其他\n目录");
        let (paste_request, cwd, query) = search(&mut panel);
        assert_eq!(cwd, None);
        assert_eq!(query, "其他 目录");
        assert!(panel.install(
            paste_request,
            listing(vec![summary("beta", "/other", "其他目录")])
        ));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Resume(0)));

        panel.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let (_, cwd, _) = search(&mut panel);
        assert_eq!(cwd, None);
        panel.key(key(KeyCode::Left));
        let (_, cwd, _) = search(&mut panel);
        assert_eq!(cwd, Some(PathBuf::from("/project")));
        panel.paste("RUST 测试");
        let (typed_request, _, query) = search(&mut panel);
        assert_eq!(query, "RUST 测试");
        assert!(panel.install(
            typed_request,
            listing(vec![summary("alpha", "/project", "修复 Rust 测试")])
        ));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Resume(0)));
        panel.key(key(KeyCode::Backspace));
        panel.key(key(KeyCode::Char('错')));
        let (narrowed_request, _, query) = search(&mut panel);
        assert_eq!(query, "RUST 测错");
        assert!(panel.install(narrowed_request, listing(vec![])));
        assert!(
            render(&mut panel, 100, 30)
                .0
                .contains("No matching sessions.")
        );
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::Cancel));
    }

    #[test]
    fn stale_requests_never_replace_newer_results_or_selection() {
        let mut panel = panel();
        panel.key(key(KeyCode::Char('a')));
        let (first, _, _) = search(&mut panel);
        panel.key(key(KeyCode::Char('b')));
        let (second, _, query) = search(&mut panel);
        assert_eq!(query, "ab");
        assert!(second > first);
        // 迟到的旧请求结果被丢弃，包括失败回复。
        assert!(!panel.install(first, all_listing()));
        assert!(!panel.fail(first, "stale".into()));
        assert!(panel.error.is_none());
        assert!(panel.install(second, project_listing()));
        assert!(!panel.pending);
    }

    #[test]
    fn resume_panel_deletion_confirms_blocks_repeats_and_keeps_query() {
        let mut panel = panel();
        panel.key(key(KeyCode::Delete));
        assert!(matches!(panel.phase, Phase::Ready), "不能删除当前会话");
        panel.key(key(KeyCode::Down));
        panel.key(key(KeyCode::Delete));
        assert!(
            render(&mut panel, 100, 20)
                .0
                .contains("Delete selected session?")
        );
        panel.key(key(KeyCode::Esc));
        assert!(matches!(panel.phase, Phase::Ready));
        panel.key(key(KeyCode::Backspace));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Delete(1)));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        panel.delete_finished(false);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Resume(1)));

        // 删除成功后同步刷新：旧行在回复前不可操作，失败时清空旧行。
        panel.delete_finished(true);
        let (request_id, _, _) = panel.refresh_request();
        assert!(
            render(&mut panel, 100, 20)
                .0
                .contains("Searching sessions…")
        );
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::None));
        assert!(!panel.install(request_id - 1, project_listing()));
        assert!(!panel.fail(request_id - 1, "stale".into()));
        assert!(panel.fail(request_id, "index unavailable".into()));
        assert!(render(&mut panel, 100, 20).0.contains("index unavailable"));
        panel.paste("Rust");
        let (_, _, query) = panel.take_search().expect("paste edits query");
        assert_eq!(query, "Rust");
    }

    #[test]
    fn resume_panel_pages_resizes_and_keeps_cursor_inside_small_screens() {
        let mut panel = panel();
        let (request_id, _, _) = panel.sync_request();
        assert!(
            panel.install(
                request_id,
                listing(
                    (0..20)
                        .map(|i| summary(&format!("id-{i}"), "/project", &format!("session-{i}")))
                        .collect()
                )
            )
        );
        render(&mut panel, 100, 14);
        panel.key(key(KeyCode::PageDown));
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Resume(2)));
        panel.key(key(KeyCode::End));
        assert!(render(&mut panel, 100, 10).0.contains("❯ session-19"));
        render(&mut panel, 100, 40);
        panel.wheel(true);
        assert!(matches!(panel.key(key(KeyCode::Enter)), Action::Resume(18)));
        panel.restore_started();
        assert!(matches!(panel.key(key(KeyCode::Esc)), Action::None));
        panel.restore_failed();
        panel.paste(&"中文👩‍💻".repeat(20));
        for (width, height) in [(0, 0), (1, 1), (2, 3), (8, 8), (20, 5)] {
            let (_, terminal) = render(&mut panel, width, height);
            if width > 2 && height >= 3 {
                let cursor = terminal.backend().cursor_position();
                assert!(cursor.x < width && cursor.y < height);
            }
        }
    }

    #[test]
    fn resume_metadata_formats_time_and_size_boundaries() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(40_000_000);
        assert_eq!(format_age(now + Duration::from_secs(60), now), "just now");
        for (seconds, expected) in [
            (59, "just now"),
            (60, "1 minute ago"),
            (3600, "1 hour ago"),
            (86400, "1 day ago"),
            (604800, "1 week ago"),
            (2592000, "1 month ago"),
            (31536000, "1 year ago"),
        ] {
            assert_eq!(
                format_age(now - Duration::from_secs(seconds), now),
                expected
            );
        }
        assert_eq!(format_size(1023), "1023B");
        assert_eq!(format_size(1024), "1.0KB");
        assert_eq!(format_size(1_048_576), "1.0MB");
    }
}
