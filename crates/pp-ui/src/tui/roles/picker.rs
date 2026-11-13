//! 模型／档位选择器的过滤、进入与光标移动。

use super::*;

impl Panel {
    pub(super) fn model_indices(&self) -> Vec<usize> {
        let provider = self
            .sidebar
            .checked_sub(2)
            .and_then(|index| self.providers.get(index));
        let query = self.query.to_lowercase();
        let mut indices: Vec<_> = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                provider.is_none_or(|p| {
                    choice
                        .reference
                        .split_once('/')
                        .is_some_and(|(id, _)| id == p)
                }) && (choice.reference.to_lowercase().contains(&query)
                    || choice.model.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        // 稳定排序：收藏模型优先，同组保持模型目录中的顺序。
        indices.sort_by_key(|&index| !self.favorites.contains(&self.choices[index].reference));
        indices
    }

    pub(super) fn begin_models(&mut self) {
        let selected = self.selected_model().model;
        self.sidebar = 1;
        self.sidebar_focus = false;
        self.view = View::Models;
        self.query.clear();
        self.selected = self
            .model_indices()
            .iter()
            .position(|&index| self.choices[index].reference == selected)
            .unwrap_or(0);
        self.scroll = 0;
        self.message.clear();
    }

    pub(super) fn begin_effort(&mut self, model: String) {
        let Some(choice) = self.choices.iter().find(|choice| choice.reference == model) else {
            self.message = "Model is no longer available; select a model again".into();
            return;
        };
        let old = self.selected_model().effort;
        let effort = if choice.thinking.supports(old) {
            old
        } else {
            choice.thinking.effective_default(self.default_effort)
        };
        self.selected = choice
            .thinking
            .levels()
            .position(|level| level == effort)
            .unwrap_or(0);
        let mut assignment = self
            .roles
            .get(&self.role())
            .cloned()
            .unwrap_or(RoleAssignment {
                enabled: !self.editing_fallback,
                model: String::new(),
                effort: self.default_effort,
                fallback: None,
            });
        if self.editing_fallback {
            assignment.fallback = Some(RoleModel { model, effort });
        } else {
            assignment.model = model;
            assignment.effort = effort;
        }
        self.pending = Some(assignment);
        self.view = View::Effort;
        self.sidebar_focus = false;
        self.scroll = 0;
        self.message.clear();
    }

    pub(super) fn efforts(&self) -> Vec<ThinkLevel> {
        self.pending
            .as_ref()
            .and_then(|assignment| {
                self.choices.iter().find(|choice| {
                    choice.reference
                        == if self.editing_fallback {
                            assignment
                                .fallback
                                .as_ref()
                                .map_or("", |f| f.model.as_str())
                        } else {
                            assignment.model.as_str()
                        }
                })
            })
            .map(|choice| choice.thinking.levels().collect())
            .unwrap_or_default()
    }

    pub(super) fn activate_sidebar(&mut self) {
        if self.view == View::Roles {
            self.editing_fallback = self.on_fallback();
        }
        self.view = if self.sidebar == 0 {
            View::Roles
        } else {
            View::Models
        };
        self.query.clear();
        self.selected = 0;
        self.scroll = 0;
        self.pending = None;
    }

    pub(super) fn move_selection(&mut self, down: bool, amount: usize) {
        let len = if self.sidebar_focus {
            self.providers.len() + 2
        } else {
            match self.view {
                View::Roles => self.rows().len(),
                View::Models => self.model_indices().len(),
                View::Effort => self.efforts().len(),
            }
        };
        let selected = if self.sidebar_focus {
            &mut self.sidebar
        } else if self.view == View::Roles {
            &mut self.role_index
        } else {
            &mut self.selected
        };
        *selected = if down {
            selected.saturating_add(amount).min(len.saturating_sub(1))
        } else {
            selected.saturating_sub(amount)
        };
        if self.sidebar_focus {
            self.activate_sidebar();
        }
    }
}
