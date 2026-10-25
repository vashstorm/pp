use super::*;

pub(super) fn install(home: &std::path::Path, folder: &str, name: &str) -> std::path::PathBuf {
    let file = config_path(home, "skills").join(folder).join("SKILL.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let body = (0..20)
        .map(|index| format!("secret-skill-line-{index:02}\n"))
        .collect::<String>();
    std::fs::write(
        &file,
        format!("---\nname: {name}\ndescription: Rust 测试指导\n---\n{body}"),
    )
    .unwrap();
    file
}
pub(super) fn send_load(request: Request, protocol: &str, name: &str) {
    if protocol == "response" {
        response_reply(
            request,
            json!([response_call("load", "load_skill", json!({"name":name}))]),
        );
    } else {
        calls_reply(
            request,
            json!([call("load", "load_skill", json!({"name":name}))]),
        );
    }
}
fn finish_request(request: Request, protocol: &str) {
    if protocol == "response" {
        response_final(request, "ok");
    } else {
        final_reply(request, "ok");
    }
}
fn loaded(body: &Value, protocol: &str) -> Value {
    if protocol == "response" {
        let item = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        response_result(item, "load")
    } else {
        let item = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| item["role"] == "tool")
            .unwrap();
        result(item, "load")
    }
}
fn system(body: &Value, protocol: &str) -> String {
    if protocol == "response" {
        body["instructions"].as_str().unwrap().into()
    } else {
        body["messages"][0]["content"].as_str().unwrap().into()
    }
}

pub(super) fn install_source(home: &std::path::Path, root: &str, name: &str) {
    let file = home.join(root).join(name).join("SKILL.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        file,
        format!("---\nname: {name}\ndescription: {name} guidance\n---\n{name} body\n"),
    )
    .unwrap();
}

#[test]
fn skills_sources_inherit_toggle_individually_and_restore_both_protocols() {
    for protocol in ["completion", "response"] {
        let path = if protocol == "response" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let server = server();
        let home = tempfile::tempdir().unwrap();
        install_source(home.path(), ".claude/skills", "claude-skill");
        install_source(home.path(), ".agents/skills", "codex-skill");
        install_source(home.path(), ".codex/skills", "legacy-skill");
        let mut cli = Cli::start_with(
            home.path(),
            None,
            "# 保留注释\nthink_level: high\n",
            &response_models(&server, Some(protocol)),
        );
        for (command, expected) in [
            ("/skills sources", [false, false, false]),
            ("/skills sources enable claude", [true, false, false]),
            ("/skills sources enable codex", [true, true, true]),
            ("/skills disable codex-skill", [true, false, true]),
            ("/skills sources disable codex", [true, false, false]),
            ("/skills sources enable codex", [true, false, true]),
        ] {
            cli.input(&format!("{command}\ncheck\n"));
            let (request, body) = receive_at(&server, path);
            let summary = system(&body, protocol);
            for (name, enabled) in ["claude-skill", "codex-skill", "legacy-skill"]
                .into_iter()
                .zip(expected)
            {
                assert_eq!(
                    summary.contains(&format!("{name} guidance")),
                    enabled,
                    "{command}: {summary}"
                );
            }
            finish_request(request, protocol);
        }
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            std::fs::read_to_string(config_path(home.path(), "settings.yml"))
                .unwrap()
                .contains("# 保留注释")
        );
        let mut cli = Cli::start_with_existing_files(home.path(), None);
        cli.input("restart\n");
        let (request, body) = receive_at(&server, path);
        let summary = system(&body, protocol);
        assert!(summary.contains("claude-skill guidance"));
        assert!(summary.contains("legacy-skill guidance"));
        assert!(!summary.contains("codex-skill guidance"));
        send_load(request, protocol, "codex-skill");
        let (request, body) = receive_at(&server, path);
        assert_eq!(loaded(&body, protocol)["ok"], false);
        finish_request(request, protocol);
        cli.input("/skills enable codex-skill\nload\n");
        let (request, body) = receive_at(&server, path);
        assert!(system(&body, protocol).contains("codex-skill guidance"));
        send_load(request, protocol, "codex-skill");
        let (request, body) = receive_at(&server, path);
        assert!(
            loaded(&body, protocol)["content"]
                .as_str()
                .unwrap()
                .contains("codex-skill body")
        );
        finish_request(request, protocol);
        cli.input("/quit\n");
        assert!(cli.finish().status.success());
        no_more(&server);
    }
}

