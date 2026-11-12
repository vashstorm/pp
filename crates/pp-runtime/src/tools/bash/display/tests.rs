use super::*;
use brush_core::parser::Parser;
use std::io::Cursor;

fn parsed(source: &str) -> (Trace, ast::Program, u64) {
    let mut reader = Cursor::new(source.as_bytes());
    let program = Parser::new(&mut reader, &Default::default())
        .parse_program()
        .unwrap();
    let trace = Trace::new();
    let unit = trace.parsed(&program, Some(source), None).unwrap();
    (trace, program, unit)
}

fn text(trace: &Trace) -> Vec<String> {
    trace
        .snapshot()
        .unwrap()
        .rows
        .iter()
        .map(|row| {
            format!(
                "{}{}",
                "  ".repeat(usize::from(row.depth)),
                row.segments
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<String>()
            )
        })
        .collect()
}

#[test]
fn shell_display_matches_omp_reference_rows() {
    // 预期值由本机 OMP 18.19.0 RunTrace.present 的默认 Pseudo 视图核对。
    for (source, expected) in [
        (
            "ls -la && echo \"---\" && git log --oneline -5",
            vec![
                "ls -la",
                "  and print(\"---\")",
                "  and git log --oneline -5",
            ],
        ),
        (
            "grep -rn emit crates/pp-runtime/src/agent.rs | head -20; echo ===; ls crates/pp-ui/src/tui | head -40",
            vec![
                "grep -rn ‹emit› crates/pp-runtime/src/agent.rs ⇡ first 20",
                "print(\"===\")",
                "ls crates/pp-ui/src/tui ⇡ first 40",
            ],
        ),
        (
            "if test -f a; then cat a; else echo missing; fi",
            vec![
                "if is_file(\"a\"):",
                "  cat a",
                "else:",
                "  print(\"missing\")",
            ],
        ),
        ("echo \"a && b; c | d\"", vec!["print(\"a && b; c | d\")"]),
        (
            "for x in a b; do echo \"$x\"; done",
            vec!["for x in [\"a\", \"b\"]:", "  print(f\"{x}\")"],
        ),
    ] {
        let (trace, _, _) = parsed(source);
        assert_eq!(text(&trace), expected, "{source}");
    }
}

#[test]
fn shell_display_execution_identity_and_repeated_runs() {
    let (trace, program, unit) = parsed("echo same; echo same");
    let list = &program.complete_commands[0];
    let first = &list.0[0].0.first.seq[0];
    let second = &list.0[1].0.first.seq[0];
    let a = trace.begin(Some(unit), first).unwrap();
    let b = trace.begin(Some(unit), first).unwrap();
    assert_ne!(a, b);
    trace.end(a, Some(1));
    assert_eq!(trace.snapshot().unwrap().rows[0].state, ShellState::Running);
    trace.end(b, Some(0));
    let snapshot = trace.snapshot().unwrap();
    assert_eq!(snapshot.rows[0].state, ShellState::Ok);
    assert_eq!(snapshot.rows[1].state, ShellState::Pending);
    trace.skipped(Some(unit), second.location());
    assert_eq!(trace.snapshot().unwrap().rows[1].state, ShellState::Skipped);
    let c = trace.begin(Some(unit), second).unwrap();
    trace.end(c, None);
    trace.finish();
    assert_eq!(
        trace.snapshot().unwrap().rows[1].state,
        ShellState::Interrupted
    );
}

#[test]
fn shell_display_overflow_preserves_original_and_has_bounded_nodes() {
    let source = "echo x;".repeat(MAX_NODES + 10);
    let (trace, _, _) = parsed(&source);
    assert_eq!(text(&trace), vec![source]);
    trace.with(|model| assert!(model.nodes.len() <= MAX_NODES));
    // 用语句节点构造真实语法嵌套；算术括号属于单条表达式。
    for (open, close) in [("if true; then ", "fi; "), ("{ ", "}; ")] {
        let source = format!(
            "{}echo x;{}",
            open.repeat(usize::from(MAX_DEPTH) + 1),
            close.repeat(usize::from(MAX_DEPTH) + 1)
        );
        let (trace, _, _) = parsed(&source);
        assert_eq!(text(&trace), vec![source]);
    }
}

#[test]
fn shell_display_quotes_documents_and_dynamic_values_stay_literal() {
    let (trace, _, _) = parsed("cat <<'EOF'\na && b; c | d\nEOF\necho 'literal $HOME'\n");
    let rows = text(&trace);
    assert!(rows.iter().any(|row| row.contains("a && b; c | d")));
    assert!(rows.iter().any(|row| row.contains("literal $HOME")));
    let (trace, _, _) = parsed("x=hello; echo \"$x\"; echo \"$HOME\"");
    let rows = text(&trace);
    assert!(rows.iter().any(|row| row == "print(f\"{‹hello›}\")"));
    assert!(rows.iter().any(|row| row == "print(f\"{HOME}\")"));
}

