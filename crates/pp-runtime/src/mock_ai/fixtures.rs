//! 编译内嵌场景；每场使用独立的配置、会话、工具与临时工作区。

use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs,
    path::{Component, Path},
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
use crate::{
    agent::Agent,
    subagent::Binding,
    tools::{PipeStore, Tools},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockTest {
    Task,
    Ui,
    All,
    Read,
    Write,
    Edit,
    Grep,
    Glob,
    Bash,
    BashTask,
    Wait,
    Todo,
    Ask,
}

impl MockTest {
    pub const ALL: [Self; 13] = [
        Self::Task,
        Self::Ui,
        Self::All,
        Self::Read,
        Self::Write,
        Self::Edit,
        Self::Grep,
        Self::Glob,
        Self::Bash,
        Self::BashTask,
        Self::Wait,
        Self::Todo,
        Self::Ask,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Task => "/test-task",
            Self::Ui => "/test-ui",
            Self::All => "/test-tools",
            Self::Read => "/test-read",
            Self::Write => "/test-write",
            Self::Edit => "/test-edit",
            Self::Grep => "/test-grep",
            Self::Glob => "/test-glob",
            Self::Bash => "/test-bash",
            Self::BashTask => "/test-bash-task",
            Self::Wait => "/test-wait",
            Self::Todo => "/test-todo",
            Self::Ask => "/test-ask",
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
    files: BTreeMap<String, String>,
    lanes: BTreeMap<String, Value>,
}

const READ: &str = include_str!("scenarios/read.json");
const WRITE: &str = include_str!("scenarios/write.json");
const EDIT_REPLACE: &str = include_str!("scenarios/edit-replace.json");
const EDIT_HASHLINE: &str = include_str!("scenarios/edit-hashline.json");
const GREP: &str = include_str!("scenarios/grep.json");
const GLOB: &str = include_str!("scenarios/glob.json");
const BASH: &str = include_str!("scenarios/bash.json");
const BASH_TASK: &str = include_str!("scenarios/bash-task.json");
const WAIT: &str = include_str!("scenarios/wait.json");
const TODO: &str = include_str!("scenarios/todo.json");
const ASK: &str = include_str!("scenarios/ask.json");
const UI: &str = include_str!("scenarios/ui.json");
const TASK: &str = include_str!("scenarios/task.json");

fn scenarios(test: MockTest) -> &'static [&'static str] {
    match test {
        MockTest::All => &[
            READ,
            WRITE,
            EDIT_REPLACE,
            EDIT_HASHLINE,
            GREP,
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
        MockTest::Grep => &[GREP],
        MockTest::Glob => &[GLOB],
        MockTest::Bash => &[BASH],
        MockTest::BashTask => &[BASH_TASK],
        MockTest::Wait => &[WAIT],
        MockTest::Todo => &[TODO],
        MockTest::Ask => &[ASK],
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
    let mut result = (|| {
        let workspace = temp.path().join("workspace");
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
        "tools":{"approvalMode":"yolo"}, "rate_limit_retry_budget_seconds":0, "request_timeout_seconds":300
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
            ensure!(
                !temp.path().join("escape.txt").exists(),
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
    fn mock_ai_agent_preserves_reported_usage_in_context() {
        let server = super::super::MockServer::start().unwrap();
        let usage = pp_core::context::TokenUsage {
            input_tokens: 120,
            output_tokens: 30,
            cached_tokens: Some(40),
            cache_write_tokens: Some(5),
        };
        let scenario = serde_json::from_value(json!({
            "prompt":"Report the scripted usage.", "edit_mode":"replace", "files":{},
            "lanes":{"root":[{"think":"用量", "content":"reported", "usage":usage}]}
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

    #[test]
    fn mock_ai_fixture_setup_rejects_parent_and_absolute_paths() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        for path in ["../escape.txt", "/absolute.txt", "", "./relative.txt"] {
            assert!(setup_file(&workspace, path, "not written").is_err());
        }
        assert!(!root.path().join("escape.txt").exists());
        setup_file(&workspace, "nested/note.txt", "kept").unwrap();
        assert_eq!(
            fs::read_to_string(workspace.join("nested/note.txt")).unwrap(),
            "kept"
        );
    }
}