#[test]
fn skills_missing_roots_are_silent_and_existing_roots_still_load() {
    for settings in [
        "{}",
        r#"{"skill_dirs":["missing", "skills", "also-missing"]}"#,
    ] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let has_skill = settings != "{}";
        if has_skill {
            install(home.path(), "rust", "rust");
        }
        let mut cli = Cli::start_with(home.path(), None, settings, &fixture_models(&server));
        cli.input("/skills\ncheck\n");
        let (request, body) = receive(&server);
        assert_eq!(
            system(&body, "completion").contains("Rust 测试指导"),
            has_skill
        );
        final_reply(request, "ok");
        cli.input("/quit\n");
        let output = cli.finish();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(output.stdout, b"ok\n");
        assert!(!stderr.contains("skill:"), "{stderr}");
        if !has_skill {
            assert!(!config_path(home.path(), "skills").exists());
        }
    }
}
#[test]
fn skills_load_on_demand_disable_persist_and_reset_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let file = install(home.path(), "rust", "rust");
        let path = if protocol == "response" {
            "/v1/responses"
        } else {
            "/v1/chat/completions"
        };
        let mut cli = Cli::start_with(
            home.path(),
            None,
            "# 保留配置注释\nthink_level: high\n",
            &response_models(&server, Some(protocol)),
        );
        cli.input("first\n");
        let (request, body) = receive_at(&server, path);
        assert!(system(&body, protocol).contains("Rust 测试指导"));
        assert!(!body.to_string().contains("secret-skill-line"));
        assert!(
            body["tools"].as_array().unwrap().iter().any(
                |tool| tool["name"] == "load_skill" || tool["function"]["name"] == "load_skill"
            )
        );
        send_load(request, protocol, "rust");
        let (request, body) = receive_at(&server, path);
        let output = loaded(&body, protocol);
        assert_eq!(output["ok"], true);
        assert_eq!(
            output["path"],
            file.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        assert_eq!(
            output["base_dir"],
            file.parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .as_ref()
        );
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .contains("secret-skill-line-19")
        );
        finish_request(request, protocol);
        cli.input("/skills disable rust\nsecond\n");
        let (request, body) = receive_at(&server, path);
        assert!(!system(&body, protocol).contains("Rust 测试指导"));
        assert!(body.to_string().contains("secret-skill-line-19"));
        send_load(request, protocol, "rust");
        let (request, body) = receive_at(&server, path);
        assert_eq!(loaded(&body, protocol)["ok"], false);
        finish_request(request, protocol);
        cli.input("/new\nthird\n");
        let (request, body) = receive_at(&server, path);
        assert!(!body.to_string().contains("secret-skill-line"));
        assert!(!system(&body, protocol).contains("Rust 测试指导"));
        finish_request(request, protocol);
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"ok\nok\nok\n");
        let settings = std::fs::read_to_string(config_path(home.path(), "settings.yml")).unwrap();
        assert!(settings.contains("# 保留配置注释"));
        assert!(settings.contains("think_level: high"));
        let mut cli = Cli::start_with_existing_files(home.path(), None);
        cli.input("restart\n");
        let (request, body) = receive_at(&server, path);
        assert!(!system(&body, protocol).contains("Rust 测试指导"));
        finish_request(request, protocol);
        cli.input("/quit\n");
        assert!(cli.finish().status.success());
    }
}
#[test]
fn skills_survive_protocol_switch_and_manual_enable() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    install(home.path(), "rust", "rust");
    let models = json!({"providers":{
        "a": slash_provider(&server, "a", "completion", json!({"m": {"model":"a"}})),
        "b": slash_provider(&server, "b", "response", json!({"m": {"model":"b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        home.path(),
        None,
        r#"{"disabled_skills":["rust"]}"#,
        &models,
    );
    cli.input("/skills enable rust\nfirst\n");
    let (request, _) = receive(&server);
    send_load(request, "completion", "rust");
    let (request, _) = receive(&server);
    final_reply(request, "loaded");
    cli.input("/model b/m\nsecond\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert!(system(&body, "response").contains("Rust 测试指导"));
    assert!(body.to_string().contains("secret-skill-line-19"));
    response_final(request, "switched");
    cli.input("/new\n/model a/m\nthird\n");
    let (request, body) = receive(&server);
    assert!(system(&body, "completion").contains("Rust 测试指导"));
    assert!(!body.to_string().contains("secret-skill-line"));
    final_reply(request, "reset");
    cli.input("/quit\n");
    assert_eq!(cli.finish().stdout, b"loaded\nswitched\nreset\n");
}
#[test]
fn global_configs_ignore_project_files_and_keep_tool_cwd() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"skill_dirs":[]}"#,
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    std::fs::write(
        project.path().join("settings.yml"),
        "invalid project config",
    )
    .unwrap();
    std::fs::write(project.path().join("models.yml"), "invalid project config").unwrap();
    let cli = Cli::spawn(
        project.path(),
        &["-p", "write"],
        &[("HOME", home.path().to_str().unwrap())],
    );
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call(
            "write",
            "write",
            json!({"path":"proof.txt","content":"cwd retained"})
        )]),
    );
    let (request, _) = receive(&server);
    final_reply(request, "ok");
    assert!(cli.finish().status.success());
    assert_eq!(
        std::fs::read_to_string(project.path().join("proof.txt")).unwrap(),
        "cwd retained"
    );
    assert!(!home.path().join("proof.txt").exists());
}
