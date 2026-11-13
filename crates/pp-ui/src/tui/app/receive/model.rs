//! 模型目录与路由事件：重载快照、切换、默认值保存与角色保存。

use crate::tui::{App, Level, Selection, State, WorkerEvent};

impl App {
    /// 模型事件族；`ModelState` 只在修订号命中时置脏。
    pub(super) fn on_model_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::ModelsReloaded(result) => {
                self.model_refresh_pending = false;
                match result {
                    Ok(listing) => {
                        self.choices = listing.choices;
                        self.default_model_reference = Some(listing.default_reference);
                        self.favorite_models = listing.favorites;
                        self.model_usage = listing.usage;
                        self.model_roles = listing.roles;
                        self.default_think_level = listing.default_effort;
                        self.current_source = Some(listing.source);
                        self.override_selection = listing.override_selection;
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_choices(self.choices.clone());
                            panel.update_favorites(self.favorite_models.clone());
                            panel.update_override(self.override_selection.clone());
                        }
                        self.refresh_candidates(false);
                    }
                    Err(error) => {
                        self.notify(Level::Error, format!("Could not refresh models: {error:#}"))
                    }
                }
                changed = true;
            }

            WorkerEvent::ModelSwitched(result) => {
                match result {
                    Ok((choice, level)) => {
                        self.current_model = Some(choice);
                        self.think_level = level;
                        // 容量随模型变化；等待紧随的新快照，不显示旧分母。
                        self.usage = None;
                        self.token_speed.clear();
                    }
                    Err(error) => self.notify(Level::Error, format!("{error:#}")),
                }
                self.state = State::Ready;
                changed = true;
            }

            WorkerEvent::ModelState {
                revision,
                model,
                effort,
                source,
            } => {
                if revision >= self.model_revision {
                    self.model_revision = revision;
                    self.current_model = Some(model.clone());
                    self.think_level = effort;
                    self.current_source = Some(source);
                    self.override_selection = match source {
                        pp_core::model::ModelSource::Override => Some(Selection {
                            reference: model.reference,
                            effort,
                        }),
                        pp_core::model::ModelSource::Default => None,
                        // 角色/fallback 只覆盖当前路由，不改变手动覆盖槽。
                        pp_core::model::ModelSource::Role
                        | pp_core::model::ModelSource::Fallback => self.override_selection.clone(),
                    };
                    if let Some(panel) = &mut self.roles_panel {
                        panel.update_override(self.override_selection.clone());
                    }
                    changed = true;
                }
            }

            WorkerEvent::ModelRolesSaved(result) => {
                if let Some(panel) = &mut self.roles_panel {
                    panel.busy = false;
                }
                match result {
                    Ok((roles, default, effort)) => {
                        self.model_roles = roles;
                        self.default_model_reference = Some(default);
                        self.default_think_level = effort;
                        if let Some(panel) = &mut self.roles_panel {
                            panel.saved();
                        }
                        self.notify(Level::Normal, "Role configuration saved".into());
                    }
                    Err(error) => {
                        self.notify(Level::Error, format!("Failed to save role: {error:#}"))
                    }
                }
                self.state = State::Ready;
                changed = true;
            }

            WorkerEvent::DefaultThinkLevelSaved(result) => {
                match result {
                    Ok(level) => {
                        self.default_think_level = level;
                        self.notify(
                            Level::Normal,
                            format!(
                                "Default thinking effort saved: {} (takes effect next launch)",
                                level.label()
                            ),
                        );
                    }
                    Err(error) => self.notify(
                        Level::Error,
                        format!("Failed to save default thinking effort: {error:#}"),
                    ),
                }
                self.state = State::Ready;
                changed = true;
            }

            WorkerEvent::FavoriteModelsSaved(result) => {
                if let Some(panel) = &mut self.roles_panel {
                    panel.busy = false;
                }
                match result {
                    Ok(favorites) => {
                        self.favorite_models = favorites;
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_favorites(self.favorite_models.clone());
                        }
                    }
                    Err(error) => self.notify(
                        Level::Error,
                        format!("Failed to save favorite models: {error:#}"),
                    ),
                }
                self.state = State::Ready;
                changed = true;
            }

            WorkerEvent::DefaultModelSaved(result) => {
                match result {
                    Ok(reference) => {
                        self.notify(
                            Level::Normal,
                            format!("Default model saved: {reference} (takes effect next launch)"),
                        );
                        self.default_model_reference = Some(reference);
                    }
                    Err(error) => self.notify(
                        Level::Error,
                        format!("Failed to save default model: {error:#}"),
                    ),
                }
                self.state = State::Ready;
                changed = true;
            }
            _ => {}
        }
        changed
    }
}
