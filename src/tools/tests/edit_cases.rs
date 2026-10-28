use super::*;

#[test]
fn hashline_edit_rejects_invalid_edits_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let content = read_content(&tools, "demo.txt").0;
    assert_eq!(row_text(&content), ["one", "two", "three"]);
    for op in [
        "PUT 0.=1:\n+x",
        "PUT 02.=2:\n+x",
        "PUT 99999999999999999999.=3:\n+x",
        "PUT 9.=9:\n+x",
        "PUT 3.=2:\n+x",
        "PUT 1.=2:\n+x\nPUT 2.=3:\n+y",
        "PUT 1.=3:\n+x\nPUT <2:\n+y",
        "PUT 2.=2:\n+x\nPUT <2:\n+y",
        "PUT 2.=2:\n+x\nPUT >2:\n+y",
        "PUT <1:\n+x\nPUT <1:\n+y",
        "PUT >1:\n+x\nPUT <2:\n+y",
        "REM",
        "MV other",
        "PUT 1*:\n+x",
        "PUT 1.=1:",
        "PUT <1:",
        "CUT 1.=1 @name",
        "PUT <1:\n+x\ry",
        "",
        "PUT 1.=1:\n+x\n[other#A1B2]\nCUT 1.=1",
        "PUT 1.=1:\n+one",
    ] {
        let result = execute(&tools, "edit", hashline_args(&content, op));
        assert_eq!(result["ok"], false, "{op}: {result}");
        assert_eq!(fs::read(&path).unwrap(), b"one\ntwo\nthree\n");
    }
    for args in [
        json!({"path":"demo.txt","edits":[]}),
        json!({"input":"[demo.txt#A1B2]\nCUT 1.=1","extra":true}),
        json!({"input":null}),
    ] {
        assert_eq!(execute(&tools, "edit", args)["ok"], false);
    }
    for header in [
        "[demo.txt#abcd]",
        "[demo.txt]",
        "[demo.txt#ABC]",
        "[demo.txt#ABCDE]",
    ] {
        assert_eq!(
            execute(
                &tools,
                "edit",
                json!({"input":format!("{header}\nCUT 1.=1")})
            )["ok"],
            false
        );
    }
    assert!(tools.changes_view().files.is_empty());
}

#[test]
fn grep_hashline_edit_only_accepts_complete_displayed_lines() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(
        &path,
        format!("first\nneedle\n{}needle\nlast\n", "x".repeat(600)),
    )
    .unwrap();
    let result = execute(
        &tools,
        "grep",
        json!({"pattern":"needle","path":"demo.txt"}),
    );
    let content = result["content"].as_str().unwrap();
    assert!(content.contains("[demo.txt#"), "{content}");
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(content, "PUT 3.=3:\n+invalid")
        )["ok"],
        false
    );
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(content, "PUT 2.=2:\n+changed")
        )["ok"],
        true
    );
    assert!(fs::read_to_string(path).unwrap().contains("changed"));
}

#[test]
fn grep_hashline_output_budget_does_not_authorize_omitted_lines() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let path = dir.path().join("budget.txt");
    fs::write(&path, format!("needle {}\n", "x".repeat(450)).repeat(200)).unwrap();
    let result = execute(
        &tools,
        "grep",
        json!({"pattern":"needle","path":"budget.txt"}),
    );
    let content = result["content"].as_str().unwrap();
    assert!(content.contains("Output truncated"), "{content}");
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(content, "PUT 199.=199:\n+invalid")
        )["ok"],
        false
    );
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(content, "PUT 1.=1:\n+changed")
        )["ok"],
        true
    );
}

#[test]
fn hashline_edit_rejects_stale_snapshot_and_unread_lines() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let content = read_content(&tools, "demo.txt:2-2").0;
    for op in ["PUT 1.=1:\n+x", "CUT 1.=3", "PUT >3:\n+x"] {
        let result = execute(&tools, "edit", hashline_args(&content, op));
        assert_eq!(result["ok"], false, "{result}");
        assert!(result["error"].as_str().unwrap().contains("unread"));
    }
    // 修改未显示行、仅行尾空格变化也必须让旧快照失效。
    fs::write(&path, "external  \ntwo\nthree\n").unwrap();
    let result = execute(&tools, "edit", hashline_args(&content, "PUT 2.=2:\n+TWO"));
    assert_eq!(result["ok"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("snapshot changed")
    );
    let fresh = read_content(&tools, "demo.txt").0;
    let args = hashline_args(&fresh, "PUT 2.=2:\n+TWO");
    assert_eq!(execute(&tools, "edit", args.clone())["ok"], true);
    assert_eq!(execute(&tools, "edit", args)["ok"], false);
    assert_eq!(fs::read(&path).unwrap(), b"external  \nTWO\nthree\n");
}

