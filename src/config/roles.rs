use super::*;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ModelRole {
    Default,
    Fast,
    Slow,
    Vision,
    Plan,
    Review,
}

impl ModelRole {
    pub(crate) const ALL: [Self; 6] = [
        Self::Default,
        Self::Fast,
        Self::Slow,
        Self::Vision,
        Self::Plan,
        Self::Review,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Default => "DEFAULT",
            Self::Fast => "FAST",
            Self::Slow => "SLOW",
            Self::Vision => "VISION",
            Self::Plan => "PLAN",
            Self::Review => "REVIEW",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoleAssignment {
    #[serde(default = "role_enabled_by_default")]
    pub(crate) enabled: bool,
    #[serde(default)]
    pub(crate) model: String,
    #[serde(default)]
    pub(crate) effort: ThinkLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fallback: Option<RoleModel>,
}

/// 单个模型及其独立思考档位；空主模型在数据库中表示为 NULL。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoleModel {
    pub(crate) model: String,
    pub(crate) effort: ThinkLevel,
}

/// 记录角色来源与任务开始时的主备选择，不能由实际模型名称反推。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoleRoute {
    pub(crate) role: ModelRole,
    pub(crate) primary: RoleModel,
    pub(crate) fallback: Option<RoleModel>,
}

impl RoleAssignment {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.enabled || !self.model.is_empty(),
            "role primary model is required before enabling"
        );
        for model in std::iter::once(self.model.as_str())
            .filter(|m| !m.is_empty())
            .chain(self.fallback.iter().map(|m| m.model.as_str()))
        {
            ensure!(
                model
                    .split_once('/')
                    .is_some_and(|(p, m)| !p.trim().is_empty() && !m.trim().is_empty()),
                "model_roles model must use provider/model"
            );
        }
        ensure!(
            self.fallback.as_ref().is_none_or(|f| f.model != self.model),
            "role primary and fallback must differ"
        );
        Ok(())
    }
}

fn role_enabled_by_default() -> bool {
    true
}

pub(crate) type ModelRoles = BTreeMap<ModelRole, RoleAssignment>;

impl ModelCatalog {
    pub(crate) fn role_route(&self, role: ModelRole) -> Option<RoleRoute> {
        let assignment = self
            .model_roles()
            .get(&role)
            .filter(|a| a.enabled && !a.model.is_empty())?;
        Some(RoleRoute {
            role,
            primary: RoleModel {
                model: assignment.model.clone(),
                effort: assignment.effort,
            },
            fallback: assignment.fallback.clone(),
        })
    }

    pub(crate) fn model_roles(&self) -> &ModelRoles {
        &self.preferences.model_roles
    }

    /// 未配置或未选中的角色保持调用方模型；显式配置不可用时不能悄悄换模型。
    pub(crate) fn role_selection(
        &self,
        role: ModelRole,
        model: &str,
        effort: ThinkLevel,
    ) -> Result<RoleAssignment> {
        let assignment = self
            .preferences
            .model_roles
            .get(&role)
            .filter(|assignment| assignment.enabled)
            .cloned()
            .unwrap_or_else(|| RoleAssignment {
                fallback: None,
                enabled: false,
                model: model.to_owned(),
                effort,
            });
        let config = self.resolve(&assignment.model)?;
        ensure!(
            config.thinking.supports(assignment.effort),
            "{} role effort is not supported by its model",
            role.name()
        );
        Ok(assignment)
    }

