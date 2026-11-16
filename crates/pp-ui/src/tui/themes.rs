//! 主题调色板、图标模式（Nerd Font / ASCII）激活态与 `/themes` 独立分类设置面板。
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use pp_config::config::{IconMode, Theme, TuiSettings};
use ratatui::layout::Rect;

pub(in crate::tui) use palette::{Palette, palette_for};

mod palette;
mod render;

thread_local! {
    static ACTIVE: Cell<(Theme, IconMode)> = const { Cell::new((Theme::Default, IconMode::Nerd)) };
}

pub(in crate::tui) fn current() -> (Theme, IconMode) {
    ACTIVE.with(Cell::get)
}

pub(in crate::tui) fn current_theme() -> Theme {
    current().0
}

pub(in crate::tui) fn current_icons() -> IconMode {
    current().1
}

pub(in crate::tui) fn is_ascii() -> bool {
    current_icons() == IconMode::Ascii
}

pub(in crate::tui) fn set_active(theme: Theme, icons: IconMode) {
    ACTIVE.with(|cell| cell.set((theme, icons)));
}

pub(in crate::tui) fn palette() -> &'static Palette {
    palette_for(current_theme())
}

pub(super) struct SaveRequest {
    pub(super) path: PathBuf,
    pub(super) theme: Theme,
    pub(super) icons: IconMode,
}

pub(super) struct SaveReply {
    pub(super) theme: Theme,
    pub(super) icons: IconMode,
    pub(super) result: Result<()>,
}

