use super::*;

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
    assert_eq!(failure["stdout"], "out");
    assert_eq!(failure["stderr"], "err");
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
    for args in [
        json!({"path":"file","offset":0}),
        json!({"path":"file","offset":-1}),
        json!({"path":"file","limit":0}),
        json!({"path":"file","limit":2}),
        json!({"path":"file","offset":"2"}),
        json!({"path":"file","limit":null}),
    ] {
        assert_eq!(execute(&tools, "read", args)["ok"], false);
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
    let outside = tempfile::tempdir().unwrap();
    let path = outside.path().join("file");
    fs::write(&path, "original\n").unwrap();
    symlink(&path, dir.path().join("link")).unwrap();
    for mode in [EditMode::Replace, EditMode::Hashline] {
        let tools = Tools::new(dir.path().into(), mode).unwrap();
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
