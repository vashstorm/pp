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
fn rg_context_case_fixed_word_and_validation() {
    {
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

    {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "NEEDLE\nneedle\nNeedle\n").unwrap();
        for mode in ["content", "files_with_matches", "count"] {
            for pattern in ["needle", "need(?=le)"] {
                let mut store = hashline::Store::default();
                let mut args = json!({"pattern":pattern,"path":"file","output_mode":mode,"-i":true,"context":0});
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

    {
        let dir = tempfile::tempdir().unwrap();
        for extra in [
            json!({"context":1,"-C":2}),
            json!({"case":true,"-i":true}),
            json!({"case":false,"-i":false}),
            json!({"output_mode":"unknown"}),
            json!({"-i":"yes"}),
            json!({"-c":"maybe"}),
            json!({"-c":true,"output_mode":"content"}),
            json!({"head":5,"head_limit":6}),
            json!({"max_output_tokens":0}),
            json!({"max_output_tokens":-1}),
            json!({"max_output_tokens":1.5}),
            json!({"max_output_tokens":1e100}),
            json!({"max_output_tokens":"0"}),
            json!({"max_output_tokens":true}),
            json!({"head_limit":-1}),
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

    {
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
}

#[test]
fn rg_count_and_files_modes_pagination_multiline_and_coverage() {
    {
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

    {
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

    {
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

    {
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

    {
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
}

#[test]
fn rg_hashline_isolation_types_and_override_globs() {
    {
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

    {
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

    {
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
        // `glob` is an alias for `-g`: when both are given they merge into the override set.
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
}

/// Common model argument drift (taken from real session failure records) must be normalized, not rejected:
/// stringified booleans/numbers, ripgrep CLI style aliases, `-c`/`-l` mode flags and null values;
/// unknown keys are ignored and reported as a hint in the result.
#[test]
fn rg_normalizes_common_model_argument_drift() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("file"),
        "one\nNeedle\nthree\nneedle\nfive\n",
    )
    .unwrap();
    let run = |args: Value| {
        let mut args = args;
        args["pattern"] = json!("needle");
        args["path"] = json!("file");
        query(dir.path(), args, &mut hashline::Store::default()).unwrap()
    };
    for (drifted, canonical) in [
        (json!({"-n":"true","-C":"1"}), json!({"-C":1})),
        (
            json!({"-i":"TRUE","A":"0","B":0}),
            json!({"-i":true,"-A":0,"-B":0}),
        ),
        (
            json!({"i":true,"context":"0"}),
            json!({"-i":true,"context":0}),
        ),
        (json!({"-c":"true"}), json!({"output_mode":"count"})),
        (
            json!({"-l":true}),
            json!({"output_mode":"files_with_matches"}),
        ),
        (json!({"mode":"count"}), json!({"output_mode":"count"})),
        (
            json!({"output_mode":null,"max_output_tokens":null}),
            json!({}),
        ),
        (
            json!({"max_tokens":"4000"}),
            json!({"max_output_tokens":4000}),
        ),
        (
            json!({"case_insensitive":"true","context":0}),
            json!({"-i":true,"context":0}),
        ),
        (json!({"offset":"1"}), json!({"skip":1})),
        (json!({"gitignore":"false"}), json!({"gitignore":false})),
    ] {
        assert_eq!(run(drifted.clone()), run(canonical), "{drifted}");
    }
    let ignored = run(json!({"head_file":"x","-v":true}));
    let text = ignored["content"].as_str().unwrap();
    assert!(text.contains("*4:needle"), "{ignored}");
    assert!(
        text.contains("Ignored unsupported arguments") && text.contains("head_file"),
        "{ignored}"
    );
    assert_eq!(
        ignored["details"]["ignoredArguments"],
        json!(["-v", "head_file"])
    );
}

#[test]
fn rg_head_limit_caps_lines_and_entries() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("file"),
        "needle 1\nneedle 2\nneedle 3\nneedle 4\n",
    )
    .unwrap();
    for i in 0..5 {
        fs::write(dir.path().join(format!("{i}.txt")), "needle\n").unwrap();
    }
    let run = |args: Value| query(dir.path(), args, &mut hashline::Store::default()).unwrap();
    // content: counted by body lines (context lines included); the aliases `head_limit`/`head`/`limit` are equivalent.
    for key in ["head_limit", "head", "limit", "max_lines"] {
        let capped = run(json!({"pattern":"needle","path":"file","context":0,key:"2"}));
        let text = capped["content"].as_str().unwrap();
        assert!(
            text.contains("needle 2") && !text.contains("needle 3"),
            "{capped}"
        );
        assert!(text.contains("head_limit=2"), "{capped}");
        assert_eq!(capped["details"]["headLimitReached"], true, "{capped}");
        assert_eq!(capped["details"]["truncated"], true, "{capped}");
    }
    let unlimited = run(json!({"pattern":"needle","path":"file","context":0,"head_limit":0}));
    assert_eq!(unlimited["details"]["matchCount"], 4, "{unlimited}");
    let exact = run(json!({"pattern":"needle","path":"file","context":0,"head_limit":4}));
    assert_eq!(exact["details"]["headLimitReached"], false, "{exact}");
    // Other modes count entries and give a per-entry pagination skip.
    let files = run(json!({"pattern":"needle","-g":"*.txt","-l":true,"head_limit":2}));
    let text = files["content"].as_str().unwrap();
    assert!(
        text.starts_with("0.txt\n1.txt\n\n") && !text.contains("2.txt"),
        "{files}"
    );
    assert!(text.contains("head_limit=2 entries. Use skip=2"), "{files}");
    let next = run(json!({"pattern":"needle","-g":"*.txt","-l":true,"head_limit":2,"skip":2}));
    assert!(
        next["content"]
            .as_str()
            .unwrap()
            .starts_with("2.txt\n3.txt\n"),
        "{next}"
    );
    let counted = run(json!({"pattern":"needle","-g":"*.txt","-c":true,"head_limit":1}));
    assert_eq!(counted["details"]["totalMatchCount"], 5, "{counted}");
    assert_eq!(counted["details"]["fileCount"], 1, "{counted}");
    assert_eq!(counted["details"]["truncated"], true, "{counted}");
}

#[test]
fn rg_missing_path_suggests_workspace_matches() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("crates/pp-config/src")).unwrap();
    fs::create_dir_all(dir.path().join("crates/pp-runtime/src")).unwrap();
    fs::write(dir.path().join("crates/pp-config/src/auth.rs"), "x\n").unwrap();
    fs::write(
        dir.path().join("crates/pp-runtime/src/system_prompt.rs"),
        "x\n",
    )
    .unwrap();
    let error = |path: &str| {
        query(
            dir.path(),
            json!({"pattern":"x","path":path}),
            &mut hashline::Store::default(),
        )
        .unwrap_err()
        .to_string()
    };
    let moved = error("./crates/pp-llm/src/auth.rs");
    assert!(
        moved.contains("Path not found: ./crates/pp-llm/src/auth.rs")
            && moved.contains("crates/pp-config/src/auth.rs"),
        "{moved}"
    );
    // A name without an extension matches a same-stem file; a line-range selector does not affect suggestions.
    let stem = error("crates/pp-runtime/src/system_prompt:3");
    assert!(
        stem.contains("did you mean: crates/pp-runtime/src/system_prompt.rs?"),
        "{stem}"
    );
    let bare = error("crates/pp-runtime/src/system_promp");
    assert!(!bare.contains("did you mean"), "{bare}");
    let outside = error("../elsewhere/auth.rs");
    assert!(!outside.contains("did you mean"), "{outside}");
}