pub(super) struct Worker {
    requests: Option<Sender<SaveRequest>>,
    pub(super) replies: Receiver<SaveReply>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(super) fn new() -> Result<Self> {
        let (requests, receiver) = mpsc::channel::<SaveRequest>();
        let (sender, replies) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("pp-themes".into())
            .spawn(move || {
                while let Ok(request) = receiver.recv() {
                    let result =
                        TuiSettings::save_appearance(&request.path, request.theme, request.icons);
                    if sender
                        .send(SaveReply {
                            theme: request.theme,
                            icons: request.icons,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("starting theme settings worker")?;
        Ok(Self {
            requests: Some(requests),
            replies,
            thread: Some(thread),
        })
    }

    pub(super) fn save(&self, path: PathBuf, theme: Theme, icons: IconMode) -> Result<()> {
        self.requests
            .as_ref()
            .context("theme settings worker stopped")?
            .send(SaveRequest { path, theme, icons })
            .map_err(|_| anyhow::anyhow!("theme settings worker stopped"))
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Category {
    IconMode,
    Theme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Preview(Theme, IconMode),
    Save(Theme, IconMode),
    Cancel(Theme, IconMode),
}

pub(super) struct Panel {
    saved_theme: Theme,
    saved_icons: IconMode,
    category: Category,
    sidebar_focus: bool,
    icon_index: usize,
    theme_index: usize,
    pub(super) area: Rect,
    sidebar_area: Rect,
    list_area: Rect,
    list_scroll: usize,
    pub(super) busy: bool,
    error: Option<String>,
}

impl Panel {
    pub(super) fn category_label(&self) -> &'static str {
        match self.category {
            Category::IconMode => "Icon Style",
            Category::Theme => "Color Theme",
        }
    }

    pub(super) fn saved_appearance(&self) -> (Theme, IconMode) {
        (self.saved_theme, self.saved_icons)
    }

    pub(super) fn new(saved_theme: Theme, saved_icons: IconMode) -> Self {
        let icon_index = IconMode::ALL
            .iter()
            .position(|&mode| mode == saved_icons)
            .unwrap_or(0);
        let theme_index = Theme::ALL
            .iter()
            .position(|&theme| theme == saved_theme)
            .unwrap_or(0);
        Self {
            saved_theme,
            saved_icons,
            category: Category::IconMode,
            sidebar_focus: false,
            icon_index,
            theme_index,
            area: Rect::default(),
            sidebar_area: Rect::default(),
            list_area: Rect::default(),
            list_scroll: 0,
            busy: false,
            error: None,
        }
    }

    pub(super) fn selected_index(&self) -> usize {
        match self.category {
            Category::IconMode => self.icon_index,
            Category::Theme => self.theme_index,
        }
    }

    pub(super) fn selected_icons(&self) -> IconMode {
        IconMode::ALL[self.icon_index]
    }

    pub(super) fn selected_theme(&self) -> Theme {
        Theme::ALL[self.theme_index]
    }

    fn preview_appearance(&self) -> (Theme, IconMode) {
        match self.category {
            Category::IconMode => (self.saved_theme, self.selected_icons()),
            Category::Theme => (self.selected_theme(), self.saved_icons),
        }
    }

    pub(super) fn set_error(&mut self, message: String) {
        self.busy = false;
        self.error = Some(pp_core::text::display_text(&message).into_owned());
    }

    pub(super) fn handle(&mut self, event: Event) -> Action {
        if self.busy {
            if let Event::Key(key) = event
                && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                && key.code == KeyCode::Esc
            {
                return Action::Cancel(self.saved_theme, self.saved_icons);
            }
            return Action::None;
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT)
                {
                    return Action::None;
                }
                match key.code {
                    KeyCode::Esc => Action::Cancel(self.saved_theme, self.saved_icons),
                    KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                    KeyCode::Home => self.jump_selection(0),
                    KeyCode::End => self.jump_selection(self.list_len().saturating_sub(1)),
                    KeyCode::Enter if self.sidebar_focus => {
                        self.sidebar_focus = false;
                        Action::None
                    }
                    KeyCode::Enter => {
                        self.busy = true;
                        self.error = None;
                        let (theme, icons) = self.preview_appearance();
                        Action::Save(theme, icons)
                    }
                    KeyCode::Tab | KeyCode::BackTab if key.kind == KeyEventKind::Press => {
                        if self.sidebar_area.width > 0 {
                            self.sidebar_focus = !self.sidebar_focus;
                        } else {
                            let next = match self.category {
                                Category::IconMode => Category::Theme,
                                Category::Theme => Category::IconMode,
                            };
                            return self.set_category(next);
                        }
                        Action::None
                    }
                    _ => Action::None,
                }
            }
            Event::Mouse(mouse) if self.area.contains((mouse.column, mouse.row).into()) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.move_selection(-1),
                    MouseEventKind::ScrollDown => self.move_selection(1),
                    MouseEventKind::Down(MouseButton::Left) => {
                        let pos = (mouse.column, mouse.row).into();
                        if self.sidebar_area.contains(pos) {
                            self.sidebar_focus = true;
                            let row = usize::from(mouse.row - self.sidebar_area.y);
                            return match row {
                                0 => self.set_category(Category::IconMode),
                                1 => self.set_category(Category::Theme),
                                _ => Action::None,
                            };
                        }
                        if self.list_area.contains(pos) {
                            self.sidebar_focus = false;
                            let row = usize::from(mouse.row - self.list_area.y) + self.list_scroll;
                            if row < self.list_len() {
                                return self.jump_selection(row);
                            }
                        }
                        Action::None
                    }
                    _ => Action::None,
                }
            }
            _ => Action::None,
        }
    }

    fn set_category(&mut self, category: Category) -> Action {
        let previous = self.preview_appearance();
        self.category = category;
        self.list_scroll = 0;
        self.error = None;
        let (theme, icons) = self.preview_appearance();
        if previous != (theme, icons) {
            Action::Preview(theme, icons)
        } else {
            Action::None
        }
    }

    fn list_len(&self) -> usize {
        match self.category {
            Category::IconMode => IconMode::ALL.len(),
            Category::Theme => Theme::ALL.len(),
        }
    }

    fn move_selection(&mut self, delta: isize) -> Action {
        if self.sidebar_focus {
            return self.set_category(if delta < 0 {
                Category::IconMode
            } else {
                Category::Theme
            });
        }
        let len = self.list_len();
        let current = match self.category {
            Category::IconMode => self.icon_index,
            Category::Theme => self.theme_index,
        };
        let next = if delta < 0 {
            if current == 0 { len - 1 } else { current - 1 }
        } else {
            (current + 1) % len
        };
        self.jump_selection(next)
    }

    fn jump_selection(&mut self, index: usize) -> Action {
        if self.sidebar_focus {
            return self.set_category(if index == 0 {
                Category::IconMode
            } else {
                Category::Theme
            });
        }
        let clamped = index.min(self.list_len().saturating_sub(1));
        self.error = None;
        let changed = match self.category {
            Category::IconMode => {
                let changed = self.icon_index != clamped;
                self.icon_index = clamped;
                changed
            }
            Category::Theme => {
                let changed = self.theme_index != clamped;
                self.theme_index = clamped;
                changed
            }
        };
        if changed {
            let (theme, icons) = self.preview_appearance();
            Action::Preview(theme, icons)
        } else {
            Action::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(panel: &mut Panel, code: KeyCode) -> Action {
        panel.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn render_screen(panel: &mut Panel, width: u16, height: u16) -> String {
        let (theme, icons) = panel.preview_appearance();
        set_active(theme, icons);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let result = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        set_active(Theme::Default, IconMode::Nerd);
        result
    }

    #[test]
    fn categories_preview_and_save_independently() {
        for (category, expected) in [
            (Category::IconMode, (Theme::Default, IconMode::Ascii)),
            (Category::Theme, (Theme::TokyoNight, IconMode::Nerd)),
        ] {
            let mut panel = Panel::new(Theme::Default, IconMode::Nerd);
            assert_eq!(
                key(&mut panel, KeyCode::Down),
                Action::Preview(Theme::Default, IconMode::Ascii)
            );
            let icons = render_screen(&mut panel, 110, 32);
            assert!(icons.contains("Themes — Icon Style"), "{icons}");
            assert!(icons.contains("Nerd Font") && icons.contains("ASCII"));

            // 切换分类仅预览该分类，另一分类使用已保存值。
            assert_eq!(
                panel.set_category(Category::Theme),
                Action::Preview(Theme::Default, IconMode::Nerd)
            );
            assert_eq!(
                key(&mut panel, KeyCode::Down),
                Action::Preview(Theme::TokyoNight, IconMode::Nerd)
            );
            let colors = render_screen(&mut panel, 110, 32);
            assert!(colors.contains("Themes — Color Theme"), "{colors}");
            assert!(colors.contains("Tokyo Night") && colors.contains("Solarized Light"));
            for code in [KeyCode::Backspace, KeyCode::Char('h'), KeyCode::Char('l')] {
                assert_eq!(key(&mut panel, code), Action::None);
                assert_eq!(panel.category, Category::Theme);
            }

            if category == Category::IconMode {
                assert_eq!(
                    panel.set_category(category),
                    Action::Preview(Theme::Default, IconMode::Ascii)
                );
            }
            assert_eq!(
                key(&mut panel, KeyCode::Enter),
                Action::Save(expected.0, expected.1)
            );
            assert!(panel.busy);
            assert_eq!(panel.category, category);
            assert_eq!(
                key(&mut panel, KeyCode::Esc),
                Action::Cancel(Theme::Default, IconMode::Nerd)
            );
        }
    }
}
