//! Compiled-in scenarios; each run uses its own config, sessions, tools and temporary workspace.

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use pp_config::config::{EditMode, ModelCatalog, ModelChoice, ModelRole, RoleAssignment};
use pp_core::{
    cancellation::{self, CancellationToken, RunOutcome},
    context::ContextUsage,
    events::AgentEvent,
    interaction::SharedAskHandler,
    model::ThinkLevel,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::MockEndpoint;
use pp_bash::PipeStore;
use pp_runtime::{agent::Agent, subagent::Binding};
use pp_tools::Tools;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockTest {
    Task,
    Ui,
    All,
    Read,
    Write,
    Edit,
    Rg,
    Glob,
    Bash,
    BashTask,
    EvalPy,
    EvalJs,
    Wait,
    Todo,
    Ask,
    Fail,
}

impl MockTest {
    pub const ALL: [Self; 16] = [
        Self::Task,
        Self::Ui,
        Self::All,
        Self::Read,
        Self::Write,
        Self::Edit,
        Self::Rg,
        Self::Glob,
        Self::Bash,
        Self::BashTask,
        Self::EvalPy,
        Self::EvalJs,
        Self::Wait,
        Self::Todo,
        Self::Ask,
        Self::Fail,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Task => "/test-task",
            Self::Ui => "/test-ui",
            Self::All => "/test-tools",
            Self::Read => "/test-read",
            Self::Write => "/test-write",
            Self::Edit => "/test-edit",
            Self::Rg => "/test-rg",
            Self::Glob => "/test-glob",
            Self::Bash => "/test-bash",
            Self::BashTask => "/test-bash-task",
            Self::EvalPy => "/test-eval-py",
            Self::EvalJs => "/test-eval-js",
            Self::Wait => "/test-wait",
            Self::Todo => "/test-todo",
            Self::Ask => "/test-ask",
            Self::Fail => "/test-fail",
        }
    }
}

pub struct MockTestSnapshot {
    pub current_model: ModelChoice,
    pub choices: Vec<ModelChoice>,
    pub think_level: ThinkLevel,
    pub default_think_level: ThinkLevel,
    pub usage: ContextUsage,
    pub subagent_control: Arc<Mutex<Option<Binding>>>,
    pub pipe_store: PipeStore,
}

pub enum MockTestEvent {
    Prepared(MockTestSnapshot),
    Progress(AgentEvent),
}

#[derive(Debug)]
pub struct MockTestReport {
    pub calls: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    prompt: String,
    edit_mode: EditMode,
    /// Fixed `/tmp` workspace so a human can watch the real files from another terminal. By default each
    /// run uses a separate workspace inside the system temporary directory.
    #[serde(default)]
    workspace: Option<String>,
    files: BTreeMap<String, String>,
    /// Overrides the scenario's automatic retry budget; defaults to 0 (no retries). Only scenarios that
    /// exercise failure paths enable it, so other scenarios never really wait on backoff.
    #[serde(default)]
    retry_budget_seconds: u64,
    lanes: BTreeMap<String, Value>,
}

/// If an escaping write really reached the disk, it would show up under this fixed name in the workspace parent.
const ESCAPE_FILE: &str = "pp-mock-escape.txt";

/// Fixed workspace declared by the scenario. It is removed at the end only when this run created it;
/// pre-existing directories are kept as is. A failed removal only leaves an empty temp directory and is not upgraded to `CleanupError`.
struct FixedWorkspace {
    path: PathBuf,
    created: bool,
}

impl FixedWorkspace {
    fn prepare(declared: &str) -> Result<Self> {
        let path = Path::new(declared);
        ensure!(
            path.is_absolute()
                && !path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
                && (path.starts_with("/tmp") || path.starts_with("/private/tmp"))
                && path != Path::new("/tmp")
                && path != Path::new("/private/tmp"),
            "fixed mock workspace must be an absolute /tmp directory"
        );
        let created = !path.exists();
        fs::create_dir_all(path).context("creating fixed mock workspace")?;
        Ok(Self {
            path: path.to_owned(),
            created,
        })
    }
}

