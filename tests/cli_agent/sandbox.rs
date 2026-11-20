use std::path::Path;

use super::streaming::{LiveCli, Pipe};
use super::*;

fn runtime_sandbox_available(cwd: &Path) -> bool {
    pp_sandbox::Sandbox::new(pp_config::config::SandboxSettings::default(), cwd)
        .status()
        .effective
        != pp_sandbox::EffectiveMode::Off
}

#[test]
fn sandbox_default_blocks_protected_write_and_flag_off_allows_it() {
    // 默认配置（不显式写 tools.sandbox）在平台可用时启用 workspace-write；
    // 验证受保护的 .pp/ 目录拒绝写入，且 --sandbox off 可显式关闭。
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sandbox_active = runtime_sandbox_available(&root);
    write_settings(&root, "rate_limit_retry_budget_seconds: 0\n").unwrap();
    std::fs::write(config_path(&root, "models.yml"), fixture_models(&server)).unwrap();
    std::fs::create_dir_all(root.join(".pp")).unwrap();

    let blocked_target = root.join(".pp/blocked.txt");
    let cli = Cli::spawn(&root, &["-p", "try protected write"], &[]);
    let (request, body) = receive_at(&server, "/v1/chat/completions");
    let system_prompt = body["messages"][0]["content"].as_str().unwrap_or("");
    if sandbox_active {
        assert!(
            system_prompt.contains("workspace-write") && system_prompt.contains(".pp"),
            "system prompt should reflect active sandbox policy: {system_prompt}"
        );
    }
    calls_reply(
        request,
        json!([call(
            "sbx-write-1",
            "bash",
            json!({"command": "touch .pp/blocked.txt"})
        )]),
    );
    let (request, body) = receive_at(&server, "/v1/chat/completions");
    let tool_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["role"] == "tool")
        .unwrap();
    let tool_res: Value = serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    if sandbox_active {
        assert_eq!(tool_res["ok"], false, "{tool_res}");
        assert_eq!(tool_res["sandbox"]["mode"], "workspace-write", "{tool_res}");
        assert!(!blocked_target.exists(), "blocked file must not be created");
    }
    final_reply(request, "done-default");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    no_more(&server);

    // 显式 --sandbox off 关闭沙盒后，同一受保护路径写入应成功。
    let allowed_target = root.join(".pp/allowed.txt");
    let cli = Cli::spawn(
        &root,
        &["--sandbox", "off", "-p", "allow protected write"],
        &[],
    );
    let (request, _) = receive_at(&server, "/v1/chat/completions");
    calls_reply(
        request,
        json!([call(
            "sbx-write-2",
            "bash",
            json!({"command": "printf allowed > .pp/allowed.txt"})
        )]),
    );
    let (request, body) = receive_at(&server, "/v1/chat/completions");
    let tool_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["role"] == "tool")
        .unwrap();
    let tool_res: Value = serde_json::from_str(tool_msg["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_res["ok"], true, "{tool_res}");
    assert_eq!(std::fs::read_to_string(&allowed_target).unwrap(), "allowed");
    final_reply(request, "done-off");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    no_more(&server);
}

#[test]
fn sandbox_line_mode_slash_command_inspects_and_overrides_runtime_mode() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sandbox_active = runtime_sandbox_available(&root);
    let mut cli = LiveCli::new(Cli::start_with(
        &root,
        None,
        "rate_limit_retry_budget_seconds: 0\ntools: {approvalMode: yolo}\n",
        &fixture_models(&server),
    ));

    cli.input("/sandbox\n");
    if sandbox_active {
        cli.wait_for(
            Pipe::Stderr,
            "Sandbox: effective=workspace-write, configured=auto",
        );
        cli.input("/sandbox read-only\n");
        cli.wait_for(
            Pipe::Stderr,
            "Sandbox: effective=read-only, configured=read-only",
        );
    } else {
        cli.wait_for(Pipe::Stderr, "Sandbox: effective=off, configured=auto");
        cli.input("/sandbox read-only\n");
        cli.wait_for(Pipe::Stderr, "error:");
    }

    cli.input("/sandbox off\n");
    cli.wait_for(Pipe::Stderr, "Sandbox: effective=off, configured=off");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    no_more(&server);
}
