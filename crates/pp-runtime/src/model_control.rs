//! 配置期望值和实际生效状态分离；各 Agent 只在提交边界领取目标。
//!
//! 唯一的模型决议权威是 [`ModelState`]（`a` 手动覆盖 → `b` 配置默认）。本模块只负责：
//! 把 UI/CLI 的意图串行化成 `ModelState` 的写操作，并通过 revision 把"待应用的目标"
//! 发布给主 Agent 与子 Agent；读侧永远回到 `ModelState::resolve()`。
use anyhow::Result;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::config::ModelRole;
use pp_config::config::ModelRoles;
use pp_config::config::RoleAssignment;
use pp_config::model_state::ModelState;
use pp_config::model_state::Selection;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, mpsc},
    thread::{self, JoinHandle},
};

/// 主 Agent 或子 Agent 待应用的模型目标。
#[derive(Clone)]
pub struct Target {
    pub revision: u64,
    /// `None`：没有手动覆盖，跟随配置默认。
    pub selection: Option<Selection>,
    pub source: ModelSource,
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
    /// 主 Agent 的手动覆盖目标；`selection == None` 表示清除覆盖、跟随默认。
    main: Option<Target>,
    /// 子 Agent 的角色目标；DEFAULT 不再是角色，只由 `main` 表达。
    roles: BTreeMap<ModelRole, Target>,
    /// 仅 `Change::Think` 写入；只有主 Agent 消费。
    think: Option<ThinkTarget>,
    reports: VecDeque<Report>,
    applied: BTreeMap<String, (u64, bool)>,
}

#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<State>>);

pub enum Change {
    /// 主 Agent 手动选择（⌥P、`/model`、`--model`）：写 `a`。
    Override(Selection),
    /// 清除手动覆盖，回到配置默认。
    ClearOverride,
    /// 保存全局默认模型：写 `b`，不切换运行中的会话。
    SaveDefault(String),
    /// 角色目标，只写 `roles` 槽（DEFAULT 走 `b`，不进 `roles`）。
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
        // 主 Agent 只消费手动覆盖槽；子 Agent 只消费自己的角色槽。
        let picked = if self.agent.is_none() {
            state.main.clone()
        } else {
            self.role.and_then(|role| state.roles.get(&role).cloned())
        };
        let target = picked
            .filter(|target| target.revision > self.seen)
            .inspect(|target| {
                self.seen = target.revision;
            });
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
    /// 只读句柄：UI 用它把一次选择解析成完整 `Selection`，写操作仍经控制线程。
    models: Arc<ModelState>,
    join: Option<JoinHandle<()>>,
}

impl Service {
    /// `models` 是进程内唯一的模型决议权威；本线程是它唯一的串行写者。
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
                    let result = apply(&worker_models, &state, change);
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
            models,
            join: Some(join),
        })
    }

    pub fn submit(&self, change: Change) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.send(change).is_ok())
    }

    /// 把一次模型选择解析成完整 `Selection`，沿用目标档位策略（显式默认 → 当前档 → 有效默认）。
    pub fn resolve_reference(
        &self,
        reference: &str,
        current: Option<ThinkLevel>,
    ) -> Result<Selection> {
        self.models.resolve_reference(reference, current)
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

/// 单次配置变更：写 `ModelState` 后把待应用目标与报告发布到 registry。
fn apply(models: &Arc<ModelState>, registry: &Registry, change: Change) -> Result<()> {
    match change {
        Change::Think(level) => {
            models.set_override_effort(level)?;
            let mut state = registry.0.lock().unwrap_or_else(|e| e.into_inner());
            state.catalog = Some(Arc::new(models.catalog_snapshot()));
            // revision 必须由本 registry 单调递增：`ModelState` 的 revision 只在覆盖变化时前进，
            // 无覆盖会话（例如默认模型上改档位）会停在 0，导致目标被 `seen_think` 过滤掉。
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
            models.set_role(role, assignment.clone())?;
            let mut state = registry.0.lock().unwrap_or_else(|e| e.into_inner());
            state.catalog = Some(Arc::new(models.catalog_snapshot()));
            state.revision += 1;
            let revision = state.revision;
            state.reports.push_back(Report::Saved {
                roles: models.model_roles(),
                default: models.catalog_snapshot().default_reference().to_owned(),
                effort: models.default_think_level(),
            });
            // DEFAULT 角色即 `b`：只改配置默认，不切换运行中的主会话。
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
    state.main = Some(Target {
        revision,
        selection: selection.clone(),
        source,
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
