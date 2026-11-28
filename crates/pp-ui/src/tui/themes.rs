//! Theme palettes, the active icon mode (Nerd Font / ASCII) and the `/themes` per-category settings panel.
use std::{
    cell::Cell,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use pp_config::config::{IconMode, Theme, ToolStyle, TuiSettings};
use ratatui::layout::Rect;

pub(in crate::tui) use palette::{Palette, palette_for};

mod palette;
mod preview;
mod render;
#[cfg(test)]
mod tests;
mod tools_preview;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui) struct Appearance {
    pub theme: Theme,
    pub icons: IconMode,
    pub tools: ToolStyle,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: Theme::Default,
            icons: IconMode::Nerd,
            tools: ToolStyle::Classic,
        }
    }
}

thread_local! {
    static ACTIVE: Cell<Appearance> = const { Cell::new(Appearance {
        theme: Theme::Default,
        icons: IconMode::Nerd,
        tools: ToolStyle::Classic,
    }) };
}

pub(in crate::tui) fn current_appearance() -> Appearance {
    ACTIVE.with(Cell::get)
}

pub(in crate::tui) fn current() -> (Theme, IconMode) {
    let a = ACTIVE.with(Cell::get);
    (a.theme, a.icons)
}

pub(in crate::tui) fn current_theme() -> Theme {
    current_appearance().theme
}

pub(in crate::tui) fn current_icons() -> IconMode {
    current_appearance().icons
}

pub(in crate::tui) fn current_tool_style() -> ToolStyle {
    current_appearance().tools
}

pub(in crate::tui) fn is_ascii() -> bool {
    current_icons() == IconMode::Ascii
}

pub(in crate::tui) fn set_active(theme: Theme, icons: IconMode) {
    ACTIVE.with(|cell| {
        let mut a = cell.get();
        a.theme = theme;
        a.icons = icons;
        cell.set(a);
    });
}

pub(in crate::tui) fn set_active_appearance(theme: Theme, icons: IconMode, tools: ToolStyle) {
    ACTIVE.with(|cell| {
        cell.set(Appearance {
            theme,
            icons,
            tools,
        })
    });
}

pub(in crate::tui) fn palette() -> &'static Palette {
    palette_for(current_theme())
}

pub(super) struct SaveRequest {
    pub(super) path: PathBuf,
    pub(super) appearance: Appearance,
}

