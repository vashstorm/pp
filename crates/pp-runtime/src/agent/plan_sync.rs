use super::persistence::SessionJournal;
use super::plan_management::is_missing;
use anyhow::{Result, bail, ensure};
use pp_core::plan;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanStatus;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use pp_tools::Tools;
use pp_tools::plan_catalog;
use std::path::Path;

const DELETED_PLAN: &str = "Plan was deleted; choose another plan from /plan-list.";
const COMPLETED_CONFLICT: &str =
    "Plan registration conflicts with the completed state; resolve it in /plan-list.";
const REGISTER_CONFLICT: &str =
    "Plan file name is registered to another plan; resolve it in /plan-list.";

/// Reconciliation outcome by ID: the binding adopts the registration, registers for the first time, or
/// already agrees.
enum PlanReconciliation {
    /// The archived file is gone; the current session may keep working on its own.
    DetachArchived,
    /// The catalog wins: sync the binding to the canonical name/location/status.
    Adopt {
        file_name: String,
        location: PlanLocation,
        status: PlanStatus,
        leftover: Option<String>,
    },
    /// First registration: register in place under the migration rules without moving the file.
    Register {
        file_name: String,
        location: PlanLocation,
    },
    /// First registration that also moves a legacy Draft/Ready Plans binding to Drafts per the migration
    /// rules.
    RegisterAndMove {
        file_name: String,
        location: PlanLocation,
        source: (String, PlanLocation),
        bytes: Vec<u8>,
    },
}

