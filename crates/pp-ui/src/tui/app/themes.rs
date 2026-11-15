use crate::tui::{App, Level, PendingKind, Style, commands, editor, themes, transcript_style};
use pp_config::config::{IconMode, Theme};

impl App {
    pub(in crate::tui) fn complete_theme_argument(&mut self, value: &str, confirm: bool) -> bool {
        let mut words: Vec<String> = self.input.lines()[0]
            .split_whitespace()
            .skip(1)
            .map(str::to_owned)
            .collect();
        let trailing = self.input.lines()[0]
            .chars()
            .last()
            .is_some_and(char::is_whitespace);
        if !trailing && !words.is_empty() {
            words.pop();
        }
        let next = if words.is_empty() {
            format!("/themes {value}")
        } else {
            format!("/themes {} {value}", words.join(" "))
        };
        self.input = editor();
        self.input.insert_str(next);
        self.refresh_candidates(true);
        confirm && self.submit(PendingKind::Steering)
    }

    pub(in crate::tui) fn open_themes(&mut self) {
        self.clear_other_settings_tabs(Some(super::super::settings::Tab::Themes));
        self.themes_panel = Some(themes::Panel::new(self.theme, self.icons));
        self.menu = None;
        self.file_query = None;
        self.menu_dismissed = true;
    }

    pub(in crate::tui) fn submit_themes_command(&mut self, command: commands::ThemeCommand) {
        if command.theme.is_none() && command.icons.is_none() {
            self.remember_input();
            self.clear_input();
            self.open_themes();
            return;
        }
        let theme = command.theme.unwrap_or(self.theme);
        let icons = command.icons.unwrap_or(self.icons);
        self.theme = theme;
        self.icons = icons;
        self.apply_appearance(theme, icons);
        self.remember_input();
        self.clear_input();
        self.append(
            "Themes",
            &format!(
                "Theme: {} ({}) · Icons: {} ({})",
                theme.label(),
                theme.as_str(),
                icons.label(),
                icons.as_str()
            ),
        );
        if let Some(path) = self.settings_path.clone()
            && let Err(error) = self.themes_worker.save(path, theme, icons)
        {
            self.notify(Level::Error, format!("{error:#}"));
        }
    }

    pub(in crate::tui) fn apply_themes_action(&mut self, action: themes::Action) {
        match action {
            themes::Action::None => {}
            themes::Action::Preview(theme, icons) => {
                self.apply_appearance(theme, icons);
            }
            themes::Action::Cancel(theme, icons) => {
                self.apply_appearance(theme, icons);
                self.themes_panel = None;
                self.menu_dismissed = true;
            }
            themes::Action::Save(theme, icons) => {
                if let Some(path) = self.settings_path.clone() {
                    if let Err(error) = self.themes_worker.save(path, theme, icons) {
                        if let Some(panel) = &mut self.themes_panel {
                            panel.set_error(format!("{error:#}"));
                        } else {
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                } else {
                    self.theme = theme;
                    self.icons = icons;
                    self.apply_appearance(theme, icons);
                    self.themes_panel = None;
                    self.menu_dismissed = true;
                }
            }
        }
    }

    pub(in crate::tui) fn apply_appearance(&mut self, theme: Theme, icons: IconMode) {
        themes::set_active(theme, icons);
        self.input
            .set_style(Style::default().fg(transcript_style::text()));
        if let Some(draft) = &mut self.model_picker_draft {
            draft
                .input
                .set_style(Style::default().fg(transcript_style::text()));
        }
        self.conversation.invalidate_theme();
        if let Some(review) = &mut self.review_transcript {
            review.invalidate_theme();
        }
    }

    pub(in crate::tui) fn receive_themes(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.themes_worker.replies.try_recv() {
            match reply.result {
                Ok(()) => {
                    self.theme = reply.theme;
                    self.icons = reply.icons;
                    if self.themes_panel.is_some() {
                        self.apply_appearance(reply.theme, reply.icons);
                        self.themes_panel = None;
                        self.menu_dismissed = true;
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if let Some(panel) = &mut self.themes_panel {
                        panel.set_error(message);
                    } else {
                        self.notify(Level::Error, message);
                    }
                }
            }
            changed = true;
        }
        changed
    }
}