impl Drop for FixedWorkspace {
    fn drop(&mut self) {
        if self.created {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

const READ: &str = include_str!("scenarios/read.json");
const WRITE: &str = include_str!("scenarios/write.json");
const EDIT_REPLACE: &str = include_str!("scenarios/edit-replace.json");
const EDIT_HASHLINE: &str = include_str!("scenarios/edit-hashline.json");
const RG: &str = include_str!("scenarios/rg.json");
const GLOB: &str = include_str!("scenarios/glob.json");
const BASH: &str = include_str!("scenarios/bash.json");
const BASH_TASK: &str = include_str!("scenarios/bash-task.json");
const EVAL_PY: &str = include_str!("scenarios/eval-py.json");
const EVAL_JS: &str = include_str!("scenarios/eval-js.json");
const WAIT: &str = include_str!("scenarios/wait.json");
const TODO: &str = include_str!("scenarios/todo.json");
const ASK: &str = include_str!("scenarios/ask.json");
const FAIL: &str = include_str!("scenarios/fail.json");
const UI: &str = include_str!("scenarios/ui.json");
const TASK: &str = include_str!("scenarios/task.json");

fn scenarios(test: MockTest) -> &'static [&'static str] {
    match test {
        MockTest::All => &[
            READ,
            WRITE,
            EDIT_REPLACE,
            EDIT_HASHLINE,
            RG,
            GLOB,
            BASH,
            BASH_TASK,
            WAIT,
            TODO,
            ASK,
        ],
        MockTest::Edit => &[EDIT_REPLACE, EDIT_HASHLINE],
        MockTest::Read => &[READ],
        MockTest::Write => &[WRITE],
        MockTest::Rg => &[RG],
        MockTest::Glob => &[GLOB],
        MockTest::Bash => &[BASH],
        MockTest::BashTask => &[BASH_TASK],
        MockTest::EvalPy => &[EVAL_PY],
        MockTest::EvalJs => &[EVAL_JS],
        MockTest::Wait => &[WAIT],
        MockTest::Todo => &[TODO],
        MockTest::Ask => &[ASK],
        MockTest::Fail => &[FAIL],
        MockTest::Ui => &[UI],
        MockTest::Task => &[TASK],
    }
}

pub fn run_test(
    test: MockTest,
    endpoint: MockEndpoint,
    ask: Option<SharedAskHandler>,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(MockTestEvent),
) -> Result<MockTestReport> {
    let mut calls = 0;
    for embedded in scenarios(test) {
        cancellation::check(cancellation)?;
        let scenario: Scenario =
            serde_json::from_str(embedded).context("decoding embedded mock scenario")?;
        run_scenario(
            scenario,
            test,
            &endpoint,
            ask.clone(),
            cancellation,
            &mut |event| {
                if matches!(
                    &event,
                    MockTestEvent::Progress(AgentEvent::ToolStarted { .. })
                ) {
                    calls += 1;
                }
                emit(event);
            },
        )?;
    }
    Ok(MockTestReport { calls })
}

struct RunQueues<'a> {
    endpoint: &'a MockEndpoint,
    id: String,
}
impl Drop for RunQueues<'_> {
    fn drop(&mut self) {
        self.endpoint.cancel_run(&self.id);
    }
}

