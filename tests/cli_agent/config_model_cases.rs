use super::*;

#[test]
fn yaml_block_configs_drive_both_protocols() {
    for (protocol, path) in [
        ("completion", "/v1/chat/completions"),
        ("response", "/v1/responses"),
    ] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let models = format!(
            "# Model configuration\nextra: {{version: 1}}\nproviders:\n  fixture:\n    id: ignored-provider-id\n    protocol: {protocol}\n    api_key: fixture-key\n    base_url: http://{}/v1\n    models:\n      primary:\n        id: ignored-model-id\n        api_key: ignored-model-key\n        base_url: http://127.0.0.1:0/ignored\n        protocol: ignored\n        model: yaml-model\n        cost:\n          input: 2.0\n          cacheRead: 0.2\n          output: 8.0\n",
            server.addr()
        );
        let cli = Cli::start_with(
            home.path(),
            Some("hello"),
            r#"
think_level: low # startup level
debug: false
rate_limit_retry_budget_seconds: 0
extra: [1, 2]
hooks: {Stop: []}
tui:
  cost: {currency: EUR, exchange_rate: 1}
tools:
  permissionMode: danger-full-access
  extra: !future {nested: [1, 2]}
lsp:
  formatOnWrite: true
  shared: true
"#,
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
fn selected_anthropic_refuses_both_cli_modes_without_requests_or_tools() {
    let anthropic = server();
    let openai = server();
    let models = json!({
        "providers": {
            "anthropic": {
                "protocol": "anthropic",
                "api_key": "secret-marker-anthropic",
                "base_url": format!("http://{}/v1", anthropic.addr()),
                "models": {"primary": { "model": "anthropic-model"}}
            },
            "openai": {
                "protocol": "completion",
                "api_key": "secret-marker-openai",
                "base_url": format!("http://{}/v1", openai.addr()),
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
