use crate::tui::{App, Level, PendingKind, Style, commands, editor, themes, transcript_style};

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
        self.themes_panel = Some(themes::Panel::new(themes::Appearance {
            theme: self.theme,
            icons: self.icons,
            tools: self.tool_style,
        }));
        self.menu = None;
        self.file_query = None;
        self.menu_dismissed = true;
    }

    pub(in crate::tui) fn submit_themes_command(&mut self, command: commands::ThemeCommand) {
        if command.theme.is_none() && command.icons.is_none() && command.tools.is_none() {
            self.remember_input();
            self.clear_input();
            self.open_themes();
            return;
        }
        let appearance = themes::Appearance {
            theme: command.theme.unwrap_or(self.theme),
            icons: command.icons.unwrap_or(self.icons),
            tools: command.tools.unwrap_or(self.tool_style),
        };
        self.theme = appearance.theme;
        self.icons = appearance.icons;
        self.tool_style = appearance.tools;
        self.apply_appearance(appearance);
        self.remember_input();
        self.clear_input();
        self.append(
            "Themes",
            &format!(
                "Theme: {} ({}) · Icons: {} ({}) · Tools: {} ({})",
                appearance.theme.label(),
                appearance.theme.as_str(),
                appearance.icons.label(),
                appearance.icons.as_str(),
                appearance.tools.label(),
                appearance.tools.as_str(),
            ),
        );
        if let Some(path) = self.settings_path.clone()
            && let Err(error) = self.themes_worker.save(path, appearance)
        {
            self.notify(Level::Error, format!("{error:#}"));
        }
    }

    pub(in crate::tui) fn apply_themes_action(&mut self, action: themes::Action) {
        match action {
            themes::Action::None => {}
            themes::Action::Preview(appearance) => {
                self.apply_appearance(appearance);
            }
            themes::Action::Cancel(appearance) => {
                self.apply_appearance(appearance);
                self.themes_panel = None;
                self.menu_dismissed = true;
            }
            themes::Action::Save(appearance) => {
                if let Some(path) = self.settings_path.clone() {
                    if let Err(error) = self.themes_worker.save(path, appearance) {
                        if let Some(panel) = &mut self.themes_panel {
                            panel.set_error(format!("{error:#}"));
                        } else {
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                } else {
                    self.theme = appearance.theme;
                    self.icons = appearance.icons;
                    self.tool_style = appearance.tools;
                    self.apply_appearance(appearance);
                    self.themes_panel = None;
                    self.menu_dismissed = true;
                }
            }
        }
    }

    pub(in crate::tui) fn apply_appearance(&mut self, appearance: themes::Appearance) {
        themes::set_active_appearance(appearance.theme, appearance.icons, appearance.tools);
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
                    self.theme = reply.appearance.theme;
                    self.icons = reply.appearance.icons;
                    self.tool_style = reply.appearance.tools;
                    self.apply_appearance(reply.appearance);
                    if self.themes_panel.is_some() {
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
