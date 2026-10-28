use super::*;
use crate::plan::PlanLocation;
use std::{fs, sync::mpsc::sync_channel};

fn execute(tools: &Tools, name: &str, args: Value) -> Value {
    let workflow = WorkflowState::default();
    let token = CancellationToken::new();
    let execution = tools.execute(
        name,
        &args.to_string(),
        "test-call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |_| {},
    );
    assert!(
        execution.stop_error.is_none(),
        "unexpected cleanup failure: {:?}",
        execution.stop_error
    );
    serde_json::from_str(&execution.output).unwrap()
}
fn read_content(tools: &Tools, path: &str) -> (String, bool) {
    let read = execute(tools, "read", json!({"path": path}));
    assert_eq!(read["ok"], true, "{read}");
    (
        read["content"].as_str().unwrap().to_owned(),
        read["truncated"].as_bool().unwrap(),
    )
}

fn row_text(content: &str) -> Vec<&str> {
    content
        .lines()
        .filter(|line| !line.starts_with('['))
        .map(|line| line.split_once(':').unwrap().1)
        .collect()
}

fn hashline_args(content: &str, operations: &str) -> Value {
    json!({"input":format!("{}\n{operations}", content.lines().next().unwrap())})
}

fn background_wait(tools: &Tools, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let snapshot = execute(tools, "bash_task", json!({"task_id":id,"action":"status"}));
        if ready(&snapshot) {
            return snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "background task did not reach expected state: {snapshot}"
        );
        thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn background_marker(path: &Path) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && !text.is_empty()
        {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "missing marker: {}",
            path.display()
        );
        thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn wait_until_gone(pid: u32, what: &str) {
    use rustix::process::{Pid, test_kill_process};
    let pid = Pid::from_raw(pid as i32).expect("positive pid");
    for _ in 0..1000 {
        if test_kill_process(pid).is_err() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("{what} (pid {pid}) still alive after cancellation");
}

fn plan_document() -> crate::plan::PlanDocument {
    crate::plan::PlanDocument::new().unwrap()
}

fn assert_started_finished_pairs(events: &[AgentEvent], expected_calls: usize) {
    let mut started = Vec::new();
    let mut finished = Vec::new();
    for event in events {
        match event {
            AgentEvent::ToolStarted { id, .. } => started.push(id.clone()),
            AgentEvent::ToolFinished { id, .. } => finished.push(id.clone()),
            _ => panic!("unexpected event during denied tool call"),
        }
    }
    assert_eq!(started.len(), expected_calls, "{started:?}");
    assert_eq!(started, finished);
}

struct ChunkedReader<'a> {
    remaining: &'a [u8],
    chunk_size: usize,
}

impl Read for ChunkedReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.remaining.len().min(buffer.len()).min(self.chunk_size);
        buffer[..count].copy_from_slice(&self.remaining[..count]);
        self.remaining = &self.remaining[count..];
        Ok(count)
    }
}

fn stream_capture(bytes: &[u8], chunk_size: usize) -> (String, String, bool, usize) {
    let mut reader = ChunkedReader {
        remaining: bytes,
        chunk_size,
    };
    let (sender, receiver) = sync_channel(16);
    let mut captured = StreamText::default();
    let mut live = String::new();
    let mut received = 0;
    let truncated = thread::scope(|scope| {
        let reader = &mut reader;
        let worker = scope.spawn(move || {
            drain_pipe(
                reader,
                OutputStream::Stdout,
                sender,
                &std::sync::atomic::AtomicBool::new(false),
            )
        });
        for (stream, bytes) in receiver {
            assert!(matches!(stream, OutputStream::Stdout));
            assert!(bytes.len() <= PIPE_CHUNK_SIZE);
            received += bytes.len();
            live.push_str(&captured.append(&bytes));
        }
        live.push_str(&captured.finish());
        worker.join().unwrap().unwrap()
    });
    assert!(
        reader.remaining.is_empty(),
        "over-limit bytes must be drained"
    );
    (captured.text, live, truncated, received)
}

// ---------------------------------------------------------------------
// todo routing policy, plan adapter and finished-plan detach
// ---------------------------------------------------------------------

/// Registered executing/finished plan fixture: canonical Plans entry,
/// matching file on disk, and a workflow binding the document with its
/// projected todos.
fn plan_todo_fixture(
    dir: &tempfile::TempDir,
    file_name: &str,
    status: PlanStatus,
    content: &str,
) -> (Tools, crate::plan::PlanDocument, WorkflowState) {
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let mut document = crate::plan::PlanDocument::new().unwrap();
    document.revision = 1;
    document.title = "Todo integration".into();
    document.content = content.to_owned();
    document.origin_prompt = "original task".into();
    document.file_name = Some(file_name.to_owned());
    document.location = PlanLocation::Plans;
    document.status = status;
    document.approved_tasks = Some(
        crate::plan::plan_tasks(content)
            .iter()
            .map(|task| task.text.clone())
            .collect(),
    );
    let workspace = tools.plan_workspace();
    {
        let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(plan_catalog::PlanCatalogEntry {
            id: document.id.clone(),
            file_name: file_name.to_owned(),
            location: PlanLocation::Plans,
            deleted: false,
            deleted_file_identity: None,
        });
        plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
    }
    let path = crate::plan::path_for(file_name, PlanLocation::Plans);
    let target = workspace.target(std::path::Path::new(&path), true).unwrap();
    target.save_new(content.as_bytes()).unwrap();
    let workflow = WorkflowState {
        mode: WorkMode::Execute,
        plan: Some(document.clone()),
        review: None,
        todos: todo::project_plan_todos(&document, &[], false).unwrap(),
        todo_plan_id: Some(document.id.clone()),
    };
    (tools, document, workflow)
}

fn todo_call(tools: &Tools, workflow: &WorkflowState, arguments: Value) -> ToolExecution {
    let token = CancellationToken::new();
    tools.execute(
        "todo",
        &arguments.to_string(),
        "todo-call",
        ToolPolicy::Execute { workflow },
        &token,
        &mut |_| {},
    )
}

mod background_cases;
mod changes_cases;
mod edit_cases;
mod file_cases;
mod policy_cases;
mod streaming_cases;
mod todo_cases;
