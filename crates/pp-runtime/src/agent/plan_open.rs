//! Open an existing plan from `/plan-list`: always continue a draft, or enter the pre-execution review,
//! in a brand-new session that carries only the plan.
//!
//! The candidate session copies no message, reasoning, summary or origin prompt from the original
//! session: the initial sole user content is the local selection action, the plan ID, the real path and
//! the full Markdown snapshot. The current session is replaced only after the candidate journal is fully
//! committed; any failure keeps the original session, mode, input and files unchanged (the candidate
//! session file may stay in the session list, matching the failure residue of a Fresh approval). Opening
//! never calls the model: ContinueDraft waits for the user's next input, and Execute only enters local
//! review, so execution still requires an explicit `approve_plan` approval.

use super::{
    Agent, ExecutionPreparation, Session,
    persistence::Metadata,
    plan_management::{PlanOpenIntent, PlanSelection, plan_display_title},
};
use anyhow::{Result, bail, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::plan;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanStatus;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;

/// A draft that already has committed tasks but whose revision-0 was never first-saved must not be
/// silently dropped by "open another session".
const UNSAVED_DRAFT: &str = "Leave the current Plan mode before opening another plan.";

/// Initial sole user content: local selection action + plan ID + real path + full Markdown snapshot;
/// it never carries the original session's chat or reasoning.
fn selection_prompt(intent: PlanOpenIntent, id: &str, path: &str, content: &str) -> String {
    let action = match intent {
        PlanOpenIntent::ContinueDraft => "Continue planning the selected Markdown plan below.",
        PlanOpenIntent::Execute => "Execute the selected Markdown plan below after local approval.",
    };
    format!(
        "{action}\n\nPlan id: {id}\nPath: {path}\n\n[Selected Markdown snapshot]\n{content}\n[/Selected Markdown snapshot]"
    )
}

/// Fixed origin_prompt copy; `<path>` is the real project-relative path resolved from the catalog.
fn origin_prompt(intent: PlanOpenIntent, path: &str) -> String {
    match intent {
        PlanOpenIntent::ContinueDraft => {
            format!("Continue planning the selected Markdown plan at {path}.")
        }
        PlanOpenIntent::Execute => {
            format!("Execute the selected Markdown plan at {path} after local approval.")
        }
    }
}

impl Agent {
    /// Open a plan from the list: ContinueDraft creates a new Plan/Draft session that continues the
    /// draft; Execute creates a new session and opens the six-item action menu, Ready when a TODO exists
    /// and Draft otherwise (execution still needs local approval, and `validate` explicitly allows the
    /// Plan+Ready+Plans pre-execution review state).
    ///
    /// Order: journal.check on the current session plus workflow/registration sync of the current draft ->
    /// take the plan lease and re-read the registration by ID, verifying the exact bytes -> fully commit
    /// the candidate session/journal -> Plan-mode MCP shutdown gate -> only when everything succeeded is
    /// the current session replaced and an ExecutionPreparation with `replay = Some(empty)` published.
    /// On any failure path the candidate is dropped and the original session/mode/input/files stay
    /// unchanged.
    pub fn open_listed_plan(
        &mut self,
        selection: &PlanSelection,
        intent: PlanOpenIntent,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        self.journal.check()?;
        // An unsaved revision-0 draft has no work to save; save or discard it first so that "open
        // another session" cannot look like an implicit discard.
        if let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            bail!("{UNSAVED_DRAFT}");
        }
        // The workflow/registration of the already saved draft must sync successfully before a candidate
        // is prepared.
        self.reconcile_plan_location()?;
        // Release the lease held by the current task so the same process cannot self-conflict on the same
        // lock file; failure paths do not restore it (the worker only calls between tasks and re-takes it
        // on demand).
        self.plan_lease = None;
        // Take the lease, re-read the registration by ID and verify the current path and exact bytes;
        // from here on the lock order is fixed as lease -> catalog lock.
        let verified = self.verify_selection(selection)?;
        // Intent must match the lifecycle location: continuing is limited to Drafts (constraint c) while
        // execution review allows Drafts/Plans; Done is read-only and cannot be continued or executed from
        // the list.
        ensure!(
            match intent {
                PlanOpenIntent::ContinueDraft => verified.location == PlanLocation::Drafts,
                PlanOpenIntent::Execute => matches!(
                    verified.location,
                    PlanLocation::Drafts | PlanLocation::Plans
                ),
            },
            "This plan cannot be opened from its current location."
        );
        let document = PlanDocument {
            id: verified.id.clone(),
            revision: 1,
            title: plan_display_title(&verified.content, &verified.file_name),
            content: verified.content.clone(),
            status: match intent {
                PlanOpenIntent::ContinueDraft => PlanStatus::Draft,
                PlanOpenIntent::Execute if plan::task_progress(&verified.content).1 > 0 => {
                    PlanStatus::Ready
                }
                PlanOpenIntent::Execute => PlanStatus::Draft,
            },
            origin_prompt: origin_prompt(intent, &verified.path),
            file_name: Some(verified.file_name.clone()),
            location: verified.location,
            approved_tasks: None,
        };
        plan::validate_content(&document.content)?;
        // Loading a plan only creates a planning session; the task projection is initialized after local
        // execution approval.
        let candidate_workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(document),
            ..Default::default()
        };
        candidate_workflow.validate()?;
        let catalog = self.models.catalog_snapshot();
        let (selection, role_route, model_source) = self.resolve_plan_model(&catalog)?;
        let config = catalog.resolve(&selection.reference)?;
        let client = pp_llm::OpenAiClient::new(&config)?;
        let prompt = selection_prompt(intent, &verified.id, &verified.path, &verified.content);
        let prepared = (|| -> Result<(Session, super::persistence::SessionJournal)> {
            let definitions = self.tools.all_definitions(WorkMode::Plan);
            let system = self.compose_prompt(
                &crate::subagent::catalog::workflow_instructions(&candidate_workflow),
                "plan",
                &config.model,
                definitions.iter().map(|tool| tool.name.clone()).collect(),
            )?;
            let mut session = Session::new(
                config.model.clone(),
                config.protocol,
                system,
                self.tools.edit_mode(),
            );
            session.set_think_level(selection.effort);
            session.set_tools(definitions);
            session.apply_cache_policy(
                config.prompt_cache_key,
                config.prompt_cache_retention,
                || super::generate_prompt_cache_key(None),
            );
            super::turn::append_user_to(&mut session, &prompt);
            let metadata = Metadata {
                role_route: role_route.as_ref(),
                cwd: self.tools.cwd(),
                model_reference: &selection.reference,
                think_level: selection.effort,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            // The candidate journal forks independently: the target session is fully committed first, so a
            // commit failure leaves the original session's journal never replaced.
            let mut journal = self.journal.fork_empty();
            journal.commit_plan_handoff(
                &metadata,
                (&session).into(),
                &candidate_workflow,
                &prompt,
            )?;
            Ok((session, journal))
        })();
        let (session, journal) = prepared?;
        self.ensure_plan_mcp_access(cancellation)?;
        // The current session is replaced only after the full commit; only a real switch clears the undo
        // memory entitlement.
        self.catalog = catalog;
        self.role_route = role_route;
        self.session = session;
        self.reset_lsp_diagnostics();
        self.client = client;
        self.model_source = model_source;
        self.current_model = pp_config::config::ModelChoice {
            reference: selection.reference,
            model: config.model,
            protocol: config.protocol,
            thinking: config.thinking,
            context_window: config.context_window,
        };
        self.think_level = selection.effort;
        self.prompt_cache_key_enabled = config.prompt_cache_key;
        self.prompt_cache_retention = config.prompt_cache_retention;
        self.tools.permission.clear_session();
        self.journal = journal;
        self.tools.artifacts = Default::default();
        if let Some(id) = self.journal.id() {
            let _ = self.tools.artifacts.bind(self.journal.directory(), id);
        }
        self.workflow = candidate_workflow;
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        self.tools.clear_caches();
        self.clear_session_usage();
        self.thinking_history.clear();
        self.clear_session_cost();
        self.pending_instructions = None;
        drop(verified);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(Vec::new()),
            todo_snapshot: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{EXPLORATION, err_message, runtime_input_agent, seed_session};
    use pp_tools::plan_catalog::{PlanCatalogEntry, PlanCatalogLock, load_catalog, save_catalog};
    use std::os::unix::fs::PermissionsExt;

    const LISTED_CONTENT: &str =
        "# Listed plan\n\n- [ ] First canary task.\n- [ ] Second canary task.\n";
    const LISTED_FILE: &str = "continue-listed.md";
    const LISTED_ID: &str = "0123456789abcdef0123456789abcdef";

    fn listed_path() -> String {
        plan::path_for(LISTED_FILE, PlanLocation::Drafts)
    }

    fn write_plan_bytes(agent: &Agent, location: PlanLocation, file_name: &str, content: &str) {
        let path = agent.tools.cwd().join(plan::path_for(file_name, location));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn write_listed_file(agent: &Agent) {
        write_plan_bytes(agent, PlanLocation::Drafts, LISTED_FILE, LISTED_CONTENT);
    }

    fn register_entry(agent: &Agent, id: &str, file_name: &str, location: PlanLocation) {
        let workspace = agent.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted: false,
            deleted_file_identity: None,
        });
        save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
    }

    fn register_listed_entry(agent: &Agent) {
        register_entry(agent, LISTED_ID, LISTED_FILE, PlanLocation::Drafts);
    }

    fn selection() -> PlanSelection {
        PlanSelection {
            id: LISTED_ID.to_owned(),
            path: listed_path(),
            content: LISTED_CONTENT.to_owned(),
        }
    }

    /// Native user-text extraction identical to the plan_workflow tests, used to assert candidate session
    /// content.
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

    fn native_entry_count(agent: &Agent) -> usize {
        let request = &serde_json::to_value(&agent.session).unwrap()["request"];
        request["messages"]
            .as_array()
            .or_else(|| request["input"].as_array())
            .unwrap()
            .len()
    }

    #[test]
    fn open_listed_plan_continue_and_execute_intents() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let preparation = agent
                .open_listed_plan(
                    &selection,
                    PlanOpenIntent::ContinueDraft,
                    &CancellationToken::new(),
                )
                .unwrap();
            // Candidate workflow: Plan/Draft, same ID/name, revision=1, no frozen approval.
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(document.status, PlanStatus::Draft);
            assert_eq!(document.id, LISTED_ID);
            assert_eq!(document.file_name.as_deref(), Some(LISTED_FILE));
            assert_eq!(document.location, PlanLocation::Drafts);
            assert_eq!(document.revision, 1);
            assert_eq!(document.content, LISTED_CONTENT);
            assert_eq!(document.approved_tasks, None);
            assert_eq!(document.title, "Listed plan");
            assert_eq!(
                document.origin_prompt,
                format!(
                    "Continue planning the selected Markdown plan at {}.",
                    listed_path()
                )
            );
            // Published product: empty replay, session_id pointing at the candidate session.
            assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
            assert_eq!(preparation.session_id.as_deref(), agent.journal.id());
            assert_ne!(preparation.session_id.as_deref(), None);
            assert_eq!(preparation.workflow, agent.workflow);
            // The candidate session holds no old chat: the single user message carries the selection
            // action, the ID and the full snapshot, while the original session's exploration text never
            // appears; opening never produced an assistant reply or a request.
            let texts = user_texts(&agent);
            assert_eq!(texts.len(), 1);
            assert!(texts[0].contains("Continue planning the selected Markdown plan below."));
            assert!(texts[0].contains(LISTED_ID));
            assert!(texts[0].contains(&format!("Path: {}", listed_path())));
            assert!(texts[0].contains(LISTED_CONTENT));
            assert!(!texts[0].contains(EXPLORATION));
            assert_eq!(
                native_entry_count(&agent),
                if protocol == "completion" { 2 } else { 1 }
            );
            // The original session file stays: the candidate fork produces a second session record.
            let sessions = std::fs::read_dir(dir.path().join("sessions"))
                .unwrap()
                .count();
            assert!(sessions >= 2, "source session must remain restorable");
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let preparation = agent
                .open_listed_plan(
                    &selection,
                    PlanOpenIntent::Execute,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert!(agent.workflow.todos.is_empty());
            assert!(agent.workflow.todo_plan_id.is_none());
            assert!(preparation.todo_snapshot.is_none());
            // Plan/Ready pre-execution review state: validate explicitly allows it; unapproved, no report.
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.content, selection.content);
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(document.status, PlanStatus::Ready);
            assert_eq!(document.location, PlanLocation::Drafts);
            assert_eq!(document.approved_tasks, None);
            assert!(agent.workflow.review.is_none());
            assert_eq!(
                document.origin_prompt,
                format!(
                    "Execute the selected Markdown plan at {} after local approval.",
                    listed_path()
                )
            );
            assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
            // read_plan re-issues the review token on the new session, bound to the candidate session ID.
            let review = agent.read_plan().unwrap();
            assert_eq!(review.token.session_id, agent.journal.id().unwrap());
            assert_eq!(review.token.plan_id, LISTED_ID);
            assert_eq!(review.token.content, LISTED_CONTENT);
            // No approval means no execution: the candidate session has no assistant reply and connects no
            // MCP.
            assert_eq!(user_texts(&agent).len(), 1);
            let request = serde_json::to_value(&agent.session).unwrap()["request"].clone();
            let tools = request["tools"].as_array().unwrap();
            let names: Vec<&str> = tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().or(tool["name"].as_str()))
                .collect();
            assert!(names.contains(&"write"));
            assert!(!names.contains(&"bash"));
        }
    }

    #[test]
    fn open_listed_plan_failures_keep_original_session() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let previous_session = serde_json::to_value(&agent.session).unwrap();
            let previous_workflow = agent.workflow.clone();
            agent.tools.reset_project_instructions([dir
                .path()
                .canonicalize()
                .unwrap()
                .join("AGENTS.md")]);
            // Project-rule loading refuses symlinks: candidate preparation fails before any write.
            let target = dir.path().join("agents-source.md");
            std::fs::write(&target, "rules\n").unwrap();
            std::os::unix::fs::symlink(&target, dir.path().join("AGENTS.md")).unwrap();
            let error = err_message(agent.open_listed_plan(
                &selection,
                PlanOpenIntent::ContinueDraft,
                &CancellationToken::new(),
            ));
            assert!(!error.is_empty());
            // The original session/mode/input and the plan file all stay unchanged.
            assert_eq!(
                serde_json::to_value(&agent.session).unwrap(),
                previous_session
            );
            assert_eq!(agent.workflow, previous_workflow);
            assert_eq!(
                std::fs::read_to_string(dir.path().join(listed_path())).unwrap(),
                LISTED_CONTENT
            );
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            write_listed_file(&agent);
            register_listed_entry(&agent);
            let selection = selection();
            let previous_session = serde_json::to_value(&agent.session).unwrap();
            let previous_workflow = agent.workflow.clone();
            // Read-only sessions directory: the candidate journal fails on its first file creation. A
            // privileged environment can bypass permission bits, in which case this failure injection is
            // skipped.
            let sessions = dir.path().join("sessions");
            let original = std::fs::metadata(&sessions).unwrap().permissions();
            std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();
            let probe = sessions.join("probe.jsonl");
            let enforced = std::fs::write(&probe, b"probe").is_err();
            if enforced {
                let _ = std::fs::remove_file(&probe);
                let error = err_message(agent.open_listed_plan(
                    &selection,
                    PlanOpenIntent::ContinueDraft,
                    &CancellationToken::new(),
                ));
                assert!(!error.is_empty());
                assert_eq!(
                    serde_json::to_value(&agent.session).unwrap(),
                    previous_session
                );
                assert_eq!(agent.workflow, previous_workflow);
            }
            std::fs::set_permissions(&sessions, original).unwrap();
            assert_eq!(
                std::fs::read_to_string(dir.path().join(listed_path())).unwrap(),
                LISTED_CONTENT
            );
        }
    }

    #[test]
    fn open_listed_plan_done_prose_and_unsaved_guards() {
        {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
            write_plan_bytes(&agent, PlanLocation::Done, LISTED_FILE, LISTED_CONTENT);
            register_entry(&agent, LISTED_ID, LISTED_FILE, PlanLocation::Done);
            let selection = PlanSelection {
                id: LISTED_ID.to_owned(),
                path: plan::path_for(LISTED_FILE, PlanLocation::Done),
                content: LISTED_CONTENT.to_owned(),
            };
            for intent in [PlanOpenIntent::ContinueDraft, PlanOpenIntent::Execute] {
                let error = err_message(agent.open_listed_plan(
                    &selection,
                    intent,
                    &CancellationToken::new(),
                ));
                assert!(error.contains("cannot be opened"), "{error}");
            }
            // A failure produces no session or binding change.
            assert!(agent.workflow.plan.is_none());
            assert!(agent.journal.id().is_none());
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
            // The menu also opens without task prose, but the plan stays Draft and execution is still
            // rejected by the approval gate.
            write_listed_file(&agent);
            std::fs::write(
                dir.path().join(listed_path()),
                "# No tasks\n\nJust prose.\n",
            )
            .unwrap();
            register_listed_entry(&agent);
            let no_task_selection = PlanSelection {
                id: LISTED_ID.to_owned(),
                path: listed_path(),
                content: "# No tasks\n\nJust prose.\n".to_owned(),
            };
            let preparation = agent
                .open_listed_plan(
                    &no_task_selection,
                    PlanOpenIntent::Execute,
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Draft
            );
            assert_eq!(preparation.workflow.mode, WorkMode::Plan);
            // A never-saved revision-0 draft refuses "open another session" so it cannot be dropped
            // silently.
            let mut document = PlanDocument::new().unwrap();
            document.origin_prompt = "pending first save".to_owned();
            agent.workflow = WorkflowState {
                mode: WorkMode::Plan,
                plan: Some(document),
                ..Default::default()
            };
            let selection = selection();
            let error = err_message(agent.open_listed_plan(
                &selection,
                PlanOpenIntent::ContinueDraft,
                &CancellationToken::new(),
            ));
            assert_eq!(error, UNSAVED_DRAFT);
            // Unregistered selections are rejected: never touch the disk by list index alone.
            agent.workflow = WorkflowState::default();
            let unregistered = PlanSelection {
                id: "ffffffffffffffffffffffffffffffff".to_owned(),
                path: listed_path(),
                content: LISTED_CONTENT.to_owned(),
            };
            let error = err_message(agent.open_listed_plan(
                &unregistered,
                PlanOpenIntent::ContinueDraft,
                &CancellationToken::new(),
            ));
            assert!(!error.is_empty());
        }
    }
}
