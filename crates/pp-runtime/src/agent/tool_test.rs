//! Debug-only local tool display fixtures.
//!
//! Only compiled when `debug_assertions` is enabled; release builds exclude the
//! module, its `Agent` entry point, and the CLI display code with it.
//!
//! Every command runs the real `Tools` execution path in a fresh temporary
//! workspace below the startup cwd. Fixture failures are ordinary tool cards;
//! cancellation and process cleanup errors stop the suite.

#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolTest {
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

#[cfg(debug_assertions)]
impl ToolTest {
    pub const ALL: [Self; 11] = [
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

use crate::tools::ToolPolicy;
use crate::tools::Tools;
use crate::tools::todo::TodoChange;
use anyhow::{Context, Result, ensure};
use pp_config::config::EditMode;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use serde_json::{Value, json};
use std::{fs, path::Path};

pub struct ToolTestReport {
    pub calls: usize,
}

struct Suite<'a> {
    tools: &'a Tools,
    workflow: &'a mut WorkflowState,
    cancellation: &'a CancellationToken,
    emit: &'a mut dyn FnMut(AgentEvent),
    calls: &'a mut usize,
    suite: &'static str,
    next_id: usize,
}

impl Suite<'_> {
    fn call(&mut self, name: &str, arguments: Value) -> Result<Value> {
        cancellation::check(self.cancellation)?;
        let id = format!("test-{}-{}", self.suite, self.next_id);
        self.next_id += 1;
        *self.calls += 1;
        let arguments = arguments.to_string();
        let mut fetch = |_: &str, _: &CancellationToken| {
            anyhow::bail!("local tool fixtures do not perform network requests")
        };
        let execution = self.tools.execute_with_fetch(
            name,
            &arguments,
            &id,
            ToolPolicy::Execute {
                workflow: self.workflow,
            },
            self.cancellation,
            self.emit,
            &mut fetch,
        );
        if let Some(error) = execution.stop_error {
            return Err(error);
        }
        if let Some(change) = execution.todo_change {
            apply_local_todo(self.workflow, change);
        }
        cancellation::check(self.cancellation)?;
        serde_json::from_str(&execution.output).context("decoding local tool result")
    }
}

fn apply_local_todo(workflow: &mut WorkflowState, change: TodoChange) {
    workflow.todos = change.phases;
    workflow.todo_plan_id = change.plan.map(|plan| plan.plan_id);
}

fn write_fixture(root: &Path, relative: &str, content: &str) -> Result<()> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating fixture directory {}", parent.display()))?;
    }
    fs::write(&path, content).with_context(|| format!("writing fixture {}", path.display()))
}

fn header(content: &str) -> Result<&str> {
    let header = content
        .lines()
        .next()
        .context("hashline read header missing")?;
    ensure!(
        header.starts_with('[') && header.ends_with(']'),
        "hashline read header missing"
    );
    Ok(header)
}

