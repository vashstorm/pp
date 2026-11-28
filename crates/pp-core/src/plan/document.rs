use anyhow::{Result, anyhow, ensure};
use serde::{Deserialize, Serialize};

use crate::todo::TodoPhase;

pub const PLAN_MAX_BYTES: usize = 262_144;

/// First H1 text; when missing or blank, fall back to the file name (without the `.md` suffix). The
/// display title is trimmed and constrained to 1..=120 characters without touching the body.
pub fn plan_display_title(content: &str, file_name: &str) -> String {
    let heading = first_h1(content)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    let fallback = file_name.strip_suffix(".md").unwrap_or(file_name);
    let title = heading.unwrap_or_else(|| fallback.trim().to_owned());
    if title.is_empty() {
        return fallback.chars().take(120).collect();
    }
    title.chars().take(120).collect()
}

/// Extract the plain text of the first level-one heading (concatenated Text/Code events); returns
/// `None` when there is none.
fn first_h1(content: &str) -> Option<String> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let mut inside = false;
    let mut text = String::new();
    for event in Parser::new_ext(content, Options::empty()) {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            }) => inside = true,
            Event::End(TagEnd::Heading(HeadingLevel::H1)) if inside => return Some(text),
            Event::Text(value) | Event::Code(value) if inside => text.push_str(&value),
            _ => {}
        }
    }
    None
}

/// Lifecycle directory of a plan file; new documents land explicitly in Drafts.
/// The serde default Plans only serves deserializing old records without a `location` field.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanLocation {
    Drafts,
    #[default]
    Plans,
    Done,
}

impl PlanLocation {
    /// Directory name under `.pp/`.
    pub fn dir_name(self) -> &'static str {
        match self {
            PlanLocation::Drafts => "drafts",
            PlanLocation::Plans => "plans",
            PlanLocation::Done => "done",
        }
    }
}

/// Execution-phase progress rules, injected by both approval handoffs and the Execute workflow.
pub(crate) const PROGRESS_INSTRUCTIONS: &str = include_str!("../prompts/plan-progress.md");

