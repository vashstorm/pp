use super::*;
use crate::agent::tests::{
    EXPLORATION, ORIGIN, PLAN_CONTENT, err_message, journal_workflow, plan_workflow,
    runtime_input_agent, seed_session, write_plan_file,
};
use crate::config::{EditMode, ModelCatalog};
use crate::tools::Tools;
use serde_json::json;

mod handoff_cases;
mod progress_archive_cases;
mod reconcile_resume_cases;
mod revision_cases;
mod todo_sync_cases;

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
    crate::tools::plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
}

fn registered_entry(agent: &Agent, id: &str) -> (String, PlanLocation) {
    let workspace = agent.tools.plan_workspace();
    let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
    let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
    let entry = snapshot.catalog.active_by_id(id).expect("entry registered");
    (entry.file_name.clone(), entry.location)
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
