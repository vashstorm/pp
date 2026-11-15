//! 计划变更落地与 TODO／进度快照发布。
use super::*;

/// Plan file tracked during execution: only an Execute-mode run with an
/// Executed plan watches `FileChanged` events for plan-progress refreshes.
/// The actual short path is cached so the progress callback stays independent
/// of the workflow borrow without cloning the whole document.
pub(super) fn tracked_plan_path(workflow: &pp_core::plan::WorkflowState) -> Option<String> {
    if workflow.mode == pp_core::plan::WorkMode::Execute {
        workflow
            .plan
            .as_ref()
            .filter(|plan| plan.status == pp_core::plan::PlanStatus::Executed)
            .and_then(|plan| plan.path())
    } else {
        None
    }
}

/// Whether one progress event touches the tracked plan file.
pub(super) fn plan_file_touched(
    tools: &Tools,
    plan_path: &Option<String>,
    event: &AgentEvent,
) -> bool {
    match (event, plan_path) {
        (AgentEvent::FileChanged { path, .. }, Some(expected_path)) => {
            tools.is_plan_path(path, expected_path)
        }
        _ => false,
    }
}

pub(super) fn apply_plan_change(
    workflow: &mut pp_core::plan::WorkflowState,
    journal: &mut SessionJournal,
    sealed: &mut bool,
    change: Option<crate::tools::PlanChange>,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    let Some(change) = change else { return Ok(()) };
    ensure!(
        workflow.mode == pp_core::plan::WorkMode::Plan && !*sealed,
        "Invalid plan tool transition."
    );
    if matches!(change, crate::tools::PlanChange::Submitted) {
        *sealed = true;
        return Ok(());
    }
    let previous = workflow.clone();
    let plan = workflow
        .plan
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("No current plan."))?;
    if let crate::tools::PlanChange::Written {
        title,
        file_name,
        content,
    } = change
    {
        plan.revision = plan.next_revision()?;
        plan.title = title;
        plan.file_name = Some(file_name);
        plan.content = content;
        plan.status = pp_core::plan::PlanStatus::Draft;
    }
    workflow.todos.clear();
    workflow.todo_plan_id = None;
    if let Err(error) = journal.write_workflow(workflow) {
        *workflow = previous;
        return Err(error.context("Plan file saved but session synchronization failed."));
    }
    publish_plan_progress(workflow, emit);
    Ok(())
}

pub(in crate::agent) fn todo_snapshot(
    workflow: &pp_core::plan::WorkflowState,
) -> Option<AgentEvent> {
    if workflow.mode == pp_core::plan::WorkMode::Plan {
        return None;
    }
    let plan = workflow.plan.as_ref()?;
    (workflow.todo_plan_id.as_deref() == Some(plan.id.as_str())).then(|| AgentEvent::TodoSnapshot {
        plan_id: plan.id.clone(),
        revision: plan.revision,
        phases: workflow.todos.clone(),
        status: plan.status,
    })
}

pub(in crate::agent) fn publish_todo_snapshot(
    workflow: &pp_core::plan::WorkflowState,
    journal: &mut SessionJournal,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    if let Some(event) = todo_snapshot(workflow) {
        journal.event(&event);
        journal.check()?;
        emit(event);
        publish_todo_state(workflow, emit);
    }
    Ok(())
}

pub(in crate::agent) fn publish_todo_state(
    workflow: &pp_core::plan::WorkflowState,
    emit: &mut dyn FnMut(AgentEvent),
) {
    emit(AgentEvent::TodoStateUpdated {
        state: pp_core::events::TodoState::from_workflow(workflow),
    });
}

pub(in crate::agent) fn publish_plan_progress(
    workflow: &pp_core::plan::WorkflowState,
    emit: &mut dyn FnMut(AgentEvent),
) {
    if let Some(document) = &workflow.plan {
        emit(AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision: document.revision,
            content: document.content.clone(),
            file_name: document.file_name.clone(),
            location: Some(document.location),
            status: Some(document.status),
        });
    }
}

pub(in crate::agent) fn apply_todo_change(
    workflow: &mut pp_core::plan::WorkflowState,
    journal: &mut SessionJournal,
    change: Option<crate::tools::todo::TodoChange>,
) -> Result<()> {
    let Some(change) = change else { return Ok(()) };
    if let Some(plan_change) = &change.plan {
        let document = workflow
            .plan
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No current plan."))?;
        ensure!(
            document.id == plan_change.plan_id
                && workflow.todo_plan_id.as_deref() == Some(document.id.as_str())
                && plan_change.revision
                    == if plan_change.content.is_some() {
                        document.next_revision()?
                    } else {
                        document.revision
                    },
            "Invalid plan todo transition."
        );
    }
    let source = change.plan.as_ref().map(|plan| plan.plan_id.clone());
    let previous_todos = std::mem::replace(&mut workflow.todos, change.phases);
    let previous_source = std::mem::replace(&mut workflow.todo_plan_id, source);
    let previous_document = change.plan.and_then(|change| {
        change.content.map(|content| {
            let document = workflow.plan.as_mut().expect("plan effect validated");
            (
                std::mem::replace(&mut document.content, content),
                std::mem::replace(&mut document.revision, change.revision),
            )
        })
    });
    let saved_progress = previous_document.is_some();
    if let Err(error) = journal.write_workflow(workflow) {
        workflow.todos = previous_todos;
        workflow.todo_plan_id = previous_source;
        if let Some((content, revision)) = previous_document {
            let document = workflow.plan.as_mut().expect("plan effect validated");
            document.content = content;
            document.revision = revision;
        }
        return Err(if saved_progress {
            error.context("Plan progress saved but session synchronization failed; the plan file was not rolled back.")
        } else {
            error
        });
    }
    Ok(())
}