fn run_scenario(
    scenario: Scenario,
    test: MockTest,
    endpoint: &MockEndpoint,
    ask: Option<SharedAskHandler>,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(MockTestEvent),
) -> Result<()> {
    let mut random = [0; 16];
    getrandom::fill(&mut random).context("generating mock run identity")?;
    let mut id = String::with_capacity(32);
    for byte in random {
        write!(&mut id, "{byte:02x}").expect("writing a String cannot fail");
    }
    let queues = RunQueues { endpoint, id };
    let temp = tempfile::Builder::new()
        .prefix("pp-mock-")
        .tempdir()
        .context("creating mock run directory")?;
    let fixed = scenario
        .workspace
        .as_deref()
        .map(FixedWorkspace::prepare)
        .transpose()?;
    let workspace = match &fixed {
        Some(fixed) => fixed.path.clone(),
        None => temp.path().join("workspace"),
    };
    let mut result = (|| {
        let config_dir = temp.path().join("config");
        let sessions = temp.path().join("sessions");
        let locks = temp.path().join("locks");
        for dir in [&workspace, &config_dir, &sessions, &locks] {
            fs::create_dir_all(dir)?;
        }
        for (relative, text) in &scenario.files {
            setup_file(&workspace, relative, text)?;
        }
        let edit = match scenario.edit_mode {
            EditMode::Replace => "replace",
            EditMode::Hashline => "hashline",
        };
        fs::write(config_dir.join("settings.yml"), json!({
        "edit":edit, "auto_compact":false, "lsp":{"enabled":false},
        "rate_limit_retry_budget_seconds":scenario.retry_budget_seconds, "request_timeout_seconds":300
    }).to_string())?;
        fs::write(
            config_dir.join("mcp.json"),
            r#"{"mcpServers":{},"enabledServers":[],"disabledServers":[]}"#,
        )?;
        let providers: BTreeMap<_, _> = ["root", "fast", "slow", "review"].into_iter().map(|lane| {
        (format!("mock-{lane}"), json!({
            "protocol":"response", "api_key":"pp-mock-key",
            "base_url":format!("http://127.0.0.1:{}/mock/{}/{lane}/v1", endpoint.port(), queues.id),
            "models":{"primary":{"model":format!("pp-mock-{lane}"),"context_window":128000,"thinking":{"defaultLevel":"none"}}}
        }))
    }).collect();
        fs::write(
            config_dir.join("models.yml"),
            json!({"providers":providers}).to_string(),
        )?;
        let mut catalog = ModelCatalog::from_files(
            &config_dir.join("settings.yml"),
            &config_dir.join("models.yml"),
        )?;
        catalog.save_default_model("mock-root/primary")?;
        if test == MockTest::Task {
            for (role, lane) in [
                (ModelRole::Fast, "fast"),
                (ModelRole::Slow, "slow"),
                (ModelRole::Review, "review"),
            ] {
                catalog.save_model_role(
                    role,
                    Some(RoleAssignment {
                        enabled: true,
                        model: format!("mock-{lane}/primary"),
                        effort: ThinkLevel::None,
                        fallback: None,
                    }),
                )?;
            }
        }
        ensure!(
            scenario.lanes.contains_key("root"),
            "mock scenario has no root lane"
        );
        let lane_count = scenario.lanes.len();
        let mut scripts = BTreeMap::new();
        for (lane, script) in scenario.lanes {
            ensure!(
                ["root", "fast", "slow", "review"].contains(&lane.as_str()),
                "mock scenario has an unknown lane"
            );
            scripts.insert(format!("mock-{lane}/primary"), Arc::new(script));
        }
        // `Tools::new` defaults to an empty `McpManager`: mock scenarios never read real user MCP definitions.
        let mut tools = Tools::new(workspace.clone(), scenario.edit_mode)?;
        tools.ask_handler = ask;
        let mut agent = Agent::new_mock(catalog, tools, sessions, locks, Arc::new(scripts))?;
        let mut result = (|| {
            emit(MockTestEvent::Prepared(MockTestSnapshot {
                current_model: agent.current_model().clone(),
                choices: agent.model_choices(),
                think_level: agent.think_level(),
                default_think_level: agent.default_think_level(),
                usage: agent.context_usage()?,
                subagent_control: Arc::clone(&agent.subagent_control),
                pipe_store: agent.pipe_store(),
            }));
            match agent.run(&scenario.prompt, cancellation, None, &mut |event| {
                emit(MockTestEvent::Progress(event))
            })? {
                RunOutcome::Completed => {}
                RunOutcome::Cancelled => return Err(cancellation::Cancelled.into()),
                RunOutcome::AwaitingPlanReview => {
                    anyhow::bail!("mock scenario unexpectedly entered plan review")
                }
            }
            endpoint.assert_all_exhausted(&queues.id, lane_count)?;
            let parent = workspace
                .parent()
                .context("mock workspace has no parent directory")?;
            ensure!(
                !parent.join(ESCAPE_FILE).exists(),
                "mock write escaped its workspace"
            );
            Ok(())
        })();
        if let Err(error) = agent.stop_mock() {
            result = Err(match result {
                Err(previous) => error.context(format!("mock run failed: {previous:#}")),
                Ok(()) => error,
            });
        }
        drop(agent);
        result
    })();
    drop(queues);
    if let Err(error) = temp.close() {
        let error = anyhow::Error::new(error).context("removing mock run directory");
        result = Err(match result {
            Err(previous) if previous.is::<cancellation::CleanupError>() => {
                previous.context(format!("{error:#}"))
            }
            Err(previous) => error.context(format!("mock run failed: {previous:#}")),
            Ok(()) => error,
        });
    }
    result
}

