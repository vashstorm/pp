//! 键鼠事件路由：/Roles 面板的唯一输入入口。

use super::*;

impl Panel {
    pub(in crate::tui) fn handle(&mut self, event: Event) -> Action {
        if self.busy {
            return Action::None;
        }
        match event {
            Event::Paste(text) if self.view == View::Models && !self.sidebar_focus => {
                self.query.push_str(&text.replace(['\r', '\n'], " "));
                self.selected = 0;
            }
            Event::Mouse(mouse) => {
                if !self.area.contains((mouse.column, mouse.row).into()) {
                    return Action::None;
                }
                let sidebar = self.sidebar_area.contains((mouse.column, mouse.row).into());
                if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
                    if sidebar {
                        let row = usize::from(mouse.row.saturating_sub(self.sidebar_area.y))
                            + self.sidebar_scroll;
                        let index = match row {
                            0..=1 => Some(row),
                            3.. => Some(row - 1),
                            _ => None,
                        };
                        if let Some(index) = index.filter(|index| *index < self.providers.len() + 2)
                        {
                            self.sidebar = index;
                            self.sidebar_focus = true;
                            self.activate_sidebar();
                        }
                    } else if self.body.contains((mouse.column, mouse.row).into()) {
                        self.sidebar_focus = false;
                        let content_y = self.body.y + self.content_offset();
                        let row = usize::from(mouse.row.saturating_sub(content_y)) + self.scroll;
                        if self.view == View::Roles {
                            if mouse.row >= content_y && row < self.rows().len() {
                                self.role_index = row;
                                if mouse.column == self.body.x + 2 && !self.on_fallback() {
                                    return self.toggle_role();
                                }
                            }
                        } else if self.view == View::Models {
                            self.selected = row.min(self.model_indices().len().saturating_sub(1));
                        }
                    }
                } else if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.move_selection(mouse.kind == MouseEventKind::ScrollDown, 3);
                }
            }
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if key.code == KeyCode::Esc {
                    if self.view == View::Effort {
                        self.view = View::Models;
                        self.pending = None;
                        self.selected = 0;
                    } else if self.view == View::Models {
                        self.saved();
                    } else {
                        return Action::Close;
                    }
                    return Action::None;
                }
                if key.code == KeyCode::Char('f')
                    && key.modifiers == KeyModifiers::CONTROL
                    && self.view == View::Models
                    && !self.sidebar_focus
                    && key.kind == KeyEventKind::Press
                {
                    return self
                        .model_indices()
                        .get(self.selected)
                        .map(|&index| Action::Favorite(self.choices[index].reference.clone()))
                        .unwrap_or(Action::None);
                }
                if !key.modifiers.is_empty() && key.modifiers != KeyModifiers::SHIFT {
                    return Action::None;
                }
                match key.code {
                    KeyCode::Char(' ')
                        if self.view == View::Roles && key.kind == KeyEventKind::Press =>
                    {
                        self.sidebar_focus = false;
                        return self.toggle_role();
                    }
                    KeyCode::Up | KeyCode::Down => {
                        self.move_selection(key.code == KeyCode::Down, 1)
                    }
                    KeyCode::PageUp | KeyCode::PageDown => self.move_selection(
                        key.code == KeyCode::PageDown,
                        usize::from(self.body.height.saturating_sub(3)).max(1),
                    ),
                    KeyCode::Left if self.view == View::Effort => self.move_selection(false, 1),
                    KeyCode::Right if self.view == View::Effort => self.move_selection(true, 1),
                    KeyCode::Left | KeyCode::BackTab => self.sidebar_focus = true,
                    KeyCode::Right if self.sidebar_focus => self.sidebar_focus = false,
                    KeyCode::Tab if self.sidebar_focus => self.sidebar_focus = false,
                    KeyCode::Enter if self.sidebar_focus => self.sidebar_focus = false,
                    KeyCode::Enter if key.kind == KeyEventKind::Press => match self.view {
                        View::Roles => {
                            self.editing_fallback = self.on_fallback();
                            self.begin_models();
                        }
                        View::Models => {
                            if let Some(&index) = self.model_indices().get(self.selected) {
                                self.begin_effort(self.choices[index].reference.clone());
                            }
                        }
                        View::Effort => {
                            if let Some(&effort) = self.efforts().get(self.selected)
                                && let Some(mut assignment) = self.pending.clone()
                            {
                                if self.editing_fallback {
                                    if let Some(fallback) = &mut assignment.fallback {
                                        fallback.effort = effort;
                                    }
                                } else {
                                    assignment.effort = effort;
                                }
                                return Action::Save(self.role(), Some(assignment));
                            }
                        }
                    },
                    KeyCode::Char('f') if self.view == View::Roles && !self.sidebar_focus => {
                        self.editing_fallback = true;
                        self.begin_models();
                    }
                    KeyCode::Char('x')
                        if self.view == View::Roles
                            && !self.sidebar_focus
                            && key.kind == KeyEventKind::Press =>
                    {
                        return self.clear_selected();
                    }
                    KeyCode::Char('e' | 't') if self.view == View::Roles && !self.sidebar_focus => {
                        self.editing_fallback = self.on_fallback();
                        self.begin_effort(self.selected_model().model)
                    }
                    KeyCode::Backspace
                        if self.view == View::Roles
                            && !self.sidebar_focus
                            && key.kind == KeyEventKind::Press
                            && self.override_selection.is_some() =>
                    {
                        return Action::ClearOverride;
                    }
                    KeyCode::Delete
                        if self.view == View::Roles
                            && !self.sidebar_focus
                            && key.kind == KeyEventKind::Press =>
                    {
                        return Action::Save(self.role(), None);
                    }
                    KeyCode::Backspace if self.view == View::Models && !self.sidebar_focus => {
                        self.query.pop();
                        self.selected = 0;
                    }
                    KeyCode::Char(c) if self.view == View::Models && !self.sidebar_focus => {
                        self.query.push(c);
                        self.selected = 0;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Action::None
    }
}
