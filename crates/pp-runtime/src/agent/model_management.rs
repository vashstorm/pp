//! 模型目录、思考档位与角色绑定的只读查询和保存入口。
use super::Agent;
use anyhow::{Result, bail};
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::config::ModelRole;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use std::collections::BTreeMap;

impl Agent {
    pub fn reload_models(&mut self) -> Result<()> {
        self.stop_subagents()?;
        self.models.reload_models()?;
        self.catalog = self.models.catalog_snapshot();
        if let Some(binding) = &self.tools.subagent {
            binding.set_model_roles(self.catalog.subagent_roles());
        }
        if self.task_status.accounting.is_some() {
            self.continue_task_status();
        }
        Ok(())
    }

    pub fn model_choices(&self) -> Vec<ModelChoice> {
        self.catalog.choices()
    }

    pub fn current_model(&self) -> &ModelChoice {
        &self.current_model
    }

    pub fn think_level(&self) -> ThinkLevel {
        self.think_level
    }

    pub fn default_think_level(&self) -> ThinkLevel {
        self.models.default_think_level()
    }

    /// 保存全局默认档位（写 `b`），不切换运行中的会话。
    pub fn save_default_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        self.models.save_default_think_level(level)?;
        self.catalog = self.models.catalog_snapshot();
        Ok(())
    }

    pub fn default_model_reference(&self) -> String {
        self.models
            .catalog_snapshot()
            .default_reference()
            .to_owned()
    }

    /// 保存全局默认模型（写 `b`），不切换运行中的会话。
    pub fn save_default_model(&mut self, reference: &str) -> Result<()> {
        self.models.set_default(reference)?;
        self.catalog = self.models.catalog_snapshot();
        Ok(())
    }

    pub fn model_roles(&self) -> pp_config::config::ModelRoles {
        self.models.model_roles()
    }

    pub fn save_model_role(
        &mut self,
        role: ModelRole,
        assignment: Option<pp_config::config::RoleAssignment>,
    ) -> Result<()> {
        self.models.set_role(role, assignment)?;
        self.catalog = self.models.catalog_snapshot();
        if let Some(binding) = &self.tools.subagent {
            binding.set_model_roles(self.catalog.subagent_roles());
        }
        Ok(())
    }

    pub fn favorite_models(&self) -> Vec<String> {
        self.models.favorite_models()
    }

    pub fn toggle_favorite_model(&mut self, reference: &str) -> Result<Vec<String>> {
        let favorites = self.models.toggle_favorite_model(reference)?;
        self.catalog = self.models.catalog_snapshot();
        Ok(favorites)
    }

    /// 实际尝试发送的逻辑模型请求累计次数；读取失败时不得伪装成零。
    pub fn model_usage(&self) -> Result<BTreeMap<String, u64>> {
        self.models.model_usage()
    }

    pub fn set_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        if !self.current_model.thinking.supports(level) {
            let mut message =
                String::from("thinking level is not supported by the current model; options: ");
            for (index, allowed) in self.current_model.thinking.levels().enumerate() {
                if index != 0 {
                    message.push_str(", ");
                }
                message.push_str(allowed.as_str());
            }
            bail!(message);
        }
        self.journal.think(level)?;
        self.session.set_think_level(level);
        self.think_level = level;
        if let Some(route) = &mut self.role_route {
            if self.current_model.reference == route.primary.model {
                route.primary.effort = level;
            } else if let Some(fallback) = &mut route.fallback {
                fallback.effort = level;
            }
        }
        if self.role_route.is_some() {
            self.checkpoint_route()?;
        }
        if self.model_source == ModelSource::Override {
            self.models.set_override_effort(level)?;
        }
        Ok(())
    }

    pub fn model_catalog(&self) -> ModelCatalog {
        self.catalog.clone()
    }

    /// 当前生效模型的来源（覆盖/默认/角色/fallback）。
    pub fn model_source(&self) -> pp_core::model::ModelSource {
        self.model_source
    }
}
