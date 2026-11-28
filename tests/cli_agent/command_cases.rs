use super::*;

#[test]
fn slash_local_commands_and_unavailable_help_never_request_models() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": {
        "a": slash_provider(&a, "a", "completion", json!({
            "primary": { "model": "vendor-a"}})),
        "gateway": slash_provider(&b, "gateway", "response", json!({
            "team/secondary": { "model": "vendor-b"}}))}})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("/help\n/model\n/bogus\n/model missing/nope\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for command in ["/help", "/model", "/new", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    for value in [
        "a/primary",
        "vendor-a",
        "completion",
        "gateway/team/secondary",
        "vendor-b",
        "response",
        "model not found; use /model to list configured models",
    ] {
        assert!(stderr.contains(value), "{stderr}");
    }
    // 未知命令静默忽略，不写 stderr。
    assert!(!stderr.contains("unknown command"), "{stderr}");
    let listing = stderr.find("a/primary").unwrap();
    assert!(listing < stderr.find("gateway/team/secondary").unwrap());
    assert!(
        stderr
            .lines()
            .any(|line| line.contains('*') && line.contains("a/primary"))
    );
    assert!(!stderr.contains("a-key"));
    assert!(!stderr.contains("gateway-key"));
    no_more(&a);
    no_more(&b);

    let unavailable = tempfile::tempdir().unwrap();
    let mut cli = Cli::start_with(unavailable.path(), None, "{}", "{}");
    cli.input("/help\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for command in ["/help", "/model", "/new", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    no_more(&a);
    no_more(&b);
}
