mod integration {
    use super::super::*;

    #[test]
    fn file_boundaries_and_strict_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let content = "x".repeat(OUTPUT_LIMIT);
        assert_eq!(
            execute(
                &tools,
                "write",
                json!({"path":"nested/file","content":content})
            )["bytes_written"],
            OUTPUT_LIMIT
        );
        let exact = execute(&tools, "read", json!({"path":"nested/file"}));
        assert_eq!(exact["truncated"], false);
        fs::write(dir.path().join("nested/file"), "x".repeat(OUTPUT_LIMIT + 1)).unwrap();
        let over = execute(&tools, "read", json!({"path":"nested/file"}));
        assert_eq!(over["ok"], false);
        for (name, args) in [
            ("write", json!({"path":"forbidden","content":"x","extra":1})),
            ("read", json!({"path":" "})),
            ("read", json!({"path":"nested"})),
            ("bash", json!({"command":" "})),
        ] {
            assert_eq!(execute(&tools, name, args)["ok"], false);
        }
        assert!(!dir.path().join("forbidden").exists());
        let failure = execute(
            &tools,
            "bash",
            json!({"command":"printf out; printf err >&2; exit 7"}),
        );
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["exit_code"], 7);
        assert!(failure["output"].as_str().unwrap().contains("out"));
        assert!(failure["output"].as_str().unwrap().contains("err"));
    }

    #[test]
    fn read_pagination_validates_arguments_and_retains_hashline_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "one\ntwo\nthree\n").unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        tools.limits.read_max_lines = 1;
        let page = execute(&tools, "read", json!({"path":"file:2"}));
        assert_eq!(page["lines_read"], 1);
        assert_eq!(page["next_offset"], 3);
        assert!(
            page["content"]
                .as_str()
                .unwrap()
                .lines()
                .nth(1)
                .unwrap()
                .starts_with("2:")
        );
        let content = page["content"].as_str().unwrap();
        assert_eq!(
            execute(&tools, "edit", hashline_args(content, "PUT 2.=2:\n+edited"))["ok"],
            true
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("file")).unwrap(),
            "one\nedited\nthree\n"
        );
        // Accepts the model's usual offset/limit/ranges (plus numeric strings and null) and folds them into a path selector.
        let by_offset_limit = execute(
            &tools,
            "read",
            json!({"path":"file","offset":"2","limit":1}),
        );
        assert_eq!(by_offset_limit["ok"], true, "{by_offset_limit}");
        assert_eq!(by_offset_limit["lines_read"], 1);
        assert!(
            by_offset_limit["content"]
                .as_str()
                .unwrap()
                .contains("2:edited"),
            "{by_offset_limit}"
        );
        let by_limit_only = execute(&tools, "read", json!({"path":"file","limit":1}));
        assert!(
            by_limit_only["content"].as_str().unwrap().contains("1:one"),
            "{by_limit_only}"
        );
        let by_ranges = execute(&tools, "read", json!({"path":"file","path_ranges":"2-3"}));
        assert_eq!(by_ranges["ok"], true, "{by_ranges}");
        assert!(
            by_ranges["content"].as_str().unwrap().contains("2:edited"),
            "{by_ranges}"
        );
        let with_null = execute(&tools, "read", json!({"path":"file","limit":null}));
        assert_eq!(with_null["ok"], true, "{with_null}");
        for args in [
            json!({"path":"file","offset":0}),
            json!({"path":"file","offset":-1}),
            json!({"path":"file","limit":0}),
            json!({"path":"file","offset":"abc"}),
            json!({"path":"file","offset":1.5}),
            json!({"path":"file:1-2","limit":2}),
            json!({"path":"file","offset":1,"ranges":"1-2"}),
            json!({"path":"file","ranges":"invalid"}),
            json!({"path":"file","path2":"other"}),
        ] {
            assert_eq!(execute(&tools, "read", args.clone())["ok"], false, "{args}");
        }
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "readable").unwrap();
        assert_eq!(
            execute(&tools, "read", json!({"path":outside.path()}))["ok"],
            true
        );
    }

    #[test]
    fn write_and_both_edit_modes_reject_external_and_symlink_paths() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = super::super::outside_dir();
        let path = outside.path().join("file");
        fs::write(&path, "original\n").unwrap();
        symlink(&path, dir.path().join("link")).unwrap();
        let agent_dir = dir.path().join("agent");
        fs::create_dir_all(&agent_dir).unwrap();
        for mode in [EditMode::Replace, EditMode::Hashline] {
            let mut tools = Tools::new(dir.path().into(), mode).unwrap();
            tools.permission = pp_permission::Gate::for_process(
                &agent_dir,
                Some(pp_config::config::PermissionMode::WorkspaceWrite),
                false,
            );
            for path in [path.clone(), dir.path().join("link")] {
                assert_eq!(
                    execute(&tools, "write", json!({"path":path,"content":"bad"}))["ok"],
                    false
                );
                let edits = match mode {
                    EditMode::Replace => json!([{"oldText":"original", "newText":"bad"}]),
                    EditMode::Hashline => {
                        json!([{"op":"replace","start":"1:deadbeefdeadbeef","end":"1:deadbeefdeadbeef","lines":["bad"]}])
                    }
                };
                assert_eq!(
                    execute(
                        &tools,
                        "edit",
                        if mode == EditMode::Hashline {
                            json!({"input":format!("[{}#A1B2]\nPUT 1.=1:\n+bad",path.display())})
                        } else {
                            json!({"path":path,"edits":edits})
                        }
                    )["ok"],
                    false
                );
            }
        }
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn temp_directory_is_writable_through_both_spellings() {
        // `/tmp` and `/private/tmp` are writable even outside the startup directory: both spellings hit the same file,
        // and hashline mode uses the same real absolute path as the snapshot key: read it, then modify the same file.
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let name = temp
            .path()
            .strip_prefix("/tmp")
            .expect("tempdir lies under /tmp");
        let aliased = Path::new("/tmp").join(name).join("probe.mjs");
        let real = Path::new("/private/tmp").join(name).join("probe.mjs");
        for (path, content) in [(&aliased, "one\n"), (&real, "two\n")] {
            assert_eq!(
                execute(&tools, "write", json!({"path":path,"content":content}))["ok"],
                true,
                "{path:?}"
            );
            assert_eq!(fs::read_to_string(&real).unwrap(), content);
        }
        assert_eq!(
            execute(
                &tools,
                "edit",
                json!({"path":aliased,"edits":[{"oldText":"two","newText":"three"}]})
            )["ok"],
            true
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "three\n");
        // Hashline mode uses the same real absolute path as the snapshot key: read it, then modify the same file.
        let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        assert_eq!(
            execute(
                &tools,
                "write",
                json!({"path":aliased,"content":"one\ntwo\n"})
            )["ok"],
            true
        );
        let (content, _) = read_content(&tools, aliased.to_str().unwrap());
        assert!(
            content
                .lines()
                .next()
                .unwrap()
                .contains(real.to_str().unwrap()),
            "{content}"
        );
        assert_eq!(
            execute(
                &tools,
                "edit",
                hashline_args(&content, "PUT 1.=1:\n+edited")
            )["ok"],
            true
        );
        assert_eq!(fs::read_to_string(&real).unwrap(), "edited\ntwo\n");
    }
}
