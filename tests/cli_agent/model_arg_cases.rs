use super::*;

// 两个 provider 共用模型 id `flash`：省略 provider 时同档内收藏优先，wire 名、
// 密钥与档位都必须跟随实际选中的 provider，而不是 catalog 里的第一个。
fn flash_catalog(server: &TestServer, fixture_thinking: Value) -> String {
    let mut fixture = json!({"model": "fixture-flash"});
    if !fixture_thinking.is_null() {
        fixture["thinking"] = fixture_thinking;
    }
    json!({"providers": {
        "fixture": slash_provider(server, "fixture", "completion", json!({"flash": fixture})),
        "gateway": slash_provider(server, "gateway", "completion", json!({"flash": {
            "model": "gateway-flash",
            "thinking": {"efforts": ["low", "high"], "defaultLevel": "low"}
        }}))
    }})
    .to_string()
}

// 旧 settings 字段在启动迁移时导入 agent.db；用它播种收藏，不读个人偏好。
fn favorite_gateway_settings() -> String {
    format!("{}favorite_models: [gateway/flash]\n", fixture_settings())
}

fn flash_home(server: &TestServer, thinking: Value) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        favorite_gateway_settings(),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        flash_catalog(server, thinking),
    )
    .unwrap();
    install_command(home.path(), "commit.md", "COMMIT-MARKER $1");
    home
}

// 启动命令的唯一一次请求：收藏 provider 的 wire 名与密钥、显式档位、展开后的模板。
fn assert_gateway_commit_request(request: &CapturedRequest, body: &Value) {
    assert_eq!(body["model"], "gateway-flash");
    assert_eq!(authorization(request), "Bearer gateway-key");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    let prompt = slash_message_text(&messages[1]);
    assert!(prompt.contains("COMMIT-MARKER"), "{prompt}");
}

#[test]
fn model_arg_selects_favorite_wire_model_and_effort_for_initial_command() {
    let server = server();
    let home = flash_home(&server, Value::Null);
    let cli = Cli::spawn(home.path(), &["--model", "flash:high", "/commit"], &[]);
    let (request, body) = receive(&server);
    assert_gateway_commit_request(&request, &body);
    assert_eq!(body["reasoning_effort"], "high");
    final_reply(request, "commit fixture reply");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Model: gateway/flash (effort high)"),
        "{stderr}"
    );
    assert_eq!(output.stdout, b"commit fixture reply\n");
    no_more(&server);
}

#[test]
fn model_arg_without_effort_uses_the_model_default_level() {
    let server = server();
    let home = flash_home(&server, Value::Null);
    let cli = Cli::spawn(home.path(), &["--model", "flash", "/commit"], &[]);
    let (request, body) = receive(&server);
    assert_gateway_commit_request(&request, &body);
    // 省略档位按普通新启动规则取模型显式默认（defaultLevel: low）。
    assert_eq!(body["reasoning_effort"], "low");
    final_reply(request, "commit default effort reply");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("Model: gateway/flash (effort low)"),
        "{stderr}"
    );
    assert_eq!(output.stdout, b"commit default effort reply\n");
    no_more(&server);
}

#[test]
fn model_arg_rejects_unknown_model_and_unsupported_effort_before_requests() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        flash_catalog(&server, json!({"efforts": ["low"]})),
    )
    .unwrap();

    let unknown = Cli::spawn(home.path(), &["--model", "nope", "-p", "x"], &[]);
    let output = unknown.finish();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no configured model matches"), "{stderr}");
    assert!(stderr.contains("nope"), "{stderr}");

    let unsupported = Cli::spawn(
        home.path(),
        &["--model", "fixture/flash:max", "-p", "x"],
        &[],
    );
    let output = unsupported.finish();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("does not support effort max"), "{stderr}");
    // 允许项来自该模型自己的 thinking 策略。
    assert!(stderr.contains("options: low"), "{stderr}");
    // 选择失败早于任何会话与请求。
    assert!(!home.path().join(".pp/agent/sessions").exists());
    no_more(&server);
}

#[test]
fn initial_command_requires_slash_prefix_and_reports_unknown_commands() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 位置参数校验先于配置与网络：普通文本不是启动命令。
    let plain = Cli::spawn(home.path(), &["fix-it"], &[]);
    let output = plain.finish();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("must start with '/'"), "{stderr}");

    // 行模式的未知启动命令必须让脚本看到非零退出码，且不发送模型请求。
    let unknown = Cli::spawn(home.path(), &["/nope"], &[]);
    let output = unknown.finish();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unknown command: /nope"), "{stderr}");
    no_more(&server);
}