pub fn execution_prompt(plan: &PlanDocument, context: ExecutionContext) -> String {
    let mut prompt = String::from(
        "Implement the explicitly approved plan below. Normal system, current project and skill rules still apply. Inspect the relevant code's current state before changing it. Use the approved plan as the implementation basis; exploration history is evidence, not permission to revive abandoned alternatives. If the plan conflicts with an important user requirement in retained history, clarify the conflict before making changes.\n\n",
    );
    prompt.push_str(PROGRESS_INSTRUCTIONS);
    if context == ExecutionContext::Keep {
        prompt.push_str("\n\nOriginal user task (retained context):\n");
        prompt.push_str(&plan.origin_prompt);
    }
    use std::fmt::Write;
    // The approval flow guarantees that only a saved (revision>=1) plan reaches the execution
    // handoff.
    let path = plan
        .path()
        .expect("approved plan has a saved lifecycle path");
    write!(prompt, "\n\nApproved plan: {}\nPath: {}\nRevision: {}\n\n[Approved Markdown snapshot]\n{}\n[/Approved Markdown snapshot]", plan.id, path, plan.revision, plan.content).expect("writing to String");
    prompt
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    #[default]
    Execute,
    Plan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Ready,
    Executed,
    Completed,
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDocument {
    pub id: String,
    pub revision: u64,
    pub title: String,
    pub content: String,
    pub status: PlanStatus,
    pub origin_prompt: String,
    /// Stable file name chosen by the AI (including `.md`); bound on first save and left None when an
    /// old record lacks it.
    #[serde(default)]
    pub file_name: Option<String>,
    /// Lifecycle location; the default Plans only serves deserializing old records, new documents use
    /// Drafts explicitly.
    #[serde(default)]
    pub location: PlanLocation,
    /// Ordered task text frozen at approval from the body verified by ReviewToken; importing
    /// execution progress must not overwrite it. None for old records and unapproved drafts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved_tasks: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowState {
    pub mode: WorkMode,
    pub plan: Option<PlanDocument>,
    #[serde(default)]
    pub review: Option<PlanReviewReport>,
    /// Phase/task projection cache derived from the plan, and the carrier of active/blocked runtime
    /// state; Markdown always stays the primary record of tasks and checkmarks, so this is not a
    /// competing second record. Omitted when empty, and old session records rely on the serde default,
    /// so the checkpoint format gets no version bump.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoPhase>,
    /// None means a plain list; Some(id) means the current list is a derived projection of that plan
    /// and must match the currently bound plan.id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_plan_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanReviewReport {
    pub plan_id: String,
    pub revision: u64,
    pub model_reference: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionContext {
    Keep,
    Fresh,
    Compact,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewToken {
    pub session_id: String,
    pub plan_id: String,
    /// The actual short path read safely during review; rename/move/delete invalidates an old token.
    pub path: String,
    pub revision: u64,
    pub content: String,
}

pub struct PlanReview {
    pub token: ReviewToken,
    pub document: PlanDocument,
    pub report: Option<PlanReviewReport>,
}

impl PlanDocument {
    pub fn new() -> Result<Self> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("Could not allocate a plan ID."))?;
        let id = format!("{:032x}", u128::from_be_bytes(bytes));
        Ok(Self {
            id,
            revision: 0,
            title: String::new(),
            content: String::new(),
            status: PlanStatus::Draft,
            origin_prompt: String::new(),
            file_name: None,
            location: PlanLocation::Drafts,
            approved_tasks: None,
        })
    }

    /// Actual short path of the current binding. Returns None for an unsaved revision-0; a named
    /// document returns its lifecycle directory path; an old revision>=1 without a file name
    /// temporarily returns the old `<id>.md` path, and that fallback is dropped once an explicit file
    /// name is registered.
    pub fn path(&self) -> Option<String> {
        match (&self.file_name, self.revision) {
            (Some(name), _) => Some(path_for(name, self.location)),
            (None, 0) => None,
            (None, _) => Some(format!(".pp/plans/{}.md", self.id)),
        }
    }

    /// The next registered revision, rejecting counter overflow in one place.
    pub fn next_revision(&self) -> Result<u64> {
        self.revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Plan revision limit reached."))
    }
}

/// Plan file path inside its lifecycle directory; no longer derived from the ID.
pub fn path_for(file_name: &str, location: PlanLocation) -> String {
    format!(".pp/{}/{}", location.dir_name(), file_name)
}

pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Strict rule for new AI names and user renames: an English kebab-case Markdown file name.
/// ASCII lowercase letters/digits only, separated by single hyphens, 4..=100 bytes in total.
pub fn validate_file_name(file_name: &str) -> Result<()> {
    let stem = file_name
        .strip_suffix(".md")
        .ok_or_else(|| anyhow!("Plan file name must end with .md."))?;
    ensure!(
        (4..=100).contains(&file_name.len())
            && !stem.is_empty()
            && stem
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !stem.starts_with('-')
            && !stem.ends_with('-')
            && !stem.contains("--"),
        "Plan file name must be kebab-case Markdown, like fix-config-loading.md."
    );
    Ok(())
}

/// Lenient rule for legacy file names and directory scans: a valid single normal path component
/// ending in `.md`, 4..=255 bytes long; legacy Unicode file names are preserved.
pub fn validate_stored_file_name(file_name: &str) -> Result<()> {
    ensure!(
        (4..=255).contains(&file_name.len())
            && file_name != "."
            && file_name != ".."
            && file_name.ends_with(".md")
            && !file_name
                .chars()
                .any(|character| character == '/' || character == '\\' || character.is_control()),
        "Invalid plan file name."
    );
    Ok(())
}

pub fn validate_content(content: &str) -> Result<()> {
    ensure!(
        !content.trim().is_empty(),
        "Plan content must not be empty."
    );
    ensure!(
        content.len() <= PLAN_MAX_BYTES,
        "Plan content is too large."
    );
    ensure!(
        !content.contains('\0'),
        "Plan content must not contain NUL."
    );
    Ok(())
}

pub fn validate_title(title: &str) -> Result<()> {
    ensure!(
        !title.trim().is_empty()
            && title == title.trim()
            && title.chars().count() <= 120
            && !title.chars().any(char::is_control),
        "Plan title must contain 1 to 120 characters without control characters."
    );
    Ok(())
}

impl WorkflowState {
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            WorkMode::Execute => "execute",
            WorkMode::Plan => "plan",
        }
    }

    pub fn instructions(&self, plan_prompt: &str) -> String {
        let mut instructions = match self.mode {
            WorkMode::Plan => plan_prompt.to_owned(),
            // Progress rules are injected only for Execute+Executed; Completed/Abandoned are read-only
            // history and never get execution rules.
            WorkMode::Execute
                if self
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.status == PlanStatus::Executed) =>
            {
                format!(
                    "EXECUTE MODE — the local user has explicitly handed this plan off for ordinary execution. Plan mode tool restrictions no longer apply.\n{PROGRESS_INSTRUCTIONS}"
                )
            }
            WorkMode::Execute if self.plan.is_some() => String::from(
                "EXECUTE MODE — the bound plan is no longer being executed. The Markdown document is read-only history; do not modify it and do not infer remaining work from it.",
            ),
            WorkMode::Execute => return String::new(),
        };
        if let Some(plan) = &self.plan {
            use std::fmt::Write;
            // An unsaved new binding prints no fake path.
            let path = plan.path().unwrap_or_else(|| "Not saved yet".to_owned());
            let status_note = match plan.status {
                PlanStatus::Executed => {
                    "Executed means handed off, not all tasks completed. Read the current file before revising it; never restore missing files silently from a historical snapshot."
                }
                PlanStatus::Completed => {
                    "Completed means every approved task finished successfully and the runtime archived the file under .pp/done/. The document is read-only history."
                }
                PlanStatus::Abandoned => "Abandoned plans are read-only history.",
                PlanStatus::Draft | PlanStatus::Ready => {
                    "Read the current file before revising it; never restore missing files silently from a historical snapshot."
                }
            };
            write!(instructions, "\n\nCurrent plan id: {}\nProject-relative path: {}\nRegistered revision: {}\nStatus: {:?}. {status_note}", plan.id, path, plan.revision, plan.status).expect("writing to String");
        } else if self.mode == WorkMode::Plan {
            instructions.push_str(
                "\nNo plan document is currently bound. The next user task starts a new plan.",
            );
        }
        instructions
    }

    pub fn validate(&self) -> Result<()> {
        let invalid = "Invalid saved workflow state.";
        if let Some(plan) = &self.plan {
            ensure!(valid_id(&plan.id), "{invalid}");
            if plan.revision == 0 {
                ensure!(
                    plan.status == PlanStatus::Draft
                        && plan.title.is_empty()
                        && plan.content.is_empty()
                        && plan.file_name.is_none(),
                    "{invalid}"
                );
            } else {
                validate_title(&plan.title).map_err(|_| anyhow::anyhow!(invalid))?;
                validate_content(&plan.content).map_err(|_| anyhow::anyhow!(invalid))?;
                ensure!(!plan.origin_prompt.trim().is_empty(), "{invalid}");
                // Old records may lack file_name (registration reconciles it later); named ones are
                // validated with the lenient stored rule so legacy Unicode file names survive.
                if let Some(name) = &plan.file_name {
                    validate_stored_file_name(name).map_err(|_| anyhow::anyhow!(invalid))?;
                }
            }
            // Lifecycle location constraint: legacy Draft/Ready/Abandoned may still deserialize in
            // Plans and are reconciled by the migration rules afterwards; Completed only in Done.
            ensure!(
                match plan.status {
                    PlanStatus::Draft | PlanStatus::Ready =>
                        matches!(plan.location, PlanLocation::Drafts | PlanLocation::Plans),
                    PlanStatus::Executed => plan.location == PlanLocation::Plans,
                    PlanStatus::Completed => plan.location == PlanLocation::Done,
                    PlanStatus::Abandoned => true,
                },
                "{invalid}"
            );
            ensure!(
                self.mode == WorkMode::Plan
                    || matches!(
                        plan.status,
                        PlanStatus::Executed | PlanStatus::Completed | PlanStatus::Abandoned
                    ),
                "{invalid}"
            );
        }
        // A plan-derived todo must still be bound to the current plan; a plain list needs no source.
        if let Some(id) = &self.todo_plan_id {
            let plan = self.plan.as_ref().ok_or_else(|| anyhow::anyhow!(invalid))?;
            ensure!(*id == plan.id, "{invalid}");
        }
        if let Some(report) = &self.review {
            let plan = self.plan.as_ref().ok_or_else(|| anyhow::anyhow!(invalid))?;
            ensure!(
                report.plan_id == plan.id
                    && report.revision >= 1
                    && report.revision <= plan.revision,
                "{invalid}"
            );
            let (provider, model) = report
                .model_reference
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!(invalid))?;
            ensure!(
                !provider.is_empty()
                    && !model.is_empty()
                    && !report.model_reference.chars().any(char::is_control),
                "{invalid}"
            );
            validate_content(&report.content).map_err(|_| anyhow::anyhow!(invalid))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::todo::TodoItem;
    use crate::todo::TodoStatus;

    #[test]
    fn plan_document_serde_source_binding_and_revision_validation() {
        {
            // Old session records carry no todo field: the serde default fills it empty and the
            // checkpoint gets no version bump.
            let legacy: WorkflowState =
                serde_json::from_str(r#"{"mode":"execute","plan":null}"#).unwrap();
            assert_eq!(legacy, WorkflowState::default());
            assert!(legacy.todos.is_empty());
            assert_eq!(legacy.todo_plan_id, None);

            // An empty projection is not written to the saved workflow snapshot.
            let text = serde_json::to_string(&WorkflowState::default()).unwrap();
            assert!(!text.contains("todos"));
            assert!(!text.contains("todo_plan_id"));

            // A non-empty projection and its source round-trip completely.
            let planned = WorkflowState {
                todos: vec![TodoPhase {
                    name: "Write".into(),
                    tasks: vec![TodoItem {
                        content: "A".into(),
                        status: TodoStatus::InProgress,
                        blocker: None,
                    }],
                }],
                todo_plan_id: Some("0123456789abcdef0123456789abcdef".into()),
                ..Default::default()
            };
            let text = serde_json::to_string(&planned).unwrap();
            assert!(text.contains("todos") && text.contains("todo_plan_id"));
            assert_eq!(
                serde_json::from_str::<WorkflowState>(&text).unwrap(),
                planned
            );
        }
        {
            let mut plan = PlanDocument::new().unwrap();
            plan.revision = 1;
            plan.title = "Title".into();
            plan.content = "- [ ] task".into();
            plan.origin_prompt = "task".into();
            plan.status = PlanStatus::Executed;
            plan.location = PlanLocation::Plans;
            let base = WorkflowState {
                mode: WorkMode::Execute,
                plan: Some(plan.clone()),
                ..Default::default()
            };
            base.validate().unwrap();

            // A source matching the current plan.id is valid; projection entries are optional.
            let mut bound = base.clone();
            bound.todo_plan_id = Some(plan.id.clone());
            bound.validate().unwrap();
            bound.todos = vec![TodoPhase {
                name: "Write".into(),
                tasks: vec![TodoItem {
                    content: "task".into(),
                    status: TodoStatus::InProgress,
                    blocker: None,
                }],
            }];
            bound.validate().unwrap();

            // A source that disagrees with the current plan is rejected.
            let mut wrong = bound.clone();
            wrong.todo_plan_id = Some("ffffffffffffffffffffffffffffffff".into());
            assert!(wrong.validate().is_err());

            // A source without a bound plan is rejected.
            let mut detached = bound;
            detached.plan = None;
            assert!(detached.validate().is_err());

            // A plain list without a source is valid.
            let mut ordinary = base;
            ordinary.todos = vec![TodoPhase {
                name: "Tasks".into(),
                tasks: vec![TodoItem {
                    content: "standalone".into(),
                    status: TodoStatus::Pending,
                    blocker: None,
                }],
            }];
            ordinary.validate().unwrap();
        }
        {
            let mut plan = PlanDocument::new().unwrap();
            plan.revision = 1;
            plan.title = "Create file".into();
            plan.content = "- [ ] Create file and verify bytes".into();
            plan.origin_prompt = "Create a file".into();
            plan.status = PlanStatus::Ready;
            plan.location = PlanLocation::Drafts;
            let mut workflow = WorkflowState {
                mode: WorkMode::Plan,
                plan: Some(plan),
                ..Default::default()
            };
            workflow.validate().unwrap();
            workflow.mode = WorkMode::Execute;
            assert!(workflow.validate().is_err());
            workflow.mode = WorkMode::Plan;
            workflow.plan.as_mut().unwrap().revision = 0;
            assert!(workflow.validate().is_err());
            workflow.plan.as_mut().unwrap().revision = 1;
            workflow.plan.as_mut().unwrap().id = "../escape".into();
            assert!(workflow.validate().is_err());
        }
    }

    #[test]
    fn plan_document_paths_and_location_file_name_rules() {
        {
            // New AI naming: strict kebab-case.
            assert!(validate_file_name("fix-config-loading.md").is_ok());
            assert!(validate_file_name("a.md").is_ok());
            assert!(validate_file_name(".md").is_err());
            assert!(validate_file_name("123.md").is_ok());
            assert!(validate_file_name("-lead.md").is_err());
            assert!(validate_file_name("trail-.md").is_err());
            assert!(validate_file_name("double--dash.md").is_err());
            assert!(validate_file_name("Upper.md").is_err());
            assert!(validate_file_name("no-extension").is_err());
            assert!(validate_file_name("nested/name.md").is_err());
            assert!(validate_file_name(&format!("{}.md", "a".repeat(97))).is_ok());
            assert!(validate_file_name(&format!("{}.md", "a".repeat(98))).is_err());

            // Legacy stored naming: a single normal component `.md` file, Unicode preserved.
            assert!(validate_stored_file_name("计划-修订.md").is_ok());
            assert!(validate_stored_file_name(&format!("{}.md", "0".repeat(32))).is_ok());
            assert!(validate_stored_file_name(".md").is_err());
            assert!(validate_stored_file_name("nested/name.md").is_err());
            assert!(validate_stored_file_name("back\\slash.md").is_err());
            assert!(validate_stored_file_name("nul\0name.md").is_err());
            assert!(validate_stored_file_name("dot.md\0").is_err());
            assert!(validate_stored_file_name("../escape.md").is_err());
            assert!(validate_stored_file_name("..").is_err());
            assert!(validate_stored_file_name(&format!("{}.md", "长".repeat(84))).is_ok());
            assert!(validate_stored_file_name(&format!("{}.md", "长".repeat(85))).is_err());
        }
        {
            // An unsaved new binding has no path.
            let fresh = PlanDocument::new().unwrap();
            assert_eq!(fresh.path(), None);
            assert_eq!(fresh.location, PlanLocation::Drafts);

            // A named document resolves under its lifecycle directory.
            let mut named = PlanDocument::new().unwrap();
            named.revision = 1;
            named.file_name = Some("fix-config-loading.md".into());
            assert_eq!(
                named.path().as_deref(),
                Some(".pp/drafts/fix-config-loading.md")
            );
            named.location = PlanLocation::Done;
            assert_eq!(
                named.path().as_deref(),
                Some(".pp/done/fix-config-loading.md")
            );
            assert_eq!(
                path_for("done-plan.md", PlanLocation::Done),
                ".pp/done/done-plan.md"
            );

            // Old revision>=1 without a file name: temporary legacy path fallback.
            let mut legacy = PlanDocument::new().unwrap();
            legacy.revision = 1;
            legacy.location = PlanLocation::Plans;
            assert_eq!(
                legacy.path().as_deref(),
                Some(format!(".pp/plans/{}.md", legacy.id)).as_deref()
            );
        }
        {
            let mut plan = PlanDocument::new().unwrap();
            plan.revision = 1;
            plan.title = "Title".into();
            plan.content = "- [ ] task".into();
            plan.origin_prompt = "task".into();
            plan.file_name = Some("fix-config-loading.md".into());
            let workflow = |plan: PlanDocument| WorkflowState {
                mode: WorkMode::Plan,
                plan: Some(plan),
                ..Default::default()
            };
            // Draft/Ready are allowed in Drafts or Plans (old records).
            for location in [PlanLocation::Drafts, PlanLocation::Plans] {
                let mut document = plan.clone();
                document.location = location;
                workflow(document).validate().unwrap();
            }
            // Once named, Completed lives only in Done; Executed only in Plans.
            let mut completed = plan.clone();
            completed.status = PlanStatus::Completed;
            completed.location = PlanLocation::Done;
            workflow(completed.clone()).validate().unwrap();
            let mut wrong = completed.clone();
            wrong.location = PlanLocation::Plans;
            assert!(workflow(wrong).validate().is_err());
            let mut executed = plan.clone();
            executed.status = PlanStatus::Executed;
            executed.location = PlanLocation::Plans;
            workflow(executed.clone()).validate().unwrap();
            let mut wrong = executed.clone();
            wrong.location = PlanLocation::Drafts;
            assert!(workflow(wrong).validate().is_err());
            // A legacy Unicode name passes the stored rule.
            let mut unicode = plan.clone();
            unicode.file_name = Some("旧计划.md".into());
            workflow(unicode).validate().unwrap();
            // An illegal name is rejected.
            let mut escape = plan.clone();
            escape.file_name = Some("../escape.md".into());
            assert!(workflow(escape).validate().is_err());
            // A legacy record without file_name is still valid (legacy fallback path).
            let mut unnamed = plan;
            unnamed.file_name = None;
            unnamed.location = PlanLocation::Plans;
            workflow(unnamed).validate().unwrap();
        }
    }
}