#[test]
fn hashline_read_cap_boundaries_and_editability() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let path = dir.path().join("cap.txt");
    fs::write(&path, "x".repeat(OUTPUT_LIMIT)).unwrap();
    let (content, truncated) = read_content(&tools, "cap.txt");
    assert!(!truncated);
    assert_eq!(row_text(&content)[0].len(), OUTPUT_LIMIT);
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(&content, "PUT 1.=1:\n+edited")
        ),
        json!({"ok":true,"edits_applied":1,"bytes_written":6})
    );
    assert_eq!(fs::read(&path).unwrap(), b"edited");
    fs::write(&path, "y".repeat(OUTPUT_LIMIT + 1)).unwrap();
    assert_eq!(
        execute(&tools, "read", json!({"path":"cap.txt"}))["ok"],
        false
    );
    for source in [
        format!("ok\n{}", "z".repeat(OUTPUT_LIMIT)),
        format!("ok\r\n{}\r\ntail\r\n", "z".repeat(OUTPUT_LIMIT - 5)),
        format!("ok\n{}\ntail\n", "中".repeat(21845)),
    ] {
        fs::write(&path, source).unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(truncated);
        assert_eq!(row_text(&content), ["ok"]);
    }
}

#[test]
fn hashline_read_noneditable_sources_and_cache_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, "one\ntwo\n").unwrap();
    let content = read_content(&tools, "demo.txt").0;
    let args = hashline_args(&content, "CUT 1.=1");
    assert_eq!(
        execute(
            &tools,
            "write",
            json!({"path":"demo.txt","content":"one\ntwo\n"})
        )["ok"],
        true
    );
    assert_eq!(execute(&tools, "edit", args.clone())["ok"], false);
    read_content(&tools, "demo.txt");
    tools.begin_changes_task();
    assert_eq!(execute(&tools, "edit", args.clone())["ok"], true);
    let view = tools.changes_view();
    read_content(&tools, "demo.txt");
    tools
        .undo_file(view.files[0].undo.as_ref().unwrap())
        .unwrap();
    assert_eq!(execute(&tools, "edit", args.clone())["ok"], false);
    read_content(&tools, "demo.txt");
    tools.clear_changes();
    assert_eq!(execute(&tools, "edit", args)["ok"], false);
    for (name, bytes) in [("empty", b"".as_slice()), ("binary", b"a\xff\n".as_slice())] {
        fs::write(dir.path().join(name), bytes).unwrap();
        assert_eq!(
            execute(
                &tools,
                "edit",
                json!({"input":format!("[{name}#A1B2]\nCUT 1.=1")})
            )["ok"],
            false
        );
    }
    fs::write(
        dir.path().join("large"),
        "x\n".repeat(hashline::MAX_FILE_BYTES / 2 + 1),
    )
    .unwrap();
    let large = execute(&tools, "read", json!({"path":"large"}));
    assert_eq!(large["ok"], true);
    assert!(large["warning"].as_str().unwrap().contains("4 MiB"));
    assert!(!large["content"].as_str().unwrap().starts_with('['));
    assert!(!read_content(&tools, "demo.txt:raw").0.starts_with('['));
}

