use super::*;

fn query(dir: &Path, args: Value, store: &mut hashline::Store) -> Result<Value> {
    let cwd = dir.canonicalize()?;
    run(
        &cwd,
        RgSettings::default(),
        EditMode::Hashline,
        &save::Workspace::open(&cwd)?,
        store,
        &args.to_string(),
        &CancellationToken::new(),
        None,
    )
}

#[test]
fn rg_context_aliases_overrides_and_zero() {
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
fn rg_ignore_case_alias_works_with_both_regex_engines_and_modes() {
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
fn rg_compat_rejects_conflicts_invalid_types_and_unknown_modes() {
    let dir = tempfile::tempdir().unwrap();
    for extra in [
        json!({"context":1,"-C":2}),
        json!({"case":true,"-i":true}),
        json!({"case":false,"-i":false}),
        json!({"output_mode":"unknown"}),
        json!({"output_mode":null}),
        json!({"-i":"true"}),
        json!({"-n":"true"}),
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
    let negative = query(
        dir.path(),
        json!({"pattern":"needle","max_count":-1.0}),
        &mut hashline::Store::default(),
    )
    .unwrap_err();
    assert!(negative.to_string().contains("max_count"), "{negative:#}");
    let floored = query(
        dir.path(),
        json!({"pattern":"needle","max_count":1.5}),
        &mut hashline::Store::default(),
    )
    .unwrap();
    assert_eq!(floored["details"]["fileCount"], 0, "{floored}");
}

#[test]
fn rg_count_ignores_content_caps_and_totals_all_pages() {
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
fn rg_files_mode_paginates_paths_and_preserves_literal_names() {
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
fn rg_count_multiline_matches_and_file_ranges() {
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
fn rg_non_content_modes_never_grant_or_change_hashline_snapshots() {
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
fn rg_count_marks_partial_coverage_and_preserves_full_direct_search() {
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

#[test]
fn rg_fixed_and_word_match_literally_and_on_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("file"),
        "literal (?=x) line\na{ b{\nfoobar\nfoo-bar\nfoo. baz\nxfoo. tail\n",
    )
    .unwrap();
    let mut store = hashline::Store::default();
    let lookahead = query(
        dir.path(),
        json!({"pattern":"(?=x)","path":"file","-F":true,"context":0}),
        &mut store,
    )
    .unwrap();
    assert!(
        lookahead["content"]
            .as_str()
            .unwrap()
            .contains("literal (?=x) line"),
        "{lookahead}"
    );
    let brace = query(
        dir.path(),
        json!({"pattern":"a{","path":"file","-F":true,"context":0}),
        &mut store,
    )
    .unwrap();
    assert!(
        brace["content"].as_str().unwrap().contains("a{ b{"),
        "{brace}"
    );
    let word = query(
        dir.path(),
        json!({"pattern":"foo","path":"file","-w":true,"context":0}),
        &mut store,
    )
    .unwrap();
    let text = word["content"].as_str().unwrap();
    assert!(
        text.contains("foo-bar") && text.contains("foo. baz"),
        "{text}"
    );
    assert!(
        !text.contains("foobar") && !text.contains("xfoo."),
        "{text}"
    );
    let combined = query(
        dir.path(),
        json!({"pattern":"foo.","path":"file","-F":true,"-w":true,"context":0}),
        &mut store,
    )
    .unwrap();
    let combined = combined["content"].as_str().unwrap();
    assert!(combined.contains("foo. baz"), "{combined}");
    assert!(!combined.contains("xfoo."), "{combined}");
}

#[test]
fn rg_type_filters_cover_the_walk_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("src/b.txt"), "needle\n").unwrap();
    fs::write(dir.path().join("src/c.toml"), "needle\n").unwrap();
    let mut store = hashline::Store::default();
    let rust = query(
        dir.path(),
        json!({"pattern":"needle","-t":"rust","context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(rust["details"]["fileCount"], 1, "{rust}");
    assert!(rust["content"].as_str().unwrap().contains("a.rs"), "{rust}");
    let mixed = query(
        dir.path(),
        json!({"pattern":"needle","-t":["rust","!toml"],"context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(mixed["details"]["fileCount"], 1, "{mixed}");
    let toml = query(
        dir.path(),
        json!({"pattern":"needle","-t":"toml","context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(toml["details"]["fileCount"], 1, "{toml}");
    assert!(
        toml["content"].as_str().unwrap().contains("c.toml"),
        "{toml}"
    );
    let unknown = query(
        dir.path(),
        json!({"pattern":"needle","-t":"nope","context":0}),
        &mut store,
    )
    .unwrap_err();
    assert!(unknown.to_string().contains("nope"), "{unknown:#}");
    let explicit = query(
        dir.path(),
        json!({"pattern":"needle","path":"src/a.rs","-t":"toml","context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(explicit["details"]["fileCount"], 1, "{explicit}");
}

#[test]
fn rg_override_globs_include_exclude_and_reject_invalid_patterns() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
    fs::write(dir.path().join("app.js"), "needle\n").unwrap();
    fs::write(dir.path().join("app.min.js"), "needle\n").unwrap();
    let mut store = hashline::Store::default();
    let only_rs = query(
        dir.path(),
        json!({"pattern":"needle","-g":"*.rs","context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(only_rs["details"]["fileCount"], 1, "{only_rs}");
    assert!(
        only_rs["content"].as_str().unwrap().contains("a.rs"),
        "{only_rs}"
    );
    let no_min = query(
        dir.path(),
        json!({"pattern":"needle","-g":["!*.min.js"],"context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(no_min["details"]["fileCount"], 3, "{no_min}");
    assert!(
        !no_min["content"].as_str().unwrap().contains("app.min.js"),
        "{no_min}"
    );
    let invalid = query(
        dir.path(),
        json!({"pattern":"needle","-g":"[","context":0}),
        &mut store,
    )
    .unwrap_err();
    assert!(
        invalid.to_string().contains("invalid -g glob"),
        "{invalid:#}"
    );
    // `glob` 是 `-g` 的别名：同时给出时合并为覆盖集合。
    let merged = query(
        dir.path(),
        json!({"pattern":"needle","-g":"*.rs","glob":"*.js","context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(merged["details"]["fileCount"], 3, "{merged}");
    assert!(
        merged["content"].as_str().unwrap().contains("a.rs")
            && merged["content"].as_str().unwrap().contains("app.js"),
        "{merged}"
    );
}

#[test]
fn rg_max_count_caps_each_file_and_keeps_the_last_context() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("five"),
        "needle one\nafter-one\nneedle two\nafter-two\nneedle three\nafter-three\n",
    )
    .unwrap();
    let mut store = hashline::Store::default();
    let content = query(
        dir.path(),
        json!({"pattern":"needle","path":"five","max_count":2,"context":0,"-A":1}),
        &mut store,
    )
    .unwrap();
    let text = content["content"].as_str().unwrap();
    assert_eq!(content["details"]["matchCount"], 2, "{content}");
    assert!(text.contains("after-two"), "{text}");
    assert!(
        !text.contains("after-three") && !text.contains("needle three"),
        "{text}"
    );
    fs::create_dir(dir.path().join("count")).unwrap();
    for name in ["f1", "f2"] {
        fs::write(dir.path().join("count").join(name), "needle\n".repeat(5)).unwrap();
    }
    let counted = query(
        dir.path(),
        json!({"pattern":"needle","path":"count","output_mode":"count","max_count":2,"context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(counted["details"]["fileCount"], 2, "{counted}");
    assert_eq!(counted["details"]["totalMatchCount"], 4, "{counted}");
    assert!(
        counted["content"].as_str().unwrap().contains("f1:2\n"),
        "{counted}"
    );
    let none = query(
        dir.path(),
        json!({"pattern":"needle","max_count":0,"context":0}),
        &mut store,
    )
    .unwrap();
    assert_eq!(none["details"]["fileCount"], 0, "{none}");
    assert!(
        none["content"]
            .as_str()
            .unwrap()
            .contains("No matches found"),
        "{none}"
    );
}
