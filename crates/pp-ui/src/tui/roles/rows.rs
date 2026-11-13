//! 角色行枚举与当前行的读模型。

use super::*;

impl Panel {
    pub(super) fn rows(&self) -> Vec<(ModelRole, bool)> {
        ModelRole::ALL
            .into_iter()
            .flat_map(|role| {
                std::iter::once((role, false)).chain(
                    self.roles
                        .get(&role)
                        .and_then(|a| a.fallback.as_ref())
                        .map(|_| (role, true)),
                )
            })
            .collect()
    }

    pub(super) fn role(&self) -> ModelRole {
        self.rows()
            .get(self.role_index)
            .map_or(ModelRole::Default, |r| r.0)
    }

    pub(super) fn on_fallback(&self) -> bool {
        self.rows().get(self.role_index).is_some_and(|r| r.1)
    }

    pub(super) fn selected_model(&self) -> RoleModel {
        let assignment = self.effective();
        if self.editing_fallback {
            assignment.fallback.unwrap_or(RoleModel {
                model: String::new(),
                effort: self.default_effort,
            })
        } else {
            RoleModel {
                model: assignment.model,
                effort: assignment.effort,
            }
        }
    }

    pub(super) fn clear_selected(&self) -> Action {
        let Some(mut assignment) = self.roles.get(&self.role()).cloned() else {
            return Action::None;
        };
        if self.on_fallback() {
            assignment.fallback = None;
        } else {
            assignment.model.clear();
            assignment.enabled = false;
        }
        let assignment =
            (!assignment.model.is_empty() || assignment.fallback.is_some()).then_some(assignment);
        Action::Save(self.role(), assignment)
    }

    pub(super) fn content_offset(&self) -> u16 {
        // Roles 视图在角色行之前固定保留一行只读 Override 信息。
        2
    }

    fn effective(&self) -> RoleAssignment {
        self.roles
            .get(&self.role())
            .cloned()
            .unwrap_or_else(|| RoleAssignment {
                fallback: None,
                enabled: false,
                model: self.default_model.clone(),
                effort: self.default_effort,
            })
    }

    pub(super) fn toggle_role(&self) -> Action {
        let mut assignment = self.effective();
        if assignment.model.is_empty() {
            return Action::None;
        }
        assignment.enabled = !assignment.enabled;
        Action::Save(self.role(), Some(assignment))
    }
}