#[test]
fn hashline_read_pages_share_header_and_preserve_bom_byte_limits() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "\u{feff}a\r\nb\rc\n\u{feff}keep\n").unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    tools.limits.read_max_bytes = 5;
    assert_eq!(execute(&tools, "read", json!({"path":"file"}))["ok"], false);
    tools.limits.read_max_bytes = 6;
    tools.limits.read_max_lines = 1;
    let first = execute(&tools, "read", json!({"path":"file"}));
    assert_eq!(first["ok"], true, "{first}");
    let content = first["content"].as_str().unwrap();
    let header = content.lines().next().unwrap();
    assert_eq!(row_text(content), ["a"]);
    let second = execute(&tools, "read", json!({"path":first["next_path"]}));
    let content = second["content"].as_str().unwrap();
    assert_eq!(content.lines().next().unwrap(), header);
    assert_eq!(row_text(content), ["b"]);
    tools.limits.read_max_bytes = 100;
    let last = execute(&tools, "read", json!({"path":"./file:4-4"}));
    assert_eq!(
        last["content"].as_str().unwrap().lines().next().unwrap(),
        header
    );
    assert_eq!(
        row_text(last["content"].as_str().unwrap()),
        ["\u{feff}keep"]
    );
    // 多段读取补齐已读集合后，可以针对同一原始版本编辑完整范围。
    let all = execute(&tools, "read", json!({"path":"file:1-2,3-4"}));
    assert_eq!(
        all["content"].as_str().unwrap().lines().next().unwrap(),
        header
    );
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(all["content"].as_str().unwrap(), "CUT 1.=4")
        )["ok"],
        false
    );
    tools.limits.read_max_lines = 4;
    let all = execute(&tools, "read", json!({"path":"file:1-2,3-4"}));
    assert_eq!(
        execute(
            &tools,
            "edit",
            hashline_args(all["content"].as_str().unwrap(), "CUT 1.=4")
        )["ok"],
        true
    );
    assert_eq!(
        fs::read(dir.path().join("file")).unwrap(),
        "\u{feff}".as_bytes()
    );
}

#[test]
fn replace_read_returns_plain_text_with_original_newlines() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    fs::write(dir.path().join("plain.txt"), "one\ntwo\nthree\n").unwrap();
    assert_eq!(
        read_content(&tools, "plain.txt"),
        ("one\ntwo\nthree\n".to_owned(), false)
    );

    fs::write(dir.path().join("crlf.txt"), b"a\r\nb\r\n").unwrap();
    assert_eq!(
        read_content(&tools, "crlf.txt"),
        ("a\r\nb\r\n".to_owned(), false)
    );

    fs::write(dir.path().join("empty.txt"), b"").unwrap();
    assert_eq!(read_content(&tools, "empty.txt"), (String::new(), false));

    fs::write(dir.path().join("lossy.txt"), b"ok\xff\xf0\x9f\n").unwrap();
    assert_eq!(
        read_content(&tools, "lossy.txt"),
        ("ok\u{fffd}\u{fffd}\n".to_owned(), false)
    );

    fs::write(dir.path().join("cap.txt"), "x".repeat(OUTPUT_LIMIT)).unwrap();
    let (content, truncated) = read_content(&tools, "cap.txt");
    assert!(!truncated);
    assert_eq!(content, "x".repeat(OUTPUT_LIMIT));

    fs::write(dir.path().join("cap.txt"), "y".repeat(OUTPUT_LIMIT + 1)).unwrap();
    let result = execute(&tools, "read", json!({"path":"cap.txt"}));
    assert_eq!(result["ok"], false);

    // 字节上限落在多字节字符内时，也只保留前面的完整行。
    let bytes = format!("ok\n{}", "中".repeat(21_846));
    fs::write(dir.path().join("cap.txt"), bytes.as_bytes()).unwrap();
    let (content, truncated) = read_content(&tools, "cap.txt");
    assert!(truncated);
    assert_eq!(content, "ok\n");
}

#[test]
fn replace_edit_roundtrip_counts_bytes_and_allows_mixed_noop() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, "keep “Ｋ”  \nsay “Hi”—x  \ntail\n").unwrap();
    let result = execute(
        &tools,
        "edit",
        json!({"path":"demo.txt","edits":[
            {"oldText":"tail","newText":"TAIL"},
            {"oldText":"say \"Hi\"-x","newText":"said"}
        ]}),
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["edits_applied"], 2);
    let expected = "keep “Ｋ”  \nsaid  \nTAIL\n";
    assert_eq!(result["bytes_written"], expected.len());
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);

    // A no-op item alongside a real change still succeeds for the batch.
    let result = execute(
        &tools,
        "edit",
        json!({"path":"demo.txt","edits":[
            {"oldText":"said","newText":"said"},
            {"oldText":"TAIL","newText":"recovered"}
        ]}),
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["edits_applied"], 2);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "keep “Ｋ”  \nsaid  \nrecovered\n"
    );
}

