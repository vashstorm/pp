use super::project_instructions::{finish_request, receive_for, system};
use super::streaming::{LiveCli, Pipe};
use super::*;
use std::{fs, path::Path};

pub(super) const LIVE_TEMPLATE: &str = "PROMPT_MODEL={{model}}\nPROMPT_MODE={{work_mode}}\nPROMPT_TOOLS={{#each tools}}[{{this}}]{{/each}}\n";

pub(super) fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

pub(super) fn assert_live(body: &Value, protocol: &str, mode: &str) {
    let text = system(body, protocol);
    assert!(
        text.contains(&format!(
            "PROMPT_MODEL={}\n",
            body["model"].as_str().unwrap()
        )),
        "{text}"
    );
    assert!(text.contains(&format!("PROMPT_MODE={mode}\n")), "{text}");
    let names: String = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            let name = if protocol == "completion" {
                &tool["function"]["name"]
            } else {
                &tool["name"]
            };
            format!("[{}]", name.as_str().unwrap())
        })
        .collect();
    assert!(text.contains(&format!("PROMPT_TOOLS={names}\n")), "{text}");
}

fn configure(server: &Server, cwd: &Path, protocol: &str) {
    fs::write(config_path(cwd, "settings.yml"), fixture_settings()).unwrap();
    fs::write(
        config_path(cwd, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
}

#[test]
fn system_prompt_cli_and_discovery_keep_literal_and_required_blocks_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        configure(&server, cwd.path(), protocol);
        write(cwd.path(), ".pp/agent/SYSTEM.md", "global base");
        write(cwd.path(), ".pp/agent/APPEND_SYSTEM.md", "global append");
        write(cwd.path(), ".pp/SYSTEM.md", "project base {{unknown}}");
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", "{{#if malformed}}");
        write(
            cwd.path(),
            ".pp/APPEND_SYSTEM.md",
            "project append {{unknown}}",
        );
        write(cwd.path(), "AGENTS.md", "project rule {{unknown}}");
        for (flags, base, append) in [
            (
                vec![],
                "project base {{unknown}}",
                "project append {{unknown}}",
            ),
            (
                vec![
                    "--system-prompt",
                    "CLI {{unknown}}",
                    "--append-system-prompt",
                    "CLI append",
                ],
                "CLI {{unknown}}",
                "CLI append",
            ),
            (
                vec!["--system-prompt", "", "--append-system-prompt", ""],
                "You are a coding assistant.",
                "",
            ),
        ] {
            let mut args = vec!["-p", "inspect"];
            args.extend(flags);
            let cli = Cli::spawn(cwd.path(), &args, &[]);
            let (request, body) = receive_for(&server, protocol);
            let text = system(&body, protocol);
            assert!(text.starts_with(base), "{text}");
            assert!(text.contains("project rule {{unknown}}"));
            assert!(text.contains("Each edit entry replaces a unique oldText"));
            assert!(text.contains("write/edit are atomic per call"));
            assert_eq!(
                text.matches("update AGENTS.md as part of the same task")
                    .count(),
                1
            );
            assert!(!text.contains("global base"));
            assert!(!text.contains("global append"));
            if append.is_empty() {
                assert!(!text.contains("project append"));
            } else {
                assert_eq!(text.matches(append).count(), 1);
                assert!(text.find("project rule").unwrap() < text.find(append).unwrap());
            }
            finish_request(request, protocol, "loaded");
            assert!(cli.finish().status.success());
        }
    }
}

#[test]
fn system_prompt_startup_failures_are_fatal_and_cli_conflicts_send_nothing() {
    let server = server();
    let cwd = tempfile::tempdir().unwrap();
    configure(&server, cwd.path(), "completion");
    for content in ["", "{{#if model}}", "{{unknown}}"] {
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", content);
        // 行模式也必须非零退出，不能进入“模型不可用”的交互兜底。
        let output = Cli::spawn(cwd.path(), &[], &[]).finish();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("SYSTEM_TEMPLATE.md"), "{error}");
        assert!(!error.contains("model tasks are disabled"));
        no_more(&server);
    }
    for flags in [
        vec![
            "--system-prompt",
            "text",
            "--system-prompt-template",
            "file",
        ],
        vec!["--system-prompt-template", "missing.md"],
        vec!["--system-prompt", ".pp"],
        vec![
            "-r",
            "0123456789abcdef0123456789abcdef",
            "--system-prompt",
            "text",
        ],
        vec!["-c", "--append-system-prompt", "text"],
    ] {
        let output = Cli::spawn(cwd.path(), &flags, &[]).finish();
        assert!(!output.status.success());
        no_more(&server);
    }
}

#[test]
fn system_prompt_sources_stay_fixed_across_tools_new_and_model_switch_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", LIVE_TEMPLATE);
        write(cwd.path(), ".pp/APPEND_SYSTEM.md", "fixed append {{model}}");
        let mut models: Value =
            serde_json::from_str(&response_models(&server, Some(protocol))).unwrap();
        models["providers"]["fixture"]["models"]["secondary"] = json!({"model":"second-model"});
        let mut cli = LiveCli::new(Cli::start_with(
            cwd.path(),
            None,
            fixture_settings(),
            &models.to_string(),
        ));
        cli.input("first task\n");
        let (request, body) = receive_for(&server, protocol);
        assert_live(&body, protocol, "execute");
        write(cwd.path(), ".pp/APPEND_SYSTEM.md", "changed append");
        write(cwd.path(), "AGENTS.md", "fresh project rule");
        let arguments = json!({"path":".pp/SYSTEM_TEMPLATE.md","content":"{{#if broken}}"});
        if protocol == "completion" {
            calls_reply(request, json!([call("change-source", "write", arguments)]));
        } else {
            response_reply(
                request,
                json!([response_call("change-source", "write", arguments)]),
            );
        }
        let (request, body) = receive_for(&server, protocol);
        assert_live(&body, protocol, "execute");
        let text = system(&body, protocol);
        assert!(text.contains("fresh project rule"));
        assert_eq!(text.matches("fixed append {{model}}").count(), 1);
        assert!(!text.contains("changed append"));
        finish_request(request, protocol, "first done");
        cli.wait_for(Pipe::Stdout, "first done");
        cli.input("/new\n/model fixture/secondary\nsecond task\n");
        let (request, body) = receive_for(&server, protocol);
        assert_eq!(body["model"], "second-model");
        assert_live(&body, protocol, "execute");
        assert!(system(&body, protocol).contains("fixed append {{model}}"));
        finish_request(request, protocol, "second done");
        assert!(cli.finish().status.success());
        no_more(&server);
    }
}

