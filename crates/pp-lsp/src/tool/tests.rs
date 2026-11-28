use super::position::resolve_column;
use super::suggestion::suggestion_conflicts;
use super::*;
use crate::config;
use pp_config::config::LspSettings;
use pp_workspace::save::Workspace;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

struct TestHost {
    workspace: Workspace,
    lsp: Option<Registry>,
    owner: crate::writethrough::Owner,
}

impl TestHost {
    fn new(cwd: PathBuf, registry: Registry) -> Self {
        Self {
            workspace: Workspace::open(&cwd).unwrap(),
            lsp: Some(registry),
            owner: crate::writethrough::Owner::new(),
        }
    }
}

impl LspToolHost for TestHost {
    fn lsp_registry(&self) -> Option<&Registry> {
        self.lsp.as_ref()
    }

    fn lsp_owner_id(&self) -> &str {
        self.owner.id()
    }

    fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

fn fixture_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py")
}

/// Isolated fixture registry: matches only `.rs`, and logs stay in the temp directory for "was it started" assertions.
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

fn fixture_tools(dir: &Path) -> (TestHost, Registry) {
    let cwd = canonical(dir);
    let registry = fixture_registry(&cwd);
    let host = TestHost::new(cwd, registry.clone());
    (host, registry)
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

fn finish(host: TestHost, registry: Registry) {
    drop(host);
    cleanup(registry);
}

fn call(host: &TestHost, arguments: Value, access: Access) -> Result<Value> {
    let text = arguments.to_string();
    let mut emitted = Vec::new();
    let mut emit = |event: AgentEvent| emitted.push(event);
    run(
        host,
        &text,
        "call-1",
        access,
        &CancellationToken::new(),
        &mut emit,
    )
}

#[test]
fn lsp_tool_gates_access_tiers_validation_and_targets() {
    {
        assert_eq!(
            permission_tier(r#"{"action":"definition","file":"a.rs"}"#),
            Tier::Read
        );
        assert_eq!(
            permission_tier(r#"{"action":"hover","symbol":"x"}"#),
            Tier::Read
        );
        assert_eq!(
            permission_tier(r#"{"action":"diagnostics","file":"a.rs"}"#),
            Tier::Read
        );
        assert_eq!(
            permission_tier(r#"{"action":"diagnostics","file":"*"}"#),
            Tier::Read
        );
        assert_eq!(permission_tier(r#"{"action":"diagnostics"}"#), Tier::Write);
        assert_eq!(
            permission_tier(r#"{"action":"rename","file":"a.rs","new_name":"b","apply":false}"#),
            Tier::Read
        );
        assert_eq!(
            permission_tier(r#"{"action":"rename_file","file":"a","new_name":"b"}"#),
            Tier::Read
        );
        assert_eq!(
            permission_tier(r#"{"action":"request","query":"x"}"#),
            Tier::Write
        );
        assert_eq!(permission_tier(r#"{"action":"reload"}"#), Tier::Write);
        assert_eq!(permission_tier("not json"), Tier::Write);
        assert_eq!(permission_tier(r#"{"action":"unknown"}"#), Tier::Write);

        assert!(
            validate_access(r#"{"action":"definition","file":"a.rs"}"#, Access::ReadOnly).is_ok()
        );
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
        assert!(
            validate_access(r#"{"action":"code_actions","file":"a"}"#, Access::ReadOnly).is_ok()
        );
        assert!(
            validate_access(r#"{"action":"diagnostics","file":"*"}"#, Access::ReadOnly).is_err()
        );
        assert!(validate_access("not json", Access::ReadOnly).is_err());
        assert!(validate_access("not json", Access::Execute).is_ok());
    }

    {
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

    {
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

    {
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

    {
        let dir = tempfile::tempdir().unwrap();
        let (tools, registry) = fixture_tools(dir.path());
        let error = call(&tools, json!({"action":"symbols"}), Access::Execute).unwrap_err();
        assert!(
            format!("{error:#}").contains("requires `file`"),
            "{error:#}"
        );
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

    {
        let text = "let value = value;\n";
        assert_eq!(resolve_column(text, 1, None).unwrap(), 0);
        assert_eq!(resolve_column(text, 1, Some("value")).unwrap(), 4);
        assert_eq!(resolve_column(text, 1, Some("value#2")).unwrap(), 12);
        assert_eq!(resolve_column(text, 1, Some("VALUE")).unwrap(), 4);
        assert!(resolve_column(text, 1, Some("missing")).is_err());
        assert!(resolve_column(text, 1, Some("value#3")).is_err());
        assert_eq!(resolve_column("😀 value\n", 1, Some("value")).unwrap(), 3);
        assert_eq!(resolve_column("  \n", 2, None).unwrap(), 0);
        // When the target line has no match, candidate line numbers within ±5 lines sorted by distance are included.
        let multiline = "fn alpha() {}\n// 2\n// 3\nfn target() {}\n// 5\nfn target() {}\n";
        let near_one = resolve_column(multiline, 2, Some("alpha"))
            .unwrap_err()
            .to_string();
        assert!(
            near_one.contains("was not found on line 2") && near_one.contains("nearby line 1"),
            "{near_one}"
        );
        let near_many = resolve_column(multiline, 5, Some("target"))
            .unwrap_err()
            .to_string();
        assert!(near_many.contains("nearby lines 4, 6"), "{near_many}");
        let far = resolve_column("fn far() {}\n\n\n\n\n\n\n\n", 8, Some("far"))
            .unwrap_err()
            .to_string();
        assert!(!far.contains("nearby"), "{far}");
    }

    {
        let make = |name: &str, after: &str| json!({"details":{"serverName":name,"suggestion":{"files":[{"path":"a.rs","before":"old","after":after}]}}});
        assert!(suggestion_conflicts(&[make("a", "new"), make("b", "new")]).is_empty());
        let conflicts = suggestion_conflicts(&[make("a", "new"), make("b", "other")]);
        assert_eq!(conflicts[0]["path"], "a.rs");
        assert_eq!(conflicts[0]["servers"], json!(["a", "b"]));
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let token = CancellationToken::new();

        let nav = resolve_read_targets(
            &cwd,
            &json!({"action": "definition", "file": "src/lib.rs"}).to_string(),
            &token,
        )
        .unwrap();
        assert_eq!(nav, vec![cwd.join("src/lib.rs")]);

        let glob = resolve_read_targets(
            &cwd,
            &json!({"action": "diagnostics", "file": "src/**/*.rs"}).to_string(),
            &token,
        )
        .unwrap();
        assert_eq!(glob, vec![cwd.join("src")]);

        let rename = resolve_read_targets(
            &cwd,
            &json!({"action": "rename_file", "file": "a.rs", "new_name": "../b.rs"}).to_string(),
            &token,
        )
        .unwrap();
        assert_eq!(rename, vec![cwd.join("a.rs"), cwd.join("../b.rs")]);

        let wild = resolve_read_targets(
            &cwd,
            &json!({"action": "diagnostics", "file": "*"}).to_string(),
            &token,
        )
        .unwrap();
        assert_eq!(wild, vec![cwd.clone()]);

        let status =
            resolve_read_targets(&cwd, &json!({"action": "status"}).to_string(), &token).unwrap();
        assert!(status.is_empty());
    }
}

#[test]
fn lsp_tool_navigation_hover_and_diagnostics() {
    {
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

    {
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

    {
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

    {
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
}

fn suggestion_tools(dir: &Path, extra: &[&str]) -> (TestHost, Registry) {
    let cwd = canonical(dir);
    let registry = fixture_registry_with(&cwd, extra);
    let host = TestHost::new(cwd, registry.clone());
    (host, registry)
}

#[test]
fn lsp_tool_format_suggestions_and_reload_recovery() {
    {
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
        finish(tools, registry);
    }

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

    {
        let dir = tempfile::tempdir().unwrap();
        let cwd = canonical(dir.path());
        std::fs::create_dir(cwd.join(".pp")).unwrap();
        std::fs::write(cwd.join(".pp/lsp.json"), "{").unwrap();
        let registry = Registry::new(cwd.clone(), None, LspSettings::default());
        assert!(registry.config().is_err());
        let healthy = fixture_registry(&cwd);
        drop(healthy);
        let host = TestHost::new(cwd, registry.clone());
        let value = call(&host, json!({"action":"reload"}), Access::Execute).unwrap();
        assert_eq!(value["ok"], true, "{value}");
        assert!(registry.config().is_ok());
        finish(host, registry);
    }
}

#[test]
fn lsp_tool_code_action_resolve_and_diagnostics_context() {
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

    {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "sample.rs", "wrong\n");
        let (tools, registry) = suggestion_tools(
            dir.path(),
            &["--real-navigation", "--code-action-mode", "resolve"],
        );
        let value = call(
            &tools,
            json!({"action":"code_actions","file":"sample.rs","query":"quickfix","selection":"0"}),
            Access::ReadOnly,
        )
        .unwrap();
        assert_eq!(value["ok"], true, "{value}");

        let log = std::fs::read_to_string(dir.path().join("requests.jsonl")).unwrap();
        let code_action_req: Value = log
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|msg| msg["method"] == "textDocument/codeAction")
            .unwrap();
        assert!(
            !code_action_req["params"]["context"]["diagnostics"]
                .as_array()
                .unwrap()
                .is_empty(),
            "unopened file should still supply diagnostics context: {code_action_req}"
        );
        finish(tools, registry);
    }
}

#[test]
fn lsp_tool_rename_file_versions_and_partial_failures() {
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
        if version == "1" {
            assert_eq!(
                value["details"]["suggestions"][0]["details"]["suggestion"]["workspace_edit"]["documentChanges"]
                    [0]["textDocument"]["version"],
                1
            );
        }
        finish(tools, registry);
    }

    {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "sample.rs", "old\n");
        let (tools, registry) = suggestion_tools(dir.path(), &["--real-navigation"]);
        let config_path = dir.path().join(".pp/lsp.json");
        let mut config: Value =
            serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
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
}