#[test]
fn replace_edit_rejects_whole_batch_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let original = fs::read(&path).unwrap();

    let missing = execute(
        &tools,
        "edit",
        json!({"path":"absent.txt","edits":[{"oldText":"a","newText":"b"}]}),
    );
    assert_eq!(missing["ok"], false, "{missing}");
    assert!(!dir.path().join("absent.txt").exists());

    let cases = [
        ("empty edits", json!({"path":"demo.txt","edits":[]})),
        (
            "empty oldText",
            json!({"path":"demo.txt","edits":[{"oldText":"","newText":"b"}]}),
        ),
        (
            "missing oldText",
            json!({"path":"demo.txt","edits":[{"oldText":"nope","newText":"b"}]}),
        ),
        (
            "non-unique oldText",
            json!({"path":"demo.txt","edits":[{"oldText":"o","newText":"b"}]}),
        ),
        (
            "overlapping edits",
            json!({"path":"demo.txt","edits":[
                {"oldText":"one","newText":"x"},
                {"oldText":"ne\nt","newText":"y"}
            ]}),
        ),
        (
            "one valid one missing",
            json!({"path":"demo.txt","edits":[
                {"oldText":"one","newText":"ONE"},
                {"oldText":"nope","newText":"b"}
            ]}),
        ),
        (
            "whole batch is a no-op",
            json!({"path":"demo.txt","edits":[{"oldText":"one","newText":"one"}]}),
        ),
        (
            "unknown item field",
            json!({"path":"demo.txt","edits":[{"oldText":"a","newText":"b","why":"nope"}]}),
        ),
        (
            "missing newText",
            json!({"path":"demo.txt","edits":[{"oldText":"a"}]}),
        ),
        (
            "hashline operation shape",
            json!({"path":"demo.txt","edits":[{"op":"replace","start":"1:0","end":"1:0","lines":["x"]}]}),
        ),
        (
            "classic top-level oldText",
            json!({"path":"demo.txt","oldText":"one","newText":"two"}),
        ),
        (
            "string instead of edits array",
            json!({"path":"demo.txt","edits":"one"}),
        ),
        (
            "unknown top-level field",
            json!({"path":"demo.txt","edits":[{"oldText":"one","newText":"two"}],"reason":"nope"}),
        ),
    ];
    for (name, args) in cases {
        let result = execute(&tools, "edit", args);
        assert_eq!(result["ok"], false, "{name}: {result}");
        assert_eq!(fs::read(&path).unwrap(), original, "{name}");
    }

    fs::write(dir.path().join("empty.txt"), "").unwrap();
    let empty = execute(
        &tools,
        "edit",
        json!({"path":"empty.txt","edits":[{"oldText":"a","newText":"b"}]}),
    );
    assert_eq!(empty["ok"], false, "{empty}");
    assert_eq!(fs::read(dir.path().join("empty.txt")).unwrap(), b"");

    fs::create_dir(dir.path().join("folder")).unwrap();
    let folder = execute(
        &tools,
        "edit",
        json!({"path":"folder","edits":[{"oldText":"a","newText":"b"}]}),
    );
    assert_eq!(folder["ok"], false, "{folder}");

    fs::write(dir.path().join("bytes.txt"), b"ok\xff\n").unwrap();
    let bytes = execute(
        &tools,
        "edit",
        json!({"path":"bytes.txt","edits":[{"oldText":"ok","newText":"b"}]}),
    );
    assert_eq!(bytes["ok"], false, "{bytes}");
    assert_eq!(fs::read(dir.path().join("bytes.txt")).unwrap(), b"ok\xff\n");

    // Hashline-mode tools reject replace-shaped arguments instead of
    // trying the other mode's deserialization.
    let hashline = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let wrong_mode = execute(
        &hashline,
        "edit",
        json!({"path":"demo.txt","edits":[{"oldText":"one","newText":"two"}]}),
    );
    assert_eq!(wrong_mode["ok"], false, "{wrong_mode}");
    assert_eq!(fs::read(&path).unwrap(), original);
}