fn execute(
    tools: &crate::tools::Tools,
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
        crate::tools::ToolPolicy::Execute {
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
        .unwrap_or_else(|| panic!("未找到 {needle}: {snapshot:?}"))
}

#[test]
fn shell_display_real_execution_short_circuit_loops_and_model_boundary() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let command = "printf ok; false && echo unreachable; for x in a b c; do printf '%s' \"$x\"; done; if test -f missing; then echo no; else echo yes; fi";
    let (model, result) = execute(
        &tools,
        "bash",
        json!({"command":command}),
        &Default::default(),
        &mut |_| {},
    );
    assert!(model.get("shell").is_none(), "展示不能进入模型上下文");
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
        "条件的 false 不冒充执行错误"
    );
}

#[test]
fn shell_display_real_pipeline_failure_and_dynamic_function_resolution() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let (_, result) = execute(
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
    let (_, result) = execute(
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

#[test]
fn shell_display_live_snapshot_cancel_and_external_script_have_no_fabricated_success() {
    use pp_core::{cancellation::CancellationToken, events::AgentEvent};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let token = CancellationToken::new();
    let mut observed = false;
    let (_, result) = execute(
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
    assert!(observed, "取消必须由实际运行中快照触发");
    assert_eq!(result["cancelled"], true);
    let snapshot = shell(&result);
    assert_eq!(find(&snapshot, "sleep 60").state, ShellState::Interrupted);
    assert_eq!(find(&snapshot, "after").state, ShellState::Pending);
    let (_, result) = execute(
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
        "外部脚本内部没有执行身份"
    );
    assert_eq!(find(&snapshot, "nested").state, ShellState::Pending);
}

#[test]
fn shell_display_background_snapshot_updates_and_freezes() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let token = pp_core::cancellation::CancellationToken::new();
    let (model, started) = execute(
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
        let (model, result) = execute(
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
        assert!(Instant::now() < deadline, "后台命令未启动：{result}");
        std::thread::yield_now();
    }
    let (_, result) = execute(
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

#[test]
fn shell_display_constant_folding_requires_unconditional_setter() {
    for source in [
        "false && x=wrong; echo \"$x\"",
        "x=1; echo $((x++)); echo \"$x\"",
        "x=1; builtin read x; echo \"$x\"",
    ] {
        let (trace, _, _) = parsed(source);
        assert!(
            text(&trace).iter().any(|row| row.contains("{x}")),
            "{source}: {:?}",
            text(&trace)
        );
    }
}

#[test]
fn shell_display_omp_functions_idioms_and_heredoc_language() {
    for (source, expected) in [
        (
            "foo(){ echo \"$1\"; }; foo hi",
            vec!["def foo(arg1):", "  print(f\"{arg1}\")", "foo(\"hi\")"],
        ),
        (
            "while read -r line; do echo \"$line\"; done",
            vec!["for line in lines:", "  print(f\"{line}\")"],
        ),
        (
            "timeout 5 ls; false || true",
            vec!["⏱ 5s ls", "false may fail"],
        ),
        (
            "curl -s -o /dev/null -w \"%{http_code}\" https://example.com",
            vec!["http_status(\"https://example.com\")"],
        ),
        (
            "cat <<'PY'\nprint(1)\nPY",
            vec!["cat ⟵ py ↓", "  py · heredoc «PY»", "  print(1)"],
        ),
    ] {
        let (trace, _, _) = parsed(source);
        assert_eq!(text(&trace), expected, "{source}");
    }
}

#[test]
fn shell_display_unselected_function_definition_does_not_change_builtin_display() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let (_, result) = execute(
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

#[test]
fn shell_display_timeout_and_condition_error_preserve_real_states() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let (_, result) = execute(
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
    let (_, result) = execute(
        &tools,
        "bash",
        json!({"command":"if sh -c 'exit 2'; then echo wrong; fi"}),
        &Default::default(),
        &mut |_| {},
    );
    let snapshot = shell(&result);
    assert_eq!(find(&snapshot, "if ").state, ShellState::Fail);
    assert!(find(&snapshot, "if ").meta.contains("exit 2"));
    let (_, result) = execute(
        &tools,
        "bash",
        json!({"command":"while false; do echo never; done"}),
        &Default::default(),
        &mut |_| {},
    );
    assert_eq!(find(&shell(&result), "while ").meta, "↻ 0");
}

#[test]
fn shell_display_escaped_words_and_regex_option_values_keep_argument_boundaries() {
    let (trace, _, _) = parsed("cat file\\ name; rg --max-count 20 emit src");
    assert_eq!(
        text(&trace),
        ["cat \"file name\"", "rg --max-count 20 ‹emit› src"]
    );
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let tools =
        crate::tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let (_, result) = execute(
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
