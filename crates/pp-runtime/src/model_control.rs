//! Configured intent and actual effective state are separated; each agent takes its target only at commit boundaries.
//!
//! The only model resolution authority is [`ModelState`] (`a` manual override → `b` configured default). This module only:
//! serializes UI/CLI intent into `ModelState` writes and publishes "targets awaiting application"
//! to the root agent and subagents by revision; the read side always goes back to `ModelState::resolve()`.
use anyhow::Result;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::config::ModelRole;
use pp_config::config::ModelRoles;
use pp_config::config::RoleAssignment;
use pp_config::config::RoleRoute;
use pp_config::model_state::ModelState;
use pp_config::model_state::Selection;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

/// Model target awaiting application by the root agent or a subagent.
#[derive(Clone)]
pub struct Target {
    pub revision: u64,
    /// `None`: no manual override, follow the configured default.
    pub selection: Option<Selection>,
    pub source: ModelSource,
    pub(crate) plan_route: Option<RoleRoute>,
}

/// Manual thinking-level target for the main session; only the root agent takes it, and no config file is written.
#[derive(Clone)]
pub struct ThinkTarget {
    pub revision: u64,
    pub effort: ThinkLevel,
}

#[derive(Default)]
struct State {
    revision: u64,
    catalog: Option<Arc<ModelCatalog>>,
    /// Manual override target for the root agent; `selection == None` clears the override and follows the default.
    main: Option<Target>,
    /// PLAN is taken by the root agent while planning; other roles are taken by their bound subagents.
    roles: BTreeMap<ModelRole, Target>,
    /// Written only by `Change::Think`; consumed by the root agent only.
    think: Option<ThinkTarget>,
    reports: VecDeque<Report>,
    applied: BTreeMap<String, (u64, bool)>,
}

#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<State>>);

pub enum Change {
    /// Root agent manual selection (⌥P, `/model`, `--model`): writes `a`.
    Override(Selection),
    /// Clears the manual override and returns to the configured default.
    ClearOverride,
    /// Saves the global default model: writes `b` without switching the running session.
    SaveDefault(String),
    /// Role target, writes the `roles` slot only (DEFAULT goes through `b` and never enters `roles`).
    Role(ModelRole, Option<RoleAssignment>),
    /// Manual level for the main session, writes the `think` slot only; never reads or writes config files.
    Think(ThinkLevel),
    #[cfg(test)]
    Barrier(mpsc::Sender<()>),
}

pub enum Report {
    Cleared {
        role: ModelRole,
        revision: u64,
    },
    Queued {
        role: ModelRole,
        revision: u64,
        selection: Selection,
        source: ModelSource,
    },
    Saved {
        roles: ModelRoles,
        default: String,
        effort: ThinkLevel,
    },
    Applied {
        agent: Option<String>,
        revision: u64,
        model: ModelChoice,
        effort: ThinkLevel,
        source: ModelSource,
    },
    Failed {
        agent: Option<String>,
        revision: u64,
        message: String,
    },
    ThinkQueued {
        revision: u64,
        effort: ThinkLevel,
    },
    ThinkApplied {
        revision: u64,
        effort: ThinkLevel,
    },
    ThinkFailed {
        revision: u64,
        effort: ThinkLevel,
        message: String,
    },
}

#[derive(Clone)]
pub struct Binding {
    pub registry: Registry,
    pub role: Option<ModelRole>,
    pub agent: Option<String>,
    pub seen: u64,
    pub seen_think: u64,
}

impl Binding {
    /// Only checks for targets awaiting application, keeping the current take progress.
    pub fn pending(&self, follow_plan: bool) -> bool {
        let mut probe = self.clone();
        probe.take_targets(follow_plan).0.is_some()
    }

    pub fn main(registry: Registry) -> Self {
        Self {
            registry,
            role: None,
            agent: None,
            seen: 0,
            seen_think: 0,
        }
    }

    pub fn child(&self, role: Option<ModelRole>, agent: String) -> Self {
        Self {
            registry: self.registry.clone(),
            role,
            agent: Some(agent),
            seen: 0,
            seen_think: 0,
        }
    }

