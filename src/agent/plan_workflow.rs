//! Worker-side Plan mode lifecycle: mode entry/exit with MCP lifecycle
//! safety, safe external plan import, review-token validation, and the two
//! post-approval execution handoffs.
//!
//! Every permission change is persisted through the journal's transactional
//! workflow record before or as part of the transition, and a failed MCP
//! cleanup keeps the fail-closed `plan_cleanup_error` gate so no request can
//! start with half-removed capabilities.

use super::{
    Agent, ExecutionPreparation, Session,
    persistence::{Metadata, SessionJournal},
};
use crate::{
    agent::input::InputQueue,
    cancellation::{self, CancellationToken},
    mcp::Registry,
    plan::{
        self, ExecutionContext, PlanDocument, PlanLocation, PlanReview, PlanStatus, ReviewToken,
        WorkMode, WorkflowState,
    },
    project_instructions,
    tools::{Tools, plan_catalog},
};
use anyhow::{Result, anyhow, bail, ensure};
use std::path::Path;

const NOT_CURRENT: &str = "Plan review is no longer current.";
const CHANGED: &str = "Plan changed; review the new revision before executing.";
const NO_TASKS: &str = "Plan must contain at least one Markdown TODO item.";
const PENDING: &str = "Apply pending input before approving the plan.";
const NOT_PLAN: &str = "Plan mode is not active.";
const NO_PLAN: &str = "No saved plan.";
const NO_ORIGIN: &str = "Enter the task before reviewing this plan.";
const DELETED_PLAN: &str = "Plan was deleted; choose another plan from /plan-list.";
const COMPLETED_CONFLICT: &str =
    "Plan registration conflicts with the completed state; resolve it in /plan-list.";
const REGISTER_CONFLICT: &str =
    "Plan file name is registered to another plan; resolve it in /plan-list.";
const TASKS_CHANGED: &str = "Plan tasks changed; re-plan before completing.";

/// 生命周期搬迁的待清理端点：源/目标各自的 (文件名, 位置)。
type PlanMoveEndpoints = ((String, PlanLocation), (String, PlanLocation));

pub(super) enum SavedInstructions {
    Completion(async_openai::types::chat::ChatCompletionRequestSystemMessageContent),
    Response(Option<String>),
}

impl SavedInstructions {
    pub(super) fn take(session: &mut Session) -> Self {
        match session {
            Session::Completion(request) => {
                let Some(async_openai::types::chat::ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                else {
                    unreachable!("session has system instructions");
                };
                Self::Completion(std::mem::replace(&mut message.content, "".into()))
            }
            Session::Response(request) => Self::Response(request.instructions.take()),
        }
    }

    pub(super) fn restore(self, session: &mut Session) {
        match (self, session) {
            (Self::Completion(content), Session::Completion(request)) => {
                let Some(async_openai::types::chat::ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                else {
                    unreachable!("session has system instructions");
                };
                message.content = content;
            }
            (Self::Response(instructions), Session::Response(request)) => {
                request.instructions = instructions
            }
            _ => unreachable!("approval does not change protocol"),
        }
    }
}

impl Agent {
    /// 新来源按块渲染；旧会话继续使用不可拆分的基础指令。
    pub(super) fn compose_prompt(
        &self,
        instructions: &str,
        mode: &str,
        model: &str,
        tool_names: Vec<String>,
    ) -> Result<String> {
        self.compose_prompt_snapshot(instructions, mode, model, tool_names)
            .map(|(system, _, _)| system)
    }

    pub(super) fn compose_prompt_snapshot(
        &self,
        instructions: &str,
        mode: &str,
        model: &str,
        tool_names: Vec<String>,
    ) -> Result<(String, Vec<crate::skills::SkillHeader>, String)> {
        let project = project_instructions::load(self.tools.cwd())?;
        let skills = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow!("skill registry unavailable"))?
            .headers();
        let summary = crate::skills::Skills::format_summary(&skills);
        let system = if let Some(source) = &self.prompt_source {
            let context = crate::system_prompt::RenderContext::new(
                self.tools.cwd(),
                model,
                self.tools.edit_mode(),
                mode,
                tool_names,
            );
            source.compose(
                &context,
                self.tools.limits,
                &project,
                &summary,
                instructions,
            )
        } else {
            let mut system = project_instructions::compose(
                &self.base_system,
                self.tools.cwd(),
                &project,
                &summary,
            );
            if !instructions.is_empty() {
                system.push_str("\n\n");
                system.push_str(instructions);
            }
            Ok(system)
        }?;
        Ok((system, skills, project))
    }

    /// 候选会话使用目标模式的工具清单，逐请求刷新使用原生请求里的实际清单。
    pub(super) fn compose_system(&self, workflow: &WorkflowState) -> Result<String> {
        self.compose_prompt(
            &workflow.instructions(),
            workflow.mode_name(),
            &self.current_model.model,
            self.tools
                .all_definitions(workflow.mode)
                .into_iter()
                .map(|tool| tool.name)
                .collect(),
        )
    }

    /// Persist a workflow change; on failure restore the whole `previous`
    /// state so callers keep one transactional rollback idiom.
    pub(super) fn commit_workflow(&mut self, previous: WorkflowState) -> Result<()> {
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(())
    }

    /// 恢复协调入口：计划读写、审阅、审批、进度与恢复访问前按 ID 同步
    /// 实际路径。revision-0/未绑定直接通过；已绑定文档不 clone 整篇正文。
    pub(crate) fn reconcile_plan_location(&mut self) -> Result<()> {
        reconcile_plan_state(
            self.tools.plan_workspace(),
            &mut self.journal,
            &mut self.workflow,
        )
    }

    /// 审批后的 Drafts→Plans 交接：发布目标链接 → CAS 更新登记 → 提交
    /// 绑定（纯路径变化，不递增 revision）→ 交接 journal 成功后由调用方
    /// 移除源链接。已在 Plans 的计划只核对/更新登记，不重复搬迁。返回
    /// 待清理的源/目标身份。
    fn promote_approved_plan(&mut self, token: &ReviewToken) -> Result<Option<PlanMoveEndpoints>> {
        let document = self.workflow.plan.as_ref().expect("ready plan bound");
        if document.location == PlanLocation::Plans {
            return Ok(None);
        }
        let file_name = document
            .file_name
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let source = (file_name.clone(), document.location);
        let target = (file_name, PlanLocation::Plans);
        {
            let workspace = self.tools.plan_workspace();
            plan_catalog::publish_plan_move(
                workspace,
                &document.id,
                (source.0.as_str(), source.1),
                (target.0.as_str(), target.1),
                token.content.as_bytes(),
            )?;
        }
        let previous = self.workflow.clone();
        self.workflow
            .plan
            .as_mut()
            .expect("ready plan bound")
            .location = PlanLocation::Plans;
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(Some((source, target)))
    }

    /// 交接 journal 提交成功后的源链接清理；失败不撤回已提交状态，残留
    /// 源链接交给按 ID 协调做同 inode 确认清理，不冒报回滚。
    fn finish_approved_move(&self, pending_move: Option<PlanMoveEndpoints>, token: &ReviewToken) {
        let Some((source, target)) = pending_move else {
            return;
        };
        let workspace = self.tools.plan_workspace();
        if let Err(error) = plan_catalog::commit_plan_move(
            workspace,
            (source.0.as_str(), source.1),
            (target.0.as_str(), target.1),
            token.content.as_bytes(),
        ) {
            tracing::warn!(event = "plan_move_cleanup_failed", error = %error);
        }
    }
}

/// 按 ID 协调结果：绑定需采纳登记、登记需首次建立，或已一致。
enum PlanReconciliation {
    /// 登记表优先：把绑定同步到 canonical 名称/位置/状态。
    Adopt {
        file_name: String,
        location: PlanLocation,
        status: PlanStatus,
        leftover: Option<String>,
    },
    /// 首次登记：按迁移规则原位登记，不搬迁文件。
    Register {
        file_name: String,
        location: PlanLocation,
    },
    /// 首次登记并按迁移规则搬迁到 Drafts（旧 Draft/Ready 的 Plans 绑定）。
    RegisterAndMove {
        file_name: String,
        location: PlanLocation,
        source: (String, PlanLocation),
        bytes: Vec<u8>,
    },
}

/// 按 ID 协调计划身份：登记表始终优先，把绑定文档同步到 canonical
/// 名称/位置；纯路径/状态变化不递增正文 revision，但使旧 path token
/// 失效。未登记的 revision>=1 旧绑定按迁移规则首次登记：Draft/Ready 的
/// 旧 Plans 绑定保留旧名迁回 Drafts（安全搬迁），Executed 仍在 Plans，
/// Abandoned 保留原位置只读，历史 Executed 登记到 Done 时只读同步
/// Completed，不重新授权。墓碑、路径/正文缺失一律报错，绝不从历史
/// 快照恢复，也不让历史 Draft/Ready 绑定把已提升的位置退回 drafts。
fn reconcile_plan_state(
    workspace: &crate::tools::save::Workspace,
    journal: &mut SessionJournal,
    workflow: &mut WorkflowState,
) -> Result<()> {
    let Some(document) = workflow.plan.as_ref() else {
        return Ok(());
    };
    if document.revision == 0 {
        return Ok(());
    }
    let id = document.id.clone();
    let previous_file_name = document.file_name.clone();
    let previous_location = document.location;
    let previous_status = document.status;
    // revision>=1 的绑定一定有可解析路径（旧 <id>.md 兜底或显式名称）。
    let previous_path = document.path().expect("saved plan has a lifecycle path");

    let adopted = {
        let _lock = plan_catalog::PlanCatalogLock::acquire(workspace)?;
        let snapshot = plan_catalog::load_catalog(workspace)?;
        if snapshot
            .catalog
            .entries
            .iter()
            .any(|entry| entry.deleted && entry.id == id)
        {
            bail!("{DELETED_PLAN}");
        }
        match snapshot.catalog.active_by_id(&id) {
            Some(entry) => {
                let status = if entry.location == PlanLocation::Done {
                    // 登记到 Done 的历史 Executed 只读同步 Completed。
                    PlanStatus::Completed
                } else {
                    ensure!(
                        previous_status != PlanStatus::Completed,
                        "{COMPLETED_CONFLICT}"
                    );
                    previous_status
                };
                if previous_file_name.as_deref() == Some(entry.file_name.as_str())
                    && previous_location == entry.location
                    && previous_status == status
                {
                    return Ok(());
                }
                let canonical = plan::path_for(&entry.file_name, entry.location);
                PlanReconciliation::Adopt {
                    file_name: entry.file_name.clone(),
                    location: entry.location,
                    status,
                    leftover: (canonical != previous_path).then_some(previous_path),
                }
            }
            None => {
                // 未登记的旧绑定：首次登记按迁移规则；名称冲突 fail-closed，
                // 不抢注其他计划的名称。
                let (file_name, mut location) = match &previous_file_name {
                    Some(name) => (name.clone(), previous_location),
                    None => (format!("{id}.md"), previous_location),
                };
                ensure!(
                    snapshot.catalog.active_by_file_name(&file_name).is_none(),
                    "{REGISTER_CONFLICT}"
                );
                // 登记绝不指向读不到的路径：正文缺失在此报错。
                let source = workspace.target(Path::new(&previous_path), false)?;
                let bytes = source.read_bounded(plan::PLAN_MAX_BYTES)?;
                let moved = matches!(previous_status, PlanStatus::Draft | PlanStatus::Ready)
                    && location == PlanLocation::Plans;
                if moved {
                    location = PlanLocation::Drafts;
                    let target_path = plan::path_for(&file_name, location);
                    // 发布目标链接后再 CAS 建立登记；失败保留源链接。
                    workspace.link_plan(
                        Path::new(&previous_path),
                        Path::new(&target_path),
                        &bytes,
                    )?;
                    let mut catalog = snapshot.catalog;
                    catalog.entries.push(plan_catalog::PlanCatalogEntry {
                        id: id.clone(),
                        file_name: file_name.clone(),
                        location,
                        deleted: false,
                        deleted_file_identity: None,
                    });
                    plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)?;
                    PlanReconciliation::RegisterAndMove {
                        file_name,
                        location,
                        source: (
                            previous_file_name.unwrap_or_else(|| format!("{id}.md")),
                            PlanLocation::Plans,
                        ),
                        bytes,
                    }
                } else {
                    let mut catalog = snapshot.catalog;
                    catalog.entries.push(plan_catalog::PlanCatalogEntry {
                        id: id.clone(),
                        file_name: file_name.clone(),
                        location,
                        deleted: false,
                        deleted_file_identity: None,
                    });
                    plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)?;
                    PlanReconciliation::Register {
                        file_name,
                        location,
                    }
                }
            }
        }
    };
    match adopted {
        PlanReconciliation::Adopt {
            file_name,
            location,
            status,
            leftover,
        } => {
            let previous = workflow.clone();
            {
                let document = workflow.plan.as_mut().expect("plan bound");
                document.file_name = Some(file_name.clone());
                document.location = location;
                document.status = status;
            }
            if let Err(error) = journal.write_workflow(workflow) {
                *workflow = previous;
                return Err(error);
            }
            if let Some(leftover) = leftover {
                let canonical = plan::path_for(&file_name, location);
                plan_catalog::cleanup_confirmed_leftover(workspace, &canonical, &leftover)?;
            }
            Ok(())
        }
        PlanReconciliation::Register {
            file_name,
            location,
        } => {
            let previous = workflow.clone();
            {
                let document = workflow.plan.as_mut().expect("plan bound");
                document.file_name = Some(file_name);
                document.location = location;
            }
            if let Err(error) = journal.write_workflow(workflow) {
                *workflow = previous;
                return Err(error);
            }
            Ok(())
        }
        PlanReconciliation::RegisterAndMove {
            file_name,
            location,
            source,
            bytes,
        } => {
            let previous = workflow.clone();
            {
                let document = workflow.plan.as_mut().expect("plan bound");
                document.file_name = Some(file_name.clone());
                document.location = location;
            }
            if let Err(error) = journal.write_workflow(workflow) {
                *workflow = previous;
                return Err(error);
            }
            plan_catalog::commit_plan_move(
                workspace,
                (source.0.as_str(), source.1),
                (file_name.as_str(), location),
                &bytes,
            )?;
            Ok(())
        }
    }
}

