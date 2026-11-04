use super::*;
use crate::tools::Tools;
use pp_config::config::EditMode;
use std::fs;
fn fixture() -> (tempfile::TempDir, Tools) {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    (dir, tools)
}
fn text_edits(value: Value) -> Vec<TextEdit> {
    serde_json::from_value(value).unwrap()
}
fn uri(path: &Path) -> String {
    crate::lsp::uri::path_to_uri(path)
}
fn edit(value: Value) -> WorkspaceEdit {
    serde_json::from_value(value).unwrap()
}

#[test]
fn lsp_edits_text_utf16_emoji_positions() {
    // 第 0 行 `let a = "😀";`：emoji 占 UTF-16 两列（9..11）。
    let content = "let a = \"😀\";\nlet b = 1;\n";
    let emoji = text_edits(serde_json::json!([{
        "range": {"start": {"line": 0, "character": 9}, "end": {"line": 0, "character": 11}},
        "newText": "x"
    }]));
    assert_eq!(
        apply_text_edits(content, &emoji).unwrap(),
        "let a = \"x\";\nlet b = 1;\n"
    );

    // 落在代理对中间的列必须被拒绝，不能猜测相邻边界。
    let split = text_edits(serde_json::json!([{
        "range": {"start": {"line": 0, "character": 10}, "end": {"line": 0, "character": 10}},
        "newText": "!"
    }]));
    assert!(apply_text_edits(content, &split).is_err());

    // CRLF 的 `\r` 不属于行内容；跨行替换保留其余行。
    let crlf = "a\r\nb😀c\r\n";
    let across = text_edits(serde_json::json!([{
        "range": {"start": {"line": 0, "character": 0}, "end": {"line": 1, "character": 4}},
        "newText": "z"
    }]));
    assert_eq!(apply_text_edits(crlf, &across).unwrap(), "z\r\n");

    // 行/列越界同样是错误，不做钳制。
    let beyond = text_edits(serde_json::json!([{
        "range": {"start": {"line": 9, "character": 0}, "end": {"line": 9, "character": 0}},
        "newText": "!"
    }]));
    assert!(apply_text_edits(content, &beyond).is_err());
}

#[test]
fn lsp_edits_text_overlap_and_duplicate_edits() {
    let content = "abcdefghij";
    let overlap = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 5}}, "newText": "X"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 7}}, "newText": "Y"}
    ]));
    assert!(apply_text_edits(content, &overlap).is_err());

    // 完全相同的非空范围编辑是服务器重复输出，去重后只应用一次。
    let duplicate = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": "Z"},
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}, "newText": "Z"}
    ]));
    assert_eq!(apply_text_edits(content, &duplicate).unwrap(), "Zcdefghij");

    // 同一位置的多次插入保持数组声明顺序，不是互相覆盖。
    let inserts = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "B"}
    ]));
    assert_eq!(apply_text_edits(content, &inserts).unwrap(), "abcABdefghij");

    // 完全相同的插入（空范围）同样去重：只插入一次。
    let repeated_insert = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"}
    ]));
    assert_eq!(
        apply_text_edits(content, &repeated_insert).unwrap(),
        "abcAdefghij"
    );

    // 重复项被同位置的其它插入隔开时也要去掉，且不重排剩下的声明顺序。
    let separated_duplicate = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "B"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "A"}
    ]));
    assert_eq!(
        apply_text_edits(content, &separated_duplicate).unwrap(),
        "abcABdefghij"
    );

    // 首尾相接（earlier.end == later.start）不是重叠。
    let touching = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "1"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}, "newText": "2"}
    ]));
    // 0..3 换成 "1"、3..6 换成 "2"，剩下 "ghij"。
    assert_eq!(apply_text_edits(content, &touching).unwrap(), "12ghij");
}

#[test]
fn lsp_edits_text_rejects_out_of_range_and_snippet() {
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
}

#[test]
fn lsp_edits_text_same_start_insert_and_replace_follow_protocol_order() {
    let content = "abcdef";
    // LSP: same-start edits must be inserts followed by at most one replace.
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

    // 替换先声明、插入后声明违反协议顺序：拒绝而不是静默丢掉插入。
    let invalid = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Y"},
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}, "newText": "X"}
    ]));
    assert!(apply_text_edits(content, &invalid).is_err());

    // 插入在替换结束位置是合法的首尾相接。
    let at_end = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "Y"},
        {"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 3}}, "newText": "Z"}
    ]));
    assert_eq!(apply_text_edits(content, &at_end).unwrap(), "YZdef");

    // 插入点严格位于替换范围内部是真重叠。
    let inside = text_edits(serde_json::json!([
        {"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}, "newText": "Y"},
        {"range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 2}}, "newText": "X"}
    ]));
    assert!(apply_text_edits(content, &inside).is_err());
}

#[test]
fn lsp_edits_saturating_position_format_does_not_panic() {
    let reversed = text_edits(serde_json::json!([{
        "range": {
            "start": {"line": 4294967295u32, "character": 0},
            "end": {"line": 0, "character": 0}
        },
        "newText": "x"
    }]));
    assert!(apply_text_edits("content", &reversed).is_err());
}

#[test]
fn lsp_edits_suggestions_validate_resources_in_memory_only() {
    let (dir, tools) = fixture();
    let from = dir.path().join("a.txt");
    let to = dir.path().join("nested/b.txt");
    fs::write(&from, "old").unwrap();
    let proposal = edit(serde_json::json!({"documentChanges":[
        {"kind":"rename","oldUri":uri(&from),"newUri":uri(&to)},
        {"textDocument":{"uri":uri(&to),"version":null},"edits":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"}]}
    ]}));
    let preview = plan(&tools, None, &proposal, &CancellationToken::new()).unwrap();
    assert_eq!(
        preview.records[Path::new("nested/b.txt")].after.as_deref(),
        Some(b"new".as_slice())
    );
    assert_eq!(fs::read(&from).unwrap(), b"old");
    assert!(!to.exists());
    assert!(!dir.path().join("nested").exists());
    assert!(tools.changes_view().files.is_empty());
}
#[test]
fn lsp_edits_suggestions_reject_unproved_versions_and_symlinks() {
    let (dir, tools) = fixture();
    let path = dir.path().join("a.txt");
    fs::write(&path, "old").unwrap();
    let proposal = edit(
        serde_json::json!({"documentChanges":[{"textDocument":{"uri":uri(&path),"version":999},"edits":[]}]}),
    );
    assert!(plan(&tools, None, &proposal, &CancellationToken::new()).is_err());
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    let proposal = edit(
        serde_json::json!({"changes":{uri(&link):[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"new"}]}}),
    );
    assert!(plan(&tools, None, &proposal, &CancellationToken::new()).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"old");
}
