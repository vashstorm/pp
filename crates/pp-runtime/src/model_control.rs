//! 配置期望值和实际生效状态分离；各 Agent 只在提交边界领取配置。
use anyhow::Result;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::config::ModelRole;
use pp_config::config::ModelRoles;
use pp_config::config::RoleAssignment;
use pp_core::model::ThinkLevel;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

#[derive(Clone)]
pub struct Target {
    pub role: Option<ModelRole>,
    pub revision: u64,
    pub reference: String,
    pub effort: Option<ThinkLevel>,
    pub catalog: Arc<ModelCatalog>,
}
/// 主会话手动思考档位目标；只有主 Agent 领取，不写配置文件。
#[derive(Clone)]
pub struct ThinkTarget {
    pub revision: u64,
    pub effort: ThinkLevel,
}
#[derive(Default)]
struct State {
    revision: u64,
    catalog: Option<Arc<ModelCatalog>>,
    /// 仅 `Change::Select` 写入，`Target.role == None`。
    main: Option<Target>,
    /// `Change::Role` 写入，`Target.role == Some(role)`，含 `Default`。
    roles: BTreeMap<ModelRole, Target>,
    /// 仅 `Change::Think` 写入；只有主 Agent 消费。
    think: Option<ThinkTarget>,
    reports: VecDeque<Report>,
    applied: BTreeMap<String, (u64, bool)>,
}
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<State>>);
pub enum Change {
    /// 主 Agent 手动 `/model`，只写 `main` 槽。
    Select(String),
    /// 角色目标，只写 `roles` 槽（包含 `Default`）。
    Role(ModelRole, Option<RoleAssignment>),
    /// 主会话手动档位，只写 `think` 槽；不读不写配置文件。
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
    /// 语义不变：只关心待应用的 model 目标（main/roles 两槽），probe clone 不污染 `seen`。
    pub fn pending(&self) -> bool {
        let mut probe = self.clone();
        probe.take_targets().0.is_some()
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
    /// 一次加锁取齐两个槽，保证同批快照里两个 revision 可比。
    pub fn take_targets(&mut self) -> (Option<Target>, Option<ThinkTarget>) {
        let state = self.registry.0.lock().unwrap_or_else(|e| e.into_inner());
        let picked = if self.agent.is_none() {
            // 主 Agent 在两个槽里取 revision 最大者，两个槽互不覆盖。
            match (state.main.as_ref(), state.roles.get(&ModelRole::Default)) {
                (Some(main), Some(default)) if default.revision > main.revision => Some(default),
                (Some(main), _) => Some(main),
                (None, default) => default,
            }
        } else {
            // 子 Agent 只消费自己的角色槽；`Default` 不再被排除。
            self.role.and_then(|role| state.roles.get(&role))
        };
        let target = match picked.filter(|target| target.revision > self.seen) {
            Some(target) => {
                self.seen = target.revision;
                let mut target = target.clone();
                if let Some(catalog) = &state.catalog {
                    target.catalog = catalog.clone();
                }
                Some(target)
            }
            None => None,
        };
        // 手动档位只下发给主 Agent；子 Agent 只跟随角色 effort。
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
pub struct Service {
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
                // 目标落到哪个槽位：手动选择只写 main，角色目标只写 roles。
                enum Slot {
                    Main,
                    Roles,
                }
                while let Ok(change) = receiver.recv() {
                    #[cfg(test)]
                    if let Change::Barrier(reply) = change {
                        let _ = reply.send(());
                        continue;
                    }
                    if let Change::Think(level) = change {
                        // 只登记期望档位：不重载模型目录、不写 models.yml/agent.db。
                        let mut inner = state.0.lock().unwrap_or_else(|e| e.into_inner());
                        inner.revision += 1;
                        let revision = inner.revision;
                        inner.think = Some(ThinkTarget {
                            revision,
                            effort: level,
                        });
                        inner.reports.push_back(Report::ThinkQueued {
                            revision,
                            effort: level,
                        });
                        continue;
                    }
                    let result = (|| -> Result<()> {
                        catalog.reload_models()?;
                        let (slot, role, assignment) = match change {
                            Change::Select(reference) => {
                                catalog.resolve(&reference)?;
                                (Slot::Main, ModelRole::Default, Some((reference, None)))
                            }
                            Change::Role(role, assignment) => {
                                catalog.save_model_role(role, assignment.clone())?;
                                state.report(Report::Saved {
                                    roles: catalog.model_roles().clone(),
                                    default: catalog.default_reference().into(),
                                    effort: catalog.default_think_level(),
                                });
                                (
                                    Slot::Roles,
                                    role,
                                    assignment
                                        .filter(|a| a.enabled)
                                        .map(|a| (a.model, Some(a.effort))),
                                )
                            }
                            #[cfg(test)]
                            Change::Barrier(_) => unreachable!("barrier handled before applying"),
                            Change::Think(_) => {
                                unreachable!("think change registered before applying")
                            }
                        };
                        let mut inner = state.0.lock().unwrap_or_else(|e| e.into_inner());
                        inner.catalog = Some(Arc::new(catalog.clone()));
                        inner.revision += 1;
                        let revision = inner.revision;
                        let role_route = match slot {
                            Slot::Main => None,
                            Slot::Roles => Some(role),
                        };
                        if let Some((reference, effort)) = assignment {
                            let target = Target {
                                role: role_route,
                                revision,
                                reference: reference.clone(),
                                effort,
                                catalog: Arc::new(catalog.clone()),
                            };
                            match slot {
                                Slot::Main => inner.main = Some(target),
                                Slot::Roles => {
                                    inner.roles.insert(role, target);
                                }
                            }
                            inner.reports.push_back(Report::Queued {
                                role,
                                revision,
                                reference,
                            });
                        } else {
                            match slot {
                                Slot::Main => inner.main = None,
                                Slot::Roles => {
                                    inner.roles.remove(&role);
                                }
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
