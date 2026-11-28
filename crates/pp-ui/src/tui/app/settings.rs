//! Unified Setting TUI coordination layer: tab switching, general settings panel
//! interaction, preview rollback and command/completion wiring.
use crate::tui::{
    App, Event, KeyCode, KeyEventKind, KeyModifiers, Level, PendingKind, editor, roles, settings,
    skills,
};
use crossterm::event::{MouseButton, MouseEventKind};
use pp_config::config::Settings;
use ratatui::layout::Rect;

impl App {
    pub(in crate::tui) fn active_settings_tab(&self) -> Option<settings::Tab> {
        if self.settings_panel.is_some() {
            Some(settings::Tab::General)
        } else if self.roles_panel.is_some() {
            Some(settings::Tab::Roles)
        } else if self.themes_panel.is_some() {
            Some(settings::Tab::Themes)
        } else if self.skill_panel.is_some() {
            Some(settings::Tab::Skills)
        } else {
            None
        }
    }

    fn settings_tab_busy(&self) -> bool {
        self.settings_panel
            .as_ref()
            .is_some_and(settings::Panel::is_saving)
            || self.roles_panel.as_ref().is_some_and(|panel| panel.busy)
            || self.themes_panel.as_ref().is_some_and(|panel| panel.busy)
            || self
                .skill_panel
                .as_ref()
                .is_some_and(skills::Panel::is_saving)
    }

    fn roles_picker_open(&self) -> bool {
        self.roles_panel
            .as_ref()
            .is_some_and(roles::Panel::is_picking)
    }

    fn settings_typing_input(&self) -> bool {
        match self.active_settings_tab() {
            Some(settings::Tab::General) => self
                .settings_panel
                .as_ref()
                .is_some_and(settings::Panel::is_typing_input),
            Some(settings::Tab::Roles) => self
                .roles_panel
                .as_ref()
                .is_some_and(|panel| panel.is_typing_input()),
            _ => false,
        }
    }

    fn active_settings_bar(&self) -> Option<(settings::Tab, Rect, Option<&str>)> {
        if let Some(panel) = &self.settings_panel {
            Some((settings::Tab::General, panel.area, None))
        } else if let Some(panel) = &self.roles_panel {
            Some((settings::Tab::Roles, panel.area, None))
        } else if let Some(panel) = &self.themes_panel {
            Some((settings::Tab::Themes, panel.area, None))
        } else {
            self.skill_panel
                .as_ref()
                .map(|panel| (settings::Tab::Skills, panel.area, None))
        }
    }

    pub(in crate::tui) fn clear_other_settings_tabs(&mut self, keep: Option<settings::Tab>) {
        if keep != Some(settings::Tab::General) {
            self.settings_panel = None;
        }
        if keep != Some(settings::Tab::Roles) {
            self.roles_panel = None;
        }
        if keep != Some(settings::Tab::Themes)
            && let Some(panel) = self.themes_panel.take()
        {
            // Leaving Themes without saving restores the appearance persisted before entry, so
            // hover previews never leak out.
            self.apply_appearance(panel.saved_appearance());
        }
        if keep != Some(settings::Tab::Skills) {
            self.skill_panel = None;
            self.skills_pending = false;
        }
    }

    pub(in crate::tui) fn open_settings(&mut self, tab: settings::Tab) {
        match tab {
            settings::Tab::General => {
                self.clear_other_settings_tabs(Some(settings::Tab::General));
                self.menu = None;
                self.file_query = None;
                self.menu_dismissed = true;
                if let Some(path) = self.settings_path.clone() {
                    let mut panel = settings::Panel::loading(
                        path.display().to_string(),
                        self.cached_settings.clone(),
                    );
                    if let Err(error) = self.settings_worker.load(&path) {
                        panel.apply_loaded(Err(error));
                    }
                    self.settings_panel = Some(panel);
                } else {
                    let mut panel = settings::Panel::loading(
                        "(memory only)".to_owned(),
                        self.cached_settings.clone(),
                    );
                    panel.apply_loaded(Ok(Settings::default()));
                    self.settings_panel = Some(panel);
                }
            }
            settings::Tab::Roles => self.open_roles(),
            settings::Tab::Themes => self.open_themes(),
            settings::Tab::Skills => self.request_skills(skills::Request::Open),
        }
    }

    pub(in crate::tui) fn submit_setting_command(&mut self, tab: crate::commands::SettingTab) {
        let target = settings::Tab::from_command_tab(tab);
        self.remember_input();
        self.clear_input();
        self.open_settings(target);
    }

    pub(in crate::tui) fn complete_setting_tab(
        &mut self,
        tab: settings::Tab,
        confirm: bool,
    ) -> bool {
        let command = self
            .input
            .lines()
            .first()
            .and_then(|line| line.split_whitespace().next())
            .filter(|name| matches!(*name, "/setting" | "/settings" | "/config"))
            .unwrap_or("/setting")
            .to_owned();
        self.input = editor();
        self.input.insert_str(format!("{command} {}", tab.slug()));
        self.refresh_candidates(true);
        confirm && self.submit(PendingKind::Steering)
    }

