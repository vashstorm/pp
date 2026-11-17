use super::*;

fn row_of(text: &str, needle: &str) -> u16 {
    text.lines()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("missing {needle:?} in:\n{text}")) as u16
}

fn col_of(text: &str, needle: &str) -> (u16, u16) {
    for (row, line) in text.lines().enumerate() {
        if let Some(col) = line.find(needle) {
            return (Line::raw(&line[..col]).width() as u16, row as u16);
        }
    }
    panic!("missing {needle:?} in:\n{text}");
}

#[test]
fn eval_card_distinguishes_python_javascript_and_fallback_from_bash() {
    struct Case<'a> {
        arg_lang: Option<&'a str>,
        code: &'a str,
        keyword: &'a str,
        expected_label: &'a str,
        nerd_icon: &'a str,
        ascii_icon: &'a str,
        expected_icon_color: fn() -> Color,
        expected_color: fn() -> Color,
        highlighted: bool,
    }

    let cases = [
        Case {
            arg_lang: Some("py"),
            code: "def add(a, b):\n    return a + b",
            keyword: "def",
            expected_label: "Python",
            nerd_icon: "\u{f0320}",
            ascii_icon: "P",
            expected_icon_color: palette::eval_py_icon,
            expected_color: palette::eval_py_title,
            highlighted: true,
        },
        Case {
            arg_lang: Some("python"),
            code: "return True",
            keyword: "return",
            expected_label: "Python",
            nerd_icon: "\u{f0320}",
            ascii_icon: "P",
            expected_icon_color: palette::eval_py_icon,
            expected_color: palette::eval_py_title,
            highlighted: true,
        },
        Case {
            arg_lang: Some("js"),
            code: "const add = (a, b) => a + b;",
            keyword: "const",
            expected_label: "JavaScript",
            nerd_icon: "\u{f031e}",
            ascii_icon: "J",
            expected_icon_color: palette::eval_js_icon,
            expected_color: palette::eval_js_title,
            highlighted: true,
        },
        Case {
            arg_lang: Some("javascript"),
            code: "function greet() { return 1; }",
            keyword: "function",
            expected_label: "JavaScript",
            nerd_icon: "\u{f031e}",
            ascii_icon: "J",
            expected_icon_color: palette::eval_js_icon,
            expected_color: palette::eval_js_title,
            highlighted: true,
        },
        Case {
            arg_lang: Some("lua"),
            code: "local x = 1",
            keyword: "local",
            expected_label: "lua",
            nerd_icon: "\u{f0169}",
            ascii_icon: "E",
            expected_icon_color: palette::eval_title,
            expected_color: palette::eval_title,
            highlighted: false,
        },
        Case {
            arg_lang: None,
            code: "1 + 1",
            keyword: "1 + 1",
            expected_label: "Eval",
            nerd_icon: "\u{f0169}",
            ascii_icon: "E",
            expected_icon_color: palette::eval_title,
            expected_color: palette::eval_title,
            highlighted: false,
        },
    ];

    assert_ne!(palette::eval_py_title(), palette::eval_js_title());
    assert_ne!(palette::eval_py_icon(), palette::eval_py_title());
    assert_ne!(eval_border(), bash_border());

    for case in cases {
        let mut args = json!({"code": case.code, "title": "lang check"});
        if let Some(lang) = case.arg_lang {
            args["language"] = json!(lang);
        }
        let mut transcript = Transcript::new();
        tool(&mut transcript, "eval-1", "eval", args);
        transcript.progress(AgentEvent::ToolFinished {
            id: "eval-1".into(),
            result: json!({"ok": true, "output": "ok\n", "execution_count": 1}),
        });

        assert_eq!(transcript.entries[0].kind, EntryKind::Eval);

        // Nerd Font 模式下验证顶栏标签、语言图标/颜色与语法高亮。
        let prev = crate::tui::themes::current();
        crate::tui::themes::set_active(prev.0, pp_config::config::IconMode::Nerd);
        transcript.invalidate_theme();
        let buffer = render(&mut transcript, 90, 14);
        let text = screen(&buffer);
        let expected_header = format!("╭─ {} {} · done", case.nerd_icon, case.expected_label);
        assert!(text.contains(&expected_header), "{text}");
        if case.expected_label != "Eval" {
            assert!(!text.contains("Eval · "), "{text}");
        }
        assert!(!text.contains("Bash"), "{text}");
        assert_eq!(buffer[(0, 0)].fg, eval_border());
        assert_eq!(buffer[(2, 1)].bg, eval_background());
        let (out_col, out_row) = col_of(&text, "ok");
        assert_eq!(buffer[(out_col, out_row)].fg, palette::muted());

        let (icon_col, top_row) = col_of(&text, case.nerd_icon);
        assert_eq!(buffer[(icon_col, top_row)].fg, (case.expected_icon_color)());
        let (label_col, label_row) = col_of(&text, case.expected_label);
        assert_eq!(buffer[(label_col, label_row)].fg, (case.expected_color)());
        let (kw_col, kw_row) = col_of(&text, case.keyword);
        if case.highlighted {
            assert_eq!(buffer[(kw_col, kw_row)].fg, palette::syntax_keyword());
        } else {
            assert_eq!(buffer[(kw_col, kw_row)].bg, eval_background());
        }

        // ASCII 模式下切换为单字符语言标记 P / J / E。
        crate::tui::themes::set_active(prev.0, pp_config::config::IconMode::Ascii);
        transcript.invalidate_theme();
        let ascii_text = screen(&render(&mut transcript, 90, 14));
        let expected_ascii = format!("╭─ {} {} · done", case.ascii_icon, case.expected_label);
        assert!(ascii_text.contains(&expected_ascii), "{ascii_text}");
        if case.expected_label != "Eval" {
            assert!(!ascii_text.contains("Eval · "), "{ascii_text}");
        }
        crate::tui::themes::set_active(prev.0, prev.1);
    }
}