pub(super) struct SaveReply {
    pub(super) appearance: Appearance,
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
                    let a = request.appearance;
                    let result =
                        TuiSettings::save_appearance(&request.path, a.theme, a.icons, a.tools);
                    if sender
                        .send(SaveReply {
                            appearance: a,
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

    pub(super) fn save(&self, path: PathBuf, appearance: Appearance) -> Result<()> {
        self.requests
            .as_ref()
            .context("theme settings worker stopped")?
            .send(SaveRequest { path, appearance })
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
    Tools,
}

impl Category {
    const ALL: [Self; 3] = [Self::IconMode, Self::Theme, Self::Tools];

    fn next(self) -> Self {
        match self {
            Self::IconMode => Self::Theme,
            Self::Theme => Self::Tools,
            Self::Tools => Self::IconMode,
        }
    }

    fn step(self, delta: isize) -> Self {
        let idx = Self::ALL.iter().position(|&c| c == self).unwrap_or(0);
        let len = Self::ALL.len();
        let next = if delta < 0 {
            (idx + len - 1) % len
        } else {
            (idx + 1) % len
        };
        Self::ALL[next]
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Preview(Appearance),
    Save(Appearance),
    Cancel(Appearance),
}

pub(super) struct Panel {
    saved: Appearance,
    category: Category,
    sidebar_focus: bool,
    icon_index: usize,
    theme_index: usize,
    tools_index: usize,
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
            Category::Tools => "Tools",
        }
    }

    pub(super) fn saved_appearance(&self) -> Appearance {
        self.saved
    }

    pub(super) fn new(saved: Appearance) -> Self {
        let icon_index = IconMode::ALL
            .iter()
            .position(|&mode| mode == saved.icons)
            .unwrap_or(0);
        let theme_index = Theme::ALL
            .iter()
            .position(|&theme| theme == saved.theme)
            .unwrap_or(0);
        let tools_index = ToolStyle::ALL
            .iter()
            .position(|&style| style == saved.tools)
            .unwrap_or(0);
        Self {
            saved,
            category: Category::IconMode,
            sidebar_focus: false,
            icon_index,
            theme_index,
            tools_index,
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
            Category::Tools => self.tools_index,
        }
    }

    pub(super) fn selected_icons(&self) -> IconMode {
        IconMode::ALL[self.icon_index]
    }

    pub(super) fn selected_theme(&self) -> Theme {
        Theme::ALL[self.theme_index]
    }

    pub(super) fn selected_tools(&self) -> ToolStyle {
        ToolStyle::ALL[self.tools_index]
    }

    fn preview_appearance(&self) -> Appearance {
        match self.category {
            Category::IconMode => Appearance {
                icons: self.selected_icons(),
                ..self.saved
            },
            Category::Theme => Appearance {
                theme: self.selected_theme(),
                ..self.saved
            },
            Category::Tools => Appearance {
                tools: self.selected_tools(),
                ..self.saved
            },
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
                return Action::Cancel(self.saved);
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
                    KeyCode::Esc => Action::Cancel(self.saved),
                    KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                    KeyCode::PageUp if !self.sidebar_focus => {
                        let step = usize::from(self.list_area.height).max(1);
                        self.jump_selection(self.selected_index().saturating_sub(step))
                    }
                    KeyCode::PageDown if !self.sidebar_focus => {
                        let step = usize::from(self.list_area.height).max(1);
                        self.jump_selection(self.selected_index().saturating_add(step))
                    }
                    KeyCode::Home => self.jump_selection(0),
                    KeyCode::End => {
                        let len = if self.sidebar_focus {
                            Category::ALL.len()
                        } else {
                            self.list_len()
                        };
                        self.jump_selection(len.saturating_sub(1))
                    }
                    KeyCode::Enter if self.sidebar_focus => {
                        self.sidebar_focus = false;
                        Action::None
                    }
                    KeyCode::Enter => {
                        self.busy = true;
                        self.error = None;
                        Action::Save(self.preview_appearance())
                    }
                    KeyCode::Tab | KeyCode::BackTab if key.kind == KeyEventKind::Press => {
                        if self.sidebar_area.width > 0 {
                            self.sidebar_focus = !self.sidebar_focus;
                            Action::None
                        } else {
                            let next = if key.code == KeyCode::BackTab {
                                self.category.step(-1)
                            } else {
                                self.category.next()
                            };
                            self.set_category(next)
                        }
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
                            return match Category::ALL.get(row).copied() {
                                Some(cat) => self.set_category(cat),
                                None => Action::None,
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
        let next = self.preview_appearance();
        if previous != next {
            Action::Preview(next)
        } else {
            Action::None
        }
    }

    fn list_len(&self) -> usize {
        match self.category {
            Category::IconMode => IconMode::ALL.len(),
            Category::Theme => Theme::ALL.len(),
            Category::Tools => ToolStyle::ALL.len(),
        }
    }

    fn move_selection(&mut self, delta: isize) -> Action {
        if self.sidebar_focus {
            return self.set_category(self.category.step(delta));
        }
        let len = self.list_len();
        let current = self.selected_index();
        let next = if delta < 0 {
            if current == 0 { len - 1 } else { current - 1 }
        } else {
            (current + 1) % len
        };
        self.jump_selection(next)
    }

    fn jump_selection(&mut self, index: usize) -> Action {
        if self.sidebar_focus {
            let clamped = index.min(Category::ALL.len() - 1);
            return self.set_category(Category::ALL[clamped]);
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
            Category::Tools => {
                let changed = self.tools_index != clamped;
                self.tools_index = clamped;
                changed
            }
        };
        if changed {
            Action::Preview(self.preview_appearance())
        } else {
            Action::None
        }
    }
}
