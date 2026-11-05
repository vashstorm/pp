use super::*;

fn query(dir: &Path, args: Value, store: &mut hashline::Store) -> Result<Value> {
    let cwd = dir.canonicalize()?;
    run(
        &cwd,
        GrepSettings::default(),
        EditMode::Hashline,
        &save::Workspace::open(&cwd)?,
        store,
        &args.to_string(),
        &CancellationToken::new(),
        None,
    )
}

#[test]
fn grep_context_aliases_overrides_and_zero() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("file"),
        "one\ntwo\nthree\nneedle\nfive\nsix\nseven\neight\n",
    )
    .unwrap();
    let mut store = hashline::Store::default();
    let search = |extra: Value, store: &mut hashline::Store| {
        let mut args = json!({"pattern":"needle", "path":"file"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        query(dir.path(), args, store).unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let default = search(json!({}), &mut store);
    assert!(
        default.contains(" 3:three") && default.contains(" 7:seven"),
        "{default}"
    );
    assert!(!default.contains(" 2:two") && !default.contains(" 8:eight"));
    let context = search(json!({"context":2}), &mut store);
    assert_eq!(context, search(json!({"-C":2}), &mut store));
    assert_eq!(context, search(json!({"context":2,"-C":2}), &mut store));
    assert!(context.contains(" 2:two") && context.contains(" 6:six"));
    assert!(!context.contains(" 1:one") && !context.contains(" 7:seven"));
    let sides = search(json!({"context":2,"-B":0,"-A":1}), &mut store);
    assert!(sides.contains("*4:needle") && sides.contains(" 5:five"));
    assert!(!sides.contains("three") && !sides.contains("six"));
    let zero = search(json!({"-C":0}), &mut store);
    assert_eq!(zero.lines().count(), 2, "{zero}");
    let huge = search(json!({"context":usize::MAX}), &mut store);
    assert!(huge.contains(" 1:one") && huge.contains(" 8:eight"));
}

#[test]
fn grep_ignore_case_alias_works_with_both_regex_engines_and_modes() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "NEEDLE\nneedle\nNeedle\n").unwrap();
    for mode in ["content", "files_with_matches", "count"] {
        for pattern in ["needle", "need(?=le)"] {
            let mut store = hashline::Store::default();
            let mut args =
                json!({"pattern":pattern,"path":"file","output_mode":mode,"-i":true,"context":0});
            let insensitive = query(dir.path(), args.clone(), &mut store).unwrap();
            args.as_object_mut().unwrap().remove("-i");
            args["case"] = json!(false);
            assert_eq!(
                insensitive,
                query(dir.path(), args.clone(), &mut store).unwrap()
            );
            args["-i"] = json!(true);
            assert_eq!(
                insensitive,
                query(dir.path(), args.clone(), &mut store).unwrap()
            );
            args["-i"] = json!(false);
            args["case"] = json!(true);
            let sensitive = query(dir.path(), args, &mut store).unwrap();
            if mode == "files_with_matches" {
                assert_eq!(insensitive["content"], "file\n");
                assert!(insensitive["details"]["matchCount"].is_null());
            } else {
                assert_eq!(insensitive["details"]["matchCount"], 3);
                assert_eq!(sensitive["details"]["matchCount"], 1);
            }
        }
    }
}

#[test]
fn grep_compat_rejects_conflicts_invalid_types_and_unknown_modes() {
    let dir = tempfile::tempdir().unwrap();
    for extra in [
        json!({"context":1,"-C":2}),
        json!({"case":true,"-i":true}),
        json!({"case":false,"-i":false}),
        json!({"output_mode":"unknown"}),
        json!({"output_mode":null}),
        json!({"-i":"true"}),
        json!({"unexpected":true}),
    ]
    .into_iter()
    .chain(
        ["context", "-C", "-A", "-B"]
            .into_iter()
            .flat_map(|key| [-1.0, 1.5].into_iter().map(move |value| json!({key:value}))),
    ) {
        let mut args = json!({"pattern":"needle"});
        args.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(
            query(dir.path(), args.clone(), &mut hashline::Store::default()).is_err(),
            "{args}"
        );
    }
}