#[test]
fn eval_card_lifecycle_hides_empty_output_and_renders_repl_footer_and_errors() {
    // 1) 运行中与静默完成：运行期展示 Output 区，无输出完成时省略空 Output 区并展示 Cell 序号与 reset。
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "e1",
        "eval",
        json!({"language": "py", "code": "x = 1", "reset": true, "timeout_seconds": 25}),
    );
    let running_text = screen(&render(&mut transcript, 90, 12));
    assert!(running_text.contains("Python · running"), "{running_text}");
    assert!(!running_text.contains("Eval · "), "{running_text}");
    assert!(running_text.contains("├─ Output ─"), "{running_text}");
    assert!(running_text.contains("Elapsed:"), "{running_text}");
    assert!(running_text.contains("Timeout: 25s"), "{running_text}");

    transcript.progress(AgentEvent::ToolFinished {
        id: "e1".into(),
        result: json!({"ok": true, "language": "py", "output": "", "execution_count": 3}),
    });
    let silent_text = screen(&render(&mut transcript, 90, 12));
    assert!(!silent_text.contains("├─ Output ─"), "{silent_text}");
    assert!(silent_text.contains("Cell #3"), "{silent_text}");
    assert!(silent_text.contains("Timeout: 25s"), "{silent_text}");
    assert!(silent_text.contains("reset"), "{silent_text}");
    assert!(silent_text.contains("Wall:"), "{silent_text}");

    // 2) 失败、traceback 去重、Kernel restarted 警告与 Artifact 页脚。
    let mut failed = Transcript::new();
    tool(
        &mut failed,
        "e2",
        "eval",
        json!({"language": "py", "code": "1 / 0"}),
    );
    failed.progress(AgentEvent::ToolFinished {
        id: "e2".into(),
        result: json!({
            "ok": false,
            "language": "py",
            "execution_count": 4,
            "timed_out": true,
            "kernel_restarted": true,
            "output": "before-err\nZeroDivisionError: division by zero\n",
            "error": "ZeroDivisionError: division by zero",
            "artifact": {"uri": "artifact://art-99", "complete": true, "elided_bytes": 0}
        }),
    });
    let fail_buf = render(&mut failed, 100, 16);
    let fail_text = screen(&fail_buf);
    assert!(fail_text.contains("· failed"), "{fail_text}");
    assert_eq!(fail_buf[(0, 0)].fg, palette::removed());
    assert_eq!(
        fail_text
            .matches("ZeroDivisionError: division by zero")
            .count(),
        1,
        "{fail_text}"
    );
    assert!(fail_text.contains("Artifact: art-99"), "{fail_text}");
    assert!(fail_text.contains("timed out"), "{fail_text}");
    assert!(fail_text.contains("Kernel restarted"), "{fail_text}");
    let (warn_col, warn_row) = col_of(&fail_text, "Kernel restarted");
    assert_eq!(fail_buf[(warn_col, warn_row)].fg, palette::accent());

    // 3) output 未包含 error 摘要时，error 以红色 stderr 呈现。
    let mut err_only = Transcript::new();
    tool(
        &mut err_only,
        "e3",
        "eval",
        json!({"language": "js", "code": "while (true) {}"}),
    );
    err_only.progress(AgentEvent::ToolFinished {
        id: "e3".into(),
        result: json!({
            "ok": false,
            "language": "js",
            "output": "",
            "error": "JavaScript cell timed out"
        }),
    });
    let err_buf = render(&mut err_only, 90, 12);
    let err_text = screen(&err_buf);
    let err_row = row_of(&err_text, "JavaScript cell timed out");
    assert_eq!(err_buf[(2, err_row)].fg, palette::removed());
}

