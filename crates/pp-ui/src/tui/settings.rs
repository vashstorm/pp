//! Unified `/setting` TUI: four-tab frame, the `General` category edit panel and the background config Worker.
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use pp_config::config::{SettingPatch, Settings, read_settings, update_general_setting};
use ratatui::layout::Rect;

mod items;
pub(in crate::tui) mod render;

use items::{Category, SettingItem, ValueKind};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::tui) enum Tab {
    #[default]
    General,
    Roles,
    Themes,
    Skills,
}

impl Tab {
    pub(in crate::tui) const ALL: [Self; 4] =
        [Self::General, Self::Roles, Self::Themes, Self::Skills];

    pub(in crate::tui) fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Roles => "Roles",
            Self::Themes => "Themes",
            Self::Skills => "Skills",
        }
    }

    pub(in crate::tui) fn slug(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Roles => "roles",
            Self::Themes => "themes",
            Self::Skills => "skills",
        }
    }

    pub(in crate::tui) fn from_command_tab(tab: crate::commands::SettingTab) -> Self {
        match tab {
            crate::commands::SettingTab::General => Self::General,
            crate::commands::SettingTab::Roles => Self::Roles,
            crate::commands::SettingTab::Themes => Self::Themes,
            crate::commands::SettingTab::Skills => Self::Skills,
        }
    }

    pub(in crate::tui) fn prev(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::ALL[(index + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    pub(in crate::tui) fn next(self) -> Self {
        let index = Self::ALL
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }
}

enum Request {
    Load(PathBuf),
    Save { path: PathBuf, patch: SettingPatch },
}

pub(in crate::tui) enum Reply {
    Loaded(Result<Settings>),
    Saved(Result<Settings>),
}

pub(in crate::tui) struct Worker {
    requests: Option<Sender<Request>>,
    replies: Receiver<Reply>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(in crate::tui) fn new() -> Result<Self> {
        let (requests, worker_rx) = mpsc::channel::<Request>();
        let (sender, replies) = mpsc::channel::<Reply>();
        let thread = thread::Builder::new()
            .name("pp-settings".into())
            .spawn(move || {
                while let Ok(request) = worker_rx.recv() {
                    let reply = match request {
                        Request::Load(path) => Reply::Loaded(read_settings(&path)),
                        Request::Save { path, patch } => {
                            Reply::Saved(update_general_setting(&path, patch))
                        }
                    };
                    if sender.send(reply).is_err() {
                        break;
                    }
                }
            })
            .context("starting general settings worker")?;
        Ok(Self {
            requests: Some(requests),
            replies,
            thread: Some(thread),
        })
    }

    pub(in crate::tui) fn load(&self, path: &Path) -> Result<()> {
        self.requests
            .as_ref()
            .context("settings worker stopped")?
            .send(Request::Load(path.to_path_buf()))
            .map_err(|_| anyhow::anyhow!("settings worker stopped"))
    }

    pub(in crate::tui) fn save(&self, path: &Path, patch: SettingPatch) -> Result<()> {
        self.requests
            .as_ref()
            .context("settings worker stopped")?
            .send(Request::Save {
                path: path.to_path_buf(),
                patch,
            })
            .map_err(|_| anyhow::anyhow!("settings worker stopped"))
    }

    pub(in crate::tui) fn try_recv(&self) -> Option<Reply> {
        self.replies.try_recv().ok()
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

#[derive(Clone, Debug)]
pub(super) struct InlineEdit {
    pub(super) buffer: String,
    pub(super) allow_decimal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(in crate::tui) enum Action {
    None,
    Close,
    Reload,
    Save(SettingPatch),
}

pub(in crate::tui) struct Panel {
    pub(super) settings: Option<Settings>,
    pub(super) settings_display_path: String,
    pub(super) category_index: usize,
    pub(super) item_index: usize,
    pub(super) sidebar_focus: bool,
    pub(super) item_scroll: usize,
    pub(super) editing: Option<InlineEdit>,
    pub(in crate::tui) busy: bool,
    saving: bool,
    pub(super) status: Option<String>,
    pub(super) error: Option<String>,
    pub(in crate::tui) area: Rect,
    pub(super) sidebar_area: Rect,
    pub(super) list_area: Rect,
}

impl Panel {
    pub(in crate::tui) fn loading(settings_display_path: String, cached: Option<Settings>) -> Self {
        let has_cached = cached.is_some();
        Self {
            settings: cached,
            settings_display_path,
            category_index: 0,
            item_index: 0,
            sidebar_focus: false,
            item_scroll: 0,
            editing: None,
            busy: true,
            saving: false,
            status: (!has_cached).then(|| "Loading settings.yml...".to_owned()),
            error: None,
            area: Rect::default(),
            sidebar_area: Rect::default(),
            list_area: Rect::default(),
        }
    }

    pub(in crate::tui) fn is_saving(&self) -> bool {
        self.saving
    }

    pub(in crate::tui) fn apply_loaded(&mut self, result: Result<Settings>) {
        self.busy = false;
        self.saving = false;
        match result {
            Ok(settings) => {
                self.settings = Some(settings);
                self.error = None;
                self.status = None;
            }
            Err(error) => {
                self.status = None;
                self.error = Some(display_error(&error));
            }
        }
    }

    pub(in crate::tui) fn apply_saved(&mut self, result: Result<Settings>) {
        self.busy = false;
        self.saving = false;
        match result {
            Ok(settings) => {
                self.settings = Some(settings);
                self.editing = None;
                self.error = None;
                self.status = Some("Saved to settings.yml".to_owned());
            }
            Err(error) => {
                self.status = None;
                self.error = Some(display_error(&error));
            }
        }
    }

    pub(in crate::tui) fn is_typing_input(&self) -> bool {
        self.editing.is_some()
    }

    pub(in crate::tui) fn set_error(&mut self, message: String) {
        self.status = None;
        self.error = Some(pp_core::text::display_text(&message).into_owned());
    }

    fn selected_category(&self) -> Category {
        Category::ALL[self.category_index.min(Category::ALL.len() - 1)]
    }

    fn category_items(&self) -> Vec<&'static SettingItem> {
        self.selected_category().items().collect()
    }

    fn selected_item(&self) -> Option<&'static SettingItem> {
        let items = self.category_items();
        items.get(self.item_index).copied()
    }

    pub(in crate::tui) fn handle(&mut self, event: Event) -> Action {
        if self.busy {
            if let Event::Key(key) = event
                && key.code == KeyCode::Esc
                && key.kind == KeyEventKind::Press
            {
                return Action::Close;
            }
            return Action::None;
        }
        match event {
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            _ => Action::None,
        }
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> Action {
        if !self.area.contains((mouse.column, mouse.row).into()) {
            return Action::None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.editing.is_none() {
                    self.move_selection(-1);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.editing.is_none() {
                    self.move_selection(1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (mouse.column, mouse.row).into();
                if self.sidebar_area.contains(pos) {
                    self.editing = None;
                    self.sidebar_focus = true;
                    let row = usize::from(mouse.row - self.sidebar_area.y);
                    if row < Category::ALL.len() {
                        self.select_category(row);
                    }
                } else if self.list_area.contains(pos) {
                    self.editing = None;
                    self.sidebar_focus = false;
                    let row = usize::from(mouse.row - self.list_area.y) + self.item_scroll;
                    let items = self.category_items();
                    if row < items.len() {
                        self.item_index = row;
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        if self.editing.is_some() {
            return self.handle_editing_key(key);
        }
        if key.code == KeyCode::Esc {
            return Action::Close;
        }
        if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
            return Action::None;
        }
        match key.code {
            KeyCode::Char('r') if key.kind == KeyEventKind::Press => {
                self.busy = true;
                self.error = None;
                self.status = Some("Reloading settings.yml...".to_owned());
                Action::Reload
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                Action::None
            }
            KeyCode::Home => {
                self.jump_selection(0);
                Action::None
            }
            KeyCode::End => {
                let len = if self.sidebar_focus {
                    Category::ALL.len()
                } else {
                    self.category_items().len()
                };
                self.jump_selection(len.saturating_sub(1));
                Action::None
            }
            KeyCode::Tab | KeyCode::BackTab if key.kind == KeyEventKind::Press => {
                if self.sidebar_area.width > 0 {
                    self.sidebar_focus = !self.sidebar_focus;
                } else {
                    let forward = key.code == KeyCode::Tab;
                    let next = if forward {
                        (self.category_index + 1) % Category::ALL.len()
                    } else {
                        (self.category_index + Category::ALL.len() - 1) % Category::ALL.len()
                    };
                    self.select_category(next);
                }
                Action::None
            }
            KeyCode::Char('h') => {
                if self.sidebar_focus {
                    Action::None
                } else {
                    self.adjust_selected(false)
                }
            }
            KeyCode::Char('l') => {
                if self.sidebar_focus {
                    self.sidebar_focus = false;
                    Action::None
                } else {
                    self.adjust_selected(true)
                }
            }
            KeyCode::Char('-') => {
                if self.sidebar_focus {
                    Action::None
                } else {
                    self.adjust_selected(false)
                }
            }
            KeyCode::Char('+') | KeyCode::Char('=') => {
                if self.sidebar_focus {
                    Action::None
                } else {
                    self.adjust_selected(true)
                }
            }
            KeyCode::Char(' ') if key.kind == KeyEventKind::Press => {
                if self.sidebar_focus {
                    self.sidebar_focus = false;
                    Action::None
                } else {
                    self.adjust_selected(true)
                }
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if self.sidebar_focus {
                    self.sidebar_focus = false;
                    return Action::None;
                }
                let (Some(settings), Some(item)) = (&self.settings, self.selected_item()) else {
                    return Action::None;
                };
                match item.kind {
                    ValueKind::Int { .. } | ValueKind::Float { .. } => {
                        self.error = None;
                        self.status = None;
                        self.editing = Some(InlineEdit {
                            buffer: item.raw_numeric(settings),
                            allow_decimal: matches!(item.kind, ValueKind::Float { .. }),
                        });
                        Action::None
                    }
                    _ => self.adjust_selected(true),
                }
            }
            _ => Action::None,
        }
    }

    fn handle_editing_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.editing = None;
                self.error = None;
                Action::None
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                let (Some(edit), Some(item)) = (self.editing.as_ref(), self.selected_item()) else {
                    return Action::None;
                };
                match item.parse_input(&edit.buffer) {
                    Ok(patch) => self.begin_save(patch),
                    Err(error) => {
                        self.error = Some(display_error(&error));
                        Action::None
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(edit) = self.editing.as_mut() {
                    edit.buffer.pop();
                }
                Action::None
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if let Some(edit) = self.editing.as_mut()
                    && (ch.is_ascii_digit()
                        || (edit.allow_decimal && ch == '.' && !edit.buffer.contains('.')))
                {
                    edit.buffer.push(ch);
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn adjust_selected(&mut self, forward: bool) -> Action {
        let (Some(settings), Some(item)) = (&self.settings, self.selected_item()) else {
            return Action::None;
        };
        match item.step_or_cycle(settings, forward) {
            Ok(patch) => self.begin_save(patch),
            Err(error) => {
                self.error = Some(display_error(&error));
                Action::None
            }
        }
    }

    fn begin_save(&mut self, patch: SettingPatch) -> Action {
        self.busy = true;
        self.saving = true;
        self.error = None;
        self.status = Some("Saving settings.yml...".to_owned());
        Action::Save(patch)
    }

    fn select_category(&mut self, index: usize) {
        self.category_index = index.min(Category::ALL.len() - 1);
        self.item_index = 0;
        self.item_scroll = 0;
    }

    fn move_selection(&mut self, delta: isize) {
        let len = if self.sidebar_focus {
            Category::ALL.len()
        } else {
            self.category_items().len()
        };
        if len == 0 {
            return;
        }
        let current = if self.sidebar_focus {
            self.category_index
        } else {
            self.item_index
        };
        let next = if delta < 0 {
            (current + len - 1) % len
        } else {
            (current + 1) % len
        };
        self.jump_selection(next);
    }

    fn jump_selection(&mut self, index: usize) {
        if self.sidebar_focus {
            self.select_category(index);
        } else {
            let len = self.category_items().len();
            self.item_index = index.min(len.saturating_sub(1));
        }
    }
}

pub(in crate::tui) fn display_error(error: &anyhow::Error) -> String {
    pp_core::text::display_text(&format!("{error:#}")).into_owned()
}
