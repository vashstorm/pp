use super::persistence::SessionJournal;
use crate::{
    plan::{self, PlanDocument, PlanLocation, PlanStatus, WorkMode, WorkflowState},
    tools::{Tools, plan_catalog},
};
use anyhow::{Result, bail, ensure};
use std::path::Path;

const DELETED_PLAN: &str = "Plan was deleted; choose another plan from /plan-list.";
const COMPLETED_CONFLICT: &str =
    "Plan registration conflicts with the completed state; resolve it in /plan-list.";
const REGISTER_CONFLICT: &str =
    "Plan file name is registered to another plan; resolve it in /plan-list.";

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
pub(super) fn reconcile_plan_state(
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
        super::turn::publish_plan_progress(workflow, emit);
    }
    let status_changed = workflow.plan.as_ref().expect("plan bound").status != status_before;
    if publish_snapshot && (content_changed || projection_changed || status_changed) {
        super::turn::publish_todo_snapshot(workflow, journal, emit)?;
    }
    Ok(())
}

/// 恢复只携带一次同步新生成的快照；replay 已包含相同快照时不重复追加。
fn resume_todo_snapshot(
    workflow: &WorkflowState,
    replay: &[super::persistence::ReplayRecord],
) -> Option<crate::events::AgentEvent> {
    let event = super::turn::todo_snapshot(workflow)?;
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
pub(super) fn import_plan_content(
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
