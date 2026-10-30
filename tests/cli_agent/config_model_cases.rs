use super::*;

#[test]
fn yaml_model_maps_preserve_file_order_and_default_selection() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let models = format!(
        r#"providers:
  z-first:
    protocol: completion
    api_key: fixture-key
    base_url: http://{}/v1
    models:
      z-first:
        model: vendor-first
      a-second:
        model: vendor-second
  a-second:
    api_key: another-key
    models:
      primary:
        model: vendor-other
"#,
        server.server_addr()
    );
    let mut cli = Cli::start_with(home.path(), None, "{}", &models);
    cli.input("hello\n/model\n/quit\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "vendor-first");
    assert_eq!(authorization(&request), "Bearer fixture-key");
    final_reply(request, "ordered");
    let output = cli.finish();
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{error}");
    assert_eq!(output.stdout, b"ordered\n");
    let first = error.find("z-first/z-first").unwrap();
    let second = error.find("z-first/a-second").unwrap();
    let third = error.find("a-second/primary").unwrap();
    assert!(first < second && second < third, "{error}");
    no_more(&server);
}

#[test]
fn yaml_model_maps_reject_invalid_keys_duplicates_and_legacy_entries() {
    // 使用原始 YAML 保留重复键，避免测试构造时被 map 覆盖。
    let provider = "{api_key: secret-marker, models: {primary: {model: m}}}";
    let mut cases = vec![
        format!("providers:\n  fixture: {provider}\n  fixture: {provider}\n"),
        "providers: [{id: fixture, api_key: secret-marker, models: [{id: primary, model: m}]}]"
            .into(),
        format!(
            "providers: {{fixture: {provider}, unused: {{api_key: secret-marker, models: {{same: {{model: m}}, same: {{model: n}}}}}}}}"
        ),
    ];
    for key in ["''", "'  '", "null", "true", "123", "[secret-marker]"] {
        cases.push(format!("providers: {{{key}: {provider}}}"));
        cases.push(format!(
            "providers: {{fixture: {{api_key: secret-marker, models: {{{key}: {{model: m}}}}}}}}"
        ));
    }
    cases.push(format!(
        "providers: {{'secret-marker/provider': {provider}}}"
    ));
    for value in ["null", "[]", "{}", "secret-marker", "123"] {
        cases.push(format!("providers: {value}"));
        cases.push(format!(
            "providers: {{fixture: {{api_key: secret-marker, models: {value}}}}}"
        ));
    }
    for entry in [
        "{id: fixture, api_key: secret-marker, models: {primary: {model: m}}}",
        "{api_key: secret-marker, models: [{id: primary, model: m}]}",
        "{api_key: secret-marker, models: {primary: {id: primary, model: m}}}",
    ] {
        cases.push(format!("providers: {{fixture: {entry}}}"));
    }
    for models in cases {
        let home = tempfile::tempdir().unwrap();
        let output = Cli::start_with(home.path(), Some("hello"), "{}", &models).finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(!output.status.success(), "{models}");
        assert!(output.stdout.is_empty());
        assert!(error.contains("models.yml"), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
    }
}

#[test]
fn yaml_block_configs_drive_both_protocols() {
    for (protocol, path) in [
        ("completion", "/v1/chat/completions"),
        ("response", "/v1/responses"),
    ] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let models = format!(
            "# 模型配置\nproviders:\n  fixture:\n    protocol: {protocol}\n    api_key: fixture-key\n    base_url: http://{}/v1\n    models:\n      primary:\n        model: yaml-model\n        cost:\n          input: 2.0\n          cacheRead: 0.2\n          output: 8.0\n",
            server.server_addr()
        );
        let cli = Cli::start_with(
            home.path(),
            Some("hello"),
            "think_level: low # 启动档位\ndebug: false\n",
            &models,
        );
        let (request, body) = receive_at(&server, path);
        assert_eq!(body["model"], "yaml-model");
        assert_eq!(authorization(&request), "Bearer fixture-key");
        if protocol == "completion" {
            final_reply(request, "yaml loaded");
        } else {
            response_final(request, "yaml loaded");
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"yaml loaded\n");
        no_more(&server);
    }
}

