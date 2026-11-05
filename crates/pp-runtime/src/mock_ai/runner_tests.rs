//! `run_test` 的真实 Agent 场景回归。
//!
//! 每场使用独立的 loopback mock server 和随机 run id；本文件不安装全局
//! endpoint，也不修改进程环境、HOME 或工作目录。工具结果断言只检查真实
//! `ToolFinished` 数据，不合成事件或结果。

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use pp_core::{
    cancellation::{CancellationToken, Cancelled},
    events::AgentEvent,
    model::ThinkLevel,
};
use serde_json::Value;

use super::{MockServer, MockTest, MockTestEvent, run_test};

struct Call {
    name: String,
    result: Value,
}

#[derive(Default)]
struct Recorder {
    pending: BTreeMap<String, String>,
    calls: Vec<Call>,
    started: usize,
}

impl Recorder {
    fn record(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ToolStarted { id, name, .. } => {
                self.started += 1;
                assert!(
                    self.pending.insert(id, name).is_none(),
                    "duplicate tool call id"
                );
            }
            AgentEvent::ToolFinished { id, result } => {
                let name = self
                    .pending
                    .remove(&id)
                    .expect("tool finished without a start");
                self.calls.push(Call { name, result });
            }
            _ => {}
        }
    }

    fn calls(&self, name: &str) -> Vec<&Value> {
        self.calls
            .iter()
            .filter(|call| call.name == name)
            .map(|call| &call.result)
            .collect()
    }

    fn finish(&self) {
        assert!(
            self.pending.is_empty(),
            "tool call never finished: {:?}",
            self.pending
        );
        assert_eq!(self.started, self.calls.len());
    }
}