impl crate::agent::Agent {
    pub fn run_tool_test(
        &self,
        selection: ToolTest,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<ToolTestReport> {
        cancellation::check(cancellation)?;
        let temp = tempfile::Builder::new()
            .prefix(".pp-test-tools-")
            .tempdir_in(self.tools.cwd())
            .context("creating local tool fixture workspace")?;
        let root = temp.path().to_path_buf();
        let mut workflow = WorkflowState {
            mode: WorkMode::Execute,
            ..Default::default()
        };
        let mut calls = 0usize;
        let tools = Some(self.isolated_tools(&root, self.tools.edit_mode())?);
        let include_edit = matches!(selection, ToolTest::All | ToolTest::Edit);
        let alternate = include_edit
            .then(|| {
                let mode = match self.tools.edit_mode() {
                    EditMode::Replace => EditMode::Hashline,
                    EditMode::Hashline => EditMode::Replace,
                };
                self.isolated_tools(&root, mode)
            })
            .transpose()?;

        let run = (|| -> Result<()> {
            if matches!(selection, ToolTest::All | ToolTest::Read) {
                read_suite(
                    &root,
                    &mut Suite {
                        tools: tools.as_ref().expect("fixture tools"),
                        workflow: &mut workflow,
                        cancellation,
                        emit,
                        calls: &mut calls,
                        suite: "read",
                        next_id: 0,
                    },
                )?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Write) {
                write_suite(
                    &root,
                    &mut Suite {
                        tools: tools.as_ref().expect("fixture tools"),
                        workflow: &mut workflow,
                        cancellation,
                        emit,
                        calls: &mut calls,
                        suite: "write",
                        next_id: 0,
                    },
                )?;
            }
            if include_edit {
                let current = tools.as_ref().expect("fixture tools");
                let other = alternate.as_ref().context("alternate edit tools")?;
                let (replace, hashline) = if current.edit_mode() == EditMode::Replace {
                    (current, other)
                } else {
                    (other, current)
                };
                replace_suite(
                    &root,
                    replace,
                    &mut workflow,
                    cancellation,
                    emit,
                    &mut calls,
                )?;
                hashline_suite(
                    &root,
                    hashline,
                    &mut workflow,
                    cancellation,
                    emit,
                    &mut calls,
                )?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Grep) {
                grep_suite(
                    &root,
                    &mut Suite {
                        tools: tools.as_ref().expect("fixture tools"),
                        workflow: &mut workflow,
                        cancellation,
                        emit,
                        calls: &mut calls,
                        suite: "grep",
                        next_id: 0,
                    },
                )?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Glob) {
                glob_suite(
                    &root,
                    &mut Suite {
                        tools: tools.as_ref().expect("fixture tools"),
                        workflow: &mut workflow,
                        cancellation,
                        emit,
                        calls: &mut calls,
                        suite: "glob",
                        next_id: 0,
                    },
                )?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Bash) {
                bash_suite(&mut Suite {
                    tools: tools.as_ref().expect("fixture tools"),
                    workflow: &mut workflow,
                    cancellation,
                    emit,
                    calls: &mut calls,
                    suite: "bash",
                    next_id: 0,
                })?;
            }
            if matches!(selection, ToolTest::All | ToolTest::BashTask) {
                bash_task_suite(&mut Suite {
                    tools: tools.as_ref().expect("fixture tools"),
                    workflow: &mut workflow,
                    cancellation,
                    emit,
                    calls: &mut calls,
                    suite: "bash-task",
                    next_id: 0,
                })?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Wait) {
                wait_suite(&mut Suite {
                    tools: tools.as_ref().expect("fixture tools"),
                    workflow: &mut workflow,
                    cancellation,
                    emit,
                    calls: &mut calls,
                    suite: "wait",
                    next_id: 0,
                })?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Todo) {
                todo_suite(&mut Suite {
                    tools: tools.as_ref().expect("fixture tools"),
                    workflow: &mut workflow,
                    cancellation,
                    emit,
                    calls: &mut calls,
                    suite: "todo",
                    next_id: 0,
                })?;
            }
            if matches!(selection, ToolTest::All | ToolTest::Ask) {
                ask_suite(&mut Suite {
                    tools: tools.as_ref().expect("fixture tools"),
                    workflow: &mut workflow,
                    cancellation,
                    emit,
                    calls: &mut calls,
                    suite: "ask",
                    next_id: 0,
                })?;
            }
            Ok(())
        })();

        let mut cleanup = None;
        if let Some(tools) = tools.as_ref()
            && let Err(error) = tools.stop_background_tasks()
        {
            cleanup = Some(error.context("stopping local tool fixture processes"));
        }
        if let Some(tools) = alternate.as_ref()
            && let Err(error) = tools.stop_background_tasks()
        {
            cleanup.get_or_insert(error.context("stopping alternate edit fixture processes"));
        }
        drop(alternate);
        drop(tools);
        let close = temp.close();
        let mut failure = cleanup;
        if let Err(error) = close {
            let error = anyhow::Error::new(error).context("removing local tool fixture workspace");
            failure = Some(match failure {
                Some(cleanup) => cleanup.context(format!(
                    "also failed to remove the fixture workspace: {error:#}"
                )),
                None => error,
            });
        }
        match (failure, run) {
            (Some(error), Err(run)) => {
                Err(error.context(format!("local tool suite also failed: {run:#}")))
            }
            (Some(error), Ok(())) => Err(error),
            (None, Err(error)) => Err(error),
            (None, Ok(())) => Ok(ToolTestReport { calls }),
        }
    }

    fn isolated_tools(&self, cwd: &Path, edit_mode: EditMode) -> Result<Tools> {
        let mut tools = Tools::new(cwd.to_path_buf(), edit_mode)?;
        tools.limits = self.tools.limits;
        tools.ask_handler = self.tools.ask_handler.clone();
        tools.approval = self.tools.approval.clone();
        Ok(tools)
    }
}

fn read_suite(root: &Path, suite: &mut Suite<'_>) -> Result<()> {
    write_fixture(
        root,
        "read/note.txt",
        "第一行：Unicode 内容\nsecond line\nthird line after blank\nfourth preview line\nfifth preview line\nsixth-tail-marker\n",
    )?;
    suite.call("read", json!({"path": "read/note.txt"}))?;
    suite.call("read", json!({"path": "read/note.txt:2-3"}))?;
    suite.call("read", json!({"path": "read/note.txt:raw"}))?;
    suite.call("read", json!({"path": "read/missing.txt"}))?;
    Ok(())
}

fn write_suite(root: &Path, suite: &mut Suite<'_>) -> Result<()> {
    let temp_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("fixture directory name")?;
    let escape_name = format!("{temp_name}-escape.txt");
    let escape = root.parent().context("fixture parent")?.join(&escape_name);
    ensure!(!escape.exists(), "fixture escape target already exists");
    suite.call(
        "write",
        json!({
            "path": "write/nested/demo.txt",
            "content": "created line one\n第二行\nthird line\n"
        }),
    )?;
    suite.call(
        "write",
        json!({
            "path": "write/nested/demo.txt",
            "content": "created line one changed\n第二行\nthird line\nfourth line\n"
        }),
    )?;
    suite.call(
        "write",
        json!({"path": format!("../{escape_name}"), "content": "must not be written"}),
    )?;
    ensure!(
        !escape.exists(),
        "path escape fixture wrote outside the temporary workspace"
    );
    Ok(())
}

fn replace_suite(
    root: &Path,
    tools: &Tools,
    workflow: &mut WorkflowState,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
    calls: &mut usize,
) -> Result<()> {
    let mut suite = Suite {
        tools,
        workflow,
        cancellation,
        emit,
        calls,
        suite: "edit-replace",
        next_id: 0,
    };
    write_fixture(root, "edit/replace.txt", "alpha\nbeta\ngamma\n")?;
    suite.call(
        "edit",
        json!({
            "path": "edit/replace.txt",
            "edits": [
                {"oldText": "alpha", "newText": "ALPHA"},
                {"oldText": "gamma", "newText": "GAMMA"}
            ]
        }),
    )?;
    suite.call(
        "edit",
        json!({
            "path": "edit/replace.txt",
            "edits": [{"oldText": "missing old text", "newText": "nope"}]
        }),
    )?;
    Ok(())
}

fn hashline_suite(
    root: &Path,
    tools: &Tools,
    workflow: &mut WorkflowState,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
    calls: &mut usize,
) -> Result<()> {
    let mut suite = Suite {
        tools,
        workflow,
        cancellation,
        emit,
        calls,
        suite: "edit-hashline",
        next_id: 0,
    };
    write_fixture(root, "edit/hashline.txt", "one\ntwo\nthree\n")?;
    let read = suite.call("read", json!({"path": "edit/hashline.txt"}))?;
    let content = read["content"].as_str().context("hashline read content")?;
    let old_header = header(content)?.to_owned();
    suite.call(
        "edit",
        json!({"input": format!("{old_header}\nPUT 2.=2:\n+TWO\nPUT >3:\n+four")}),
    )?;
    suite.call("edit", json!({"input": format!("{old_header}\nCUT 1.=1")}))?;
    Ok(())
}

fn grep_suite(root: &Path, suite: &mut Suite<'_>) -> Result<()> {
    write_fixture(
        root,
        "grep/alpha.txt",
        "Alpha one\nneedle first\ncontext before\nneedle second\ncontext after\n",
    )?;
    write_fixture(root, "grep/beta.txt", "beta Needle case\n")?;
    write_fixture(
        root,
        "grep/long.txt",
        &format!("needle {}\n", "0123456789".repeat(700)),
    )?;
    suite.call("grep", json!({"pattern": "needle", "path": "grep"}))?;
    suite.call(
        "grep",
        json!({"pattern": "NEEDLE", "path": "grep", "case": false}),
    )?;
    suite.call(
        "grep",
        json!({"pattern": "zzz-absent-pattern", "path": "grep"}),
    )?;
    suite.call("grep", json!({"pattern": "needle", "path": "grep/missing"}))?;
    Ok(())
}

fn glob_suite(root: &Path, suite: &mut Suite<'_>) -> Result<()> {
    for index in 0..6 {
        write_fixture(root, &format!("glob/dir-{index}/file-{index}.txt"), "x\n")?;
    }
    write_fixture(root, "glob/.hidden.txt", "hidden\n")?;
    suite.call("glob", json!({"path": "glob/**/*.txt"}))?;
    suite.call("glob", json!({"path": "glob/**/*", "hidden": true}))?;
    suite.call("glob", json!({"path": "glob/**/*.txt", "limit": 1}))?;
    suite.call("glob", json!({"path": "glob/**/*.absent"}))?;
    Ok(())
}

fn bash_suite(suite: &mut Suite<'_>) -> Result<()> {
    suite.call(
        "bash",
        json!({"command": "printf 'stdout-固定\\n'; printf 'stderr-行\\n' >&2; printf 'last-line\\n'"}),
    )?;
    suite.call(
        "bash",
        json!({"command": "printf 'before-nonzero-exit\\n'; exit 7"}),
    )?;
    suite.call(
        "bash",
        json!({"command": "printf 'pipeline-alpha\\n' | cat"}),
    )?;
    suite.call(
        "bash",
        json!({"command": "i=0; while [ $i -lt 400 ]; do printf 'line-%04d-abcdefghijklmnopqrstuvwxyz\\n' \"$i\"; i=$((i+1)); done"}),
    )?;
    Ok(())
}

fn bash_task_suite(suite: &mut Suite<'_>) -> Result<()> {
    let started = suite.call(
        "bash",
        json!({
            "command": "printf 'background-start\\n'; sleep 20; printf 'background-end\\n'",
            "mode": "background"
        }),
    )?;
    let id = started["task_id"]
        .as_str()
        .context("background fixture task id")?
        .to_owned();
    suite.call("bash_task", json!({"task_id": id, "action": "status"}))?;
    suite.call("bash_task", json!({"task_id": id, "action": "stop"}))?;
    suite.call("bash_task", json!({"task_id": id, "action": "stop"}))?;
    Ok(())
}

fn wait_suite(suite: &mut Suite<'_>) -> Result<()> {
    let short = suite.call(
        "bash",
        json!({"command": "printf 'wait-short-settled\\n'", "mode": "background"}),
    )?;
    let short = short["task_id"]
        .as_str()
        .context("short background fixture task id")?
        .to_owned();
    suite.tools.start_wait_batch();
    suite.call("wait", json!({"task_ids": [short]}))?;
    let long = suite.call(
        "bash",
        json!({
            "command": "printf 'wait-long-running\\n'; sleep 1; printf 'wait-long-end\\n'",
            "mode": "background"
        }),
    )?;
    let long = long["task_id"]
        .as_str()
        .context("long background fixture task id")?
        .to_owned();
    suite.tools.start_wait_batch();
    suite.call("wait", json!({"task_ids": [long]}))?;
    Ok(())
}

fn todo_suite(suite: &mut Suite<'_>) -> Result<()> {
    for arguments in [
        json!({
            "op": "init",
            "list": [
                {"phase": "Phase A", "items": ["first task", "second task"]},
                {"phase": "Phase B", "items": ["third task"]}
            ]
        }),
        json!({"op": "start", "task": "first task"}),
        json!({"op": "done", "task": "first task"}),
        json!({"op": "block", "task": "second task", "reason": "fixture blocker"}),
        json!({"op": "unblock", "task": "second task"}),
        json!({"op": "append", "phase": "Phase B", "items": ["fourth task"]}),
        json!({"op": "rm", "task": "fourth task"}),
        json!({"op": "drop", "task": "third task"}),
        json!({"op": "view"}),
    ] {
        suite.call("todo", arguments)?;
    }
    Ok(())
}

fn ask_suite(suite: &mut Suite<'_>) -> Result<()> {
    suite.call(
        "ask",
        json!({
            "questions": [
                {
                    "id": "pick",
                    "header": "Fixture choice",
                    "question": "Pick the recommended local fixture option.",
                    "options": [
                        {"label": "First option", "description": "plain option"},
                        {"label": "Recommended option", "description": "recommended fixture option"},
                        {"label": "Preview option", "preview": "preview\nbody"}
                    ],
                    "recommended": 1
                },
                {
                    "id": "multi",
                    "question": "Select several fixture labels.",
                    "multi": true,
                    "options": [
                        {"label": "Alpha label"},
                        {"label": "Beta label"},
                        {"label": "Gamma label"}
                    ]
                },
                {
                    "id": "free",
                    "question": "Type a free-form fixture answer.",
                    "options": []
                }
            ]
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_todo_change_never_mutates_the_fixture_plan_document() {
        let document = pp_core::plan::PlanDocument {
            content: "# Fixture plan\n".into(),
            ..pp_core::plan::PlanDocument::new().unwrap()
        };
        let mut workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(document.clone()),
            todo_plan_id: Some(document.id.clone()),
            todos: vec![pp_core::todo::TodoPhase {
                name: "Old".into(),
                tasks: Vec::new(),
            }],
            ..Default::default()
        };
        apply_local_todo(
            &mut workflow,
            TodoChange {
                phases: vec![pp_core::todo::TodoPhase {
                    name: "Fixture".into(),
                    tasks: Vec::new(),
                }],
                plan: None,
            },
        );
        assert_eq!(workflow.todos[0].name, "Fixture");
        assert!(workflow.todo_plan_id.is_none());
        let unchanged = workflow.plan.as_ref().expect("fixture plan preserved");
        assert_eq!(unchanged.id, document.id);
        assert_eq!(unchanged.revision, document.revision);
        assert_eq!(unchanged.content, document.content);
    }
}
