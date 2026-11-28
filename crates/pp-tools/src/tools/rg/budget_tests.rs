use super::*;

fn query(dir: &Path, args: Value, store: &mut hashline::Store) -> Value {
    let cwd = dir.canonicalize().unwrap();
    run(
        &cwd,
        RgSettings::default(),
        EditMode::Hashline,
        &save::Workspace::open(&cwd).unwrap(),
        store,
        &args.to_string(),
        &CancellationToken::new(),
        None,
    )
    .unwrap()
}

#[test]
fn rg_output_token_budget_caps_all_modes_without_changing_count_totals() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..25 {
        fs::write(
            dir.path().join(format!("{index:02}_{}", "name".repeat(10))),
            "needle 中文\t\"\\尾\n".repeat(3),
        )
        .unwrap();
    }
    for mode in ["content", "files_with_matches", "count"] {
        let mut store = hashline::Store::default();
        let mut args = json!({"pattern":"needle","output_mode":mode,"context":0});
        let baseline = query(dir.path(), args.clone(), &mut store);
        args["max_output_tokens"] = json!(128);
        let limited = query(dir.path(), args.clone(), &mut store);
        let text = limited["content"].as_str().unwrap();
        assert!(serde_json::to_vec(text).unwrap().len() <= 384, "{limited}");
        assert!(text.len() < baseline["content"].as_str().unwrap().len());
        assert!(text.contains("token limit"), "{limited}");
        assert_eq!(limited["details"]["truncated"], true, "{limited}");
        assert_eq!(limited["details"]["tokenLimitReached"], true);
        assert!(limited["details"]["fileCount"].as_u64().unwrap() < 20);
        if mode == "count" {
            assert_eq!(limited["details"]["totalMatchCount"], 75);
            assert_eq!(limited["details"]["countComplete"], true);
        }
        // A huge budget keeps the existing paging, notice and output cap, with no multiplication overflow.
        args["max_output_tokens"] = json!(u64::MAX);
        let mut unlimited = query(dir.path(), args.clone(), &mut store);
        assert_eq!(unlimited["details"]["tokenLimitReached"], false);
        unlimited["details"]
            .as_object_mut()
            .unwrap()
            .remove("tokenLimitReached");
        assert_eq!(unlimited, baseline);
        args["max_output_tokens"] = Value::Null;
        assert_eq!(query(dir.path(), args, &mut store), baseline);
    }
}

#[test]
fn rg_output_token_budget_only_grants_returned_hashline_rows() {
    let dir = tempfile::tempdir().unwrap();
    let source = format!(
        "needle first 中文\nneedle {}\nneedle last\n",
        "x".repeat(200)
    );
    fs::write(dir.path().join("file"), &source).unwrap();
    let mut store = hashline::Store::default();
    let limited = query(
        dir.path(),
        json!({"pattern":"needle","path":"file","context":0,"max_output_tokens":64}),
        &mut store,
    );
    let text = limited["content"].as_str().unwrap();
    assert!(text.contains("*1:needle first 中文\n"), "{text}");
    assert!(
        !text.contains("*2:") && !text.contains("needle last"),
        "{text}"
    );
    assert!(serde_json::to_vec(text).unwrap().len() <= 192);
    assert_eq!(limited["details"]["matchCount"], 1);
    assert_eq!(limited["details"]["tokenLimitReached"], true);
    let tag = hashline::file_hash(&source);
    for (line, allowed) in [(1, true), (2, false), (3, false)] {
        let result = store.validate(
            Path::new("file"),
            &tag,
            &source,
            &[hashline::Edit::Replace {
                start: line,
                end: line,
                lines: vec!["changed".into()],
            }],
        );
        assert_eq!(result.is_ok(), allowed, "line {line}: {result:?}");
    }
}

#[test]
fn rg_output_token_budget_handles_tiny_budgets_and_no_matches() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "needle\n").unwrap();
    for (pattern, limit, truncated) in [
        ("needle", 1, true),
        ("needle", 8, true),
        ("missing", 1, true),
        ("missing", 64, false),
    ] {
        let mut store = hashline::Store::default();
        let result = query(
            dir.path(),
            json!({"pattern":pattern,"path":"file","max_output_tokens":limit}),
            &mut store,
        );
        let text = result["content"].as_str().unwrap();
        assert!(serde_json::to_vec(text).unwrap().len() <= limit * 3);
        assert_eq!(
            result["details"]["tokenLimitReached"], truncated,
            "{result}"
        );
        assert_eq!(result["details"]["truncated"], truncated, "{result}");
        if pattern == "needle" {
            assert!(!text.contains("No matches"), "{result}");
            assert!(
                store
                    .validate(
                        Path::new("file"),
                        &hashline::file_hash("needle\n"),
                        "needle\n",
                        &[hashline::Edit::Before {
                            anchor: 1,
                            lines: vec!["changed".into()],
                        }],
                    )
                    .is_err()
            );
        } else if !truncated {
            assert_eq!(text, "No matches found");
        }
    }
}

#[test]
fn rg_output_token_budget_keeps_byte_limit_with_large_budget() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("file"),
        format!("needle {}\n", "x".repeat(450)).repeat(200),
    )
    .unwrap();
    let mut store = hashline::Store::default();
    let result = query(
        dir.path(),
        json!({"pattern":"needle","path":"file","context":0,"max_output_tokens":u64::MAX}),
        &mut store,
    );
    let text = result["content"].as_str().unwrap();
    assert!(text.len() <= 50 * 1024);
    assert!(text.contains("byte limit"), "{text}");
    assert_eq!(result["details"]["tokenLimitReached"], false);
    assert_eq!(result["details"]["truncated"], true);
}
