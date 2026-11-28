//! Paste, key and wheel input; only advances panel state and returns list actions.

use super::*;

impl Panel {
    pub(in crate::tui) fn paste(&mut self, text: &str) {
        if !matches!(self.phase, Phase::Ready) {
            return;
        }
        let cleaned: String = text
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .collect();
        if let Some(name) = &mut self.rename {
            name.push_str(&cleaned);
        } else if self.searching {
            self.query.push_str(&cleaned);
            self.refilter();
            self.reset_selection();
        }
    }

    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        match self.phase {
            Phase::Loading => {
                return if key.code == KeyCode::Esc {
                    Action::Close
                } else {
                    Action::None
                };
            }
            // While a read/open/rename/delete is in flight, wait for the worker result and accept no keys.
            Phase::Reading | Phase::Opening | Phase::Renaming | Phase::Deleting => {
                return Action::None;
            }
            Phase::Ready => {}
        }
        // The delete-confirmation dialog takes precedence: Cancel is the default and Tab/arrows explicitly select Delete.
        if let Some(mut confirm) = self.confirm.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                    confirm.delete_selected = !confirm.delete_selected;
                    self.confirm = Some(confirm);
                }
                KeyCode::Enter if key.kind == KeyEventKind::Press => {
                    if confirm.delete_selected {
                        return Action::Delete(confirm.id);
                    }
                    // Default Cancel: close the dialog without deleting.
                }
                _ => self.confirm = Some(confirm),
            }
            return Action::None;
        }
        // Single-line rename input: prefilled with the file name, Enter submits and Esc cancels.
        if self.rename.is_some() {
            return self.rename_key(key);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Search mode accepts only query characters/paste; r/d/n/c/e never become query characters or trigger actions.
        if self.searching {
            match key.code {
                KeyCode::Esc => self.searching = false,
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                    self.refilter();
                    self.reset_selection();
                }
                KeyCode::Char('u') if control => {
                    self.query.clear();
                    self.refilter();
                    self.reset_selection();
                }
                KeyCode::Char(character) if !control && !alt => {
                    self.query.push(character);
                    self.refilter();
                    self.reset_selection();
                }
                _ => {}
            }
            return Action::None;
        }
        // Snapshot the selected entry so action branches do not borrow and mutate panel state at once.
        let selected = self.selected_entry().map(|entry| {
            (
                entry.id.clone(),
                entry.file_name.clone(),
                entry.error.clone(),
                entry.location,
                entry.total,
            )
        });
        match key.code {
            KeyCode::Esc => Action::Close,
            KeyCode::Enter => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((id, _, None, _, _)) if !id.is_empty() => Action::Preview(id),
                Some(_) => Action::Message("This file cannot be managed as a plan.".into()),
                None => Action::None,
            },
            KeyCode::Left | KeyCode::Right => {
                self.tab = if key.code == KeyCode::Left {
                    self.tab.previous()
                } else {
                    self.tab.next()
                };
                self.refilter();
                self.reset_selection();
                Action::None
            }
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.refilter();
                self.reset_selection();
                Action::None
            }
            KeyCode::BackTab => {
                self.tab = self.tab.previous();
                self.refilter();
                self.reset_selection();
                Action::None
            }
            KeyCode::Up => {
                self.move_selection(true, 1);
                Action::None
            }
            KeyCode::Down => {
                self.move_selection(false, 1);
                Action::None
            }
            KeyCode::PageUp => {
                self.move_selection(true, self.page_size);
                Action::None
            }
            KeyCode::PageDown => {
                self.move_selection(false, self.page_size);
                Action::None
            }
            KeyCode::Home => {
                self.move_selection(true, self.selected);
                Action::None
            }
            KeyCode::End => {
                self.move_selection(false, self.filtered.len());
                Action::None
            }
            KeyCode::F(5) => {
                self.error = None;
                self.set_loading();
                Action::Refresh
            }
            KeyCode::Char('/') if !control && !alt => {
                self.searching = true;
                Action::None
            }
            KeyCode::Char('n') if !control && !alt => Action::NewPlan,
            KeyCode::Char('c') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((id, _, None, PlanLocation::Drafts, _)) => {
                    Action::Open(id, PlanOpenIntent::ContinueDraft)
                }
                Some(_) => Action::Message("Continue is only available for drafts.".into()),
                None => Action::None,
            },
            KeyCode::Char('e') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((_, _, None, PlanLocation::Done, _)) => {
                    Action::Message("Completed plans are read-only.".into())
                }
                Some((id, _, None, _, _)) => Action::Open(id, PlanOpenIntent::Execute),
                None => Action::None,
            },
            KeyCode::Char('r') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((_, file_name, None, _, _)) => {
                    self.rename = Some(file_name);
                    Action::None
                }
                None => Action::None,
            },
            KeyCode::Char('d') if !control && !alt => match selected {
                Some((_, _, Some(error), _, _)) => Action::Message(error),
                Some((id, file_name, None, location, _)) => {
                    self.confirm = Some(ConfirmDelete {
                        id,
                        path: format!(".pp/{}/{}", location.dir_name(), file_name),
                        delete_selected: false,
                        width: None,
                        lines: Vec::new(),
                    });
                    Action::None
                }
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn rename_key(&mut self, key: KeyEvent) -> Action {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.rename = None;
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                let current = self
                    .selected_entry()
                    .map(|entry| (entry.id.clone(), entry.file_name.clone()));
                let Some((id, file_name)) = current else {
                    self.rename = None;
                    return Action::None;
                };
                let name = self
                    .rename
                    .take()
                    .expect("rename editor open")
                    .trim()
                    .to_owned();
                if name.is_empty() || name == file_name {
                    // An empty or unchanged name is a no-op: close the editor right away.
                    return Action::None;
                }
                return Action::Rename(id, name);
            }
            KeyCode::Backspace => {
                if let Some(name) = &mut self.rename {
                    name.pop();
                }
            }
            KeyCode::Char('u') if control => {
                if let Some(name) = &mut self.rename {
                    name.clear();
                }
            }
            KeyCode::Char(character) if !control && !alt => {
                if let Some(name) = &mut self.rename {
                    name.push(character);
                }
            }
            _ => {}
        }
        Action::None
    }

    pub(in crate::tui) fn wheel(&mut self, up: bool) {
        if matches!(self.phase, Phase::Ready) && self.confirm.is_none() && self.rename.is_none() {
            self.move_selection(up, 1);
        }
    }
}