    /// Takes both slots under a single lock so the two revisions are comparable within the same snapshot.
    pub fn take_targets(&mut self, follow_plan: bool) -> (Option<Target>, Option<ThinkTarget>) {
        let state = self.registry.0.lock().unwrap_or_else(|e| e.into_inner());
        // The root agent also takes the PLAN role target in plan mode without a manual override.
        let picked = if self.agent.is_none() {
            let plan = follow_plan
                .then(|| state.roles.get(&ModelRole::Plan))
                .flatten();
            state
                .main
                .iter()
                .chain(plan)
                .max_by_key(|target| target.revision)
        } else {
            self.role.and_then(|role| state.roles.get(&role))
        };
        let target = picked
            .filter(|target| target.revision > self.seen)
            .cloned()
            .inspect(|target| {
                self.seen = target.revision;
            });
        // The manual level is handed down to the root agent only; subagents follow the role effort.
        let think = if self.agent.is_none() {
            state
                .think
                .as_ref()
                .filter(|think| think.revision > self.seen_think)
                .cloned()
        } else {
            None
        };
        if let Some(think) = &think {
            self.seen_think = think.revision;
        }
        (target, think)
    }

    pub fn report(&self, report: Report) {
        self.registry.report(report);
    }
}

impl Registry {
    fn report(&self, report: Report) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match &report {
            Report::Applied {
                agent: Some(agent),
                revision,
                ..
            } => {
                state.applied.insert(agent.clone(), (*revision, true));
            }
            Report::Failed {
                agent: Some(agent),
                revision,
                ..
            } => {
                state.applied.insert(agent.clone(), (*revision, false));
            }
            _ => {}
        }
        state.reports.push_back(report);
    }

    pub fn describe(&self, agent: &str, role: Option<ModelRole>) -> Option<String> {
        let role = role?;
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let target = state.roles.get(&role)?;
        match state.applied.get(agent) {
            Some((revision, true)) if *revision >= target.revision => None,
            Some((revision, false)) if *revision >= target.revision => {
                Some(format!("{} change failed", role.name()))
            }
            _ => Some(format!(
                "→ {} (pending boundary)",
                target.selection.as_ref().map_or_else(
                    || role.name().to_owned(),
                    |selection| selection.reference.clone()
                )
            )),
        }
    }

    pub fn reports(&self) -> Vec<Report> {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let count = state.reports.len().min(64);
        state.reports.drain(..count).collect()
    }

    pub fn catalog(&self) -> Option<ModelCatalog> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .catalog
            .as_ref()
            .map(|c| (**c).clone())
    }
}

pub struct Service {
    sender: Option<mpsc::Sender<Change>>,
    pub registry: Registry,
    /// Read-only handle: the UI uses it to resolve a choice into a full `Selection`, while writes still go through the control thread.
    models: Arc<ModelState>,
    join: Option<JoinHandle<()>>,
}

impl Service {
    /// `models` is the only in-process model resolution authority; this thread is its only serial writer.
    pub fn start(models: Arc<ModelState>) -> std::io::Result<Self> {
        let registry = Registry::default();
        let state = registry.clone();
        {
            let mut inner = state.0.lock().unwrap_or_else(|e| e.into_inner());
            inner.catalog = Some(Arc::new(models.catalog_snapshot()));
        }
        let worker_models = Arc::clone(&models);
        let (sender, receiver) = mpsc::channel();
        let join = thread::Builder::new()
            .name("pp-model-control".into())
            .spawn(move || {
                while let Ok(change) = receiver.recv() {
                    #[cfg(test)]
                    if let Change::Barrier(reply) = change {
                        let _ = reply.send(());
                        continue;
                    }
                    let think_effort = match &change {
                        Change::Think(level) => Some(*level),
                        _ => None,
                    };
                    let result = apply(&worker_models, &state, change);
                    if let Err(error) = result {
                        let message = format!("{error:#}");
                        state.report(match think_effort {
                            Some(effort) => Report::ThinkFailed {
                                revision: 0,
                                effort,
                                message,
                            },
                            None => Report::Failed {
                                agent: None,
                                revision: 0,
                                message,
                            },
                        });
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            registry,
            models,
            join: Some(join),
        })
    }

    pub fn submit(&self, change: Change) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.send(change).is_ok())
    }

    /// Resolves a model choice into a full `Selection`, keeping the target level policy (explicit default → current level → effective default).
    pub fn resolve_reference(
        &self,
        reference: &str,
        current: Option<ThinkLevel>,
    ) -> Result<Selection> {
        self.models.resolve_reference(reference, current)
    }

