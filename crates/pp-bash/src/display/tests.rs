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
fn shell_display_deep_extended_test_falls_back_verbatim() {
    // 浅层扩展测试仍输出原语义（含 And 与括号包裹）。
    let (trace, _, _) = parsed("[[ -n x && -n y ]]");
    assert_eq!(text(&trace), vec!["\"x\" != \"\" and \"y\" != \"\""]);
    let (trace, _, _) = parsed("[[ ( -n x ) ]]");
    assert_eq!(text(&trace), vec!["(\"x\" != \"\")"]);
    // 嵌套 AST 深度超过 MAX_DEPTH 的扩展测试逐字回退 source，节点有界。
    let source = format!(
        "[[ {}-n x ]]",
        "-n x && ".repeat(usize::from(MAX_DEPTH) + 8)
    );
    let (trace, _, _) = parsed(&source);
    assert_eq!(text(&trace), vec![source]);
    trace.with(|model| assert!(model.nodes.len() <= MAX_NODES));
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
fn shell_display_escaped_words_and_regex_option_values_keep_argument_boundaries() {
    let (trace, _, _) = parsed("cat file\\ name; rg --max-count 20 emit src");
    assert_eq!(
        text(&trace),
        ["cat \"file name\"", "rg --max-count 20 ‹emit› src"]
    );
}