#[test]
fn eval_card_folds_long_code_and_output_expands_with_toggle_and_replays_identically() {
    let long_code = (0..25)
        .map(|i| format!("val_{i:02} = {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let long_output = (0..18).map(|i| format!("out-{i:02}\n")).collect::<String>();

    let events = vec![
        AgentEvent::ToolStarted {
            id: "e-fold".into(),
            name: "eval".into(),
            arguments: json!({
                "language": "py",
                "title": "long cell",
                "code": long_code,
            })
            .to_string(),
        },
        AgentEvent::ToolFinished {
            id: "e-fold".into(),
            result: json!({
                "ok": true,
                "language": "py",
                "execution_count": 2,
                "output": long_output,
            }),
        },
    ];

    let mut live = Transcript::new();
    for event in events.clone() {
        live.progress(event);
    }
    let folded = screen(&render(&mut live, 90, 30));
    assert!(folded.contains("▸ long cell"), "{folded}");
    assert!(
        folded.contains("earlier lines (Ctrl + O to expand)"),
        "{folded}"
    );
    assert!(folded.contains("val_24 = 24"), "{folded}");
    assert!(!folded.contains("val_00 = 0"), "{folded}");
    assert!(folded.contains("out-17"), "{folded}");
    assert!(!folded.contains("out-00"), "{folded}");

    live.toggle_tool_output();
    let expanded = screen(&render(&mut live, 90, 60));
    assert!(expanded.contains("val_00 = 0"), "{expanded}");
    assert!(expanded.contains("out-00"), "{expanded}");
    assert!(!expanded.contains("earlier lines"), "{expanded}");

    let mut replayed = Transcript::new();
    assert!(
        replayed
            .replay(events.into_iter().map(ReplayRecord::Event).collect())
            .is_none()
    );
    let replayed_folded = screen(&render(&mut replayed, 90, 30));
    assert!(
        replayed_folded.contains("Python · done"),
        "{replayed_folded}"
    );
    assert!(!replayed_folded.contains("Eval · "), "{replayed_folded}");
    assert!(replayed_folded.contains("Cell #2"), "{replayed_folded}");
    assert!(replayed_folded.contains("out-17"), "{replayed_folded}");
    assert!(!replayed_folded.contains("out-00"), "{replayed_folded}");
}