    pub(in crate::tui) fn handle_settings_event(&mut self, event: Event, dirty: &mut bool) -> bool {
        self.escape_tap.reset();
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press);
        }
        if let Some(target) = self.handle_settings_tab_shortcut(&event) {
            if Some(target) != self.active_settings_tab() {
                self.open_settings(target);
            }
            *dirty = true;
            return false;
        }
        if let Some(panel) = &mut self.settings_panel {
            let action = panel.handle(event);
            self.apply_settings_action(action);
        } else if let Some(panel) = &mut self.roles_panel {
            let action = panel.handle(event);
            self.apply_roles_action(action);
        } else if let Some(panel) = &mut self.themes_panel {
            let action = panel.handle(event);
            self.apply_themes_action(action);
        } else if let Some(panel) = &mut self.skill_panel {
            self.escape_tap.reset();
            match panel.handle(event) {
                skills::Action::None => {}
                skills::Action::Cancel => {
                    self.skill_panel = None;
                    self.menu_dismissed = true;
                }
                skills::Action::Save(selection) => {
                    self.request_skills(skills::Request::Save(selection))
                }
                skills::Action::Reload => self.request_skills(skills::Request::Reload),
                skills::Action::Preview(selection) => {
                    self.request_skills(skills::Request::Preview(selection))
                }
            }
        }
        *dirty = true;
        false
    }

    fn handle_settings_tab_shortcut(&self, event: &Event) -> Option<settings::Tab> {
        if self.settings_tab_busy() {
            return None;
        }
        match event {
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                let (active, area, suffix) = self.active_settings_bar()?;
                settings::render::tab_at_column(active, suffix, area, mouse.column, mouse.row)
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let active = self.active_settings_tab()?;
                if self.settings_typing_input() {
                    return None;
                }
                if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::CONTROL {
                    return None;
                }
                // While the Roles-page model/effort picker is open, unmodified ←/→ scroll
                // selections and must not switch the outer tab; Ctrl + ←/→ is still the explicit
                // tab-switch gesture.
                if key.modifiers.is_empty() && self.roles_picker_open() {
                    return None;
                }
                match key.code {
                    KeyCode::Left => Some(active.prev()),
                    KeyCode::Right => Some(active.next()),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn apply_settings_action(&mut self, action: settings::Action) {
        match action {
            settings::Action::None => {}
            settings::Action::Close => {
                self.settings_panel = None;
                self.menu_dismissed = true;
            }
            settings::Action::Reload => {
                if let Some(path) = self.settings_path.clone() {
                    if let Err(error) = self.settings_worker.load(&path)
                        && let Some(panel) = &mut self.settings_panel
                    {
                        panel.apply_loaded(Err(error));
                    }
                } else if let Some(panel) = &mut self.settings_panel {
                    panel.apply_loaded(Ok(Settings::default()));
                }
            }
            settings::Action::Save(patch) => {
                if let Some(path) = self.settings_path.clone() {
                    if let Err(error) = self.settings_worker.save(&path, patch)
                        && let Some(panel) = &mut self.settings_panel
                    {
                        panel.apply_saved(Err(error));
                    }
                } else if let Some(panel) = &mut self.settings_panel {
                    panel.apply_saved(Err(anyhow::anyhow!("settings.yml path is unavailable")));
                }
            }
        }
    }

    pub(in crate::tui) fn receive_settings(&mut self) -> bool {
        let mut changed = false;
        while let Some(reply) = self.settings_worker.try_recv() {
            match reply {
                settings::Reply::Loaded(result) => {
                    if let Ok(settings) = &result {
                        self.cached_settings = Some(settings.clone());
                    }
                    if let Some(panel) = &mut self.settings_panel {
                        panel.apply_loaded(result);
                    }
                }
                settings::Reply::Saved(result) => match result {
                    Ok(saved) => {
                        self.cached_settings = Some(saved.clone());
                        self.cost_display = saved.cost.clone();
                        self.conversation
                            .set_bash_timeout(saved.bash_timeout_seconds);
                        self.pending_max_fps = Some(saved.tui.max_fps);
                        self.permission.set_config(saved.tools.clone());
                        self.permission_mode = Some(self.permission.snapshot());
                        if let Some(usage) = &mut self.usage {
                            usage.auto_compact = saved.auto_compact;
                            usage.threshold_percent = saved.auto_compact_threshold_percent;
                        }
                        if let Some(panel) = &mut self.context_panel {
                            panel.update_compaction_policy(
                                saved.auto_compact,
                                saved.auto_compact_threshold_percent,
                            );
                        }
                        if let Some(commands) = &self.commands
                            && (commands
                                .send(crate::tui::WorkerCommand::UpdateCompactionPolicy {
                                    auto_compact: saved.auto_compact,
                                    threshold_percent: saved.auto_compact_threshold_percent,
                                })
                                .is_err()
                                || commands
                                    .send(crate::tui::WorkerCommand::SyncPermissions)
                                    .is_err()
                                || commands
                                    .send(crate::tui::WorkerCommand::SyncSandboxSettings(
                                        saved.tools.sandbox.mode,
                                    ))
                                    .is_err()
                                || commands
                                    .send(crate::tui::WorkerCommand::SyncWebSearchSettings(
                                        saved.web_search.clone(),
                                    ))
                                    .is_err())
                        {
                            self.fail_worker();
                        }
                        if let Some(panel) = &mut self.settings_panel {
                            panel.apply_saved(Ok(saved));
                        }
                    }
                    Err(error) => {
                        let message = settings::display_error(&error);
                        if let Some(panel) = &mut self.settings_panel {
                            panel.apply_saved(Err(error));
                        } else {
                            self.notify(Level::Error, message);
                        }
                    }
                },
            }
            changed = true;
        }
        changed
    }
}