#[test]
fn system_prompt_resume_and_continue_use_saved_sources_and_legacy_stays_readable() {
    for protocol in ["completion", "response"] {
        let server = server();
        let cwd = tempfile::tempdir().unwrap();
        configure(&server, cwd.path(), protocol);
        write(cwd.path(), "base.md", LIVE_TEMPLATE);
        write(cwd.path(), "append.md", "saved append");
        let cli = Cli::spawn(
            cwd.path(),
            &[
                "-p",
                "initial",
                "--system-prompt-template",
                "base.md",
                "--append-system-prompt",
                "append.md",
            ],
            &[],
        );
        let (request, body) = receive_for(&server, protocol);
        assert_live(&body, protocol, "execute");
        finish_request(request, protocol, "initial done");
        assert!(cli.finish().status.success());
        let path = fs::read_dir(cwd.path().join(".pp/agent/sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let id = path.file_stem().unwrap().to_str().unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("promptSource"));
        fs::remove_file(cwd.path().join("base.md")).unwrap();
        fs::remove_file(cwd.path().join("append.md")).unwrap();
        write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", "{{#if invalid}}");
        write(cwd.path(), "AGENTS.md", "restored fresh rule");
        for args in [vec!["-r", id, "-p", "resumed"], vec!["-c", id]] {
            let cli = Cli::spawn(cwd.path(), &args, &[]);
            let (request, body) = receive_for(&server, protocol);
            assert_live(&body, protocol, "execute");
            let text = system(&body, protocol);
            assert_eq!(text.matches("saved append").count(), 1);
            assert!(text.contains("restored fresh rule"));
            finish_request(request, protocol, "restored done");
            assert!(cli.finish().status.success());
        }
        // 模拟新字段出现前的 v3 日志：只保留原 baseSystem。
        let records = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                if entry["customType"] == "pp.session_state" {
                    let data = entry["data"].as_object_mut().unwrap();
                    data.remove("promptSource");
                    data.insert("baseSystem".into(), "legacy base marker".into());
                }
                serde_json::to_string(&entry).unwrap()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&path, records).unwrap();
        let cli = Cli::spawn(cwd.path(), &["-r", id, "-p", "legacy task"], &[]);
        let (request, body) = receive_for(&server, protocol);
        let text = system(&body, protocol);
        assert!(text.starts_with("legacy base marker"));
        assert!(!text.contains("saved append"));
        assert!(text.contains("restored fresh rule"));
        finish_request(request, protocol, "legacy done");
        assert!(cli.finish().status.success());
        no_more(&server);
    }
}