/// 绑定文档的身份三元组：登记协调是否改变了名称/位置/状态。
fn plan_identity(document: &PlanDocument) -> (Option<String>, PlanLocation, PlanStatus) {
    (
        document.file_name.clone(),
        document.location,
        document.status,
    )
}

/// 只读预览投影：unchecked→pending、checked→completed，无 active/blocker，
/// 从不归一化激活，也从不继承运行态。Plan 模式下的来源会话（含 Fresh 批准
/// 后留在源会话的 Executed 绑定）一律走这里，不伪装成执行中。
pub(super) fn preview_phases(
    document: &PlanDocument,
) -> Result<Vec<crate::tools::todo::TodoPhase>> {
    let mut phases: Vec<crate::tools::todo::TodoPhase> = Vec::new();
    let mut region: Option<usize> = None;
    for task in plan::plan_tasks(&document.content) {
        if region != Some(task.phase_index) {
            region = Some(task.phase_index);
            phases.push(crate::tools::todo::TodoPhase {
                name: task.phase,
                tasks: Vec::new(),
            });
        }
        phases
            .last_mut()
            .expect("task creates its phase")
            .tasks
            .push(crate::tools::todo::TodoItem {
                content: task.text,
                status: if task.completed {
                    crate::tools::todo::TodoStatus::Completed
                } else {
                    crate::tools::todo::TodoStatus::Pending
                },
                blocker: None,
            });
    }
    Ok(phases)
}

/// Step 4 共享同步入口：把当前绑定计划的实际 Markdown 同步为会话 todo
/// 投影。适用范围固定为 Plan 模式有绑定、Execute+Executed，或仍绑定当前
/// 只读 Completed/Abandoned 计划；已用 init 脱离旧计划的独立普通清单
/// （来源为空且非空）绝不因此被覆盖。行为要点：
///
/// - 实际文件优先：先按 ID 协调登记路径，再安全读取；内容未变但投影或
///   来源缺失时仍初始化并持久化（旧会话/交接遗漏的自愈）。
/// - 旧 Executed 记录缺 approved_tasks：先以已登记正文冻结一次性迁移
///   基线并持久化，之后才读磁盘；不把迁移后的磁盘内容冒充批准集合。
/// - Execute+Executed 的磁盘任务文本/数量/顺序与批准向量不同（无论勾选
///   状态）时拒绝安装，保留最后可信的正文与运行态，阻止继续执行/提醒/
///   归档；说明与勾选变化正常同步。
/// - 执行投影只在 Execute+Executed 归一化唯一 active；预览与结束计划
///   始终无 active。revision-0 绑定从不读盘，预览为空。
/// - 无变化不写 journal、不发事件；正文/路径/状态变化发布
///   PlanProgressUpdated，投影/正文/来源的真实变化才按
///   `publish_snapshot` 发布 TodoSnapshot（先持久化 workflow，再
///   journal.event/check 后 emit）。
pub(super) fn sync_plan_todos(
    tools: &Tools,
    journal: &mut SessionJournal,
    workflow: &mut WorkflowState,
    emit: &mut dyn FnMut(crate::events::AgentEvent),
    publish_snapshot: bool,
) -> Result<()> {
    let Some(document) = workflow.plan.as_ref() else {
        return Ok(());
    };
    let bound = workflow.todo_plan_id.as_deref() == Some(document.id.as_str());
    let applicable = workflow.mode == WorkMode::Plan
        || (workflow.mode == WorkMode::Execute && document.status == PlanStatus::Executed)
        || (bound
            && matches!(
                document.status,
                PlanStatus::Completed | PlanStatus::Abandoned
            ));
    if !applicable {
        return Ok(());
    }
    let identity_before = plan_identity(document);
    let status_before = document.status;
    reconcile_plan_state(tools.plan_workspace(), journal, workflow)?;
    let identity_changed =
        plan_identity(workflow.plan.as_ref().expect("plan bound")) != identity_before;
    let executing = workflow.mode == WorkMode::Execute
        && workflow.plan.as_ref().expect("plan bound").status == PlanStatus::Executed;
    if executing
        && workflow
            .plan
            .as_ref()
            .expect("plan bound")
            .approved_tasks
            .is_none()
    {
        // 迁移基线只能来自已登记旧正文，不能让随后读取的新磁盘任务获得批准。
        let approved = plan::plan_tasks(&workflow.plan.as_ref().expect("plan bound").content)
            .into_iter()
            .map(|task| task.text)
            .collect();
        workflow.plan.as_mut().expect("plan bound").approved_tasks = Some(approved);
        if let Err(error) = journal.write_workflow(workflow) {
            workflow.plan.as_mut().expect("plan bound").approved_tasks = None;
            return Err(error.context(
                "Plan progress could not be refreshed; last synchronized progress retained.",
            ));
        }
    }
    let document = workflow.plan.as_ref().expect("plan bound");
    let content = if document.revision == 0 {
        None
    } else {
        let content = tools.read_plan(document).map_err(|error| {
            error.context(
                "Plan progress could not be refreshed; last synchronized progress retained.",
            )
        })?;
        (content != document.content).then_some(content)
    };
    let content_changed = content.is_some();
    let previous_document = if let Some(content) = content {
        let document = workflow.plan.as_mut().expect("plan bound");
        let revision = document.next_revision()?;
        let previous_status = document.status;
        if matches!(document.status, PlanStatus::Draft | PlanStatus::Ready) {
            plan::validate_content(&content)?;
            document.status = if plan::task_progress(&content).1 > 0 {
                PlanStatus::Ready
            } else {
                PlanStatus::Draft
            };
        }
        Some((
            std::mem::replace(&mut document.content, content),
            std::mem::replace(&mut document.revision, revision),
            previous_status,
        ))
    } else {
        None
    };
    let projected = {
        let document = workflow.plan.as_ref().expect("plan bound");
        if executing {
            crate::tools::todo::project_plan_todos(document, &workflow.todos, bound)
        } else {
            preview_phases(document)
        }
    };
    let phases = match projected {
        Ok(phases) => phases,
        Err(error) => {
            if let Some((content, revision, status)) = previous_document {
                let document = workflow.plan.as_mut().expect("plan bound");
                document.content = content;
                document.revision = revision;
                document.status = status;
            }
            return Err(error);
        }
    };
    let source = Some(workflow.plan.as_ref().expect("plan bound").id.clone());
    let projection_changed = workflow.todos != phases || workflow.todo_plan_id != source;
    if content_changed || projection_changed {
        let previous_phases = std::mem::replace(&mut workflow.todos, phases);
        let previous_source = std::mem::replace(&mut workflow.todo_plan_id, source);
        if let Err(error) = journal.write_workflow(workflow) {
            workflow.todos = previous_phases;
            workflow.todo_plan_id = previous_source;
            if let Some((content, revision, status)) = previous_document {
                let document = workflow.plan.as_mut().expect("plan bound");
                document.content = content;
                document.revision = revision;
                document.status = status;
            }
            return Err(error.context(
                "Plan progress could not be refreshed; last synchronized progress retained.",
            ));
        }
    }
    if content_changed || identity_changed {
        super::publish_plan_progress(workflow, emit);
    }
    let status_changed = workflow.plan.as_ref().expect("plan bound").status != status_before;
    if publish_snapshot && (content_changed || projection_changed || status_changed) {
        super::publish_todo_snapshot(workflow, journal, emit)?;
    }
    Ok(())
}

/// 恢复只携带一次同步新生成的快照；replay 已包含相同快照时不重复追加。
fn resume_todo_snapshot(
    workflow: &WorkflowState,
    replay: &[super::persistence::ReplayRecord],
) -> Option<crate::events::AgentEvent> {
    let event = super::todo_snapshot(workflow)?;
    let crate::events::AgentEvent::TodoSnapshot {
        plan_id,
        revision,
        phases,
        status,
    } = &event
    else {
        unreachable!("todo_snapshot builds a TodoSnapshot");
    };
    let replayed = replay.iter().any(|record| {
        matches!(
            record,
            super::persistence::ReplayRecord::Event(crate::events::AgentEvent::TodoSnapshot {
                plan_id: replayed_plan,
                revision: replayed_revision,
                phases: replayed_phases,
                status: replayed_status,
            }) if replayed_plan == plan_id
                && replayed_revision == revision
                && replayed_phases == phases
                && replayed_status == status
        )
    });
    (!replayed).then_some(event)
}

/// 启动恢复与交互式 resume 共用同一文件同步和快照去重边界。
pub(super) fn sync_restored_todos(
    tools: &Tools,
    journal: &mut SessionJournal,
    workflow: &mut WorkflowState,
    replay: &[super::persistence::ReplayRecord],
) -> Result<Option<crate::events::AgentEvent>> {
    let Some(document) = workflow.plan.as_ref() else {
        return Ok(None);
    };
    let previous_plan = (document.id.clone(), document.revision, document.status);
    let previous_todos = workflow.todos.clone();
    let previous_source = workflow.todo_plan_id.clone();
    reconcile_plan_state(tools.plan_workspace(), journal, workflow)?;
    sync_plan_todos(tools, journal, workflow, &mut |_| {}, false)?;
    let document = workflow.plan.as_ref().expect("restored plan bound");
    let changed = previous_todos != workflow.todos
        || previous_source != workflow.todo_plan_id
        || previous_plan != (document.id.clone(), document.revision, document.status);
    let snapshot = changed
        .then(|| resume_todo_snapshot(workflow, replay))
        .flatten();
    if let Some(event) = &snapshot {
        journal.event(event);
        journal.check()?;
    }
    Ok(snapshot)
}

/// Register on-disk plan bytes as a new revision against the bound workflow.
/// Returns `false` when the file matches the registered snapshot. Draft/Ready
/// documents become Ready only when the imported content has a genuine
/// Markdown task list; Executed/Abandoned documents remain read-only after
/// importing progress. The whole workflow is rolled back when the journal
/// rejects the write.
fn import_plan_content(
    journal: &mut SessionJournal,
    workflow: &mut WorkflowState,
    content: String,
    new_title: &str,
) -> Result<bool> {
    plan::validate_content(&content)?;
    let revision = {
        let bound = workflow.plan.as_ref().expect("plan bound");
        if bound.content == content {
            return Ok(false);
        }
        if bound.revision == 0 {
            plan::validate_title(new_title)?;
        }
        bound.next_revision()?
    };
    let previous = workflow.clone();
    let document = workflow.plan.as_mut().expect("plan bound");
    if document.revision == 0 {
        document.title = new_title.to_owned();
    }
    document.revision = revision;
    let (_, total) = plan::task_progress(&content);
    document.content = content;
    if matches!(document.status, PlanStatus::Draft | PlanStatus::Ready) {
        document.status = if total > 0 {
            PlanStatus::Ready
        } else {
            PlanStatus::Draft
        };
    }
    if let Err(error) = journal.write_workflow(workflow) {
        *workflow = previous;
        return Err(error);
    }
    Ok(true)
}

impl Agent {
    /// Stop every managed MCP process so Plan mode cannot reach them. On
    /// failure the registry, its slots and the UI's exit handle stay in place
    /// and `plan_cleanup_error` blocks run/continue until `/plan` retries.
    /// `pub(super)`：列表打开的 Plan 候选会话（plan_open）复用同一门禁。
    pub(super) fn ensure_plan_mcp_shutdown(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        cancellation::check(cancellation)?;
        if let Err(error) = self.tools.mcp.shutdown() {
            let message = format!(
                "MCP servers could not be stopped for Plan mode: {error:#}; run /plan again to retry."
            );
            self.plan_cleanup_error = Some(message.clone());
            bail!("{message}");
        }
        self.plan_cleanup_error = None;
        self.tools.mcp = Registry::default();
        self.session
            .set_tools(self.tools.all_definitions(WorkMode::Plan));
        Ok(())
    }

    fn abort_execution_preparation(
        &mut self,
        mut registry: Registry,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match registry.shutdown() {
            Ok(()) => error,
            Err(cleanup) => {
                self.plan_cleanup_error = Some(format!(
                    "Prepared MCP services could not be stopped: {cleanup:#}; run /plan again to retry."
                ));
                self.tools.mcp = registry;
                cleanup.context(format!("Execution preparation also failed: {error:#}"))
            }
        }
    }