#[test]
fn grep_count_ignores_content_caps_and_totals_all_pages() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..22 {
        fs::write(
            dir.path().join(format!("{i:02}.txt")),
            "needle needle\n".repeat(300),
        )
        .unwrap();
    }
    let mut store = hashline::Store::default();
    let mut args = json!({"pattern":"needle","output_mode":"count","-C":usize::MAX});
    let first = query(dir.path(), args.clone(), &mut store).unwrap();
    assert_eq!(first["details"]["fileCount"], 20);
    assert_eq!(first["details"]["matchCount"], 6000);
    assert_eq!(first["details"]["totalMatchCount"], 6600);
    assert_eq!(first["details"]["countComplete"], true);
    assert_eq!(first["details"]["perFileLimitReached"], false);
    assert!(first["content"].as_str().unwrap().contains("00.txt:300\n"));
    assert!(first["content"].as_str().unwrap().contains("Total: 6600"));
    args["skip"] = json!(20);
    let second = query(dir.path(), args.clone(), &mut store).unwrap();
    assert_eq!(second["details"]["fileCount"], 2);
    assert_eq!(second["details"]["totalMatchCount"], 6600);
    assert!(
        second["content"]
            .as_str()
            .unwrap()
            .starts_with("20.txt:300\n21.txt:300\n")
    );
    args["skip"] = json!(22);
    let past = query(dir.path(), args, &mut store).unwrap();
    assert!(
        past["content"]
            .as_str()
            .unwrap()
            .contains("No more results")
    );
    assert_eq!(past["details"]["totalMatchCount"], 6600);
    let single = query(
        dir.path(),
        json!({"pattern":"needle","path":"00.txt","output_mode":"count"}),
        &mut store,
    )
    .unwrap();
    assert_eq!(single["details"]["totalMatchCount"], 300);
}

#[test]
fn grep_files_mode_paginates_paths_and_preserves_literal_names() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..22 {
        fs::write(
            dir.path().join(format!("{i:02}.txt")),
            "needle\n".repeat(300),
        )
        .unwrap();
    }
    fs::write(dir.path().join("# odd.txt"), "needle\n").unwrap();
    let mut store = hashline::Store::default();
    let first = query(
        dir.path(),
        json!({"pattern":"needle","output_mode":"files_with_matches","-A":9,"-B":9}),
        &mut store,
    )
    .unwrap();
    assert_eq!(first["details"]["fileCount"], 20);
    assert_eq!(first["details"]["perFileLimitReached"], false);
    let second = query(
        dir.path(),
        json!({"pattern":"needle","output_mode":"files_with_matches","skip":20}),
        &mut store,
    )
    .unwrap();
    assert_eq!(second["details"]["fileCount"], 3);
    let single = query(
        dir.path(),
        json!({"pattern":"needle","path":"# odd.txt","output_mode":"files_with_matches"}),
        &mut store,
    )
    .unwrap();
    assert_eq!(single["content"], "# odd.txt\n");
    for mode in ["files_with_matches", "count"] {
        let empty = query(
            dir.path(),
            json!({"pattern":"absent","output_mode":mode}),
            &mut store,
        )
        .unwrap();
        assert_eq!(empty["details"]["fileCount"], 0);
        assert!(
            empty["content"]
                .as_str()
                .unwrap()
                .contains("No matches found")
        );
    }
}

#[test]
fn grep_count_multiline_matches_and_file_ranges() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file"), "foo\nbar foo\nbar\nfoo\nbar\n").unwrap();
    for pattern in [
        "foo\\nbar",
        "foo\\n(?=bar)bar",
        "(?<=foo\\n)bar",
        "foo(?=\\nbar)",
    ] {
        let mut store = hashline::Store::default();
        let all = query(
            dir.path(),
            json!({"pattern":pattern,"path":"file","output_mode":"count"}),
            &mut store,
        )
        .unwrap();
        assert_eq!(all["details"]["totalMatchCount"], 3, "{all}");
        let ranged = query(
            dir.path(),
            json!({"pattern":pattern,"path":"file:4-5","output_mode":"count"}),
            &mut store,
        )
        .unwrap();
        assert_eq!(ranged["details"]["totalMatchCount"], 1, "{ranged}");
        let paths = query(
            dir.path(),
            json!({"pattern":pattern,"path":"file:4-5","output_mode":"files_with_matches"}),
            &mut store,
        )
        .unwrap();
        assert_eq!(paths["details"]["fileCount"], 1, "{paths}");
    }
    let ranged = query(
        dir.path(),
        json!({"pattern":"foo","path":"file:2-3","output_mode":"count"}),
        &mut hashline::Store::default(),
    )
    .unwrap();
    assert_eq!(ranged["details"]["totalMatchCount"], 1);
    fs::write(dir.path().join("file"), "foo\nbar").unwrap();
    let eof = query(
        dir.path(),
        json!({"pattern":"foo\\n|$","path":"file","output_mode":"count"}),
        &mut hashline::Store::default(),
    )
    .unwrap();
    assert_eq!(eof["details"]["totalMatchCount"], 2, "{eof}");
}

