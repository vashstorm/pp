//! `Binding` 构造、模型／角色绑定与目录访问。

use anyhow::{Result, ensure};
use pp_config::config::{ModelRole, ModelRoles};
use pp_core::model::ThinkLevel;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, atomic::AtomicU64},
};
use tokio::sync::Semaphore;

use super::catalog::{self, Definition};
use super::{
    Binding, ChangeSink, Disk, Factory, Inner, McpTools, RootOptions, State, Validator, lock, now,
};

impl Binding {
    pub(crate) fn root(
        options: RootOptions,
        factory: Arc<Factory>,
        validate: Arc<Validator>,
    ) -> Result<Self> {
        let RootOptions {
            dir,
            user,
            cwd,
            model,
            thinking,
            settings,
            plan_mode,
        } = options;
        ensure!(
            (1..=32).contains(&settings.max_concurrency),
            "task.max_concurrency must be in 1..=32"
        );
        ensure!(
            settings.max_recursion_depth <= 8,
            "task.max_recursion_depth must not exceed 8"
        );
        let mut disk: Disk = if dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("pending-"))
        {
            Disk::default()
        } else {
            match fs::read(dir.join("registry.json")) {
                Ok(bytes) => serde_json::from_slice(&bytes)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Disk::default(),
                Err(error) => return Err(error.into()),
            }
        };
        ensure!(disk.version <= 1, "unrecognized subagent storage version");
        disk.version = 1;
        for agent in disk.agents.values_mut() {
            if agent.status != "aborted" {
                agent.status = "parked".into();
            }
        }
        for run in disk.runs.values_mut() {
            ensure!(
                matches!(
                    run.status.as_str(),
                    "queued" | "running" | "completed" | "failed" | "cancelled" | "interrupted"
                ),
                "unknown subagent task status"
            );
            if matches!(run.status.as_str(), "queued" | "running") {
                match fs::read(dir.join(format!("{}.result.json", run.id))) {
                    Ok(bytes) => {
                        let result: Value = serde_json::from_slice(&bytes)?;
                        ensure!(result["ok"].is_boolean(), "invalid subagent result file");
                        run.status = if result["cancelled"] == true {
                            "cancelled"
                        } else if result["ok"] == true {
                            "completed"
                        } else {
                            "failed"
                        }
                        .into();
                        run.result = Some(result);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        run.status = "interrupted".into();
                        run.result = Some(
                            json!({"ok":false,"error":"Process interrupted; explicit follow-up required to continue, tools will not be replayed."}),
                        );
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            ensure!(
                matches!(run.status.as_str(), "queued" | "running") || run.result.is_some(),
                "terminal subagent task missing result"
            );
        }
        let inner = Arc::new(Inner {
            revision: AtomicU64::new(1),
            appends: Mutex::new(BTreeMap::new()),
            observe_errors: Mutex::new(BTreeMap::new()),
            lsp_cleanup: Mutex::new(Vec::new()),
            cleanup_errors: Mutex::new(Vec::new()),
            output_previews: Mutex::new(BTreeMap::new()),
            pipeline_previews: Mutex::new(BTreeMap::new()),
            shell_previews: Mutex::new(BTreeMap::new()),
            runtime_models: Mutex::new(None),
            model_roles: Mutex::new(Default::default()),
            accounting: Mutex::default(),
            state: Mutex::new(State {
                disk,
                live: BTreeMap::new(),
                workers: 0,
                retiring: Default::default(),
                stopping: false,
            }),
            changed: Condvar::new(),
            semaphore: Arc::new(Semaphore::new(settings.max_concurrency)),
            settings,
            dir: Mutex::new(dir),
            user,
            factory: Mutex::new(Some(factory)),
            validate,
            changes: Mutex::new(None),
            mcp_tools: Mutex::default(),
            plan_mode,
        });
        Ok(Self {
            inner,
            owner: "Main".into(),
            depth: 0,
            spawns: vec!["*".into()],
            model,
            thinking,
            cwd,
            readonly: false,
        })
    }
    pub(crate) fn plan_mode(&self) -> bool {
        self.inner.plan_mode
    }
    pub(crate) fn set_runtime_models(&self, binding: Option<crate::model_control::Binding>) {
        *lock(&self.inner.runtime_models) = binding;
    }
    pub(crate) fn update_model(
        &self,
        model: &str,
        thinking: ThinkLevel,
        cost: Option<pp_config::models::ModelCost>,
    ) -> Result<()> {
        let mut state = lock(&self.inner.state);
        let previous = state.disk.agents.get(&self.owner).cloned();
        if let Some(record) = state.disk.agents.get_mut(&self.owner) {
            record.spec.model = model.into();
            record.spec.thinking = thinking;
            record.spec.cost = cost;
            record.updated_ms = now();
        }
        if let Err(error) = self.inner.save(&state) {
            if let Some(previous) = previous {
                state.disk.agents.insert(self.owner.clone(), previous);
            }
            return Err(error);
        }
        Ok(())
    }
    pub(crate) fn set_model_roles(&self, roles: pp_config::config::ModelRoles) {
        *lock(&self.inner.model_roles) = roles;
    }
    pub(super) fn model_roles(&self) -> ModelRoles {
        lock(&self.inner.runtime_models)
            .as_ref()
            .and_then(|binding| binding.registry.catalog())
            .map(|catalog| catalog.subagent_roles())
            .unwrap_or_else(|| lock(&self.inner.model_roles).clone())
    }
    pub(crate) fn allow_mcp(&self, all: Vec<String>, read_only: Vec<String>) {
        *lock(&self.inner.mcp_tools) = McpTools { all, read_only };
    }
    pub(crate) fn track_changes(
        &self,
        root: PathBuf,
        tracker: Arc<Mutex<crate::tools::changes::TaskChanges>>,
        serial: Arc<Mutex<()>>,
    ) {
        *lock(&self.inner.changes) = Some(ChangeSink {
            root,
            tracker,
            serial,
        });
    }
    pub(crate) fn set_accounting(&self, ledger: Option<pp_config::task_status::Ledger>) {
        if self.depth == 0 {
            *lock(&self.inner.accounting) = ledger;
        }
    }
    pub(crate) fn directory(&self) -> PathBuf {
        self.inner.directory()
    }
    pub(crate) fn bind_session(&self, dir: &Path, id: &str) {
        if self.depth == 0 {
            let state = lock(&self.inner.state);
            if state.disk.agents.is_empty() {
                *lock(&self.inner.dir) = dir.join(format!("{id}.subagents"));
            }
        }
    }
    pub(super) fn custom_roles(&self) -> Vec<Definition> {
        catalog::discover(&self.cwd, &self.inner.user, &self.inner.settings)
    }
    pub(super) fn available_roles<'a>(
        &'a self,
        roles: &'a ModelRoles,
        custom: &'a [Definition],
    ) -> impl Iterator<Item = (&'a Definition, Option<ModelRole>)> + 'a {
        catalog::task_roles()
            .filter(|(role, _)| roles.get(role).is_some_and(|assignment| assignment.enabled))
            .map(|(role, definition)| (definition, Some(role)))
            .chain(custom.iter().map(|definition| (definition, None)))
            .filter(|(definition, _)| {
                self.spawns
                    .iter()
                    .any(|name| name == "*" || name == &definition.name)
            })
    }
}