    /// `/plan` toggle 的进入方向：切到 Plan 待命（Plan+None），不绑定计划
    /// 文档；工作流权威先持久化，之后才关闭 MCP 能力。已在 Plan 时只重试
    /// 一次失败的关闭（`/plan <prompt>` 继续当前草稿也复用该入口）。
    pub(crate) fn enter_plan(&mut self, cancellation: &CancellationToken) -> Result<WorkflowState> {
        cancellation::check(cancellation)?;
        if self.workflow.mode == WorkMode::Plan {
            self.ensure_plan_mcp_shutdown(cancellation)?;
            return Ok(self.workflow.clone());
        }
        self.journal.check()?;
        // 旧 Executed/Completed 绑定一并解除；文件与登记保留，由 /plan-list 管理。
        let previous = std::mem::replace(
            &mut self.workflow,
            WorkflowState {
                mode: WorkMode::Plan,
                plan: None,
                ..Default::default()
            },
        );
        self.commit_workflow(previous)?;
        self.ensure_plan_mcp_shutdown(cancellation)?;
        Ok(self.workflow.clone())
    }

    /// Register on-disk plan bytes as a new revision. Returns `false` when the
    /// file matches the registered snapshot. Draft/Ready documents become
    /// Ready only when the imported content has a genuine Markdown task list;
    /// Executed/Abandoned documents remain read-only after importing progress.
    fn import_plan_revision(&mut self, content: String, new_title: &str) -> Result<bool> {
        import_plan_content(&mut self.journal, &mut self.workflow, content, new_title)
    }

