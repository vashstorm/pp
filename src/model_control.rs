//! 配置期望值和实际生效状态分离；各 Agent 只在提交边界领取配置。
use crate::config::{ModelCatalog, ModelChoice, ModelRole, ModelRoles, RoleAssignment, ThinkLevel};
use anyhow::Result;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

#[derive(Clone)]
pub(crate) struct Target {
    pub revision: u64,
    pub reference: String,
    pub effort: Option<ThinkLevel>,
    pub catalog: Arc<ModelCatalog>,
}
#[derive(Default)]
struct State {
    revision: u64,
    catalog: Option<Arc<ModelCatalog>>,
    main: Option<Target>,
    roles: BTreeMap<ModelRole, Target>,
    reports: VecDeque<Report>,
    applied: BTreeMap<String, (u64, bool)>,
}
#[derive(Clone, Default)]
pub(crate) struct Registry(Arc<Mutex<State>>);
pub(crate) enum Change {
    Select(String),
    Role(ModelRole, Option<RoleAssignment>),
    #[cfg(test)]
    Barrier(mpsc::Sender<()>),
}
pub(crate) enum Report {
    Cleared {
        role: ModelRole,
        revision: u64,
    },
    Queued {
        role: ModelRole,
        revision: u64,
        reference: String,
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
    },
    Failed {
        agent: Option<String>,
        revision: u64,
        message: String,
    },
}
#[derive(Clone)]
pub(crate) struct Binding {
    pub registry: Registry,
    pub role: Option<ModelRole>,
    pub agent: Option<String>,
    pub seen: u64,
}
impl Binding {
    pub fn pending(&self) -> bool {
        let mut probe = self.clone();
        probe.take().is_some()
    }
    pub fn main(registry: Registry) -> Self {
        Self {
            registry,
            role: None,
            agent: None,
            seen: 0,
        }
    }
    pub fn child(&self, role: Option<ModelRole>, agent: String) -> Self {
        Self {
            registry: self.registry.clone(),
            role,
            agent: Some(agent),
            seen: 0,
        }
    }
    pub fn take(&mut self) -> Option<Target> {
        let state = self.registry.0.lock().unwrap_or_else(|e| e.into_inner());
        let target = if self.agent.is_none() {
            state.main.as_ref()
        } else {
            self.role
                .filter(|role| *role != ModelRole::Default)
                .and_then(|role| state.roles.get(&role))
        }?;
        if target.revision <= self.seen {
            return None;
        }
        self.seen = target.revision;
        let mut target = target.clone();
        if let Some(catalog) = &state.catalog {
            target.catalog = catalog.clone();
        }
        Some(target)
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
        let role = role.filter(|role| *role != ModelRole::Default)?;
        let state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let target = state.roles.get(&role)?;
        match state.applied.get(agent) {
            Some((revision, true)) if *revision >= target.revision => None,
            Some((revision, false)) if *revision >= target.revision => {
                Some(format!("{} change failed", role.name()))
            }
            _ => Some(format!("→ {} (pending boundary)", target.reference)),
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
pub(crate) struct Service {
    sender: Option<mpsc::Sender<Change>>,
    pub registry: Registry,
    join: Option<JoinHandle<()>>,
}
impl Service {
    pub fn start(mut catalog: ModelCatalog) -> std::io::Result<Self> {
        let registry = Registry::default();
        let state = registry.clone();
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
                    let result = (|| -> Result<()> {
                        catalog.reload_models()?;
                        let (role, assignment) = match change {
                            Change::Select(reference) => {
                                catalog.resolve(&reference)?;
                                (ModelRole::Default, Some((reference, None)))
                            }
                            Change::Role(role, assignment) => {
                                catalog.save_model_role(role, assignment.clone())?;
                                state.report(Report::Saved {
                                    roles: catalog.model_roles().clone(),
                                    default: catalog.default_reference().into(),
                                    effort: catalog.default_think_level(),
                                });
                                (
                                    role,
                                    assignment
                                        .filter(|a| a.enabled)
                                        .map(|a| (a.model, Some(a.effort))),
                                )
                            }
                            #[cfg(test)]
                            Change::Barrier(_) => unreachable!(),
                        };
                        let mut inner = state.0.lock().unwrap_or_else(|e| e.into_inner());
                        inner.catalog = Some(Arc::new(catalog.clone()));
                        inner.revision += 1;
                        let revision = inner.revision;
                        if let Some((reference, effort)) = assignment {
                            let target = Target {
                                revision,
                                reference: reference.clone(),
                                effort,
                                catalog: Arc::new(catalog.clone()),
                            };
                            if role == ModelRole::Default {
                                inner.main = Some(target);
                            } else {
                                inner.roles.insert(role, target);
                            }
                            inner.reports.push_back(Report::Queued {
                                role,
                                revision,
                                reference,
                            });
                        } else {
                            if role == ModelRole::Default {
                                inner.main = None;
                            } else {
                                inner.roles.remove(&role);
                            }
                            inner.reports.push_back(Report::Cleared { role, revision });
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        state.report(Report::Failed {
                            agent: None,
                            revision: 0,
                            message: format!("{error:#}"),
                        });
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            registry,
            join: Some(join),
        })
    }
    pub fn submit(&self, change: Change) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.send(change).is_ok())
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