#[test]
fn grep_non_content_modes_never_grant_or_change_hashline_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let raw = "secret\nneedle\nother\n";
    fs::write(dir.path().join("file"), raw).unwrap();
    let tag = hashline::file_hash(raw);
    let visible = hashline::parse(&format!("[file#{tag}]\nCUT 2.=2")).unwrap();
    let hidden = hashline::parse(&format!("[file#{tag}]\nCUT 1.=1")).unwrap();
    for mode in ["files_with_matches", "count"] {
        let mut store = hashline::Store::default();
        let args = json!({"pattern":"secret","path":"file","output_mode":mode});
        query(dir.path(), args.clone(), &mut store).unwrap();
        assert!(
            store
                .validate(Path::new("file"), &tag, raw, &hidden.edits)
                .is_err()
        );
        query(
            dir.path(),
            json!({"pattern":"needle","path":"file","context":0}),
            &mut store,
        )
        .unwrap();
        query(dir.path(), args.clone(), &mut store).unwrap();
        assert!(
            store
                .validate(Path::new("file"), &tag, raw, &visible.edits)
                .is_ok()
        );
        assert!(
            store
                .validate(Path::new("file"), &tag, raw, &hidden.edits)
                .is_err()
        );
        let changed = "secret changed\nneedle\nother\n";
        fs::write(dir.path().join("file"), changed).unwrap();
        query(dir.path(), args, &mut store).unwrap();
        assert!(
            store
                .validate(Path::new("file"), &tag, raw, &visible.edits)
                .is_ok()
        );
        assert!(
            store
                .validate(Path::new("file"), &tag, changed, &visible.edits)
                .is_err()
        );
        fs::write(dir.path().join("file"), raw).unwrap();
    }
}

#[test]
fn grep_count_marks_partial_coverage_and_preserves_full_direct_search() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("large"),
        format!("{}needle\n", "padding\n".repeat(524_289)),
    )
    .unwrap();
    let mut store = hashline::Store::default();
    let direct = query(
        dir.path(),
        json!({"pattern":"needle","path":"large","output_mode":"count"}),
        &mut store,
    )
    .unwrap();
    assert_eq!(direct["details"]["totalMatchCount"], 1);
    assert_eq!(direct["details"]["countComplete"], true);
    let partial = query(
        dir.path(),
        json!({"pattern":"needle","output_mode":"count"}),
        &mut store,
    )
    .unwrap();
    assert_eq!(partial["details"]["countComplete"], false);
    assert_eq!(partial["details"]["truncated"], true);
    assert!(
        partial["content"]
            .as_str()
            .unwrap()
            .contains("Total (partial): 0")
    );
    let missing = query(
        dir.path(),
        json!({"pattern":"needle","path":"large;missing","output_mode":"count"}),
        &mut store,
    )
    .unwrap();
    assert_eq!(missing["details"]["totalMatchCount"], 1);
    assert_eq!(missing["details"]["countComplete"], false);
    fs::write(dir.path().join("binary"), b"needle\n\0needle\n").unwrap();
    let binary = query(
        dir.path(),
        json!({"pattern":"needle","path":"binary","output_mode":"count"}),
        &mut store,
    )
    .unwrap();
    assert_eq!(binary["details"]["countComplete"], false, "{binary}");
    assert!(
        binary["content"]
            .as_str()
            .unwrap()
            .contains("binary file not fully searched")
    );
}
