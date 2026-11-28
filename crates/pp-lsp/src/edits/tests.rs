use super::*;
use pp_workspace::save::Workspace;
use std::fs;
fn fixture() -> (tempfile::TempDir, Workspace) {
    // The save layer validates absolute paths against the canonical root; once the temporary parent is normalized, `dir.path()`
    // matches the workspace root spelling (on macOS the default TMPDIR contains a /var symlink).
    let dir = tempfile::Builder::new()
        .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
        .unwrap();
    let workspace = Workspace::open(dir.path()).unwrap();
    (dir, workspace)
}
fn text_edits(value: Value) -> Vec<TextEdit> {
    serde_json::from_value(value).unwrap()
}
fn uri(path: &Path) -> String {
    crate::uri::path_to_uri(path)
}
fn edit(value: Value) -> WorkspaceEdit {
    serde_json::from_value(value).unwrap()
}

#[test]
fn lsp_edits_text_positions_duplicates_and_protocol_order() {
    {
        let content = "let a = \"😀\";\nlet b = 1;\n";
        let emoji = text_edits(serde_json::json!([{
            "range": {"start": {"line": 0, "character": 9}, "end": {"line": 0, "character": 11}},
            "newText": "x"
        }]));
        assert_eq!(
            apply_text_edits(content, &emoji).unwrap(),
            "let a = \"x\";\nlet b = 1;\n"
        );

        let split = text_edits(serde_json::json!([{
            "range": {"start": {"line": 0, "character": 10}, "end": {"line": 0, "character": 10}},
            "newText": "!"
        }]));
        assert!(apply_text_edits(content, &split).is_err());

        let crlf = "a\r\nb😀c\r\n";
        let across = text_edits(serde_json::json!([{
            "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 4}},
            "newText": "z"
        }]));
        assert_eq!(apply_text_edits(crlf, &across).unwrap(), "z\r\n");

        let beyond = text_edits(serde_json::json!([{
            "range": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 0}},
            "newText": "!"
        }]));
        assert!(apply_text_edits(content, &beyond).is_err());
    }

    {
        let content = "abcdefghij";
        let overlap = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 5}}, "newText": "X"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7}}, "newText": "Y"}
        ]));
        assert!(apply_text_edits(content, &overlap).is_err());

        let duplicate = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": "Z"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": "Z"}
        ]));
        assert_eq!(apply_text_edits(content, &duplicate).unwrap(), "Zcdefghij");

        let inserts = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "B"}
        ]));
        assert_eq!(apply_text_edits(content, &inserts).unwrap(), "abcABdefghij");

        let repeated_insert = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"}
        ]));
        assert_eq!(
            apply_text_edits(content, &repeated_insert).unwrap(),
            "abcAdefghij"
        );

        let separated_duplicate = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "B"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"}
        ]));
        assert_eq!(
            apply_text_edits(content, &separated_duplicate).unwrap(),
            "abcABdefghij"
        );

        let touching = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "1"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}, "newText": "2"}
        ]));
        assert_eq!(apply_text_edits(content, &touching).unwrap(), "12ghij");
    }

    {
        let content = "abcdef";
        let valid = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "X"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Y"}
        ]));
        assert_eq!(apply_text_edits(content, &valid).unwrap(), "XYdef");

        let multiple = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "A"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "B"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Y"}
        ]));
        assert_eq!(apply_text_edits(content, &multiple).unwrap(), "ABYdef");

        let invalid = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Y"},
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "X"}
        ]));
        assert!(apply_text_edits(content, &invalid).is_err());

        let at_end = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Y"},
            {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "Z"}
        ]));
        assert_eq!(apply_text_edits(content, &at_end).unwrap(), "YZdef");

        let inside = text_edits(serde_json::json!([
            {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}, "newText": "Y"},
            {"range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 2}}, "newText": "X"}
        ]));
        assert!(apply_text_edits(content, &inside).is_err());
    }
}

#[test]
fn lsp_edits_text_validation_and_saturating_bounds() {
    let content = "abc";
    let reversed = text_edits(serde_json::json!([{
        "range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 1}},
        "newText": "X"
    }]));
    assert!(apply_text_edits(content, &reversed).is_err());

    let snippet = text_edits(serde_json::json!([{
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}},
        "newText": "${1:name}",
        "insertTextFormat": 2
    }]));
    assert!(apply_text_edits(content, &snippet).is_err());
    assert!(apply_text_edits("", &[]).unwrap().is_empty());

    let reversed_u32 = text_edits(serde_json::json!([{
        "range": {
            "start": {"line": 4294967295u32, "character": 0},
            "end": {"line": 0, "character": 0}
        },
        "newText": "x"
    }]));
    assert!(apply_text_edits("content", &reversed_u32).is_err());
}

#[test]
fn lsp_edits_virtual_plan_resources_versions_and_budget() {
    {
        let (dir, workspace) = fixture();
        let from = dir.path().join("a.txt");
        let to = dir.path().join("nested/b.txt");
        fs::write(&from, "old").unwrap();
        let proposal = edit(serde_json::json!({"documentChanges":[
            {"kind":"rename","oldUri":uri(&from),"newUri":uri(&to)},
            {"textDocument":{"uri":uri(&to),"version":null},"edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"}]}
        ]}));
        let preview = plan(&workspace, None, &proposal, &CancellationToken::new()).unwrap();
        assert_eq!(
            preview.records[Path::new("nested/b.txt")].after.as_deref(),
            Some(b"new".as_slice())
        );
        assert_eq!(fs::read(&from).unwrap(), b"old");
        assert!(!to.exists());
        assert!(!dir.path().join("nested").exists());
    }

    {
        let (dir, workspace) = fixture();
        let path = dir.path().join("a.txt");
        fs::write(&path, "old").unwrap();
        let proposal = edit(
            serde_json::json!({"documentChanges":[{"textDocument":{"uri":uri(&path),"version":999},"edits":[]}]}),
        );
        assert!(plan(&workspace, None, &proposal, &CancellationToken::new()).is_err());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let proposal = edit(
            serde_json::json!({"changes":{uri(&link):[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"}]}}),
        );
        assert!(plan(&workspace, None, &proposal, &CancellationToken::new()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"old");
    }

    {
        let (dir, workspace) = fixture();
        let target_dir = dir.path().join("res");
        fs::create_dir(&target_dir).unwrap();
        let sparse = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target_dir.join("big.bin"))
            .unwrap();
        sparse
            .set_len(pp_workspace::save::RESOURCE_BYTE_LIMIT)
            .unwrap();
        drop(sparse);
        let extra = target_dir.join("extra.bin");
        let renamed = dir.path().join("moved");
        let proposal = edit(serde_json::json!({
            "documentChanges":[
                {"kind":"create","uri":uri(&extra)},
                {"textDocument":{"uri":uri(&extra),"version":null},"edits":[
                    {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
                     "newText":"x"}
                ]},
                {"kind":"rename","oldUri":uri(&target_dir),"newUri":uri(&renamed)}
            ]
        }));
        let error = plan(&workspace, None, &proposal, &CancellationToken::new())
            .err()
            .expect("overlay over the byte budget must refuse the suggestion");
        assert!(error.to_string().contains("256 MiB"), "{error}");
        assert!(!extra.exists());
        assert!(!renamed.exists());
        assert!(target_dir.join("big.bin").exists());
    }
}