fn assert_all_suites(recorder: &Recorder) {
    // read：完整、range、raw、缺失文件，以及 hashline 场景的注册读取。
    let reads = recorder.calls("read");
    assert_eq!(reads.len(), 5);
    assert_eq!(reads[0]["ok"], true);
    let full = reads[0]["content"].as_str().expect("read content");
    assert!(full.contains("第一行：Unicode 内容"), "{full}");
    assert!(full.contains("sixth-tail-marker"), "{full}");
    assert_eq!(reads[1]["ok"], true);
    assert_eq!(reads[1]["lines_read"], 2);
    let ranged = reads[1]["content"].as_str().expect("range content");
    assert!(ranged.contains("second line"), "{ranged}");
    assert!(ranged.contains("third line after blank"), "{ranged}");
    assert!(!ranged.contains("sixth-tail-marker"), "{ranged}");
    assert_eq!(reads[2]["ok"], true);
    assert!(
        reads[2]["content"]
            .as_str()
            .expect("raw content")
            .contains("sixth-tail-marker")
    );
    assert_eq!(reads[3]["ok"], false);
    assert!(
        reads[3]["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    let header = reads[4]["content"]
        .as_str()
        .expect("hashline read content")
        .lines()
        .next()
        .expect("hashline read header");
    assert!(
        header.starts_with('[') && header.ends_with(']') && header.contains('#'),
        "{header}"
    );

    // write：两次嵌套写入成功，越界写入作为普通工具失败返回。
    let writes = recorder.calls("write");
    assert_eq!(writes.len(), 3);
    assert_eq!(writes[0]["ok"], true);
    assert!(
        writes[0]["bytes_written"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0)
    );
    assert_eq!(writes[1]["ok"], true);
    assert_eq!(writes[2]["ok"], false);
    assert!(
        writes[2]["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );

    // edit：replace 两处成功 + missing oldText 失败；hashline 两处 PUT 成功
    // + 旧 TAG CUT stale 失败。
    let edits = recorder.calls("edit");
    assert_eq!(edits.len(), 4);
    assert_eq!(edits[0]["ok"], true);
    assert_eq!(edits[0]["edits_applied"], 2);
    assert_eq!(edits[1]["ok"], false);
    assert_eq!(edits[2]["ok"], true);
    assert_eq!(edits[2]["edits_applied"], 2);
    assert_eq!(edits[3]["ok"], false);

    let greps = recorder.calls("grep");
    assert_eq!(greps.len(), 4);
    assert_eq!(greps[0]["ok"], true);
    assert!(
        greps[0]["content"]
            .as_str()
            .is_some_and(|text| text.contains("needle first") && text.contains("needle second"))
    );
    assert_eq!(greps[1]["ok"], true);
    assert!(
        greps[1]["content"]
            .as_str()
            .is_some_and(|text| text.contains("Needle case"))
    );
    assert_eq!(greps[2]["ok"], true);
    assert!(
        greps[2]["content"]
            .as_str()
            .is_some_and(|text| text.contains("No matches found"))
    );
    assert_eq!(greps[3]["ok"], false);

    let globs = recorder.calls("glob");
    assert_eq!(globs.len(), 4);
    assert_eq!(globs[0]["ok"], true);
    let visible = globs[0]["content"].as_str().expect("glob content");
    assert!(visible.contains("file-0.txt"), "{visible}");
    assert_eq!(globs[1]["ok"], true);
    assert!(
        globs[1]["content"]
            .as_str()
            .expect("hidden glob content")
            .contains(".hidden.txt")
    );
    assert_eq!(globs[2]["ok"], true);
    assert_eq!(globs[2]["truncated"], true);
    assert_eq!(globs[3]["ok"], true);
    assert!(
        globs[3]["content"]
            .as_str()
            .is_some_and(|text| text.contains("No files found"))
    );

    // bash 顺序：前台输出、exit 7、管道、400 行大输出，然后是 bash-task
    // 与 wait 两个场景各自启动的后台任务。
    let bash = recorder.calls("bash");
    assert_eq!(bash.len(), 7);
    assert_eq!(bash[0]["ok"], true);
    let first = bash[0]["output"].as_str().expect("bash output");
    assert!(first.contains("stdout-固定"), "{first}");
    assert!(first.contains("stderr-行"), "{first}");
    assert!(first.contains("last-line"), "{first}");
    assert_eq!(bash[1]["ok"], false);
    assert_eq!(bash[1]["exit_code"], 7);
    assert_eq!(bash[2]["ok"], true);
    assert!(
        bash[2]["output"]
            .as_str()
            .expect("pipeline output")
            .contains("pipeline-alpha")
    );
    assert_eq!(bash[3]["ok"], true);
    assert!(
        bash[3]["total_lines"]
            .as_u64()
            .is_some_and(|lines| lines >= 400)
    );
    assert_eq!(bash[4]["task_id"], "bg-1");
    assert_eq!(bash[4]["status"], "running");
    assert_eq!(bash[5]["task_id"], "bg-1");
    assert_eq!(bash[6]["task_id"], "bg-2");

    let tasks = recorder.calls("bash_task");
    assert_eq!(tasks.len(), 3);
    assert!(tasks.iter().all(|task| task["task_id"] == "bg-1"));
    assert_eq!(tasks[0]["ok"], true);
    assert_eq!(tasks[1]["ok"], true);
    assert_eq!(tasks[1]["status"], "stopped");
    assert_eq!(tasks[2]["ok"], true);
    assert_eq!(tasks[2]["status"], "stopped");

    let waits = recorder.calls("wait");
    assert_eq!(waits.len(), 2);
    for (index, marker) in ["wait-short-settled", "wait-long-end"]
        .into_iter()
        .enumerate()
    {
        let wait = waits[index];
        assert_eq!(wait["ok"], true);
        assert_eq!(wait["pending"], false);
        assert!(
            wait["results"]
                .as_array()
                .is_some_and(|results| results.is_empty())
        );
        let background = wait["background_results"]
            .as_array()
            .expect("background wait results");
        assert_eq!(background.len(), 1);
        assert_eq!(background[0]["status"], "exited");
        assert_eq!(background[0]["exit_code"], 0);
        assert!(
            background[0]["output"]
                .as_str()
                .is_some_and(|text| text.contains(marker))
        );
        let jobs = wait["jobs"].as_array().expect("wait jobs");
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["status"], "exited");
    }

    let todos = recorder.calls("todo");
    assert_eq!(todos.len(), 9);
    assert!(todos.iter().all(|todo| todo["ok"] == true));
    let view = todos[8];
    assert_eq!(view["op"], "view");
    let phases = view["phases"].as_array().expect("todo phases");
    let items = phases
        .iter()
        .flat_map(|phase| phase["tasks"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    let first = items
        .iter()
        .find(|item| item["content"] == "first task")
        .expect("completed todo item");
    assert_eq!(first["status"], "completed");
    let second = items
        .iter()
        .find(|item| item["content"] == "second task")
        .expect("unblocked todo item");
    assert_ne!(second["status"], "blocked");
    assert!(second.get("blocker").is_none_or(Value::is_null));
    let third = items
        .iter()
        .find(|item| item["content"] == "third task")
        .expect("dropped todo item");
    assert_eq!(third["status"], "abandoned");
    assert!(items.iter().all(|item| item["content"] != "fourth task"));

    let asks = recorder.calls("ask");
    assert_eq!(asks.len(), 1);
    assert_eq!(asks[0]["ok"], false);
    assert!(
        asks[0]["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
}

#[test]
fn mock_runner_all_suites_execute_real_tools_and_continue_after_failures() {
    let server = MockServer::start().expect("mock AI server");
    let mut recorder = Recorder::default();
    let mut prepared = 0usize;
    let report = run_test(
        MockTest::All,
        server.endpoint(),
        None,
        &CancellationToken::new(),
        &mut |event| match event {
            MockTestEvent::Prepared(snapshot) => {
                prepared += 1;
                assert_eq!(snapshot.current_model.reference, "mock-root/primary");
                assert!(
                    snapshot
                        .choices
                        .iter()
                        .any(|choice| choice.reference == "mock-root/primary")
                );
            }
            MockTestEvent::Progress(event) => recorder.record(event),
        },
    )
    .expect("all mock tool suites");
    assert_eq!(prepared, 11, "every suite must run in a fresh mock Agent");
    recorder.finish();
    assert_eq!(report.calls, recorder.started);
    assert_all_suites(&recorder);
}

#[test]
fn mock_runner_task_children_complete_consume_and_report_markers() {
    let server = MockServer::start().expect("mock AI server");
    let mut recorder = Recorder::default();
    let mut control = None;
    let mut summary = None;
    let mut observed_registry = None;
    let report = run_test(
        MockTest::Task,
        server.endpoint(),
        None,
        &CancellationToken::new(),
        &mut |event| match event {
            MockTestEvent::Prepared(snapshot) => {
                assert_eq!(snapshot.current_model.reference, "mock-root/primary");
                assert_eq!(snapshot.think_level, ThinkLevel::None);
                assert_eq!(snapshot.default_think_level, ThinkLevel::None);
                assert_eq!(snapshot.usage.context_window, Some(128_000));
                assert!(
                    snapshot
                        .choices
                        .iter()
                        .any(|choice| choice.reference == "mock-root/primary")
                );
                assert!(
                    snapshot
                        .subagent_control
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .is_none(),
                    "a fresh mock Agent must not own a registry before the run"
                );
                control = Some(snapshot.subagent_control.clone());
            }
            MockTestEvent::Progress(AgentEvent::AssistantFinished { text })
                if text.contains("review-result-33") =>
            {
                let guard = control
                    .as_ref()
                    .expect("prepared subagent control")
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let binding = guard
                    .as_ref()
                    .expect("subagent registry installed during the run");
                // 在 Agent 清理之前抓取注册表：结果必须已经交付并消费。
                observed_registry = Some(binding.snapshots());
                summary = Some(text);
            }
            MockTestEvent::Progress(event) => recorder.record(event),
        },
    )
    .expect("task mock scenario");
    recorder.finish();
    assert_eq!(report.calls, recorder.started);
    assert_eq!(recorder.started, 2);
    let task_calls = recorder.calls("task");
    assert_eq!(task_calls.len(), 1);
    let wait_calls = recorder.calls("wait");
    assert_eq!(wait_calls.len(), 1);
    assert_eq!(recorder.started, task_calls.len() + wait_calls.len());

    let tasks = task_calls[0]["tasks"]
        .as_array()
        .expect("task tool results");
    let ids = ["r1", "r2", "r3"];
    assert_eq!(tasks.len(), ids.len());
    for (task, id) in tasks.iter().zip(ids) {
        assert_eq!(task["run_id"], id);
        assert_eq!(task["status"], "queued");
        assert_eq!(task["blocking"], false);
    }

    let wait = wait_calls[0];
    let results = wait["results"].as_array().expect("delivered runs");
    assert_eq!(results.len(), ids.len());
    let markers = [
        "fast-result-11: 3 Rust files",
        "slow-result-22: src/events.rs:1",
        "review-result-33: src/events.rs:1 Text has no request identifier",
    ];
    for ((entry, id), marker) in results.iter().zip(ids).zip(markers) {
        assert_eq!(entry["delivery_marker"], format!("pp-subagent-result:{id}"));
        assert_eq!(entry["run"]["id"], id);
        assert_eq!(entry["run"]["status"], "completed");
        assert_eq!(entry["run"]["result"]["ok"], true);
        assert!(
            entry["run"]["result"]["output"]
                .as_str()
                .is_some_and(|output| output.contains(marker)),
            "{}",
            entry["run"]["result"]["output"]
        );
    }
    let jobs = wait["jobs"].as_array().expect("wait jobs");
    assert_eq!(jobs.len(), ids.len());
    assert!(jobs.iter().all(|job| job["status"] == "completed"));

    let summary = summary.expect("final summary after the wait");
    let expected = "fast-rust-count: completed (fast-result-11)\n\
                    slow-symbol-lookup: completed (slow-result-22)\n\
                    review-events-risk: completed (review-result-33)";
    assert!(summary.contains(expected), "{summary}");

    let registry = observed_registry.expect("registry snapshot before cleanup");
    let runs = registry["runs"].as_array().expect("registry runs");
    assert_eq!(runs.len(), ids.len());
    for (id, marker) in ids.iter().zip(markers) {
        let run = runs
            .iter()
            .find(|run| run["id"] == *id)
            .unwrap_or_else(|| panic!("missing run {id}"));
        assert_eq!(run["status"], "completed");
        assert_eq!(run["consumed"], true);
        assert!(
            run["result"]["output"]
                .as_str()
                .is_some_and(|output| output.contains(marker)),
            "{id}"
        );
    }
    let agents = registry["agents"].as_array().expect("registry agents");
    assert_eq!(agents.len(), ids.len());
    assert!(agents.iter().all(|agent| agent["status"] == "idle"));
}

fn mock_run_dir(control: &Arc<Mutex<Option<crate::subagent::Binding>>>) -> PathBuf {
    let guard = control.lock().unwrap_or_else(|error| error.into_inner());
    guard
        .as_ref()
        .expect("mock root registry")
        .directory()
        .ancestors()
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("pp-mock-"))
        })
        .expect("registry belongs to the mock temporary directory")
        .to_path_buf()
}

#[test]
fn mock_runner_ui_cancels_early_and_next_run_starts_from_the_first_chunk() {
    let server = MockServer::start().expect("mock AI server");
    let mut control = None;

    let token = CancellationToken::new();
    let mut first_dir = None;
    let mut first_thinking = None;
    let error = run_test(
        MockTest::Ui,
        server.endpoint(),
        None,
        &token,
        &mut |event| match event {
            MockTestEvent::Prepared(snapshot) => control = Some(snapshot.subagent_control),
            MockTestEvent::Progress(AgentEvent::ThinkingDelta { text }) => {
                if first_thinking.is_none() {
                    first_thinking = Some(text);
                    first_dir = Some(mock_run_dir(control.as_ref().expect("prepared registry")));
                    token.cancel();
                }
            }
            MockTestEvent::Progress(_) => {}
        },
    )
    .expect_err("cancelled mock UI run must fail");
    let first_thinking = first_thinking.expect("UI run must stream thinking before cancellation");
    assert!(
        first_thinking.contains("UI-TEST-THINKING-START"),
        "{first_thinking}"
    );
    assert!(
        error
            .chain()
            .any(|cause| cause.downcast_ref::<Cancelled>().is_some()),
        "{error:#}"
    );
    let first_dir = first_dir.expect("UI run created its temporary directory");
    assert!(
        !first_dir.exists(),
        "cancelled mock run left {} behind",
        first_dir.display()
    );

    let mut second_dir = None;
    control = None;
    let mut first_chunk = None;
    let mut thinking = String::new();
    let mut assistant = String::new();
    let mut prepared = 0usize;
    let report = run_test(
        MockTest::Ui,
        server.endpoint(),
        None,
        &CancellationToken::new(),
        &mut |event| match event {
            MockTestEvent::Prepared(snapshot) => {
                prepared += 1;
                assert_eq!(snapshot.current_model.reference, "mock-root/primary");
                control = Some(snapshot.subagent_control);
            }
            MockTestEvent::Progress(AgentEvent::ThinkingDelta { text }) => {
                if first_chunk.is_none() {
                    first_chunk = Some(text.clone());
                    second_dir = Some(mock_run_dir(control.as_ref().expect("prepared registry")));
                }
                thinking.push_str(&text);
            }
            MockTestEvent::Progress(AgentEvent::AssistantDelta { text, .. }) => {
                assistant.push_str(&text);
            }
            MockTestEvent::Progress(_) => {}
        },
    )
    .expect("second mock UI run");
    assert_eq!(prepared, 1);
    assert_eq!(report.calls, 0);
    let first_chunk = first_chunk.expect("second run must stream thinking");
    assert!(
        first_chunk.starts_with("UI-TEST-THINKING-START"),
        "a new run must start from the first chunk, got {first_chunk:?}"
    );
    assert!(thinking.contains("UI-TEST-THINKING-END"), "{thinking}");
    assert!(assistant.contains("UI-TEST-ASSISTANT-START"), "{assistant}");
    assert!(assistant.contains("UI-TEST-CODE-MARKER"), "{assistant}");
    assert!(assistant.contains("UI-TEST-ASSISTANT-END"), "{assistant}");
    let second_dir = second_dir.expect("second UI run created its temporary directory");
    assert!(
        !second_dir.exists(),
        "finished mock run left {} behind",
        second_dir.display()
    );
}