    pub fn override_selection(&self) -> Option<Selection> {
        self.models.override_selection()
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let (sender, receiver) = mpsc::channel();
        assert!(self.submit(Change::Barrier(sender)));
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Applies one config change: after writing `ModelState`, publishes the target awaiting application and the report to the registry.
fn apply(models: &Arc<ModelState>, registry: &Registry, change: Change) -> Result<()> {
    match change {
        Change::Think(level) => {
            models.set_override_effort(level)?;
            let mut state = registry.0.lock().unwrap_or_else(|e| e.into_inner());
            state.catalog = Some(Arc::new(models.catalog_snapshot()));
            // The revision must increase monotonically within this registry: `ModelState`'s revision advances only when the override changes,
            // so a session without an override (for example changing the level of the default model) stays at 0 and the target is filtered out by `seen_think`.
            state.revision += 1;
            let revision = state.revision;
            state.think = Some(ThinkTarget {
                revision,
                effort: level,
            });
            state.reports.push_back(Report::ThinkQueued {
                revision,
                effort: level,
            });
            Ok(())
        }
        Change::Override(selection) => {
            models.set_override(selection)?;
            publish_override(models, registry);
            Ok(())
        }
        Change::ClearOverride => {
            models.clear_override()?;
            publish_override(models, registry);
            Ok(())
        }
        Change::SaveDefault(reference) => {
            models.set_default(&reference)?;
            let mut state = registry.0.lock().unwrap_or_else(|e| e.into_inner());
            state.catalog = Some(Arc::new(models.catalog_snapshot()));
            state.revision += 1;
            state.reports.push_back(Report::Saved {
                roles: models.model_roles(),
                default: models.catalog_snapshot().default_reference().to_owned(),
                effort: models.default_think_level(),
            });
            Ok(())
        }
        Change::Role(role, assignment) => {
            let roles = models.set_role(role, assignment.clone())?;
            let catalog = models.catalog_snapshot();
            let plan_route = (role == ModelRole::Plan)
                .then(|| catalog.role_route(role))
                .flatten();
            let mut state = registry.0.lock().unwrap_or_else(|e| e.into_inner());
            state.revision += 1;
            let revision = state.revision;
            state.reports.push_back(Report::Saved {
                roles,
                default: catalog.default_reference().to_owned(),
                effort: catalog.default_think_level(),
            });
            state.catalog = Some(Arc::new(catalog));
            // The DEFAULT role is `b`: change the configured default only, without switching the running main session.
            if role == ModelRole::Default {
                return Ok(());
            }
            match assignment.filter(|assignment| assignment.enabled) {
                Some(assignment) => {
                    let selection = Selection {
                        reference: assignment.model,
                        effort: assignment.effort,
                    };
                    state.roles.insert(
                        role,
                        Target {
                            revision,
                            selection: Some(selection.clone()),
                            source: ModelSource::Role,
                            plan_route,
                        },
                    );
                    state.reports.push_back(Report::Queued {
                        role,
                        revision,
                        selection,
                        source: ModelSource::Role,
                    });
                }
                None => {
                    state.roles.remove(&role);
                    state.reports.push_back(Report::Cleared { role, revision });
                }
            }
            Ok(())
        }
        #[cfg(test)]
        Change::Barrier(_) => unreachable!("barrier handled before applying"),
    }
}

fn publish_override(models: &Arc<ModelState>, registry: &Registry) {
    let selection = models.override_selection();
    let source = models.source();
    let mut state = registry.0.lock().unwrap_or_else(|e| e.into_inner());
    state.catalog = Some(Arc::new(models.catalog_snapshot()));
    state.revision += 1;
    let revision = state.revision;
    // `selection` is `None` when clearing the override: the agent takes `b` from `resolve()` when applying the target.
    state.main = Some(Target {
        revision,
        selection: selection.clone(),
        source,
        plan_route: None,
    });
    match selection {
        Some(selection) => state.reports.push_back(Report::Queued {
            role: ModelRole::Default,
            revision,
            selection,
            source,
        }),
        None => state.reports.push_back(Report::Cleared {
            role: ModelRole::Default,
            revision,
        }),
    }
}
