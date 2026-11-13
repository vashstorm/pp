use super::position::resolve_column;
use super::suggestion::suggestion_conflicts;
use super::*;
use crate::lsp::config;
use pp_config::config::EditMode;
use pp_config::config::LspSettings;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py")
}

/// 隔离的 fixture 注册表：只匹配 `.rs`，日志留在临时目录里供“是否启动”断言。
fn fixture_registry(dir: &Path) -> Registry {
    fixture_registry_with(dir, &[])
}
fn fixture_registry_with(dir: &Path, extra: &[&str]) -> Registry {
    std::fs::create_dir_all(dir.join(".pp")).unwrap();
    let log = dir.join("requests.jsonl");
    let mut args = vec![
        fixture_script().display().to_string(),
        "--log".to_owned(),
        log.display().to_string(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    std::fs::write(
        dir.join(".pp/lsp.json"),
        json!({"servers": {"fixture": {
            "command": "python3",
            "args": args,
            "fileTypes": [".rs"],
            "rootMarkers": [".pp"],
        }}})
        .to_string(),
    )
    .unwrap();
    let config = config::Config::load_with(dir, None).unwrap();
    Registry::from_config(
        dir.to_path_buf(),
        LspSettings::default(),
        Ok(Arc::new(config)),
    )
}

fn canonical(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap()
}

fn fixture_tools(dir: &Path) -> (Tools, Registry) {
    let cwd = canonical(dir);
    let registry = fixture_registry(&cwd);
    let mut tools = Tools::new(cwd, EditMode::Replace).unwrap();
    tools.lsp = Some(registry.clone());
    (tools, registry)
}

fn write_file(dir: &Path, relative: &str, content: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn started(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("requests.jsonl"))
        .map(|log| {
            log.lines()
                .filter(|line| {
                    serde_json::from_str::<Value>(line)
                        .is_ok_and(|value| value["event"] == "started")
                })
                .count()
        })
        .unwrap_or(0)
}

fn cleanup(registry: Registry) {
    let mut handle = registry.cleanup();
    handle.stop().unwrap();
}

fn finish(tools: Tools, registry: Registry) {
    drop(tools);
    cleanup(registry);
}

fn call(tools: &Tools, arguments: Value, access: Access) -> Result<Value> {
    let text = arguments.to_string();
    let mut emitted = Vec::new();
    let mut emit = |event: AgentEvent| emitted.push(event);
    run(
        tools,
        &text,
        "call-1",
        access,
        &CancellationToken::new(),
        &mut emit,
    )
}

#[test]
fn lsp_tool_access_and_tier_are_derived_from_the_action() {
    assert_eq!(
        approval_tier(r#"{"action":"definition","file":"a.rs"}"#),
        Tier::Read
    );
    assert_eq!(
        approval_tier(r#"{"action":"hover","symbol":"x"}"#),
        Tier::Read
    );
    assert_eq!(
        approval_tier(r#"{"action":"diagnostics","file":"a.rs"}"#),
        Tier::Read
    );
    assert_eq!(
        approval_tier(r#"{"action":"diagnostics","file":"*"}"#),
        Tier::Read
    );
    // 缺少 file 的 diagnostics 参数无效，保守按写档。
    assert_eq!(approval_tier(r#"{"action":"diagnostics"}"#), Tier::Write);
    // 修改建议只读取信息。
    assert_eq!(
        approval_tier(r#"{"action":"rename","file":"a.rs","new_name":"b","apply":false}"#),
        Tier::Read
    );
    assert_eq!(
        approval_tier(r#"{"action":"rename_file","file":"a","new_name":"b"}"#),
        Tier::Read
    );
    assert_eq!(
        approval_tier(r#"{"action":"request","query":"x"}"#),
        Tier::Write
    );
    assert_eq!(approval_tier(r#"{"action":"reload"}"#), Tier::Write);
    assert_eq!(approval_tier("not json"), Tier::Write);
    assert_eq!(approval_tier(r#"{"action":"unknown"}"#), Tier::Write);

    assert!(validate_access(r#"{"action":"definition","file":"a.rs"}"#, Access::ReadOnly).is_ok());
    assert!(
        validate_access(
            r#"{"action":"symbols","file":"*","query":"x"}"#,
            Access::ReadOnly
        )
        .is_ok()
    );
    assert!(validate_access(r#"{"action":"rename","file":"a"}"#, Access::ReadOnly).is_ok());
    assert!(validate_access(r#"{"action":"request","query":"x"}"#, Access::ReadOnly).is_err());
    assert!(validate_access(r#"{"action":"reload"}"#, Access::ReadOnly).is_err());
    assert!(validate_access(r#"{"action":"code_actions","file":"a"}"#, Access::ReadOnly).is_ok());
    assert!(validate_access(r#"{"action":"diagnostics","file":"*"}"#, Access::ReadOnly).is_err());
    assert!(validate_access("not json", Access::ReadOnly).is_err());
    // Execute 门禁不在参数解析上失败，由 run 报告真正的参数错误。
    assert!(validate_access("not json", Access::Execute).is_ok());
}

#[test]
fn lsp_tool_status_never_starts_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    let (tools, registry) = fixture_tools(dir.path());
    let value = call(&tools, json!({"action":"status"}), Access::Execute).unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("fixture: not started"), "{content}");
    assert_eq!(started(&cwd), 0, "status must not spawn a server");
    assert_eq!(value["details"]["success"], json!(true));
    finish(tools, registry);
}

#[test]
fn lsp_tool_definition_returns_location_with_context() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    write_file(&cwd, "src/lib.rs", "fn value() {}\nlet x = 1;\n");
    let (tools, registry) = fixture_tools(dir.path());
    let value = call(
        &tools,
        json!({"action":"definition","file":"src/lib.rs","line":2,"symbol":"x"}),
        Access::Execute,
    )
    .unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("Found 1 definition(s)"), "{content}");
    assert!(content.contains("src/lib.rs:1:1"), "{content}");
    assert!(content.contains("1: fn value() {}"), "{content}");
    assert_eq!(value["details"]["serverName"], json!("fixture"));
    finish(tools, registry);
}

#[test]
fn lsp_tool_project_aware_symbol_is_required_for_navigation() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    write_file(&cwd, "src/lib.rs", "fn value() {}\nlet x = 1;\n");
    let (tools, registry) = fixture_tools(dir.path());
    let error = call(
        &tools,
        json!({"action":"definition","file":"src/lib.rs","line":2}),
        Access::Execute,
    )
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("symbol is required"),
        "{error:#}"
    );
    finish(tools, registry);
}

#[test]
fn lsp_tool_hover_flattens_markup_content() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    write_file(&cwd, "src/lib.rs", "fn value() {}\n");
    let (tools, registry) = fixture_tools(dir.path());
    let value = call(
        &tools,
        json!({"action":"hover","file":"src/lib.rs","line":1}),
        Access::Execute,
    )
    .unwrap();
    assert_eq!(value["content"], json!("fixture hover"));
    finish(tools, registry);
}

#[test]
fn lsp_tool_single_file_diagnostics_reports_issues_and_clean() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    write_file(&cwd, "src/lib.rs", "wrong\n");
    let (tools, registry) = fixture_tools(dir.path());
    let value = call(
        &tools,
        json!({"action":"diagnostics","file":"src/lib.rs"}),
        Access::Execute,
    )
    .unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("[error]"), "{content}");
    assert!(content.contains("fixture: wrong value"), "{content}");
    assert_eq!(value["details"]["success"], json!(true));
    let servers = value["diagnostics"]["servers"].as_array().unwrap();
    assert_eq!(servers[0]["diagnostics"].as_array().unwrap().len(), 1);

    write_file(&cwd, "src/lib.rs", "fn good() {}\n");
    let value = call(
        &tools,
        json!({"action":"diagnostics","file":"src/lib.rs"}),
        Access::Execute,
    )
    .unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("fixture: clean"), "{content}");
    assert!(!content.contains("[error]"), "{content}");
    finish(tools, registry);
}

#[test]
fn lsp_tool_read_only_refuses_writes_before_any_server_starts() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    let (tools, registry) = fixture_tools(dir.path());
    for args in [
        json!({"action":"reload"}),
        json!({"action":"rename","file":"a.rs","new_name":"b","apply":true}),
        json!({"action":"request","query":"fixture/ping"}),
    ] {
        assert!(call(&tools, args, Access::ReadOnly).is_err());
    }
    assert_eq!(started(&cwd), 0);
    finish(tools, registry);
}

#[test]
fn lsp_tool_request_rejects_invalid_payload_before_starting_a_server() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    let (tools, registry) = fixture_tools(dir.path());
    let error = call(
        &tools,
        json!({"action":"request","query":"fixture/ping","payload":"{not json"}),
        Access::Execute,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("payload"), "{error:#}");
    assert_eq!(started(&cwd), 0);
    finish(tools, registry);
}

#[test]
fn lsp_tool_symbols_requires_an_explicit_file() {
    let dir = tempfile::tempdir().unwrap();
    let (tools, registry) = fixture_tools(dir.path());
    let error = call(&tools, json!({"action":"symbols"}), Access::Execute).unwrap_err();
    assert!(
        format!("{error:#}").contains("requires `file`"),
        "{error:#}"
    );
    finish(tools, registry);
}

#[test]
fn lsp_tool_unknown_fields_and_zero_line_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (tools, registry) = fixture_tools(dir.path());
    assert!(
        call(
            &tools,
            json!({"action":"definition","file":"a.rs","bogus":1}),
            Access::Execute,
        )
        .is_err()
    );
    assert!(
        call(
            &tools,
            json!({"action":"definition","file":"a.rs","line":0}),
            Access::Execute,
        )
        .is_err()
    );
    assert!(
        call(
            &tools,
            json!({"action":"definition","file":"a.rs","timeout":4}),
            Access::Execute,
        )
        .is_err()
    );
    finish(tools, registry);
}

#[test]
fn lsp_tool_resolves_symbol_columns_as_utf16() {
    let text = "let value = value;\n";
    assert_eq!(resolve_column(text, 1, None).unwrap(), 0);
    assert_eq!(resolve_column(text, 1, Some("value")).unwrap(), 4);
    assert_eq!(resolve_column(text, 1, Some("value#2")).unwrap(), 12);
    // 大小写精确匹配优先，其次忽略大小写。
    assert_eq!(resolve_column(text, 1, Some("VALUE")).unwrap(), 4);
    assert!(resolve_column(text, 1, Some("missing")).is_err());
    assert!(resolve_column(text, 1, Some("value#3")).is_err());
    // 代理对按 UTF-16 计数。
    assert_eq!(resolve_column("😀 value\n", 1, Some("value")).unwrap(), 3);
    assert_eq!(resolve_column("  \n", 2, None).unwrap(), 0);
}

fn suggestion_tools(dir: &Path, extra: &[&str]) -> (Tools, Registry) {
    let cwd = canonical(dir);
    let registry = fixture_registry_with(&cwd, extra);
    let mut tools = Tools::new(cwd, EditMode::Replace).unwrap();
    tools.lsp = Some(registry.clone());
    (tools, registry)
}
#[test]
fn lsp_tool_format_suggestion_never_changes_disk_or_client_text() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "sample.rs", "old   \n\n");
    let (tools, registry) = suggestion_tools(dir.path(), &["--format", "edits"]);
    let value = call(
        &tools,
        json!({"action":"format","file":"sample.rs"}),
        Access::ReadOnly,
    )
    .unwrap();
    assert_eq!(value["ok"], true, "{value}");
    let suggestion = &value["details"]["suggestions"][0]["details"]["suggestion"];
    assert_eq!(suggestion["files"][0]["before"], "old   \n\n");
    assert_eq!(suggestion["files"][0]["after"], "old\n");
    let path = canonical(dir.path()).join("sample.rs");
    let state = registry.client("fixture").unwrap().document(&path).unwrap();
    assert_eq!(state.text, "old   \n\n");
    assert!(!state.pending_write);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), state.text);
    assert!(tools.changes_view().files.is_empty());
    finish(tools, registry);
}
#[test]
fn lsp_tool_format_null_and_malformed_have_distinct_results() {
    for (mode, ok) in [("null", true), ("malformed", false)] {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "sample.rs", "old\n");
        let (tools, registry) = suggestion_tools(dir.path(), &["--format", mode]);
        let value = call(
            &tools,
            json!({"action":"format","file":"sample.rs"}),
            Access::Execute,
        )
        .unwrap();
        assert_eq!(value["ok"], ok, "{value}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sample.rs")).unwrap(),
            "old\n"
        );
        finish(tools, registry);
    }
}
#[test]
fn lsp_tool_code_action_resolve_returns_edit_and_marks_missing_command_step() {
    for mode in ["resolve", "mixed"] {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "sample.rs", "old\n");
        let (tools, registry) = suggestion_tools(
            dir.path(),
            &["--real-navigation", "--code-action-mode", mode],
        );
        let value = call(
            &tools,
            json!({"action":"code_actions","file":"sample.rs","query":"quickfix","selection":"0"}),
            Access::ReadOnly,
        )
        .unwrap();
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["details"]["suggestion"]["files"][0]["after"], "new\n");
        assert_eq!(value["details"]["applied"], false);
        if mode == "mixed" {
            assert_eq!(value["details"]["complete"], false);
            assert!(
                value["content"]
                    .as_str()
                    .unwrap()
                    .contains("may be incomplete")
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sample.rs")).unwrap(),
            "old\n"
        );
        finish(tools, registry);
    }
}
#[test]
fn lsp_tool_rename_file_preserves_versions_and_never_moves() {
    for version in ["1", "999"] {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "sample.rs", "old\n");
        let (tools, registry) = suggestion_tools(
            dir.path(),
            &["--real-navigation", "--rename-version", version],
        );
        let value = call(
            &tools,
            json!({"action":"rename_file","file":"sample.rs","new_name":"nested/new.rs"}),
            Access::ReadOnly,
        )
        .unwrap();
        assert_eq!(value["ok"], version == "1", "{value}");
        assert_eq!(value["details"]["applied"], false);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sample.rs")).unwrap(),
            "old\n"
        );
        assert!(!dir.path().join("nested").exists());
        assert!(tools.changes_view().files.is_empty());
        if version == "1" {
            assert_eq!(
                value["details"]["suggestions"][0]["details"]["suggestion"]["workspace_edit"]["documentChanges"]
                    [0]["textDocument"]["version"],
                1
            );
        }
        finish(tools, registry);
    }
}
#[test]
fn lsp_tool_reload_recovers_from_cached_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = canonical(dir.path());
    std::fs::create_dir(cwd.join(".pp")).unwrap();
    std::fs::write(cwd.join(".pp/lsp.json"), "{").unwrap();
    let registry = Registry::new(cwd.clone(), None, LspSettings::default());
    assert!(registry.config().is_err());
    let healthy = fixture_registry(&cwd);
    drop(healthy);
    let mut tools = Tools::new(cwd, EditMode::Replace).unwrap();
    tools.lsp = Some(registry.clone());
    let value = call(&tools, json!({"action":"reload"}), Access::Execute).unwrap();
    assert_eq!(value["ok"], true, "{value}");
    assert!(registry.config().is_ok());
    finish(tools, registry);
}
#[test]
fn lsp_tool_rejects_all_execution_escape_hatches_before_startup() {
    let dir = tempfile::tempdir().unwrap();
    let (tools, registry) = fixture_tools(dir.path());
    for args in [
        json!({"action":"rename","file":"a.rs","apply":true}),
        json!({"action":"request","query":"workspace/executeCommand"}),
        json!({"action":"diagnostics","file":"*"}),
    ] {
        assert!(call(&tools, args, Access::Execute).is_err());
    }
    assert_eq!(started(dir.path()), 0);
    finish(tools, registry);
}

#[test]
fn lsp_tool_rename_file_reports_partial_server_failure_without_moving() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "sample.rs", "old\n");
    let (tools, registry) = suggestion_tools(dir.path(), &["--real-navigation"]);
    let config_path = dir.path().join(".pp/lsp.json");
    let mut config: Value = serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
    config["servers"]["broken"] = json!({"command":"python3","args":["-c","raise SystemExit(1)"],"fileTypes":[".rs"],"rootMarkers":[".pp"]});
    std::fs::write(config_path, config.to_string()).unwrap();
    registry.reload_config().unwrap();
    let result = call(
        &tools,
        json!({"action":"rename_file","file":"sample.rs","new_name":"new.rs"}),
        Access::Execute,
    )
    .unwrap();
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["details"]["complete"], false);
    assert_eq!(
        result["details"]["suggestions"].as_array().unwrap().len(),
        1
    );
    assert_eq!(result["details"]["failures"][0]["server"], "broken");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sample.rs")).unwrap(),
        "old\n"
    );
    assert!(!dir.path().join("new.rs").exists());
    finish(tools, registry);
}
#[test]
fn lsp_tool_multiserver_conflicts_are_reported_without_merging() {
    let make = |name: &str, after: &str| json!({"details":{"serverName":name,"suggestion":{"files":[{"path":"a.rs","before":"old","after":after}]}}});
    assert!(suggestion_conflicts(&[make("a", "new"), make("b", "new")]).is_empty());
    let conflicts = suggestion_conflicts(&[make("a", "new"), make("b", "other")]);
    assert_eq!(conflicts[0]["path"], "a.rs");
    assert_eq!(conflicts[0]["servers"], json!(["a", "b"]));
}
