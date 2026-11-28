use crate::tools::{ToolPolicy, Tools};
use pp_core::shell_display::{ShellPresentation, ShellRow, ShellState};
use std::time::{Duration, Instant};

fn execute_shell(
    tools: &Tools,
    name: &str,
    args: serde_json::Value,
    token: &pp_core::cancellation::CancellationToken,
    events: &mut dyn FnMut(pp_core::events::AgentEvent),
) -> (serde_json::Value, serde_json::Value) {
    let mut result = None;
    let workflow = pp_core::plan::WorkflowState::default();
    let execution = tools.execute(
        name,
        &args.to_string(),
        "display-test",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        token,
        &mut |event| {
            if let pp_core::events::AgentEvent::ToolFinished { result: value, .. } = &event {
                result = Some(value.clone());
            }
            events(event);
        },
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let model = serde_json::from_str(&execution.output).unwrap();
    (model, result.unwrap())
}

fn shell(result: &serde_json::Value) -> ShellPresentation {
    serde_json::from_value(result["shell"].clone()).unwrap()
}

fn find<'a>(snapshot: &'a ShellPresentation, needle: &str) -> &'a ShellRow {
    snapshot
        .rows
        .iter()
        .find(|row| {
            row.segments
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>()
                .contains(needle)
        })
        .unwrap_or_else(|| panic!("did not find {needle}: {snapshot:?}"))
}

#[test]
fn shell_display_execution_pipelines_and_function_overrides() {
    use serde_json::json;

    {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
        let command = "printf ok; false && echo unreachable; for x in a b c; do printf '%s' \"$x\"; done; if test -f missing; then echo no; else echo yes; fi";
        let (model, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":command}),
            &Default::default(),
            &mut |_| {},
        );
        assert!(
            model.get("shell").is_none(),
            "display must not enter the model context"
        );
        assert_eq!(model["ok"], true);
        assert_eq!(model["output"], "okabcyes\n");
        let snapshot = shell(&result);
        assert!(!snapshot.running);
        assert_eq!(find(&snapshot, "unreachable").state, ShellState::Skipped);
        assert_eq!(find(&snapshot, "for x").meta, "↻ 3");
        assert_eq!(find(&snapshot, "print(\"no\")").state, ShellState::Skipped);
        assert_eq!(find(&snapshot, "print(\"yes\")").state, ShellState::Ok);
        assert_eq!(
            find(&snapshot, "is_file").state,
            ShellState::Ok,
            "a conditional false must not masquerade as an execution error"
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"sh -c 'exit 7' | cat"}),
            &Default::default(),
            &mut |_| {},
        );
        assert_eq!(result["exit_code"], 0);
        let snapshot = shell(&result);
        let row = &snapshot.rows[0];
        assert_eq!(row.state, ShellState::Fail);
        assert!(row.meta.contains("exit 7"));
        assert!(
            row.segments
                .iter()
                .any(|s| s.text == "cat" && s.state == ShellState::Ok)
        );
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"eval 'head() { printf from_function; }'; printf upstream | head -n 1"}),
            &Default::default(),
            &mut |_| {},
        );
        let snapshot = shell(&result);
        assert_eq!(result["output"], "from_function");
        assert!(
            !snapshot
                .rows
                .iter()
                .flat_map(|row| &row.segments)
                .any(|s| s.text.contains("first 1"))
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"if false; then echo() { printf custom; }; fi; echo ordinary"}),
            &Default::default(),
            &mut |_| {},
        );
        let snapshot = shell(&result);
        assert_eq!(result["output"], "ordinary\n");
        assert_eq!(find(&snapshot, "print(\"ordinary\")").state, ShellState::Ok);
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"eval 'f() { echo \"$FOO\"; }'; FOO=value f"}),
            &Default::default(),
            &mut |_| {},
        );
        assert_eq!(result["output"], "value\n");
        let snapshot = shell(&result);
        let row = find(&snapshot, "env(");
        assert!(
            row.segments
                .iter()
                .any(|segment| segment.text == "env(FOO = \"value\") ")
        );
    }
}

#[test]
fn shell_display_cancel_timeout_and_nested_scripts() {
    {
        use pp_core::{cancellation::CancellationToken, events::AgentEvent};
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
        let token = CancellationToken::new();
        let mut observed = false;
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"sleep 60; echo after"}),
            &token,
            &mut |event| {
                if let AgentEvent::ToolShellSnapshot { snapshot, .. } = event
                    && snapshot.rows.iter().any(|r| r.state == ShellState::Running)
                {
                    observed = true;
                    token.cancel();
                }
            },
        );
        assert!(
            observed,
            "cancellation must be triggered by an actually running snapshot"
        );
        assert_eq!(result["cancelled"], true);
        let snapshot = shell(&result);
        assert_eq!(find(&snapshot, "sleep 60").state, ShellState::Interrupted);
        assert_eq!(find(&snapshot, "after").state, ShellState::Pending);
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"bash -c 'echo nested; echo nested; echo nested; echo nested; echo nested; echo nested'"}),
            &CancellationToken::new(),
            &mut |_| {},
        );
        let snapshot = shell(&result);
        assert_eq!(
            find(&snapshot, "nested").node,
            None,
            "an external script has no execution identity inside"
        );
        assert_eq!(find(&snapshot, "nested").state, ShellState::Pending);
    }

    {
        use serde_json::json;
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"sleep 60; echo after-timeout", "timeout_seconds":1}),
            &Default::default(),
            &mut |_| {},
        );
        assert_eq!(result["timed_out"], true);
        let snapshot = shell(&result);
        assert_eq!(find(&snapshot, "sleep 60").state, ShellState::Interrupted);
        assert_eq!(find(&snapshot, "after-timeout").state, ShellState::Pending);
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"if sh -c 'exit 2'; then echo wrong; fi"}),
            &Default::default(),
            &mut |_| {},
        );
        let snapshot = shell(&result);
        assert_eq!(find(&snapshot, "if ").state, ShellState::Fail);
        assert!(find(&snapshot, "if ").meta.contains("exit 2"));
        let (_, result) = execute_shell(
            &tools,
            "bash",
            json!({"command":"while false; do echo never; done"}),
            &Default::default(),
            &mut |_| {},
        );
        assert_eq!(find(&shell(&result), "while ").meta, "↻ 0");
    }
}

#[test]
fn shell_display_background_snapshot_updates_and_freezes() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let token = pp_core::cancellation::CancellationToken::new();
    let (model, started) = execute_shell(
        &tools,
        "bash",
        json!({"command":"printf ready; sleep 60", "mode":"background"}),
        &token,
        &mut |_| {},
    );
    assert!(model.get("shell").is_none());
    let initial = shell(&started);
    assert!(initial.running);
    let id = started["task_id"].as_str().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (model, result) = execute_shell(
            &tools,
            "bash_task",
            json!({"task_id":id,"action":"status"}),
            &token,
            &mut |_| {},
        );
        assert!(model.get("shell").is_none());
        if result["output"]
            .as_str()
            .is_some_and(|output| output.contains("ready"))
            && shell(&result).revision > initial.revision
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "background command did not start: {result}"
        );
        std::thread::yield_now();
    }
    let (_, result) = execute_shell(
        &tools,
        "bash_task",
        json!({"task_id":id,"action":"stop"}),
        &token,
        &mut |_| {},
    );
    let final_snapshot = shell(&result);
    assert!(!final_snapshot.running);
    assert_eq!(find(&final_snapshot, "print").state, ShellState::Ok);
    assert_eq!(
        find(&final_snapshot, "sleep 60").state,
        ShellState::Interrupted
    );
}
