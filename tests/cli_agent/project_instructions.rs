use super::streaming::{LiveCli, Pipe};
use super::*;
use std::{fs, path::Path};

const APIS: [&str; 2] = ["completion", "response"];

pub(super) fn receive_for(server: &Server, protocol: &str) -> (Request, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}

pub(super) fn system<'a>(body: &'a Value, protocol: &str) -> &'a str {
    if protocol == "completion" {
        body["messages"][0]["content"].as_str().unwrap()
    } else {
        body["instructions"].as_str().unwrap()
    }
}

fn history<'a>(body: &'a Value, protocol: &str) -> &'a [Value] {
    body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap()
}

pub(super) fn finish_request(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn tool_call(request: Request, protocol: &str, id: &str, name: &str, args: Value) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}

fn assert_tool_ok(body: &Value, protocol: &str, id: &str) {
    let item = history(body, protocol)
        .iter()
        .find(|item| {
            item["tool_call_id"] == id
                || item["call_id"] == id && item["type"] == "function_call_output"
        })
        .unwrap();
    let result = if protocol == "completion" {
        result(item, id)
    } else {
        response_result(item, id)
    };
    assert_eq!(result["ok"], true, "{result}");
}

fn start(server: &Server, cwd: &Path, protocol: &str) -> LiveCli {
    LiveCli::new(Cli::start_with(
        cwd,
        None,
        fixture_settings(),
        &response_models(server, Some(protocol)),
    ))
}