    pub(crate) fn save_model_role(
        &mut self,
        role: ModelRole,
        assignment: Option<RoleAssignment>,
    ) -> Result<()> {
        if let Some(assignment) = &assignment {
            assignment.validate()?;
            if assignment.enabled {
                for (model, effort) in std::iter::once((&assignment.model, assignment.effort))
                    .chain(assignment.fallback.iter().map(|f| (&f.model, f.effort)))
                {
                    let config = self.resolve(model)?;
                    ensure!(
                        config.thinking.supports(effort),
                        "role effort is not supported by the selected model"
                    );
                }
            }
        }
        let mut store = crate::agent_store::AgentStore::open(&self.agent_db_path)?;
        let preferences = store.set_model_role(role, assignment.as_ref())?;
        self.preferences = preferences;
        if role == ModelRole::Default {
            self.default_reference = default_reference(&self.providers, &self.preferences);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, ModelCatalog) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("models.yml"),
            r#"providers:
  fixture:
    api_key: fixture-key
    models:
      one: {model: one}
      two: {model: two, thinking: {efforts: [low, high], defaultLevel: low}}
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("settings.yml"),
            "# 保留其他设置\ndefault_model: fixture/one\nedit: hashline\n",
        )
        .unwrap();
        let catalog = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        (dir, catalog)
    }

    fn assignment(model: &str, effort: ThinkLevel) -> RoleAssignment {
        RoleAssignment {
            fallback: None,
            enabled: true,
            model: model.into(),
            effort,
        }
    }

    #[test]
    fn model_roles_persist_independent_model_and_effort_and_preserve_settings() {
        let (dir, mut catalog) = fixture();
        catalog
            .save_model_role(
                ModelRole::Fast,
                Some(assignment("fixture/two", ThinkLevel::Low)),
            )
            .unwrap();
        catalog
            .save_model_role(
                ModelRole::Review,
                Some(assignment("fixture/one", ThinkLevel::High)),
            )
            .unwrap();
        let mut restored = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        assert_eq!(
            restored
                .role_selection(ModelRole::Fast, "fixture/one", ThinkLevel::High)
                .unwrap(),
            assignment("fixture/two", ThinkLevel::Low)
        );
        assert_eq!(
            restored
                .role_selection(ModelRole::Review, "fixture/two", ThinkLevel::Low)
                .unwrap(),
            assignment("fixture/one", ThinkLevel::High)
        );
        assert_eq!(restored.default_reference(), "fixture/one");
        restored.save_model_role(ModelRole::Fast, None).unwrap();
        let mut fallback = assignment("fixture/one", ThinkLevel::Medium);
        fallback.enabled = false;
        assert_eq!(
            restored
                .role_selection(ModelRole::Fast, "fixture/one", ThinkLevel::Medium)
                .unwrap(),
            fallback
        );
        assert!(restored.model_roles().contains_key(&ModelRole::Review));
        let text = fs::read_to_string(dir.path().join("settings.yml")).unwrap();
        assert!(text.contains("# 保留其他设置") && text.contains("edit: hashline"));
    }

    #[test]
    fn model_roles_invalid_effort_or_save_failure_never_changes_committed_state() {
        let (dir, mut catalog) = fixture();
        let value = assignment("fixture/two", ThinkLevel::High);
        catalog
            .save_model_role(ModelRole::Plan, Some(value.clone()))
            .unwrap();
        for invalid in [
            assignment("fixture/two", ThinkLevel::Max),
            assignment("removed/model", ThinkLevel::Low),
        ] {
            assert!(
                catalog
                    .save_model_role(ModelRole::Plan, Some(invalid))
                    .is_err()
            );
        }
        // 禁用的 Role 同样保持复合引用约束。
        let mut malformed = assignment("garbage", ThinkLevel::Low);
        malformed.enabled = false;
        assert!(
            catalog
                .save_model_role(ModelRole::Plan, Some(malformed))
                .is_err()
        );
        assert_eq!(catalog.model_roles()[&ModelRole::Plan], value);
        // agent.db 不可用时保存失败，catalog 内存快照保持已提交状态。
        let agent_db = dir.path().join("agent.db");
        fs::remove_file(&agent_db).unwrap();
        fs::create_dir(&agent_db).unwrap();
        assert!(catalog.save_model_role(ModelRole::Plan, None).is_err());
        assert_eq!(catalog.model_roles()[&ModelRole::Plan], value);
    }

    #[test]
    fn model_roles_default_and_legacy_default_shortcuts_share_one_selection() {
        let (dir, mut catalog) = fixture();
        catalog
            .save_model_role(
                ModelRole::Default,
                Some(assignment("fixture/two", ThinkLevel::High)),
            )
            .unwrap();
        assert_eq!(catalog.default_reference(), "fixture/two");
        assert_eq!(catalog.default_think_level(), ThinkLevel::High);
        catalog.save_default_think_level(ThinkLevel::Low).unwrap();
        assert_eq!(
            catalog.model_roles()[&ModelRole::Default].effort,
            ThinkLevel::Low
        );
        catalog.save_default_model("fixture/one").unwrap();
        assert_eq!(
            catalog.model_roles()[&ModelRole::Default].model,
            "fixture/one"
        );
        let restored = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        assert_eq!(restored.default_reference(), "fixture/one");
        assert_eq!(restored.default_think_level(), ThinkLevel::Low);
    }

    #[test]
    fn model_roles_reject_unknown_names_and_malformed_assignments() {
        for input in [
            "model_roles: {smol: {model: fixture/one, effort: low}}",
            "model_roles: {fast: {model: one, effort: low}}",
            "model_roles: {fast: {model: fixture/one, effort: turbo}}",
            "model_roles: {fast: {model: fixture/one, unknown: true}}",
            "model_roles: null",
        ] {
            assert!(
                yaml::parse::<Settings>(input, "settings")
                    .and_then(|settings| settings.validate())
                    .is_err(),
                "{input}"
            );
        }
        for role in ModelRole::ALL {
            assert_eq!(
                serde_json::from_str::<ModelRole>(&format!("\"{}\"", role.name().to_lowercase()))
                    .unwrap(),
                role
            );
        }
    }

    #[test]
    fn model_roles_deselection_preserves_config_and_stops_default_and_automatic_routing() {
        let (dir, mut catalog) = fixture();
        let active = assignment("fixture/two", ThinkLevel::High);
        catalog
            .save_model_role(ModelRole::Default, Some(active.clone()))
            .unwrap();
        let mut inactive = active;
        inactive.enabled = false;
        for role in [ModelRole::Default, ModelRole::Plan, ModelRole::Review] {
            catalog
                .save_model_role(role, Some(inactive.clone()))
                .unwrap();
        }
        assert_eq!(catalog.default_reference(), "fixture/one");
        assert_eq!(catalog.default_think_level(), ThinkLevel::None);
        catalog.reload_models().unwrap();
        for role in [ModelRole::Default, ModelRole::Plan, ModelRole::Review] {
            assert!(!catalog.model_roles()[&role].enabled);
            assert_eq!(catalog.model_roles()[&role].model, "fixture/two");
            let selected = catalog
                .role_selection(role, "fixture/one", ThinkLevel::Low)
                .unwrap();
            assert_eq!(selected.model, "fixture/one");
            assert_eq!(selected.effort, ThinkLevel::Low);
        }
        let restored = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        for role in [ModelRole::Default, ModelRole::Plan, ModelRole::Review] {
            assert!(!restored.model_roles()[&role].enabled);
            assert_eq!(restored.model_roles()[&role].model, "fixture/two");
        }
    }
    #[test]
    fn fallback_roles_roundtrip_missing_primary_and_validation_are_atomic() {
        let (dir, mut catalog) = fixture();
        let mut value = assignment("fixture/two", ThinkLevel::High);
        value.fallback = Some(RoleModel {
            model: "fixture/one".into(),
            effort: ThinkLevel::Low,
        });
        catalog
            .save_model_role(ModelRole::Fast, Some(value.clone()))
            .unwrap();
        let restored = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        assert_eq!(restored.model_roles()[&ModelRole::Fast], value);
        let mut invalid = value.clone();
        invalid.fallback.as_mut().unwrap().model = invalid.model.clone();
        assert!(
            catalog
                .save_model_role(ModelRole::Fast, Some(invalid))
                .is_err()
        );
        assert_eq!(catalog.model_roles()[&ModelRole::Fast], value);
        value.model.clear();
        assert!(
            catalog
                .save_model_role(ModelRole::Fast, Some(value.clone()))
                .is_err()
        );
        value.enabled = false;
        catalog
            .save_model_role(ModelRole::Fast, Some(value.clone()))
            .unwrap();
        assert!(catalog.role_route(ModelRole::Fast).is_none());
        catalog.reload_models().unwrap();
        assert_eq!(catalog.model_roles()[&ModelRole::Fast], value);
        value.model = "fixture/two".into();
        catalog
            .save_model_role(ModelRole::Fast, Some(value))
            .unwrap();
        assert!(!catalog.model_roles()[&ModelRole::Fast].enabled);
    }
}