    /// Safe plan review snapshot: reads the current file through the NOFOLLOW
    /// workspace handle, imports external edits as a new revision, and issues
    /// a review token bound to the current session, plan and exact bytes. A
    /// missing or unreadable file is an error; the logged snapshot is never a
    /// fallback. Executed/Abandoned plans safely refresh their displayed
    /// snapshot without regaining approval eligibility. The shared todo sync
    /// reconciles the registered path, imports the revision and rebuilds the
    /// read-only projection; a detached standalone list is never replaced.
    pub(crate) fn read_plan(&mut self) -> Result<PlanReview> {
        self.journal.check()?;
        let document = self
            .workflow
            .plan
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let report = self.workflow.review.clone();
        if document.revision == 0 {
            // revision-0 绑定尚未首存：没有可解析的文件路径，从不从
            // 历史快照或猜测路径导入。
            bail!("{NO_PLAN}");
        }
        // 按 ID 协调登记路径、导入外部修订并刷新只读投影；墓碑、搬迁与
        // 首次登记在此暴露。
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            &mut |_| {},
            true,
        )?;
        let document = self.workflow.plan.as_ref().expect("plan bound").clone();
        let session_id = self
            .journal
            .id()
            .ok_or_else(|| anyhow!("{NO_ORIGIN}"))?
            .to_owned();
        Ok(PlanReview {
            token: ReviewToken {
                session_id,
                path: document.path().ok_or_else(|| anyhow!("{NO_PLAN}"))?,
                plan_id: document.id.clone(),
                revision: document.revision,
                content: document.content.clone(),
            },
            document,
            report,
        })
    }

    /// `/plan` toggle 的退出方向：回到 Execute 并解除当前计划/审阅绑定。
    /// 已保存的草稿文件和登记原样保留（不再标记 Abandoned），旧稿经
    /// /plan-list 继续。已提交任务但 revision-0 尚未首存时拒绝退出。
    pub(crate) fn leave_plan(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_PLAN}");
        if let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            bail!("Save the draft with plan_write before leaving Plan mode.");
        }
        self.journal.check()?;
        self.ensure_plan_mcp_shutdown(cancellation)?;
        let config = self
            .catalog
            .resolve(&self.current_model.reference)
            .map_err(|_| anyhow!("current model is unavailable"))?;
        let registry =
            Registry::connect(&config.settings.mcp_servers, self.tools.cwd(), cancellation)?;
        let cleanup = match registry.cleanup() {
            Ok(cleanup) => cleanup,
            Err(error) => return Err(self.abort_execution_preparation(registry, error)),
        };
        let previous = self.workflow.clone();
        self.workflow.mode = WorkMode::Execute;
        // 解除绑定但不删除任何文件；空待命与已保存草稿都直接清除。
        // 计划 todo 来源与投影一并解除，不恢复任何隐藏的旧普通清单。
        self.workflow.plan = None;
        self.workflow.review = None;
        self.workflow.todos = Vec::new();
        self.workflow.todo_plan_id = None;
        let mut definitions = self.tools.all_definitions(WorkMode::Execute);
        definitions.extend(registry.definitions());
        self.session.set_tools(definitions);
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            self.session
                .set_tools(self.tools.all_definitions(WorkMode::Plan));
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.tools.mcp = registry;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            cleanup,
            todo_snapshot: None,
        })
    }

    pub(super) fn refresh_plan_progress(
        &mut self,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
    ) -> Result<()> {
        if self.workflow.mode != WorkMode::Execute
            || !self
                .workflow
                .plan
                .as_ref()
                .is_some_and(|plan| plan.status == PlanStatus::Executed)
        {
            return Ok(());
        }
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            emit,
            false,
        )
    }

    /// 成功收尾的自动归档：仅 Execute + Executed + Plans 可进入。安全读取
    /// 当前磁盘文件并同步进度后，要求 total > 0 且 completed == total，
    /// 且当前任务文本向量精确等于批准时冻结的 approved_tasks；任务被
    /// 删除、替换、增项或重排都不能通过。未全部勾选或零任务静默跳过
    /// （返回 `Ok(false)`），保留 Executed/Plans；任务集合与批准集合不一致
    /// 返回 `Plan tasks changed; re-plan before completing.`，同样不搬迁。
    /// 搬迁复用第二阶段协议（锁顺序 PlanLease → 登记表
    /// 锁，任务 lease 由 finish_task 持有）：发布 Done 链接并 CAS 更新登记
    /// → 提交 workflow Completed（纯路径/状态变化，不递增正文 revision）
    /// → 移除 plans 源链接。登记已更新但 workflow 提交失败时保留源链接并
    /// 返回真实错误，下一次按 ID 协调到 Done/Completed；提交后的清理警告
    /// 不撤回状态。返回是否完成归档。
    pub(super) fn archive_completed_plan(
        &mut self,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
    ) -> Result<bool> {
        if self.workflow.mode != WorkMode::Execute {
            return Ok(false);
        }
        let Some(document) = self.workflow.plan.as_ref() else {
            return Ok(false);
        };
        if document.status != PlanStatus::Executed || document.location != PlanLocation::Plans {
            return Ok(false);
        }
        // 归档前安全读取当前磁盘文件并同步进度；失败保留 Executed/Plans。
        self.refresh_plan_progress(emit)?;
        let document = self.workflow.plan.as_ref().expect("executed plan bound");
        let (completed, total) = plan::task_progress(&document.content);
        if total == 0 || completed != total {
            return Ok(false);
        }
        let tasks: Vec<String> = plan::plan_tasks(&document.content)
            .into_iter()
            .map(|task| task.text)
            .collect();
        // 旧记录的迁移基线已由 refresh_plan_progress 建立；仍然缺失或与
        // 当前任务集合不一致是违反归档合同的错误，不猜测批准集合。
        ensure!(
            document.approved_tasks.as_deref() == Some(tasks.as_slice()),
            "{TASKS_CHANGED}"
        );
        let file_name = document
            .file_name
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let id = document.id.clone();
        let expected = document.content.as_bytes().to_vec();
        // 发布 Done 链接并原子更新登记；失败保留 Executed/Plans 与源链接。
        let warning = plan_catalog::publish_plan_move(
            self.tools.plan_workspace(),
            &id,
            (file_name.as_str(), PlanLocation::Plans),
            (file_name.as_str(), PlanLocation::Done),
            &expected,
        )?;
        if let Some(warning) = warning {
            tracing::warn!(event = "plan_archive_warning", warning = %warning);
        }
        // 登记已指向 Done：提交 Completed 绑定；失败保留源链接并返回真实
        // 错误，下一次按 ID 协调完成收尾，不声称已回滚。
        let previous = self.workflow.clone();
        {
            let document = self.workflow.plan.as_mut().expect("executed plan bound");
            document.status = PlanStatus::Completed;
            document.location = PlanLocation::Done;
        }
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error.context(
                "Plan archival committed to the registry but the session state was not updated; the source link remains.",
            ));
        }
        // 状态已提交：移除 plans 源链接；目录同步失败只作为警告，不撤回。
        if let Err(error) = plan_catalog::commit_plan_move(
            self.tools.plan_workspace(),
            (file_name.as_str(), PlanLocation::Plans),
            (file_name.as_str(), PlanLocation::Done),
            &expected,
        ) {
            tracing::warn!(event = "plan_move_cleanup_failed", error = %error);
        }
        let document = self.workflow.plan.as_ref().expect("completed plan bound");
        emit(crate::events::AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision: document.revision,
            content: document.content.clone(),
            file_name: document.file_name.clone(),
            location: Some(document.location),
            status: Some(document.status),
        });
        Ok(true)
    }

    /// Verify a review token against the current session, plan revision,
    /// queue and exact on-disk bytes. Returns the on-disk content when it
    /// differs from the token; the caller decides whether that is an error
    /// (`validate_plan_token`) or an import before the same error
    /// (`check_plan_token_for_approval`) — no model request ever runs on a
    /// mismatch.
    fn plan_token_status(&self, token: &ReviewToken, input: &InputQueue) -> Result<Option<String>> {
        if let Some(error) = &self.plan_cleanup_error {
            bail!("{error}");
        }
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        let document = self
            .workflow
            .plan
            .as_ref()
            .ok_or_else(|| anyhow!("{NOT_CURRENT}"))?;
        ensure!(
            document.status == PlanStatus::Ready
                && token.plan_id == document.id
                && token.revision == document.revision
                && token.content == document.content,
            "{NOT_CURRENT}"
        );
        // 重命名/移位/删除后旧路径 token 不能继续授权；与登记表解析出的
        // 实际路径（由按 ID 协调同步）精确比较。
        ensure!(
            document.path().is_some_and(|path| path == token.path),
            "{NOT_CURRENT}"
        );
        self.journal.check()?;
        ensure!(
            self.journal.id() == Some(token.session_id.as_str()),
            "{NOT_CURRENT}"
        );
        ensure!(input.is_empty()?, "{PENDING}");
        let content = self.tools.read_plan(document)?;
        Ok((content != token.content).then_some(content))
    }

    /// Shared by the isolated reviewer: a changed file is reported with the
    /// exact changed message so the caller refreshes via `read_plan`.
    pub(crate) fn validate_plan_token(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        // 校验前按 ID 协调：登记重命名/搬迁使旧 path token 失效。
        self.reconcile_plan_location()?;
        ensure!(self.plan_token_status(token, input)?.is_none(), "{CHANGED}");
        Ok(())
    }

    /// Approval variant of the token check: a changed file is imported as the
    /// next revision before the exact changed message is returned, so the
    /// worker has regenerated the current token state and the UI only needs
    /// to read the refreshed snapshot.
    fn check_plan_token_for_approval(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        self.reconcile_plan_location()?;
        if let Some(content) = self.plan_token_status(token, input)? {
            let title = self
                .workflow
                .plan
                .as_ref()
                .expect("plan bound")
                .title
                .clone();
            self.import_plan_revision(content, &title)?;
            bail!("{CHANGED}");
        }
        Ok(())
    }

    /// 安全核对已保存正文后保留草稿，再退出 Plan；不发起模型请求。
    pub(crate) fn save_plan_and_leave(
        &mut self,
        token: ReviewToken,
        input: &InputQueue,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        ensure!(input.is_empty()?, "{PENDING}");
        let review = self.read_plan()?;
        ensure!(review.token == token, "{CHANGED}");
        ensure!(
            matches!(
                review.document.status,
                PlanStatus::Draft | PlanStatus::Ready
            ),
            "{NOT_CURRENT}"
        );
        let previous = self.workflow.clone();
        self.workflow
            .plan
            .as_mut()
            .expect("saved plan bound")
            .status = PlanStatus::Draft;
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        self.leave_plan(cancellation)
    }

    /// 批准已审阅计划并准备执行交接；可先在 Plan 权限下压缩上下文。
    /// 提交前可取消地连接候选 MCP，再校验 token；失败时停止新 registry，
    /// 保持 Plan/Ready，禁止从未完成的交接发起实施请求。
    pub(crate) fn approve_plan(
        &mut self,
        token: ReviewToken,
        context: ExecutionContext,
        input: &InputQueue,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        // 审批访问前按 ID 协调实际路径；墓碑、搬迁与首次登记在此暴露。
        self.reconcile_plan_location()?;
        // Finish any pending Plan cleanup before touching approval state.
        self.ensure_plan_mcp_shutdown(cancellation)?;
        {
            let document = self
                .workflow
                .plan
                .as_ref()
                .ok_or_else(|| anyhow!("{NOT_CURRENT}"))?;
            ensure!(token.content == document.content, "{NOT_CURRENT}");
            let (_, total) = plan::task_progress(&token.content);
            ensure!(total > 0, "{NO_TASKS}");
        }
        self.check_plan_token_for_approval(&token, input)?;
        if context == ExecutionContext::Compact {
            // 压缩保持 Plan 权限；失败或取消时不交接执行。后续再次核对
            // token 与队列，拒绝压缩期间的外部修订及补充输入。
            let mut attempted = false;
            let outcome = self.compact(cancellation, &mut |event| {
                attempted |= matches!(&event, crate::events::AgentEvent::CompactionStarted { .. });
                emit(event);
            })?;
            ensure!(
                !matches!(outcome, super::context::CompactionOutcome::Cancelled),
                "Plan execution cancelled during context compaction."
            );
            ensure!(
                !attempted || !matches!(outcome, super::context::CompactionOutcome::Unchanged),
                "Context compaction did not reduce history; review the plan before executing."
            );
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
        }
        let config = self
            .catalog
            .resolve(&self.current_model.reference)
            .map_err(|_| anyhow!("current model is unavailable"))?;
        let registry =
            Registry::connect(&config.settings.mcp_servers, self.tools.cwd(), cancellation)?;
        let cleanup = match registry.cleanup() {
            Ok(cleanup) => cleanup,
            Err(error) => return Err(self.abort_execution_preparation(registry, error)),
        };
        // Re-check queue and file after preparation; the approved token bytes
        // remain the execution snapshot.
        if let Err(error) = self.check_plan_token_for_approval(&token, input) {
            return Err(self.abort_execution_preparation(registry, error));
        }
        // 批准必须持有计划 lease（锁顺序 lease → 登记表锁），成功后转交
        // 执行任务持有到 finish_task；失败出口立即释放。
        if let Err(error) = self.ensure_task_plan_lease() {
            self.plan_lease = None;
            return Err(self.abort_execution_preparation(registry, error));
        }
        // 两种批准都以经过 ReviewToken 验证的当前完整正文冻结有序任务
        // 集合（重复文本按序保留）；执行进度导入不得覆盖该字段。列表
        // 重新执行在打开时清空，因此这里冻结的是本次审阅集合。
        {
            let document = self.workflow.plan.as_mut().expect("ready plan bound");
            document.approved_tasks = Some(
                plan::plan_tasks(&token.content)
                    .into_iter()
                    .map(|task| task.text)
                    .collect(),
            );
        }
        // 第二次校验通过后发布 Drafts→Plans；失败保留源链接，返回真实错误。
        let pending_move = match self.promote_approved_plan(&token) {
            Ok(pending) => pending,
            Err(error) => {
                self.plan_lease = None;
                return Err(self.abort_execution_preparation(registry, error));
            }
        };
        match context {
            ExecutionContext::Keep | ExecutionContext::Compact => {
                self.prepare_keep_execution(&token, pending_move, registry, cleanup)
            }
            ExecutionContext::Fresh => {
                self.prepare_fresh_execution(&token, pending_move, registry, cleanup)
            }
        }
    }

    /// Keep-context handoff: append exactly one execution user message to the
    /// current session and commit it together with the Execute workflow as
    /// one atomic journal group. Failure restores the Plan/Ready state and
    /// the native history; the plan file is never rolled back.
    fn prepare_keep_execution(
        &mut self,
        token: &ReviewToken,
        pending_move: Option<PlanMoveEndpoints>,
        registry: Registry,
        cleanup: crate::mcp::Cleanup,
    ) -> Result<ExecutionPreparation> {
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("ready plan bound")
            .clone();
        debug_assert_eq!(document.content, token.content);
        let prompt = crate::plan::execution_prompt(&document, ExecutionContext::Keep);
        // 三种批准交接一致：从批准正文重建执行投影并激活首个未勾选项；
        // 投影与 Execute workflow 一起进入同一个 handoff 事务。
        let mut todos = crate::tools::todo::project_plan_todos(
            &PlanDocument {
                status: PlanStatus::Executed,
                ..document.clone()
            },
            &[],
            false,
        )?;
        crate::tools::todo::normalize_in_progress(&mut todos);
        let todo_plan_id = Some(document.id.clone());
        let previous_workflow = self.workflow.clone();
        let previous_system = SavedInstructions::take(&mut self.session);
        let previous_pending = self.pending_instructions.take();
        self.append_user(&prompt);
        self.workflow.mode = WorkMode::Execute;
        if let Some(plan) = &mut self.workflow.plan {
            plan.status = PlanStatus::Executed;
        }
        self.workflow.todos = todos;
        self.workflow.todo_plan_id = todo_plan_id;
        let mut definitions = self.tools.all_definitions(WorkMode::Execute);
        definitions.extend(registry.definitions());
        self.session.set_tools(definitions);
        let prepared = (|| -> Result<()> {
            // Switch the system prompt to the Execute progress rules before
            // the synced commit so the stored state matches the handoff.
            self.refresh_instructions()?;
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            self.journal.commit_plan_handoff(
                &metadata,
                (&self.session).into(),
                &self.workflow,
                &prompt,
            )
        })();
        if let Err(error) = prepared {
            self.workflow = previous_workflow;
            self.remove_last_user();
            previous_system.restore(&mut self.session);
            self.pending_instructions = previous_pending;
            self.session
                .set_tools(self.tools.all_definitions(WorkMode::Plan));
            // 交接失败：已发布的目标链接与登记保留，源链接尚未移除；
            // 旧 path token 失效，重试从 Plans 幂等重入。
            return Err(self.abort_execution_preparation(registry, error));
        }
        // 批准初始化快照先记入目标 journal；再随交接发布给 UI。
        let todo_snapshot = super::todo_snapshot(&self.workflow);
        if let Some(event) = &todo_snapshot {
            self.journal.event(event);
            self.journal.check()?;
        }
        self.finish_approved_move(pending_move, token);
        self.tools.mcp = registry;
        self.pending_instructions = None;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            cleanup,
            todo_snapshot,
        })
    }

    /// Fresh-context handoff: prepare an empty current-model session, a blank
    /// journal fork and an Execute workflow carrying only the approved plan,
    /// then commit the target checkpoint and the source handoff record. Only
    /// when both commits succeed is the working session replaced; the
    /// planning session stays in Plan mode with an Executed (read-only) plan.
    fn prepare_fresh_execution(
        &mut self,
        token: &ReviewToken,
        pending_move: Option<PlanMoveEndpoints>,
        registry: Registry,
        cleanup: crate::mcp::Cleanup,
    ) -> Result<ExecutionPreparation> {
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("ready plan bound")
            .clone();
        debug_assert_eq!(document.content, token.content);
        // 候选执行 workflow 从批准正文重建执行投影并激活首个未勾选项；
        // 与目标 checkpoint 一起进入同一个 handoff 事务。
        let candidate_plan = PlanDocument {
            status: PlanStatus::Executed,
            ..document.clone()
        };
        let todos = crate::tools::todo::project_plan_todos(&candidate_plan, &[], false)?;
        let candidate_workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(candidate_plan),
            todos,
            todo_plan_id: Some(document.id.clone()),
            ..Default::default()
        };
        let prompt = crate::plan::execution_prompt(&document, ExecutionContext::Fresh);
        let previous = self.workflow.clone();
        // Reload current project and skill rules; the candidate session reuses
        // the current model, thinking level and edit mode, never the planning
        // native history.
        let prepared = (|| -> Result<(Session, SessionJournal)> {
            let mut definitions = self.tools.all_definitions(WorkMode::Execute);
            definitions.extend(registry.definitions());
            let system = self.compose_prompt(
                &candidate_workflow.instructions(),
                "execute",
                &self.current_model.model,
                definitions.iter().map(|tool| tool.name.clone()).collect(),
            )?;
            let mut session = Session::new(
                self.current_model.model.clone(),
                self.current_model.protocol,
                system,
                self.tools.edit_mode(),
            );
            session.set_think_level(self.think_level);
            session.set_tools(definitions);
            super::append_user_to(&mut session, &prompt);
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            let mut journal = self.journal.fork_empty();
            // 目标执行会话先提交，随后才是源规划会话的 handoff 记录。
            journal.commit_plan_handoff(
                &metadata,
                (&session).into(),
                &candidate_workflow,
                &prompt,
            )?;
            // 批准初始化快照记入目标 journal；失败整体放弃候选。
            if let Some(event) = super::todo_snapshot(&candidate_workflow) {
                journal.event(&event);
                journal.check()?;
            }
            Ok((session, journal))
        })();
        let (session, journal) = match prepared {
            Ok(pair) => pair,
            Err(error) => {
                // The candidate session file may remain in the session list;
                // the planning session is untouched and stays Plan/Ready.
                return Err(self.abort_execution_preparation(registry, error));
            }
        };
        // Source handoff record: plan executed, planning session stays in Plan
        // mode for read-only review; bare /plan toggles out without rebinding.
        if let Some(plan) = &mut self.workflow.plan {
            plan.status = PlanStatus::Executed;
        }
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            // 两个 journal 未全部成功：源链接保留，待按 ID 协调。
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.session = session;
        self.journal = journal;
        self.workflow = candidate_workflow;
        self.tools.clear_changes();
        self.last_request = None;
        self.pending_instructions = None;
        self.pending_project_load = None;
        self.reported_project_load = None;
        self.tools.mcp = registry;
        self.finish_approved_move(pending_move, token);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(Vec::new()),
            cleanup,
            // 空 replay 的新会话总是携带批准初始化快照；源规划会话保持
            // 只读预览，绝不伪装成已执行。
            todo_snapshot: super::todo_snapshot(&self.workflow),
        })
    }

    /// Resume a saved session in the worker, adapting the managed MCP
    /// registry to the target workflow mode through the same safe cleanup
    /// path: entering a Plan session stops the current registry first, and a
    /// Plan session never connects MCP at all.
    pub(crate) fn resume(
        &mut self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        let mut restored = self.prepare_resume(id)?;
        let target_mode = restored.checkpoint.workflow.mode;
        match (self.workflow.mode, target_mode) {
            (WorkMode::Plan, WorkMode::Plan) => {}
            (WorkMode::Plan, WorkMode::Execute) => {
                // The Plan registry is empty; finish any pending cleanup, then
                // connect ordinary execution MCP.
                self.ensure_plan_mcp_shutdown(cancellation)?;
                let config = self
                    .catalog
                    .resolve(&self.current_model.reference)
                    .map_err(|_| anyhow!("current model is unavailable"))?;
                self.tools.mcp = Registry::connect(
                    &config.settings.mcp_servers,
                    self.tools.cwd(),
                    cancellation,
                )?;
            }
            (WorkMode::Execute, WorkMode::Plan) => {
                // Stop the managed registry before the planning session can
                // rely on the Plan gate; failure aborts the resume.
                self.ensure_plan_mcp_shutdown(cancellation)?;
            }
            (WorkMode::Execute, WorkMode::Execute) => {}
        }
        let cleanup = self.tools.mcp.cleanup()?;
        let todo_snapshot = sync_restored_todos(
            &self.tools,
            &mut restored.journal,
            &mut restored.checkpoint.workflow,
            &restored.replay,
        )?;
        restored.journal.history_replaced();
        self.session = restored.checkpoint.session;
        self.workflow = restored.checkpoint.workflow;
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        self.journal = restored.journal;
        self.pending_project_load = None;
        self.reported_project_load = super::reported_project_load(&restored.replay);
        self.replay = restored.replay;
        self.tools.clear_changes();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(true);
        }
        self.last_request = None;
        self.pending_instructions = None;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(std::mem::take(&mut self.replay)),
            cleanup,
            todo_snapshot,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{
        EXPLORATION, ORIGIN, PLAN_CONTENT, err_message, journal_workflow, plan_workflow,
        runtime_input_agent, seed_session, write_plan_file,
    };
    use crate::config::{EditMode, ModelCatalog};
    use crate::tools::Tools;
    use serde_json::json;

    fn user_texts(agent: &Agent) -> Vec<String> {
        let native = serde_json::to_value(&agent.session).unwrap();
        let request = &native["request"];
        let entries = match request["messages"].as_array() {
            Some(messages) => messages.clone(),
            None => request["input"].as_array().unwrap().clone(),
        };
        entries
            .iter()
            .filter(|entry| entry["role"] == "user")
            .map(|entry| match &entry["content"] {
                serde_json::Value::String(text) => text.clone(),
                parts => parts
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    fn session_tool_count(agent: &Agent) -> usize {
        serde_json::to_value(&agent.session).unwrap()["request"]["tools"]
            .as_array()
            .unwrap()
            .len()
    }

    fn plan_path(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
        dir.join(".pp/plans").join(format!("{id}.md"))
    }

    #[test]
    fn plan_external_import_registers_revisions_and_respects_readonly_statuses() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);

            // Unchanged content keeps the revision and stays Draft.
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 1);
            assert_eq!(review.document.status, PlanStatus::Draft);
            assert_eq!(review.token.content, PLAN_CONTENT);
            assert_eq!(review.token.revision, 1);
            assert_eq!(review.token.session_id, agent.session_id().unwrap());

            // An external edit becomes the next revision and reaches Ready
            // only with a genuine task list.
            let edited = format!("{PLAN_CONTENT}\nextra context\n");
            write_plan_file(&agent, &edited);
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 2);
            assert_eq!(review.document.status, PlanStatus::Ready);
            assert_eq!(review.token.content, edited);

            // Missing task lists fall back to Draft.
            write_plan_file(&agent, "# Create hello\n\nno tasks here\n");
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 3);
            assert_eq!(review.document.status, PlanStatus::Draft);

            // A missing file is an error, never a journal fallback.
            let bound_path = agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .path()
                .expect("registered plan has a path");
            std::fs::remove_file(dir.path().join(bound_path)).unwrap();
            assert!(agent.read_plan().is_err());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().revision,
                3,
                "failed import never mutates the snapshot"
            );

            // Executed plans refresh their displayed snapshot (execution
            // progress) without regaining approval eligibility.
            let document = agent.workflow.plan.as_mut().unwrap();
            document.status = PlanStatus::Executed;
            write_plan_file(&agent, "- [ ] changed externally\n");
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 4);
            assert_eq!(review.document.status, PlanStatus::Executed);
            assert_eq!(review.document.content, "- [ ] changed externally\n");

            // A revision-0 binding has no saved path: neither an origin
            // requirement nor a hand-written import applies.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, ORIGIN);
            agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
            let error = err_message(agent.read_plan());
            assert_eq!(error, "No saved plan.");

            // Without origin the review is refused before any import.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow("", "", PlanStatus::Draft);
            let error = err_message(agent.read_plan());
            assert_eq!(error, "No saved plan.");
        }
    }

    #[test]
    fn plan_token_validation_denies_stale_missing_and_pending() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let queue = crate::agent::input::InputQueue::default();
            agent.validate_plan_token(&token, &queue).unwrap();

            // Pending input denies approval without touching any state.
            queue.begin_run().unwrap();
            queue
                .enqueue(
                    crate::agent::input::PendingKind::Steering,
                    "late question".into(),
                    None,
                    false,
                )
                .unwrap();
            let error = agent
                .validate_plan_token(&token, &queue)
                .unwrap_err()
                .to_string();
            assert_eq!(error, "Apply pending input before approving the plan.");
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert!(queue.claim_steering(false).unwrap().len() == 1);

            // Changed bytes report the exact changed message without a model
            // request; approval itself imports the new revision so the next
            // ReadPlan merely refreshes the token.
            let edited = format!("{PLAN_CONTENT}\nexternal\n");
            write_plan_file(&agent, &edited);
            let error = agent
                .validate_plan_token(&token, &queue)
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                "Plan changed; review the new revision before executing."
            );
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 1);
            let error = err_message(agent.approve_plan(
                token.clone(),
                ExecutionContext::Keep,
                &queue,
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(
                error,
                "Plan changed; review the new revision before executing."
            );
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 2);
            agent.validate_plan_token(&review.token, &queue).unwrap();

            // Stale session and stale revision tokens are no longer current.
            let mut stale = review.token.clone();
            stale.session_id = "other-session".into();
            assert_eq!(
                agent
                    .validate_plan_token(&stale, &queue)
                    .unwrap_err()
                    .to_string(),
                NOT_CURRENT
            );
            let mut stale = review.token.clone();
            stale.revision = 1;
            assert_eq!(
                agent
                    .validate_plan_token(&stale, &queue)
                    .unwrap_err()
                    .to_string(),
                NOT_CURRENT
            );

            // A token bound to another path (rename/move) is no longer current.
            let mut stale = review.token.clone();
            stale.path = ".pp/plans/moved-away.md".into();
            assert_eq!(
                agent
                    .validate_plan_token(&stale, &queue)
                    .unwrap_err()
                    .to_string(),
                NOT_CURRENT
            );

            // A deleted file blocks approval instead of using the snapshot.
            let bound_path = agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .path()
                .expect("registered plan has a path");
            std::fs::remove_file(dir.path().join(bound_path)).unwrap();
            assert!(agent.validate_plan_token(&review.token, &queue).is_err());
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);

            // The cleanup gate blocks validation and ordinary tasks alike.
            agent.plan_cleanup_error = Some(
                "MCP servers could not be stopped for Plan mode: boom; run /plan again to retry."
                    .into(),
            );
            let error = agent
                .validate_plan_token(&review.token, &queue)
                .unwrap_err()
                .to_string();
            assert!(error.contains("run /plan again to retry"));
            let outcome = agent.run(
                "research task",
                &CancellationToken::new(),
                None,
                &mut |_| {},
            );
            assert_eq!(
                outcome.unwrap_err().to_string(),
                "MCP servers could not be stopped for Plan mode: boom; run /plan again to retry."
            );

            // A successful retry through /plan clears the gate.
            agent.enter_plan(&CancellationToken::new()).unwrap();
            assert!(agent.plan_cleanup_error.is_none());
        }
    }

    #[test]
    fn plan_toggle_enters_standby_and_leave_unbinds_without_abandoning() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            let session_id = agent.session_id().unwrap().to_owned();

            // 进入 Plan 只待命：Plan+None，不分配文档也不创建文件。
            let workflow = agent.enter_plan(&CancellationToken::new()).unwrap();
            assert_eq!(workflow.mode, WorkMode::Plan);
            assert!(workflow.plan.is_none());
            assert_eq!(journal_workflow(dir.path(), &session_id), workflow);

            // 已提交任务但 revision-0 尚未首存：退出被拒绝且工作流不变。
            let mut unsaved = crate::plan::PlanDocument::new().unwrap();
            unsaved.origin_prompt = "first task".into();
            agent.workflow.plan = Some(unsaved);
            assert_eq!(
                err_message(agent.leave_plan(&CancellationToken::new())),
                "Save the draft with plan_write before leaving Plan mode."
            );
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert!(agent.workflow.plan.is_some());

            // 已保存草稿退出：解除绑定而非 Abandoned；文件保留在磁盘。
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            let path = plan_path(
                dir.path(),
                &agent.workflow.plan.as_ref().unwrap().id.clone(),
            );
            let preparation = agent.leave_plan(&CancellationToken::new()).unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert!(preparation.workflow.plan.is_none());
            assert!(preparation.replay.is_none());
            assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));
            assert_eq!(
                journal_workflow(dir.path(), &session_id),
                preparation.workflow
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAN_CONTENT);

            // Leaving in Execute mode is refused.
            assert_eq!(
                err_message(agent.leave_plan(&CancellationToken::new())),
                NOT_PLAN
            );
        }
    }

    #[test]
    fn plan_save_and_leave_requires_current_saved_bytes_and_preserves_failures() {
        for protocol in ["completion", "response"] {
            for scenario in ["ready", "draft", "changed", "pending", "readonly"] {
                let dir = tempfile::tempdir().unwrap();
                let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
                agent.workflow = plan_workflow(
                    PLAN_CONTENT,
                    ORIGIN,
                    if scenario == "draft" {
                        PlanStatus::Draft
                    } else {
                        PlanStatus::Ready
                    },
                );
                write_plan_file(&agent, PLAN_CONTENT);
                seed_session(&mut agent, EXPLORATION);
                let token = agent.read_plan().unwrap().token;
                let path = dir.path().join(&token.path);
                let queue = InputQueue::default();
                if scenario == "changed" {
                    std::fs::write(&path, format!("{PLAN_CONTENT}\nExternal change\n")).unwrap();
                } else if scenario == "pending" {
                    queue.begin_run().unwrap();
                    queue
                        .enqueue(
                            crate::agent::input::PendingKind::Steering,
                            "late question".into(),
                            None,
                            false,
                        )
                        .unwrap();
                } else if scenario == "readonly" {
                    let journal = dir
                        .path()
                        .join("sessions")
                        .join(format!("{}.jsonl", token.session_id));
                    agent.journal.use_read_only_file(&journal);
                }
                let bytes = std::fs::read(&path).unwrap();
                let result = agent.save_plan_and_leave(token, &queue, &CancellationToken::new());
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                if matches!(scenario, "ready" | "draft") {
                    let preparation = result.unwrap();
                    assert_eq!(preparation.workflow.mode, WorkMode::Execute);
                    assert!(preparation.workflow.plan.is_none());
                    assert!(preparation.replay.is_none());
                } else {
                    assert!(result.is_err(), "{scenario}");
                    assert_eq!(agent.workflow.mode, WorkMode::Plan);
                    assert!(agent.workflow.plan.is_some());
                }
                assert_eq!(user_texts(&agent).len(), 1);
            }
        }
    }

    #[test]
    fn plan_keep_approval_appends_one_message_and_rolls_back_on_failure() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let session_id = agent.session_id().unwrap().to_owned();
            let token = agent.read_plan().unwrap().token;

            let preparation = agent
                .approve_plan(
                    token.clone(),
                    ExecutionContext::Keep,
                    &crate::agent::input::InputQueue::default(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );
            assert!(preparation.workflow.review.is_none());
            assert!(preparation.replay.is_none());
            assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));

            // Exactly one execution message was appended, containing the
            // origin and the full approved plan.
            let texts = user_texts(&agent);
            assert_eq!(texts.len(), 2);
            assert_eq!(texts[0], EXPLORATION);
            assert!(texts[1].contains(ORIGIN));
            assert!(texts[1].contains(PLAN_CONTENT));
            // Tool definitions and system instructions follow Execute.
            assert_eq!(
                session_tool_count(&agent),
                agent.tools.all_definitions(WorkMode::Execute).len()
            );
            let native = serde_json::to_value(&agent.session).unwrap();
            let instructions = match &native["request"]["instructions"] {
                serde_json::Value::String(text) => text.clone(),
                _ => native["request"]["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            };
            assert!(instructions.contains("EXECUTE MODE"));
            assert!(!instructions.contains("PLAN MODE"));
            // The persisted state matches the handoff.
            assert_eq!(
                journal_workflow(dir.path(), &session_id),
                preparation.workflow
            );
            // 审批交接：execution prompt 指向 plans 路径；Drafts 源链接已
            // 清理；登记表条目指向 Plans。
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            assert!(texts[1].contains(&format!(".pp/plans/{id}.md")));
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().location,
                PlanLocation::Plans
            );
            assert!(
                !dir.path()
                    .join(".pp/drafts")
                    .join(format!("{id}.md"))
                    .exists()
            );
            {
                let workspace = agent.tools.plan_workspace();
                let _lock =
                    crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
                let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
                let entry = snapshot.catalog.active_by_id(&id).unwrap();
                assert_eq!(entry.file_name, format!("{id}.md"));
                assert_eq!(entry.location, PlanLocation::Plans);
            }

            // A second approval is refused; the session token is spent.
            let error = err_message(agent.approve_plan(
                token,
                ExecutionContext::Keep,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(error, NOT_CURRENT);
            assert_eq!(user_texts(&agent).len(), 2);
            drop(preparation);

            // A journal write failure rolls back the whole handoff.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let id = agent.session_id().unwrap().to_owned();
            let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
            agent.journal.use_read_only_file(&path);
            let error = err_message(agent.approve_plan(
                token,
                ExecutionContext::Keep,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            ));
            // journal 只读后审批在首次持久化点失败：workflow 保持 Plan/Ready，
            // 已发布的目标链接保留，源链接与登记/绑定状态如实保留。
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert_eq!(user_texts(&agent), vec![EXPLORATION.to_owned()]);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            assert_eq!(
                std::fs::read(plan_path(dir.path(), &id)).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
            drop(error);
        }
    }

    #[test]
    fn plan_fresh_approval_starts_an_empty_session_and_keeps_source_readable() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            agent.workflow.review = Some(crate::plan::PlanReviewReport {
                plan_id: agent.workflow.plan.as_ref().unwrap().id.clone(),
                revision: 1,
                model_reference: "fixture/one".into(),
                content: "- [ ] review finding\n".into(),
            });
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let planning_id = agent.session_id().unwrap().to_owned();
            let token = agent.read_plan().unwrap().token;

            let preparation = agent
                .approve_plan(
                    token,
                    ExecutionContext::Fresh,
                    &crate::agent::input::InputQueue::default(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            // A fresh session with an empty replay was handed over.
            assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
            let execution_id = preparation.session_id.clone().unwrap();
            assert_ne!(execution_id, planning_id);
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );
            assert!(preparation.workflow.review.is_none());

            // The fresh history carries only the execution prompt: no origin,
            // no planning chat, no review report.
            let texts = user_texts(&agent);
            assert_eq!(texts.len(), 1);
            assert!(texts[0].contains(PLAN_CONTENT));
            assert!(!texts[0].contains("ORIGIN-MARKER"));
            assert!(!texts[0].contains("EXPLORATION-MARKER"));
            assert!(!texts[0].contains("review finding"));
            assert_eq!(
                session_tool_count(&agent),
                agent.tools.all_definitions(WorkMode::Execute).len()
            );

            // Both journals exist: the planning session stays in Plan mode
            // with an Executed, read-only plan binding.
            let source = journal_workflow(dir.path(), &planning_id);
            assert_eq!(source.mode, WorkMode::Plan);
            assert_eq!(source.plan.as_ref().unwrap().status, PlanStatus::Executed);
            assert_eq!(
                journal_workflow(dir.path(), &execution_id),
                preparation.workflow
            );
            // 两个绑定都指向同一 Plans 路径；Drafts 源链接已清理。
            let id = source.plan.as_ref().unwrap().id.clone();
            assert!(texts[0].contains(&format!(".pp/plans/{id}.md")));
            assert!(
                !dir.path()
                    .join(".pp/drafts")
                    .join(format!("{id}.md"))
                    .exists()
            );
            drop(preparation);

            // Source handoff failure keeps the planning session intact.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let id = agent.session_id().unwrap().to_owned();
            let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
            agent.journal.use_read_only_file(&path);
            let error = err_message(agent.approve_plan(
                token,
                ExecutionContext::Fresh,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            ));
            // journal 只读后审批在首次持久化点失败：workflow 保持 Plan/Ready，
            // 已发布的目标链接保留，源规划会话未被替换。
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert_eq!(user_texts(&agent), vec![EXPLORATION.to_owned()]);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            assert_eq!(
                std::fs::read(plan_path(dir.path(), &id)).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
            drop(error);
        }
    }

    #[test]
    fn plan_progress_refresh_keeps_verified_snapshot_when_file_becomes_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        agent.workflow.mode = WorkMode::Execute;
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        write_plan_file(&agent, &progress);
        let mut published = Vec::new();
        agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap();
        assert!(
            matches!(published.as_slice(), [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, content, .. }] if content == &progress)
        );
        let previous = agent.workflow.clone();
        let path = agent.tools.cwd().join(
            agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .path()
                .expect("legacy plan has a resolved path"),
        );
        std::fs::remove_file(&path).unwrap();
        published.clear();
        let error = agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap_err();
        assert!(format!("{error:#}").contains("last synchronized progress retained"));
        assert_eq!(agent.workflow, previous);
        assert!(published.is_empty());
        assert!(!path.exists());
    }

    // --- 成功收尾自动归档（阶段三）---

    const CANARY_FILE: &str = "create-canary.md";

    /// Execute + Executed + Plans 的归档夹具：AI 文件名已绑定、登记表已
    /// 注册 Plans 身份、磁盘上已有初始正文。
    fn archive_fixture(
        dir: &std::path::Path,
        protocol: &str,
        content: &str,
        approved_tasks: Option<Vec<String>>,
    ) -> Agent {
        let mut agent = runtime_input_agent(dir, protocol, "127.0.0.1:1", "");
        let mut workflow = plan_workflow(content, ORIGIN, PlanStatus::Executed);
        workflow.mode = WorkMode::Execute;
        {
            let document = workflow.plan.as_mut().expect("plan bound");
            document.file_name = Some(CANARY_FILE.to_owned());
            document.approved_tasks = approved_tasks;
        }
        agent.workflow = workflow;
        register_entry(
            &agent,
            agent.workflow.plan.as_ref().unwrap().id.as_str(),
            CANARY_FILE,
            PlanLocation::Plans,
        );
        seed_session(&mut agent, EXPLORATION);
        write_plan_file(&agent, content);
        agent
    }

    fn archive_paths(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        (
            dir.join(".pp/plans").join(CANARY_FILE),
            dir.join(".pp/done").join(CANARY_FILE),
        )
    }

    fn archive_skipped(agent: &mut Agent, content: &str) -> bool {
        write_plan_file(agent, content);
        !agent
            .archive_completed_plan(&mut |_event| {})
            .expect("skip is not an error")
    }

    fn archive_error(agent: &mut Agent, content: &str) -> String {
        write_plan_file(agent, content);
        err_message(agent.archive_completed_plan(&mut |_event| {}))
    }

    #[test]
    fn plan_archive_moves_completed_plan_to_done_under_its_file_name() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
            let mut agent =
                archive_fixture(dir.path(), protocol, PLAN_CONTENT, Some(approved.clone()));
            let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
            write_plan_file(&agent, &progress);
            let mut published = Vec::new();
            let archived = agent
                .archive_completed_plan(&mut |event| published.push(event))
                .unwrap();
            assert!(archived);
            let (plans, done) = archive_paths(dir.path());
            assert!(!plans.exists());
            assert_eq!(std::fs::read(&done).unwrap(), progress.as_bytes());
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.status, PlanStatus::Completed);
            assert_eq!(document.location, PlanLocation::Done);
            assert_eq!(document.revision, 2);
            assert_eq!(document.file_name.as_deref(), Some(CANARY_FILE));
            // 进度导入不覆盖批准时冻结的任务集合。
            assert_eq!(
                document.approved_tasks.as_deref(),
                Some(approved.as_slice())
            );
            // 正文同步事件 + 纯状态归档事件；归档不递增正文 revision。
            assert!(matches!(
                published.as_slice(),
                [
                    crate::events::AgentEvent::PlanProgressUpdated {
                        revision: 2,
                        status: Some(PlanStatus::Executed),
                        location: Some(PlanLocation::Plans),
                        ..
                    },
                    crate::events::AgentEvent::PlanProgressUpdated {
                        revision: 2,
                        status: Some(PlanStatus::Completed),
                        location: Some(PlanLocation::Done),
                        file_name: Some(name),
                        ..
                    },
                ] if name == CANARY_FILE
            ));
            // 登记表指向 Done；重复归档无害。
            {
                let workspace = agent.tools.plan_workspace();
                let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
                let snapshot = plan_catalog::load_catalog(workspace).unwrap();
                let entry = snapshot.catalog.active_by_id(&document.id).unwrap();
                assert_eq!(entry.location, PlanLocation::Done);
            }
            assert!(!agent.archive_completed_plan(&mut |_| {}).unwrap());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Completed
            );
            assert!(done.exists());
        }
    }

    #[test]
    fn plan_archive_succeeds_without_another_body_change() {
        // 正文已在之前的进度刷新中同步：归档不需要正文再次变化。
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
            let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
            let mut agent =
                archive_fixture(dir.path(), protocol, &progress, Some(approved.clone()));
            {
                let document = agent.workflow.plan.as_mut().unwrap();
                document.revision = 2;
                document.content = progress.clone();
            }
            let mut published = Vec::new();
            let archived = agent
                .archive_completed_plan(&mut |event| published.push(event))
                .unwrap();
            assert!(archived);
            let (plans, done) = archive_paths(dir.path());
            assert!(!plans.exists());
            assert!(done.exists());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().revision,
                2,
                "归档不递增正文 revision"
            );
            // 进度刷新已同步、无新变化：只有归档的纯状态事件。
            assert!(matches!(
                published.as_slice(),
                [crate::events::AgentEvent::PlanProgressUpdated {
                    revision: 2,
                    status: Some(PlanStatus::Completed),
                    ..
                }]
            ));
        }
    }

    #[test]
    fn plan_archive_rejects_incomplete_changed_or_empty_task_sets() {
        const FIRST: &str = "Create hello.txt and read back hello.";
        const SECOND: &str = "Verify the failure boundary.";
        let two_tasks = format!("# Create hello\n\n- [ ] {FIRST}\n- [ ] {SECOND}\n");
        let approved = || vec![FIRST.to_owned(), SECOND.to_owned()];

        for protocol in ["completion", "response"] {
            // 未全部勾选：保留 Executed/Plans，不搬迁。
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_fixture(dir.path(), protocol, &two_tasks, Some(approved()));
            let (plans, done) = archive_paths(dir.path());
            let partial = two_tasks.replacen("- [ ]", "- [x]", 1);
            assert!(archive_skipped(&mut agent, &partial));
            assert!(plans.exists() && !done.exists());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().location,
                PlanLocation::Plans
            );

            // 零任务：任务数量与批准向量不同即漂移，同步拒绝安装并按
            // 合同报错（不是静默跳过）；文件与 Executed/Plans 保持不变。
            assert!(
                archive_error(&mut agent, "# Create hello\n\nNo tasks here.\n")
                    .contains(TASKS_CHANGED)
            );
            assert!(plans.exists() && !done.exists());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );

            // 删除任务后勾选剩余项：任务集合不匹配，合同错误。
            let error = archive_error(&mut agent, &format!("- [x] {FIRST}\n"));
            assert!(error.contains(TASKS_CHANGED), "{error}");

            // 增项：批准集合之外的新任务。
            let error = archive_error(
                &mut agent,
                &format!(
                    "{}\n- [x] extra task\n",
                    two_tasks.replace("- [ ]", "- [x]")
                ),
            );
            assert!(error.contains(TASKS_CHANGED), "{error}");

            // 改写任务正文：要求重新规划。
            let replaced = two_tasks.replace(FIRST, "Create hello2.txt and read back hello.");
            let error = archive_error(&mut agent, &replaced.replace("- [ ]", "- [x]"));
            assert!(error.contains(TASKS_CHANGED), "{error}");

            // 重排：独立夹具；集合内容相同但顺序不同也不算完成。
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_fixture(dir.path(), protocol, &two_tasks, Some(approved()));
            let (plans, done) = archive_paths(dir.path());
            let reordered = format!("- [x] {SECOND}\n- [x] {FIRST}\n");
            let error = archive_error(&mut agent, &reordered);
            assert!(error.contains(TASKS_CHANGED), "{error}");
            assert!(plans.exists() && !done.exists());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );
        }
    }

    #[test]
    fn plan_legacy_executed_builds_approved_tasks_baseline_before_disk_progress() {
        // 旧 Executed 记录缺 approved_tasks：先以当前已登记正文建立一次
        // 迁移基线并持久化，再读取新的磁盘进度；磁盘勾选状态与后续普通
        // 说明变化不覆盖基线。
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_fixture(dir.path(), protocol, PLAN_CONTENT, None);
            let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
            write_plan_file(&agent, &progress);
            let mut published = Vec::new();
            agent
                .refresh_plan_progress(&mut |event| published.push(event))
                .unwrap();
            let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(
                document.approved_tasks.as_deref(),
                Some(approved.as_slice())
            );
            let session_id = agent.session_id().unwrap().to_owned();
            let persisted = journal_workflow(dir.path(), &session_id);
            let persisted = persisted.plan.expect("plan persisted");
            assert_eq!(
                persisted.approved_tasks.as_deref(),
                Some(approved.as_slice())
            );
            // 普通说明变化 + 勾选状态：基线保持不变。
            let with_notes = progress.replace(
                "Create hello.txt and read back hello.",
                "Create hello.txt and read back hello.\n\n  说明：验收已人工复核。",
            );
            write_plan_file(&agent, &with_notes);
            agent
                .refresh_plan_progress(&mut |event| published.push(event))
                .unwrap();
            assert_eq!(
                agent
                    .workflow
                    .plan
                    .as_ref()
                    .unwrap()
                    .approved_tasks
                    .as_deref(),
                Some(approved.as_slice())
            );
            // 基线建立后可正常归档。
            let archived = agent
                .archive_completed_plan(&mut |event| published.push(event))
                .unwrap();
            assert!(archived);
            let (plans, done) = archive_paths(dir.path());
            assert!(!plans.exists());
            assert!(done.exists());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Completed
            );
        }
    }

    #[test]
    fn plan_legacy_executed_without_file_name_restores_and_archives_after_baseline() {
        // 旧记录：无 file_name、缺 approved_tasks、走 <id>.md 兜底路径。
        // 首次进度刷新先按 ID 首次登记，再以登记正文建立迁移基线，随后
        // 正常归档到 done/<id>.md。
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let mut workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
            workflow.mode = WorkMode::Execute;
            agent.workflow = workflow;
            seed_session(&mut agent, EXPLORATION);
            let legacy_path = {
                let document = agent.workflow.plan.as_ref().unwrap();
                dir.path().join(document.path().expect("legacy path"))
            };
            std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
            let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
            std::fs::write(&legacy_path, &progress).unwrap();

            let mut published = Vec::new();
            agent
                .refresh_plan_progress(&mut |event| published.push(event))
                .unwrap();
            let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
            {
                let document = agent.workflow.plan.as_ref().unwrap();
                assert_eq!(
                    document.approved_tasks.as_deref(),
                    Some(approved.as_slice())
                );
                let file_name = document.file_name.clone().expect("registered name");
                assert_eq!(file_name, format!("{}.md", document.id));
                assert_eq!(document.location, PlanLocation::Plans);
            }
            assert!(matches!(
                published.as_slice(),
                [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, .. }]
            ));

            let archived = agent
                .archive_completed_plan(&mut |event| published.push(event))
                .unwrap();
            assert!(archived);
            let document = agent.workflow.plan.as_ref().unwrap();
            let done_path = dir
                .path()
                .join(".pp/done")
                .join(document.file_name.clone().expect("name"));
            assert!(!legacy_path.exists());
            assert_eq!(std::fs::read(&done_path).unwrap(), progress.as_bytes());
            assert_eq!(document.status, PlanStatus::Completed);
            assert_eq!(document.location, PlanLocation::Done);
        }
    }

    #[test]
    fn plan_resume_adapts_mcp_and_reports_replay() {
        // prepare_resume loads from the global sessions directory, so the
        // test re-executes itself with HOME pointed at a temporary home.
        const FIXTURE: &str = "PP_TEST_PLAN_RESUME";
        let Ok(root) = std::env::var(FIXTURE) else {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent::plan_workflow::tests::plan_resume_adapts_mcp_and_reports_replay",
                    "--nocapture",
                ])
                .env("HOME", home.path())
                .env(FIXTURE, home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        };
        let root = std::path::Path::new(&root);
        let global = root.join(".pp/agent");
        let sessions = global.join("sessions");
        std::fs::create_dir_all(&global).unwrap();
        for protocol in ["completion", "response"] {
            // A saved Plan-mode session and a saved Execute-mode session.
            let mut planner = fixture_agent(&root.join("plan"), &sessions, protocol);
            planner.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            seed_session(&mut planner, EXPLORATION);
            let planning_id = planner.session_id().unwrap().to_owned();
            let planning_session = serde_json::to_value(&planner.session).unwrap();
            let plan_id = planner.workflow.plan.as_ref().unwrap().id.clone();
            drop(planner);

            // 恢复协调按当前工作目录解析计划文件：resume 目标会话的目录
            // 里也准备同一计划的文件（旧 Plans 绑定按迁移规则迁回 Drafts）。
            let current_plan_dir = root.join("current/.pp/plans");
            std::fs::create_dir_all(&current_plan_dir).unwrap();
            std::fs::write(current_plan_dir.join(format!("{plan_id}.md")), PLAN_CONTENT).unwrap();

            let mut executor = fixture_agent(&root.join("exec"), &sessions, protocol);
            seed_session(&mut executor, "ordinary work");
            let execute_id = executor.session_id().unwrap().to_owned();
            drop(executor);

            // Resuming a Plan session from Execute stops the managed registry
            // before the planning session takes over.
            let mut agent = fixture_agent(&root.join("current"), &sessions, protocol);
            seed_session(&mut agent, "current");
            let preparation = agent
                .resume(&planning_id, &CancellationToken::new())
                .unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Plan);
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert_eq!(
                preparation.session_id.as_deref(),
                Some(planning_id.as_str())
            );
            assert!(preparation.replay.is_some());
            // The history matches; the system prompt is freshly composed for
            // the current working directory and Plan mode.
            let mut native = serde_json::to_value(&agent.session).unwrap();
            match protocol {
                "response" => {
                    native["request"]["instructions"] =
                        planning_session["request"]["instructions"].clone();
                }
                _ => {
                    native["request"]["messages"][0]["content"] =
                        planning_session["request"]["messages"][0]["content"].clone();
                }
            }
            assert_eq!(native, planning_session);

            // Resuming an Execute session from Plan reconnects ordinary MCP.
            let preparation = agent
                .resume(&execute_id, &CancellationToken::new())
                .unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert!(preparation.workflow.plan.is_none());
            assert_eq!(
                session_tool_count(&agent),
                agent.tools.all_definitions(WorkMode::Execute).len()
            );
        }
    }

    // --- 按 ID 协调（reconcile_plan_location）的定向回归 ---

    fn register_entry(agent: &Agent, id: &str, file_name: &str, location: PlanLocation) {
        let workspace = agent.tools.plan_workspace();
        let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        catalog
            .entries
            .push(crate::tools::plan_catalog::PlanCatalogEntry {
                id: id.to_owned(),
                file_name: file_name.to_owned(),
                location,
                deleted: false,
                deleted_file_identity: None,
            });
        crate::tools::plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)
            .unwrap();
    }

    fn registered_entry(agent: &Agent, id: &str) -> (String, PlanLocation) {
        let workspace = agent.tools.plan_workspace();
        let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
        let entry = snapshot.catalog.active_by_id(id).expect("entry registered");
        (entry.file_name.clone(), entry.location)
    }

    #[test]
    fn plan_reconcile_migrates_legacy_draft_to_drafts_and_moves_the_file() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            let legacy_path = format!(".pp/plans/{id}.md");

            // 旧 Draft+Plans 绑定首次登记：保留旧名迁回 Drafts，字节精确保留，
            // 纯路径变化不递增 revision。
            agent.reconcile_plan_location().unwrap();
            {
                let document = agent.workflow.plan.as_ref().unwrap();
                assert_eq!(document.revision, 1);
                assert_eq!(document.content, PLAN_CONTENT);
                assert_eq!(
                    document.file_name.as_deref(),
                    Some(format!("{id}.md").as_str())
                );
                assert_eq!(document.location, PlanLocation::Drafts);
            }
            assert_eq!(
                std::fs::read(dir.path().join(".pp/drafts").join(format!("{id}.md"))).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
            assert!(!dir.path().join(&legacy_path).exists());
            assert_eq!(
                registered_entry(&agent, &id),
                (format!("{id}.md"), PlanLocation::Drafts)
            );

            // 登记解析出的新路径使旧 path token 失效（重命名/移位后旧 token
            // 不能继续授权）。
            agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Ready;
            let stale = ReviewToken {
                session_id: agent.session_id().unwrap().to_owned(),
                plan_id: id.clone(),
                path: legacy_path,
                revision: 1,
                content: PLAN_CONTENT.to_owned(),
            };
            let error = err_message(agent.validate_plan_token(&stale, &InputQueue::default()));
            assert_eq!(error, NOT_CURRENT);

            // 再次协调是幂等的：绑定与登记一致时不再写 journal。
            let before = agent.workflow.clone();
            agent.reconcile_plan_location().unwrap();
            assert_eq!(agent.workflow, before);
        }
    }

    #[test]
    fn plan_reconcile_adopts_registry_and_cleans_confirmed_leftovers() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();

            // 另一会话已把计划重命名/移位：登记优先，绑定采纳新身份。
            let plans = dir.path().join(".pp/plans");
            std::fs::create_dir_all(&plans).unwrap();
            std::fs::write(plans.join("moved-name.md"), PLAN_CONTENT).unwrap();
            register_entry(&agent, &id, "moved-name.md", PlanLocation::Plans);
            // 旧绑定路径 <id>.md 留下同 inode 残留链接。
            std::fs::hard_link(plans.join("moved-name.md"), plans.join(format!("{id}.md")))
                .unwrap();

            agent.reconcile_plan_location().unwrap();
            {
                let document = agent.workflow.plan.as_ref().unwrap();
                assert_eq!(document.file_name.as_deref(), Some("moved-name.md"));
                assert_eq!(document.location, PlanLocation::Plans);
                assert_eq!(document.status, PlanStatus::Executed);
                assert_eq!(document.content, PLAN_CONTENT);
                assert_eq!(document.revision, 1);
            }
            // 同 inode 残留被清理；登记表条目保持原样。
            assert!(!plans.join(format!("{id}.md")).exists());
            assert_eq!(
                std::fs::read(plans.join("moved-name.md")).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
            assert_eq!(
                registered_entry(&agent, &id),
                ("moved-name.md".to_owned(), PlanLocation::Plans)
            );
        }
    }

    #[test]
    fn plan_reconcile_registers_legacy_states_in_place_and_syncs_done() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);

            // 旧 Executed 绑定仍在 Plans 原位登记，不搬迁、不改名。
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
            write_plan_file(&agent, PLAN_CONTENT);
            let executed_id = agent.workflow.plan.as_ref().unwrap().id.clone();
            agent.reconcile_plan_location().unwrap();
            {
                let document = agent.workflow.plan.as_ref().unwrap();
                assert_eq!(
                    document.file_name.as_deref(),
                    Some(format!("{executed_id}.md").as_str())
                );
                assert_eq!(document.location, PlanLocation::Plans);
                assert_eq!(document.status, PlanStatus::Executed);
            }
            assert!(
                dir.path()
                    .join(".pp/plans")
                    .join(format!("{executed_id}.md"))
                    .exists()
            );
            assert_eq!(
                registered_entry(&agent, &executed_id),
                (format!("{executed_id}.md"), PlanLocation::Plans)
            );

            // 登记到 Done 的历史 Executed 只读同步 Completed，不重新授权。
            let done_dir = dir.path().join(".pp/done");
            std::fs::create_dir_all(&done_dir).unwrap();
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
            let done_id = agent.workflow.plan.as_ref().unwrap().id.clone();
            std::fs::write(done_dir.join(format!("{done_id}.md")), PLAN_CONTENT).unwrap();
            register_entry(
                &agent,
                &done_id,
                &format!("{done_id}.md"),
                PlanLocation::Done,
            );
            agent.reconcile_plan_location().unwrap();
            {
                let document = agent.workflow.plan.as_ref().unwrap();
                assert_eq!(document.status, PlanStatus::Completed);
                assert_eq!(document.location, PlanLocation::Done);
                assert_eq!(document.content, PLAN_CONTENT);
            }
            agent.workflow.mode = WorkMode::Execute;
            agent.workflow.validate().unwrap();

            // 旧 Abandoned 保留原位置只读登记。
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Abandoned);
            write_plan_file(&agent, PLAN_CONTENT);
            let abandoned_id = agent.workflow.plan.as_ref().unwrap().id.clone();
            agent.reconcile_plan_location().unwrap();
            {
                let document = agent.workflow.plan.as_ref().unwrap();
                assert_eq!(document.location, PlanLocation::Plans);
                assert_eq!(document.status, PlanStatus::Abandoned);
            }
            assert_eq!(
                registered_entry(&agent, &abandoned_id),
                (format!("{abandoned_id}.md"), PlanLocation::Plans)
            );
        }
    }

    #[test]
    fn plan_reconcile_refuses_tombstones_and_missing_files_without_registration() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);

            // revision-0 绑定直接通过，不建立登记文件。
            agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
            agent.reconcile_plan_location().unwrap();
            assert!(!dir.path().join(".pp/plan-index.json").exists());

            // 墓碑：旧会话访问已删除的计划得到明确错误。
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            {
                let workspace = agent.tools.plan_workspace();
                let _lock =
                    crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
                let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
                let mut catalog = snapshot.catalog;
                catalog
                    .entries
                    .push(crate::tools::plan_catalog::PlanCatalogEntry {
                        id: id.clone(),
                        file_name: format!("{id}.md"),
                        location: PlanLocation::Plans,
                        deleted: true,
                        deleted_file_identity: None,
                    });
                crate::tools::plan_catalog::save_catalog(
                    workspace,
                    snapshot.raw.as_deref(),
                    &catalog,
                )
                .unwrap();
            }
            let error = err_message(agent.reconcile_plan_location());
            assert_eq!(
                error,
                "Plan was deleted; choose another plan from /plan-list."
            );

            // 正文缺失：登记被拒绝，绑定与登记表保持原状，不从快照恢复。
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            let missing_id = agent.workflow.plan.as_ref().unwrap().id.clone();
            let previous = agent.workflow.clone();
            let error = err_message(agent.reconcile_plan_location());
            assert!(!error.is_empty());
            assert_eq!(agent.workflow, previous);
            let workspace = agent.tools.plan_workspace();
            let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
            assert!(snapshot.catalog.active_by_id(&missing_id).is_none());
        }
    }

    // --- todo 生命周期同步（Step 4）---

    fn task_states(agent: &Agent) -> Vec<(String, crate::tools::todo::TodoStatus, Option<String>)> {
        agent
            .workflow
            .todos
            .iter()
            .flat_map(|phase| phase.tasks.iter())
            .map(|task| (task.content.clone(), task.status, task.blocker.clone()))
            .collect()
    }

    /// Execute+Executed 双任务夹具：批准向量冻结、AI 文件名已登记、
    /// 运行态手工置入（第一项 active、第二项 blocked 带原因），磁盘为
    /// 未勾选正文。不播种会话：调用方按需（含 todo 字段裁剪）自行 seed。
    fn executed_todo_fixture(dir: &std::path::Path, protocol: &str) -> (Agent, String, String) {
        const FIRST: &str = "Create hello.txt and read back hello.";
        const SECOND: &str = "Verify the failure boundary.";
        let content = format!("# Create hello\n\n- [ ] {FIRST}\n- [ ] {SECOND}\n");
        let mut agent = runtime_input_agent(dir, protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(&content, ORIGIN, PlanStatus::Executed);
        agent.workflow.mode = WorkMode::Execute;
        let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let file_name = format!("{plan_id}.md");
        {
            let document = agent.workflow.plan.as_mut().unwrap();
            document.file_name = Some(file_name.clone());
            document.approved_tasks = Some(vec![FIRST.to_owned(), SECOND.to_owned()]);
        }
        register_entry(&agent, &plan_id, &file_name, PlanLocation::Plans);
        agent.workflow.todos = vec![crate::tools::todo::TodoPhase {
            name: "Tasks".into(),
            tasks: vec![
                crate::tools::todo::TodoItem {
                    content: FIRST.into(),
                    status: crate::tools::todo::TodoStatus::InProgress,
                    blocker: None,
                },
                crate::tools::todo::TodoItem {
                    content: SECOND.into(),
                    status: crate::tools::todo::TodoStatus::Blocked,
                    blocker: Some("waiting on CI".into()),
                },
            ],
        }];
        agent.workflow.todo_plan_id = Some(plan_id);
        write_plan_file(&agent, &content);
        (agent, FIRST.to_owned(), SECOND.to_owned())
    }

    #[test]
    fn todo_sync_projects_preview_sources_without_active_and_executes_only_executed() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let session_id = agent.session_id().unwrap().to_owned();
            let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();

            // 草稿预览：来源绑定当前计划，全部 pending，无 active；状态持久化。
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(
                agent.workflow.todo_plan_id.as_deref(),
                Some(plan_id.as_str())
            );
            assert_eq!(agent.workflow.todos.len(), 1);
            assert_eq!(agent.workflow.todos[0].name, "Tasks");
            assert_eq!(
                task_states(&agent)[0].1,
                crate::tools::todo::TodoStatus::Pending
            );
            assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);

            // Plan+Executed 来源会话（Fresh 批准后的规划会话）：预览保持
            // 只读，不归一化激活。
            agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Executed;
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(
                task_states(&agent)[0].1,
                crate::tools::todo::TodoStatus::Pending
            );

            // Execute+Executed：执行投影归一化，激活首个未勾选项。
            agent.workflow.mode = WorkMode::Execute;
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(
                task_states(&agent)[0].1,
                crate::tools::todo::TodoStatus::InProgress
            );
            assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);

            // 已用 init 脱离旧计划的独立普通清单：查看/刷新旧 Completed
            // 文档绝不替换它，也不触碰其计划文档。
            let content = PLAN_CONTENT.replace("- [ ]", "- [x]");
            write_plan_file(&agent, &content);
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(
                task_states(&agent)[0].1,
                crate::tools::todo::TodoStatus::Completed
            );
            agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Completed;
            agent.workflow.plan.as_mut().unwrap().location = PlanLocation::Done;
            agent.workflow.todo_plan_id = None;
            agent.workflow.todos = vec![crate::tools::todo::TodoPhase {
                name: "Own".into(),
                tasks: vec![crate::tools::todo::TodoItem {
                    content: "standalone task".into(),
                    status: crate::tools::todo::TodoStatus::Pending,
                    blocker: None,
                }],
            }];
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(agent.workflow.todos[0].name, "Own");
            assert_eq!(agent.workflow.todo_plan_id, None);
        }
    }

    #[test]
    fn plan_todo_sync_merges_external_checkboxes_and_preserves_runtime() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, first, _second) = executed_todo_fixture(dir.path(), protocol);
            seed_session(&mut agent, EXPLORATION);
            let session_id = agent.session_id().unwrap().to_owned();

            // 内容未变：运行态按序保留，不产生新写入或事件。
            let mut published = Vec::new();
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |event| published.push(event),
                false,
            )
            .unwrap();
            assert!(published.is_empty());
            let states = task_states(&agent);
            assert_eq!(states[0].1, crate::tools::todo::TodoStatus::InProgress);
            assert_eq!(states[1].1, crate::tools::todo::TodoStatus::Blocked);
            assert_eq!(states[1].2.as_deref(), Some("waiting on CI"));

            // 外部勾选第一项：正文与 revision 同步，第一项转 completed，
            // 第二项保留 blocked 与原因；blocked 不自动启动。
            let progress = agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .content
                .replacen("- [ ]", "- [x]", 1);
            write_plan_file(&agent, &progress);
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |event| published.push(event),
                false,
            )
            .unwrap();
            assert!(matches!(
                published.as_slice(),
                [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, .. }]
            ));
            let states = task_states(&agent);
            assert_eq!(
                states[0],
                (first, crate::tools::todo::TodoStatus::Completed, None)
            );
            assert_eq!(states[1].1, crate::tools::todo::TodoStatus::Blocked);
            assert_eq!(states[1].2.as_deref(), Some("waiting on CI"));
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);
            assert_eq!(agent.workflow.plan.as_ref().unwrap().content, progress);
            assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);
        }
    }

    #[test]
    fn plan_todo_sync_refuses_approved_vector_drift_and_keeps_trusted_runtime() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, first, second) = executed_todo_fixture(dir.path(), protocol);
            seed_session(&mut agent, EXPLORATION);
            let document = agent.workflow.plan.as_ref().unwrap();
            let trusted = (document.content.clone(), document.revision);
            let trusted_path = agent
                .tools
                .cwd()
                .join(document.path().expect("saved plan path"));
            let trusted_todos = agent.workflow.todos.clone();

            // 改写、增项、删除、重排（即使全部未勾选）都在同步处立即拒绝；
            // 最后可信的正文、revision 与运行态原样保留，磁盘不被覆盖。
            let drifted = [
                format!("# Create hello\n\n- [ ] Rewritten task.\n- [ ] {second}\n"),
                format!("# Create hello\n\n- [ ] {first}\n- [ ] {second}\n- [ ] extra\n"),
                format!("# Create hello\n\n- [ ] {first}\n"),
                format!("- [ ] {second}\n- [ ] {first}\n"),
            ];
            for drift in &drifted {
                std::fs::write(&trusted_path, drift).unwrap();
                let error = err_message(sync_plan_todos(
                    &agent.tools,
                    &mut agent.journal,
                    &mut agent.workflow,
                    &mut |_| {},
                    false,
                ));
                assert!(error.contains(TASKS_CHANGED), "{error}");
                assert_eq!(agent.workflow.plan.as_ref().unwrap().content, trusted.0);
                assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, trusted.1);
                assert_eq!(agent.workflow.todos, trusted_todos);
                assert_eq!(
                    agent.workflow.todo_plan_id.as_deref(),
                    Some(agent.workflow.plan.as_ref().unwrap().id.as_str())
                );
            }

            // 恢复批准任务集合后同步恢复正常。
            std::fs::write(&trusted_path, trusted.0.as_bytes()).unwrap();
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(agent.workflow.todos, trusted_todos);
        }
    }

    #[test]
    fn plan_todo_sync_rolls_back_on_journal_failure_and_keeps_completed_readonly() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, _first, _second) = executed_todo_fixture(dir.path(), protocol);
            seed_session(&mut agent, EXPLORATION);
            let progress = agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .content
                .replacen("- [ ]", "- [x]", 1);
            write_plan_file(&agent, &progress);
            let previous = agent.workflow.clone();
            let session_id = agent.session_id().unwrap().to_owned();
            agent.journal.use_read_only_file(
                &dir.path()
                    .join("sessions")
                    .join(format!("{session_id}.jsonl")),
            );
            let error = err_message(sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            ));
            // journal 只读：同步失败回滚全部内存状态，不假装已同步。
            assert!(error.contains("last synchronized progress retained"));
            assert_eq!(agent.workflow, previous);
            drop(error);

            // 已结束计划仍以实际 Markdown 为准，始终只读，不恢复执行态。
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let done_content = PLAN_CONTENT.replace("- [ ]", "- [x]");
            agent.workflow = plan_workflow(&done_content, ORIGIN, PlanStatus::Completed);
            agent.workflow.mode = WorkMode::Execute;
            let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();
            let done_file = format!("{plan_id}.md");
            {
                let document = agent.workflow.plan.as_mut().unwrap();
                document.location = PlanLocation::Done;
                document.revision = 2;
                document.file_name = Some(done_file.clone());
            }
            agent.workflow.todo_plan_id = Some(plan_id.clone());
            register_entry(&agent, &plan_id, &done_file, PlanLocation::Done);
            let done_path = dir
                .path()
                .join(".pp/done")
                .join(format!("{}.md", agent.workflow.plan.as_ref().unwrap().id));
            std::fs::create_dir_all(done_path.parent().unwrap()).unwrap();
            std::fs::write(&done_path, &done_content).unwrap();
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(
                agent.workflow.todo_plan_id.as_deref(),
                Some(plan_id.as_str())
            );
            assert_eq!(
                task_states(&agent)[0].1,
                crate::tools::todo::TodoStatus::Completed
            );
            std::fs::write(&done_path, done_content.replace("[x]", "[ ]")).unwrap();
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            )
            .unwrap();
            assert_eq!(
                task_states(&agent)[0].1,
                crate::tools::todo::TodoStatus::Pending
            );
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Completed
            );
            let trusted = agent.workflow.clone();
            std::fs::remove_file(&done_path).unwrap();
            assert!(
                sync_plan_todos(
                    &agent.tools,
                    &mut agent.journal,
                    &mut agent.workflow,
                    &mut |_| {},
                    false
                )
                .is_err()
            );
            assert_eq!(agent.workflow, trusted);
        }
    }

    #[test]
    fn todo_resume_syncs_checkboxes_and_carries_snapshot_once() {
        // prepare_resume 从全局 sessions 目录读取，因此用临时 HOME 重入
        // 自身（与 plan_resume 相同模式）。
        const FIXTURE: &str = "PP_TEST_TODO_RESUME";
        let Ok(root) = std::env::var(FIXTURE) else {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent::plan_workflow::tests::todo_resume_syncs_checkboxes_and_carries_snapshot_once",
                    "--nocapture",
                ])
                .env("HOME", home.path())
                .env(FIXTURE, home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        };
        let root = std::path::Path::new(&root);
        let sessions = root.join(".pp/agent/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        for protocol in ["completion", "response"] {
            // 正常缓存已经可信，且没有快照事件：resume 不能复制工具卡片内容。
            let unchanged_dir = root.join("unchanged-exec");
            std::fs::create_dir_all(&unchanged_dir).unwrap();
            let (mut unchanged, _, _) = executed_todo_fixture(&unchanged_dir, protocol);
            unchanged.journal = SessionJournal::new(sessions.clone());
            seed_session(&mut unchanged, "already synchronized runtime");
            let unchanged_id = unchanged.session_id().unwrap().to_owned();
            let unchanged_document = unchanged.workflow.plan.as_ref().unwrap().clone();
            let unchanged_current = root.join("unchanged-current");
            std::fs::create_dir_all(unchanged_current.join(".pp/plans")).unwrap();
            std::fs::write(
                unchanged_current.join(unchanged_document.path().unwrap()),
                &unchanged_document.content,
            )
            .unwrap();
            drop(unchanged);
            let mut resumed = fixture_agent(&unchanged_current, &sessions, protocol);
            seed_session(&mut resumed, "current");
            let preparation = resumed
                .resume(&unchanged_id, &CancellationToken::new())
                .unwrap();
            assert!(preparation.todo_snapshot.is_none());
            assert_eq!(
                task_states(&resumed)[1].1,
                crate::tools::todo::TodoStatus::Blocked
            );
            drop(resumed);
            std::fs::create_dir_all(root.join("exec")).unwrap();
            let (mut source, _first, _second) = executed_todo_fixture(&root.join("exec"), protocol);
            source.journal = SessionJournal::new(sessions.clone());
            // 源会话缺 todo 字段（todos 为空、来源为空）：恢复时初始化。
            source.workflow.todos = Vec::new();
            source.workflow.todo_plan_id = None;
            seed_session(&mut source, EXPLORATION);
            let executed_id = source.session_id().unwrap().to_owned();
            let plan_id = source.workflow.plan.as_ref().unwrap().id.clone();
            // 磁盘进度：第一项已勾选（执行会话目录里准备实际文件）。
            let progress = source
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .content
                .replacen("- [ ]", "- [x]", 1);
            let current_dir = root.join("current");
            std::fs::create_dir_all(current_dir.join(".pp/plans")).unwrap();
            std::fs::write(
                current_dir.join(".pp/plans").join(format!("{plan_id}.md")),
                &progress,
            )
            .unwrap();
            drop(source);

            let mut agent = fixture_agent(&current_dir, &sessions, protocol);
            seed_session(&mut agent, "current");
            let preparation = agent
                .resume(&executed_id, &CancellationToken::new())
                .unwrap();
            // 恢复时按实际 Markdown 初始化执行投影：第一项 completed，
            // 第二项归一化激活；快照随交接发布一次。
            let states = task_states(&agent);
            assert_eq!(states[0].1, crate::tools::todo::TodoStatus::Completed);
            assert_eq!(states[1].1, crate::tools::todo::TodoStatus::InProgress);
            assert_eq!(
                agent.workflow.todo_plan_id.as_deref(),
                Some(plan_id.as_str())
            );
            assert!(matches!(
                &preparation.todo_snapshot,
                Some(crate::events::AgentEvent::TodoSnapshot { plan_id: snapshot_plan, revision: 2, phases, status: PlanStatus::Executed })
                    if snapshot_plan == &plan_id && phases == &agent.workflow.todos
            ));
            assert_eq!(agent.workflow.plan.as_ref().unwrap().content, progress);

            // 再次恢复：replay 已包含相同快照，不重复携带。
            drop(agent);
            let mut agent = fixture_agent(&current_dir, &sessions, protocol);
            seed_session(&mut agent, "current");
            let preparation = agent
                .resume(&executed_id, &CancellationToken::new())
                .unwrap();
            assert!(preparation.todo_snapshot.is_none());
            assert_eq!(
                agent.workflow.todo_plan_id.as_deref(),
                Some(plan_id.as_str())
            );
        }
    }

    /// An agent journaled into the fixture's global sessions directory, as
    /// production does, with its own working directory.
    fn fixture_agent(cwd: &std::path::Path, sessions: &std::path::Path, protocol: &str) -> Agent {
        std::fs::create_dir_all(cwd).unwrap();
        let global = sessions.parent().unwrap();
        let settings = global.join("settings.yml");
        let models = global.join("models.yml");
        std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
        std::fs::write(
            &models,
            json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":protocol,
                "models":{"one": {"model":"one"}}
            }}})
            .to_string(),
        )
        .unwrap();
        Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(cwd.to_owned(), EditMode::Replace).unwrap(),
            sessions.to_owned(),
        )
        .unwrap()
    }
}