fn setup_file(root: &Path, relative: &str, text: &str) -> Result<()> {
    let relative = Path::new(relative);
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|part| matches!(part, Component::Normal(_))),
        "mock fixture path must be relative and contain only normal components"
    );
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text).context("writing mock workspace fixture")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fail_retry_scenario_and_usage_reporting() {
        {
            use pp_core::events::RequestRetryReason;
            let server = super::super::MockServer::start().unwrap();
            let mut retries = Vec::new();
            let mut reply = String::new();
            let mut succeeded = false;
            let report = run_test(
                MockTest::Fail,
                server.endpoint(),
                None,
                &CancellationToken::new(),
                &mut |event| match event {
                    MockTestEvent::Progress(AgentEvent::RequestRetrying {
                        reason,
                        attempt,
                        wait_ms,
                    }) => retries.push((reason, attempt, wait_ms)),
                    MockTestEvent::Progress(AgentEvent::AssistantFinished { text }) => reply = text,
                    MockTestEvent::Progress(AgentEvent::ModelRequestFinished {
                        success, ..
                    }) => {
                        succeeded |= success;
                    }
                    MockTestEvent::Prepared(_) | MockTestEvent::Progress(_) => {}
                },
            )
            .unwrap();
            assert_eq!(retries.len(), 2);
            assert_eq!(retries[0], (RequestRetryReason::HttpStatus(503), 1, 2000));
            assert_eq!(retries[1], (RequestRetryReason::RateLimited, 2, 4000));
            assert!(succeeded, "third attempt must finish the request");
            assert!(reply.contains("Fail fixture complete"), "{reply}");
            assert_eq!(report.calls, 0);
        }
        {
            let server = super::super::MockServer::start().unwrap();
            let usage = pp_core::context::TokenUsage {
                input_tokens: 120,
                output_tokens: 30,
                cached_tokens: Some(40),
                cache_write_tokens: Some(5),
                reasoning_tokens: Some(0),
            };
            let scenario = serde_json::from_value(json!({
                "prompt":"Report the scripted usage.", "edit_mode":"replace", "files":{},
                "lanes":{"root":[{"think":"usage", "content":"reported", "usage":usage}]}
            }))
            .unwrap();
            let mut finished = None;
            let mut context = None;
            run_scenario(
                scenario,
                MockTest::Read,
                &server.endpoint(),
                None,
                &CancellationToken::new(),
                &mut |event| match event {
                    MockTestEvent::Progress(AgentEvent::ModelRequestFinished {
                        usage,
                        success: true,
                        ..
                    }) => finished = usage,
                    MockTestEvent::Progress(AgentEvent::ContextUpdated { usage }) => {
                        context = Some(usage)
                    }
                    _ => {}
                },
            )
            .unwrap();
            assert_eq!(finished, Some(usage));
            let context = context.unwrap();
            assert_eq!(context.last_request, Some(usage));
            assert_eq!(context.session_output_tokens, 30);
        }
    }

    #[test]
    fn fixed_mock_workspace_and_setup_path_validation() {
        {
            for declared in ["relative/dir", "/Users/mock", "/tmp", "/tmpfoo/mock", ""] {
                assert!(
                    FixedWorkspace::prepare(declared).is_err(),
                    "{declared} must be rejected"
                );
            }
            let created = format!("/tmp/pp-mock-fixed-{}", std::process::id());
            let existing = format!("/tmp/pp-mock-kept-{}", std::process::id());
            fs::create_dir_all(&existing).unwrap();
            {
                let first = FixedWorkspace::prepare(&created).unwrap();
                assert!(first.created && fs::metadata(&first.path).unwrap().is_dir());
                let kept = FixedWorkspace::prepare(&existing).unwrap();
                assert!(!kept.created);
            }
            assert!(
                !Path::new(&created).exists(),
                "created workspace is removed"
            );
            assert!(
                Path::new(&existing).exists(),
                "a pre-existing directory is kept"
            );
            fs::remove_dir_all(&existing).unwrap();
        }
        {
            let root = tempfile::tempdir().unwrap();
            let workspace = root.path().join("workspace");
            fs::create_dir(&workspace).unwrap();
            for path in ["../escape.txt", "/absolute.txt", "", "./relative.txt"] {
                assert!(setup_file(&workspace, path, "not written").is_err());
            }
            assert!(FixedWorkspace::prepare("/tmp/mock-safe/../escape").is_err());
            assert!(!root.path().join("escape.txt").exists());
            setup_file(&workspace, "nested/note.txt", "kept").unwrap();
            assert_eq!(
                fs::read_to_string(workspace.join("nested/note.txt")).unwrap(),
                "kept"
            );
        }
    }
}