#[test]
fn file_configs_override_provider_environment() {
    let server = server();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(work.path(), "settings.yml"),
        "# 应用设置\ndefault_model: chosen/primary\n",
    )
    .unwrap();
    std::fs::write(config_path(work.path(), "models.yml"), format!(
        "# 回环服务\nproviders:\n  chosen:\n    protocol: completion\n    api_key: file-key\n    base_url: http://{}/v1\n    models:\n      primary:\n        model: file-model\n",
        server.server_addr()
    ))
    .unwrap();
    let cli = Cli::spawn(
        work.path(),
        &["-p", "create file"],
        &[
            ("OPENAI_API_KEY", "wrong-environment-key"),
            ("OPENAI_MODEL", "wrong-environment-model"),
            ("OPENAI_BASE_URL", "not-a-url"),
        ],
    );
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "file-model");
    assert_eq!(authorization(&request), "Bearer file-key");
    calls_reply(
        request,
        json!([call(
            "w",
            "write",
            json!({"path":"created.txt","content":"file-config-used"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "w")["ok"], true);
    assert_eq!(
        std::fs::read_to_string(work.path().join("created.txt")).unwrap(),
        "file-config-used"
    );
    final_reply(request, "created");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"created\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("file-key"));
}

#[test]
fn missing_settings_uses_first_provider_first_model_and_defaults() {
    let unused = server();
    let server = server();
    let models = json!({
        "providers": {
            "first": {
                "protocol": "completion",
                "api_key": "first-key",
                "base_url": format!("http://{}/v1", server.server_addr()),
                "models": {
                    "primary": { "model": "first-primary"},
                    "secondary": { "model": "first-secondary"}}
            },
            "second": {
                "protocol": "completion",
                "api_key": "second-key",
                "base_url": format!("http://{}/v1", unused.server_addr()),
                "models": {"primary": { "model": "second-primary"}}
            }}
    })
    .to_string();
    let dir = tempfile::tempdir().unwrap();
    // 缺少 settings.yml 时，默认使用首个供应商的首个模型。
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
    let cli = Cli::start_with_existing_files(dir.path(), Some("hello"));
    // finish() waits for exit, so the loopback reply must come first.
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "first-primary");
    assert_eq!(authorization(&request), "Bearer first-key");
    final_reply(request, "defaults-ok");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"defaults-ok\n");
    no_more(&server);
    no_more(&unused);
}

#[test]
fn unmatched_default_model_falls_back_to_first_provider_first_model() {
    let unused = server();
    let server = server();
    let models = json!({
        "providers": {
            "first": {
                "protocol": "completion",
                "api_key": "first-key",
                "base_url": format!("http://{}/v1", server.server_addr()),
                "models": {"primary": { "model": "first-primary"}}
            },
            "second": {
                "protocol": "completion",
                "api_key": "second-key",
                "base_url": format!("http://{}/v1", unused.server_addr()),
                "models": {"primary": { "model": "second-primary"}}
            }}
    })
    .to_string();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
    std::fs::write(
        config_path(dir.path(), "settings.yml"),
        r#"{"default_model": "missing/nope"}"#,
    )
    .unwrap();
    let cli = Cli::start_with_existing_files(dir.path(), Some("hello"));
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "first-primary");
    assert_eq!(authorization(&request), "Bearer first-key");
    final_reply(request, "fallback-ok");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"fallback-ok\n");
    no_more(&server);
    no_more(&unused);
}

#[test]
fn unselected_anthropic_refuses_both_cli_modes_without_requests_or_tools() {
    let anthropic = server();
    let openai = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({
        "providers": {
            "anthropic": {
                "protocol": "anthropic",
                "api_key": "secret-marker-anthropic",
                "base_url": format!("http://{}/v1", anthropic.server_addr()),
                "models": {"primary": { "model": "anthropic-model"}}
            },
            "openai": {
                "protocol": "completion",
                "api_key": "secret-marker-openai",
                "base_url": format!("http://{}/v1", openai.server_addr()),
                "models": {"primary": { "model": "openai-model"}}
            }}
    })
    .to_string();
    for interactive in [false, true] {
        let mut cli = Cli::start_with(
            dir.path(),
            if interactive {
                None
            } else {
                Some("create must-not-exist file")
            },
            if interactive {
                "{}"
            } else {
                r#"{"default_model":"openai/primary"}"#
            },
            &models,
        );
        if interactive {
            cli.input("/new\ncreate must-not-exist file\n/quit\n");
        }
        let output = cli.finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.success(), interactive, "{error}");
        assert!(output.stdout.is_empty());
        assert!(error.contains("models.yml"), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
        assert!(!dir.path().join("must-not-exist").exists());
        no_more(&anthropic);
        no_more(&openai);
    }
}

#[test]
fn selected_anthropic_refuses_both_cli_modes_without_requests_or_tools() {
    let anthropic = server();
    let openai = server();
    let models = json!({
        "providers": {
            "anthropic": {
                "protocol": "anthropic",
                "api_key": "secret-marker-anthropic",
                "base_url": format!("http://{}/v1", anthropic.server_addr()),
                "models": {"primary": { "model": "anthropic-model"}}
            },
            "openai": {
                "protocol": "completion",
                "api_key": "secret-marker-openai",
                "base_url": format!("http://{}/v1", openai.server_addr()),
                "models": {"primary": { "model": "openai-model"}}
            }}
    })
    .to_string();
    // Missing settings must not skip the unsupported first provider.
    for settings in [None, Some(r#"{"default_model":"anthropic/primary"}"#)] {
        for interactive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(config_path(dir.path(), "models.yml"), &models).unwrap();
            if let Some(settings) = settings {
                std::fs::write(config_path(dir.path(), "settings.yml"), settings).unwrap();
            }
            let mut cli = Cli::start_with_existing_files(
                dir.path(),
                if interactive {
                    None
                } else {
                    Some("create must-not-exist file")
                },
            );
            if interactive {
                cli.input("/new\ncreate must-not-exist file\n/quit\n");
            }
            let output = cli.finish();
            let error = String::from_utf8(output.stderr).unwrap();
            assert_eq!(output.status.success(), interactive, "{error}");
            assert!(output.stdout.is_empty());
            assert!(error.contains("models.yml"), "{error}");
            assert!(!error.contains("secret-marker"), "{error}");
            assert!(!dir.path().join("must-not-exist").exists());
            no_more(&anthropic);
            no_more(&openai);
        }
    }
}