fn journal(cwd: &Path) -> std::path::PathBuf {
    let entries: Vec<_> = fs::read_dir(cwd.join(".pp/agent/sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn agents_loads_only_startup_document_in_full_with_skill_summary_both_protocols() {
    for protocol in APIS {
        let server = server();
        let parent = tempfile::tempdir().unwrap();
        let cwd = parent.path().join("project");
        fs::create_dir_all(cwd.join("child")).unwrap();
        fs::write(parent.path().join("AGENTS.md"), "parent-rule-must-not-load").unwrap();
        fs::write(cwd.join("child/AGENTS.md"), "child-rule-must-not-load").unwrap();
        let document = format!(
            "始终保留项目规则。\n{}\n完整文档结束标记",
            "规则内容\n".repeat(10000)
        );
        fs::write(cwd.join("AGENTS.md"), &document).unwrap();
        super::skills::install(&cwd, "rust", "rust");
        let cli = Cli::start_with(
            &cwd,
            Some("inspect"),
            fixture_settings(),
            &response_models(&server, Some(protocol)),
        );
        let (request, body) = receive_for(&server, protocol);
        let system = system(&body, protocol);
        assert!(system.contains(&document));
        assert_eq!(system.matches("始终保留项目规则。").count(), 1);
        assert!(system.contains(cwd.canonicalize().unwrap().to_str().unwrap()));
        assert!(!system.contains("parent-rule-must-not-load"));
        assert!(!system.contains("child-rule-must-not-load"));
        assert!(system.contains("update AGENTS.md as part of the same task"));
        assert!(system.contains("Preserve valid project constraints"));
        assert!(system.find("完整文档结束标记").unwrap() < system.find("Rust 测试指导").unwrap());
        assert!(!system.contains("secret-skill-line"));
        finish_request(request, protocol, "loaded");
        assert!(cli.finish().status.success());
        no_more(&server);
    }
}

#[test]
fn agents_missing_or_blank_and_single_prompt_init_stays_literal_both_protocols() {
    for protocol in APIS {
        for content in [None, Some(" \n\t ")] {
            let server = server();
            let cwd = tempfile::tempdir().unwrap();
            if let Some(content) = content {
                fs::write(cwd.path().join("AGENTS.md"), content).unwrap();
            }
            let cli = Cli::start_with(
                cwd.path(),
                Some("/init"),
                fixture_settings(),
                &response_models(&server, Some(protocol)),
            );
            let (request, body) = receive_for(&server, protocol);
            assert!(!system(&body, protocol).contains("[Project instructions:"));
            assert_eq!(
                slash_message_text(history(&body, protocol).last().unwrap()),
                "/init"
            );
            finish_request(request, protocol, "literal");
            assert!(cli.finish().status.success());
            if content.is_none() {
                assert!(!cwd.path().join("AGENTS.md").exists());
            }
        }
    }
}

#[test]
fn agents_refreshes_after_tool_create_edit_and_delete_both_protocols() {
    for protocol in APIS {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        let mut cli = start(&server, cwd.path(), protocol);
        cli.input("maintain rules\n");
        let (request, body) = receive_for(&server, protocol);
        assert!(!system(&body, protocol).contains("[Project instructions:"));
        tool_call(
            request,
            protocol,
            "create",
            "write",
            json!({"path":"AGENTS.md","content":"agents-created-marker"}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "create");
        assert_eq!(
            system(&body, protocol)
                .matches("agents-created-marker")
                .count(),
            1
        );
        tool_call(
            request,
            protocol,
            "update",
            "edit",
            json!({"path":"AGENTS.md","edits":[{"oldText":"agents-created-marker","newText":"agents-updated-marker"}]}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "update");
        assert!(!system(&body, protocol).contains("agents-created-marker"));
        assert_eq!(
            system(&body, protocol)
                .matches("agents-updated-marker")
                .count(),
            1
        );
        tool_call(
            request,
            protocol,
            "delete",
            "bash",
            json!({"command":"rm AGENTS.md"}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "delete");
        assert!(!system(&body, protocol).contains("[Project instructions:"));
        let final_system = system(&body, protocol).to_owned();
        finish_request(request, protocol, "rules maintained");
        cli.input("/quit\n");
        assert!(cli.finish().status.success());
        let records = records(&journal(cwd.path()));
        let changes: Vec<_> = records
            .iter()
            .filter(|record| record["customType"] == "pp.instructions")
            .collect();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes.last().unwrap()["data"]["system"], final_system);
        let state = records
            .iter()
            .find(|record| record["customType"] == "pp.session_state")
            .unwrap();
        assert!(
            !state["data"]["baseSystem"]
                .as_str()
                .unwrap()
                .contains("agents-created-marker")
        );
    }
}

#[test]
fn agents_init_creates_then_updates_using_real_tools_both_protocols() {
    for protocol in APIS {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        fs::write(
            cwd.path().join("README.md"),
            "Project fixture uses cargo test.",
        )
        .unwrap();
        install_command(cwd.path(), "init.md", "must-not-shadow-init");
        let mut cli = start(&server, cwd.path(), protocol);
        cli.input("/init extra\n/help\n/init\n");
        let (request, body) = receive_for(&server, protocol);
        let prompt = slash_message_text(history(&body, protocol).last().unwrap());
        assert!(prompt.contains("Create or update AGENTS.md"));
        assert!(prompt.contains("preserve valid rules"));
        assert!(!prompt.contains("must-not-shadow-init"));
        tool_call(
            request,
            protocol,
            "inspect",
            "read",
            json!({"path":"README.md"}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "inspect");
        let original = "Keep valid constraint.\nOld command: cargo build\n";
        tool_call(
            request,
            protocol,
            "init-create",
            "write",
            json!({"path":"AGENTS.md","content":original}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "init-create");
        assert!(system(&body, protocol).contains(original));
        finish_request(request, protocol, "agents-created");
        cli.wait_for(Pipe::Stdout, "agents-created");
        cli.input("/init\n");
        let (request, _) = receive_for(&server, protocol);
        tool_call(
            request,
            protocol,
            "inspect-agents",
            "read",
            json!({"path":"AGENTS.md"}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "inspect-agents");
        tool_call(
            request,
            protocol,
            "init-update",
            "edit",
            json!({"path":"AGENTS.md","edits":[{"oldText":"Old command: cargo build","newText":"Test command: cargo test"}]}),
        );
        let (request, body) = receive_for(&server, protocol);
        assert_tool_ok(&body, protocol, "init-update");
        assert!(system(&body, protocol).contains("Keep valid constraint."));
        assert!(system(&body, protocol).contains("Test command: cargo test"));
        assert!(!system(&body, protocol).contains("Old command:"));
        finish_request(request, protocol, "agents-updated");
        cli.input("/quit\n");
        let output = cli.finish();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("usage: /init"));
        assert!(stderr.contains("/init — Create or update"));
        assert!(stderr.contains("command name conflicts with a built-in: /init"));
        assert_eq!(
            fs::read_to_string(cwd.path().join("AGENTS.md")).unwrap(),
            "Keep valid constraint.\nTest command: cargo test\n"
        );
        no_more(&server);
    }
}

#[cfg(unix)]
#[test]
fn agents_invalid_files_block_requests_without_disclosing_contents_both_protocols() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    for protocol in APIS {
        for kind in [
            "utf8",
            "directory",
            "symlink",
            "dangling",
            "permissions",
            "fifo",
        ] {
            if kind == "permissions" && rustix::process::geteuid().is_root() {
                continue;
            }
            let server = server();
            let cwd = tempfile::tempdir().unwrap();
            let path = cwd.path().join("AGENTS.md");
            match kind {
                "utf8" => fs::write(&path, b"secret-document-content\xff").unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                "symlink" | "dangling" => {
                    let target = cwd.path().join("target");
                    if kind == "symlink" {
                        fs::write(&target, "secret-document-content").unwrap();
                    }
                    symlink(target, &path).unwrap();
                }
                "permissions" => {
                    fs::write(&path, "secret-document-content").unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                }
                "fifo" => assert!(
                    Command::new("mkfifo")
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                ),
                _ => unreachable!(),
            }
            let cli = Cli::start_with(
                cwd.path(),
                Some("must not send"),
                fixture_settings(),
                &response_models(&server, Some(protocol)),
            );
            let output = cli.finish();
            assert!(!output.status.success(), "{protocol}: {kind}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("AGENTS.md"), "{stderr}");
            assert!(!stderr.contains("secret-document-content"));
            no_more(&server);
            if kind == "permissions" {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
    }
}

#[test]
fn agents_load_errors_recover_in_same_process_and_block_after_tools_both_protocols() {
    for protocol in APIS {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("AGENTS.md");
        fs::write(&path, b"invalid\xff").unwrap();
        let mut cli = start(&server, cwd.path(), protocol);
        cli.input("rejected-before-history\n");
        cli.wait_for(Pipe::Stderr, "as UTF-8");
        no_more(&server);
        fs::write(&path, "recovered-rule").unwrap();
        cli.input("accepted-after-repair\n");
        let (request, body) = receive_for(&server, protocol);
        assert!(system(&body, protocol).contains("recovered-rule"));
        assert!(!body.to_string().contains("rejected-before-history"));
        tool_call(
            request,
            protocol,
            "invalidate",
            "bash",
            json!({"command":"rm AGENTS.md && mkdir AGENTS.md"}),
        );
        cli.wait_for(Pipe::Stderr, "AGENTS.md must be a regular file");
        no_more(&server);
        fs::remove_dir(&path).unwrap();
        fs::write(&path, "latest-rule-after-second-repair").unwrap();
        cli.input("retry-with-completed-tools\n");
        let (request, body) = receive_for(&server, protocol);
        assert!(system(&body, protocol).contains("latest-rule-after-second-repair"));
        assert!(!system(&body, protocol).contains("recovered-rule"));
        assert_tool_ok(&body, protocol, "invalidate");
        finish_request(request, protocol, "recovered");
        cli.input("/quit\n");
        assert!(cli.finish().status.success());
    }
}

#[test]
fn agents_new_and_protocol_switch_use_current_rules() {
    let server = server();
    let cwd = tempfile::tempdir().unwrap();
    let models = json!({"providers":{
        "a": slash_provider(&server, "a", "completion", json!({"m": {"model":"a"}})),
        "b": slash_provider(&server, "b", "response", json!({"m": {"model":"b"}}))}})
    .to_string();
    let mut cli = LiveCli::new(Cli::start_with(cwd.path(), None, "{}", &models));
    fs::write(cwd.path().join("AGENTS.md"), "first-project-rule").unwrap();
    cli.input("first-task\n");
    let (request, body) = receive_for(&server, "completion");
    assert!(system(&body, "completion").contains("first-project-rule"));
    final_reply(request, "first-done");
    cli.wait_for(Pipe::Stdout, "first-done");
    for (command, protocol, marker) in [
        ("/model b/m", "response", "second-project-rule"),
        ("/model a/m", "completion", "third-project-rule"),
        ("/new", "completion", "fourth-project-rule"),
    ] {
        fs::write(cwd.path().join("AGENTS.md"), marker).unwrap();
        cli.input(&format!("{command}\n{marker}-task\n"));
        let (request, body) = receive_for(&server, protocol);
        assert_eq!(system(&body, protocol).matches(marker).count(), 1);
        assert!(!system(&body, protocol).contains("first-project-rule"));
        if command == "/new" {
            assert!(!body.to_string().contains("first-task"));
        }
        finish_request(request, protocol, &format!("{marker}-done"));
        cli.wait_for(Pipe::Stdout, &format!("{marker}-done"));
    }
    cli.input("/quit\n");
    assert!(cli.finish().status.success());
}

#[test]
fn agents_resume_and_continue_refresh_from_restored_cwd_and_journal_both_protocols() {
    for protocol in APIS {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        let path = cwd.path().join("AGENTS.md");
        fs::write(&path, "saved-project-rule").unwrap();
        let cli = Cli::start_with(
            cwd.path(),
            Some("saved-task"),
            fixture_settings(),
            &response_models(&server, Some(protocol)),
        );
        let (request, _) = receive_for(&server, protocol);
        finish_request(request, protocol, "saved-answer");
        assert!(cli.finish().status.success());
        let file = journal(cwd.path());
        let id = file.file_stem().unwrap().to_str().unwrap();
        let launch = tempfile::tempdir().unwrap();
        fs::write(
            launch.path().join("AGENTS.md"),
            "wrong-launch-directory-rule",
        )
        .unwrap();
        for (args, marker) in [
            (vec!["-r", id, "-p", "resumed-task"], "current-resumed-rule"),
            (vec!["-c", id], "current-continued-rule"),
        ] {
            fs::write(&path, marker).unwrap();
            let cli = Cli::spawn(
                launch.path(),
                &args,
                &[("HOME", cwd.path().to_str().unwrap())],
            );
            let (request, body) = receive_for(&server, protocol);
            assert!(system(&body, protocol).contains(marker));
            assert!(!system(&body, protocol).contains("saved-project-rule"));
            assert!(!system(&body, protocol).contains("wrong-launch-directory-rule"));
            assert!(body.to_string().contains("saved-task"));
            let sent_system = system(&body, protocol).to_owned();
            finish_request(request, protocol, "resume-done");
            assert!(cli.finish().status.success());
            assert_eq!(journal(cwd.path()), file);
            let records = records(&file);
            let update = records
                .iter()
                .rev()
                .find(|record| record["customType"] == "pp.instructions")
                .unwrap();
            assert_eq!(update["data"]["system"], sent_system);
        }
    }
}

#[test]
fn agents_manual_and_auto_compaction_refresh_before_summary_and_followup_both_protocols() {
    for protocol in APIS {
        for automatic in [false, true] {
            let server = server();
            let cwd = tempfile::tempdir().unwrap();
            let mut models: Value =
                serde_json::from_str(&response_models(&server, Some(protocol))).unwrap();
            models["providers"]["fixture"]["models"]["primary"]["context_window"] = json!(80000);
            let settings =
                json!({"auto_compact":automatic,"auto_compact_threshold_percent":50}).to_string();
            let mut cli = LiveCli::new(Cli::start_with(
                cwd.path(),
                None,
                &settings,
                &models.to_string(),
            ));
            cli.input("old-task\n");
            let (request, _) = receive_for(&server, protocol);
            finish_request(request, protocol, &"old-material ".repeat(5000));
            cli.wait_for(Pipe::Stdout, "old-material");
            cli.input("recent-task\n");
            let (request, _) = receive_for(&server, protocol);
            finish_request(request, protocol, "recent-answer");
            cli.wait_for(Pipe::Stdout, "recent-answer");
            fs::write(
                cwd.path().join("AGENTS.md"),
                format!("summary-project-rule\n{}", "project-fact ".repeat(5000)),
            )
            .unwrap();
            cli.input(if automatic {
                "after-automatic-compaction\n"
            } else {
                "/compact\n"
            });
            let (request, body) = receive_for(&server, protocol);
            assert!(system(&body, protocol).contains("summary-project-rule"));
            assert!(body.get("tools").is_none_or(Value::is_null));
            fs::write(cwd.path().join("AGENTS.md"), "followup-project-rule").unwrap();
            finish_request(request, protocol, "compact-summary");
            if !automatic {
                cli.wait_for(Pipe::Stderr, "Context compacted:");
                cli.input("after-manual-compaction\n");
            }
            let (request, body) = receive_for(&server, protocol);
            assert!(system(&body, protocol).contains("followup-project-rule"));
            assert!(!system(&body, protocol).contains("summary-project-rule"));
            assert!(body.to_string().contains("[Context summary]"));
            assert!(body.to_string().contains("recent-answer"));
            finish_request(request, protocol, "compaction-done");
            cli.input("/quit\n");
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("compact-summary"));
        }
    }
}

#[test]
fn agents_growing_during_auto_summary_rechecks_capacity_before_followup_both_protocols() {
    for protocol in APIS {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        let mut models: Value =
            serde_json::from_str(&response_models(&server, Some(protocol))).unwrap();
        models["providers"]["fixture"]["models"]["primary"]["context_window"] = json!(80000);
        let mut cli = LiveCli::new(Cli::start_with(
            cwd.path(),
            None,
            r#"{"auto_compact_threshold_percent":50}"#,
            &models.to_string(),
        ));
        cli.input("old-task\n");
        let (request, _) = receive_for(&server, protocol);
        finish_request(request, protocol, &"old-material ".repeat(5000));
        cli.wait_for(Pipe::Stdout, "old-material");
        cli.input("recent-task\n");
        let (request, _) = receive_for(&server, protocol);
        finish_request(request, protocol, "recent-answer");
        cli.wait_for(Pipe::Stdout, "recent-answer");
        fs::write(cwd.path().join("AGENTS.md"), "project-fact ".repeat(5000)).unwrap();
        cli.input("must-recheck-after-summary\n");
        let (request, body) = receive_for(&server, protocol);
        assert!(body.get("tools").is_none_or(Value::is_null));
        fs::write(
            cwd.path().join("AGENTS.md"),
            "oversized-project-rule ".repeat(20000),
        )
        .unwrap();
        finish_request(request, protocol, "compact-summary");
        cli.wait_for(Pipe::Stderr, "error:");
        no_more(&server);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("compact-summary"));
    }
}