/// Reconcile plan identity by ID: the catalog always wins, so the bound document is synced to the
/// canonical name/location. Pure path/status changes do not bump the body revision, but they invalidate
/// the old path token. An unregistered legacy binding with revision>=1 registers for the first time under
/// the migration rules: a legacy Draft/Ready Plans binding keeps its name and moves back to Drafts (a
/// safe move), Executed stays in Plans, Abandoned stays where it is read-only, and a historical Executed
/// registered into Done read-only syncs to Completed. When the file of a Done registration is missing the
/// session binding is detached and the project registration is kept. Tombstones and any other missing
/// path/body still error out; never rebuild a file from a historical snapshot or downgrade a lifecycle
/// location.
pub(super) fn reconcile_plan_state(
    workspace: &pp_workspace::save::Workspace,
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
    // A revision>=1 binding always has a resolvable path (the legacy <id>.md fallback or an explicit name).
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
            Some(entry)
                if entry.location == PlanLocation::Done
                    && archived_plan_is_missing(workspace, &entry.file_name)? =>
            {
                PlanReconciliation::DetachArchived
            }
            Some(entry) => {
                let status = if entry.location == PlanLocation::Done {
                    // A historical Executed registered into Done read-only syncs to Completed.
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
                // Unregistered legacy binding: register for the first time under the migration rules. Name
                // conflicts fail closed; never squat on another plan's name.
                let (file_name, mut location) = match &previous_file_name {
                    Some(name) => (name.clone(), previous_location),
                    None => (format!("{id}.md"), previous_location),
                };
                ensure!(
                    snapshot.catalog.active_by_file_name(&file_name).is_none(),
                    "{REGISTER_CONFLICT}"
                );
                // A registration never points at an unreadable path: a missing body errors out here.
                let source = workspace.target(Path::new(&previous_path), false)?;
                let bytes = source.read_bounded(plan::PLAN_MAX_BYTES)?;
                let moved = matches!(previous_status, PlanStatus::Draft | PlanStatus::Ready)
                    && location == PlanLocation::Plans;
                if moved {
                    location = PlanLocation::Drafts;
                    let target_path = plan::path_for(&file_name, location);
                    // Publish the target link first, then register with CAS; a failure keeps the source link.
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
        PlanReconciliation::DetachArchived => detach_archived_plan(journal, workflow),
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

fn archived_plan_is_missing(workspace: &pp_workspace::save::Workspace, name: &str) -> Result<bool> {
    let path = plan::path_for(name, PlanLocation::Done);
    match plan_catalog::file_identity(workspace, Path::new(&path)) {
        Ok(identity) => Ok(identity.is_none()),
        Err(error) if is_missing(&error) => Ok(true),
        Err(error) => Err(error),
    }
}

fn detach_archived_plan(journal: &mut SessionJournal, workflow: &mut WorkflowState) -> Result<()> {
    let previous = workflow.clone();
    workflow.plan = None;
    workflow.review = None;
    if workflow.todo_plan_id.take().is_some() {
        workflow.todos.clear();
    }
    if let Err(error) = journal.write_workflow(workflow) {
        *workflow = previous;
        return Err(error);
    }
    Ok(())
}

/// The binding document's identity triple: whether registration reconciliation changed
/// name/location/status.
fn plan_identity(document: &PlanDocument) -> (Option<String>, PlanLocation, PlanStatus) {
    (
        document.file_name.clone(),
        document.location,
        document.status,
    )
}

/// Read-only projection of a finished plan: derived from Markdown checkbox state, keeping no
/// active/blocker.
fn readonly_phases(document: &PlanDocument) -> Result<Vec<pp_core::todo::TodoPhase>> {
    let mut phases: Vec<pp_core::todo::TodoPhase> = Vec::new();
    let mut region: Option<usize> = None;
    for task in plan::plan_tasks(&document.content) {
        if region != Some(task.phase_index) {
            region = Some(task.phase_index);
            phases.push(pp_core::todo::TodoPhase {
                name: task.phase,
                tasks: Vec::new(),
            });
        }
        phases
            .last_mut()
            .expect("task creates its phase")
            .tasks
            .push(pp_core::todo::TodoItem {
                content: task.text,
                status: if task.completed {
                    pp_core::todo::TodoStatus::Completed
                } else {
                    pp_core::todo::TodoStatus::Pending
                },
                blocker: None,
            });
    }
    Ok(phases)
}

/// Safely import the real Markdown of a bound plan; execution mode also syncs the task projection.
/// Plan only updates body, revision and status, and clears the previous version's TODO preview.
/// Execute+Executed freezes the approval baseline from the old record before reading disk, rejects task
/// set drift and keeps legitimate active/blocked entries; finished plans are read-only and a plain list
/// that got detached is never overwritten. All state is persisted before it is published, and a failure
/// restores the last trusted body and runtime state.
pub(super) fn sync_plan_todos(
    tools: &Tools,
    journal: &mut SessionJournal,
    workflow: &mut WorkflowState,
    emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    publish_snapshot: bool,
) -> Result<()> {
    if workflow.mode == WorkMode::Plan {
        // Legacy-session compatibility: staleness that includes an unbound list left behind by a deleted
        // plan is cleaned up before a request or a resume.
        let previous_todos = std::mem::take(&mut workflow.todos);
        let previous_source = workflow.todo_plan_id.take();
        if !previous_todos.is_empty() || previous_source.is_some() {
            if let Err(error) = journal.write_workflow(workflow) {
                workflow.todos = previous_todos;
                workflow.todo_plan_id = previous_source;
                return Err(error);
            }
            if publish_snapshot {
                super::turn::publish_todo_state(workflow, emit);
            }
        }
    }
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
    let Some(document) = workflow.plan.as_ref() else {
        if publish_snapshot {
            super::turn::publish_todo_state(workflow, emit);
        }
        return Ok(());
    };
    let identity_changed = plan_identity(document) != identity_before;
    let executing = workflow.mode == WorkMode::Execute && document.status == PlanStatus::Executed;
    if executing && document.approved_tasks.is_none() {
        // The migrated baseline may only come from the already registered old body; a newly read task on
        // disk must not become approved afterwards.
        let approved = plan::plan_tasks(&document.content)
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
        let content = match tools.read_plan(document) {
            Ok(content) => content,
            // The file can disappear between the two disk accesses that follow reconciliation.
            Err(error) if document.status == PlanStatus::Completed && is_missing(&error) => {
                detach_archived_plan(journal, workflow)?;
                if publish_snapshot {
                    super::turn::publish_todo_state(workflow, emit);
                }
                return Ok(());
            }
            Err(error) => {
                return Err(error.context(
                    "Plan progress could not be refreshed; last synchronized progress retained.",
                ));
            }
        };
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
        if workflow.mode == WorkMode::Plan {
            Ok(Vec::new())
        } else if executing {
            pp_tools::todo::project_plan_todos(document, &workflow.todos, bound)
        } else {
            readonly_phases(document)
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
    let source = (workflow.mode == WorkMode::Execute)
        .then(|| workflow.plan.as_ref().expect("plan bound").id.clone());
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

/// Resume carries the freshly generated snapshot only once; when replay already holds the same snapshot
/// it is not appended again.
fn resume_todo_snapshot(
    workflow: &WorkflowState,
    replay: &[super::persistence::ReplayRecord],
) -> Option<pp_core::events::AgentEvent> {
    let event = super::turn::todo_snapshot(workflow)?;
    let pp_core::events::AgentEvent::TodoSnapshot {
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
            super::persistence::ReplayRecord::Event(pp_core::events::AgentEvent::TodoSnapshot {
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

/// Startup restore and interactive resume share the same file-sync and snapshot-dedup boundary.
pub(super) fn sync_restored_todos(
    tools: &Tools,
    journal: &mut SessionJournal,
    workflow: &mut WorkflowState,
    replay: &[super::persistence::ReplayRecord],
) -> Result<Option<pp_core::events::AgentEvent>> {
    let Some(document) = workflow.plan.as_ref() else {
        sync_plan_todos(tools, journal, workflow, &mut |_| {}, false)?;
        return Ok(None);
    };
    let previous_plan = (document.id.clone(), document.revision, document.status);
    let previous_todos = workflow.todos.clone();
    let previous_source = workflow.todo_plan_id.clone();
    reconcile_plan_state(tools.plan_workspace(), journal, workflow)?;
    sync_plan_todos(tools, journal, workflow, &mut |_| {}, false)?;
    let Some(document) = workflow.plan.as_ref() else {
        return Ok(None);
    };
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
