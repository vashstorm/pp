use super::*;

#[test]
fn permission_cli_defaults_to_workspace_write_and_supports_yolo_flag() {
    let home = tempfile::tempdir().unwrap();
    write_settings(
        home.path(),
        "tools: { sandbox: { mode: auto, network: false } }",
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        "providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:9/v1\n    models:\n      primary:\n        model: fixture-model\n",
    )
    .unwrap();

    // 1. 默认启动为 Ask for approval (workspace-write)，且不持久化旧 preset。
    let default_out = Cli::spawn(home.path(), &["/permissions"], &[]).finish();
    assert!(default_out.status.success(), "{default_out:?}");
    let default_err = String::from_utf8_lossy(&default_out.stderr);
    assert!(
        default_err.contains("Permission mode: Ask for approval (workspace-write)"),
        "{default_err}"
    );
    assert!(
        default_err.contains("network=off (loopback only)")
            || default_err.contains("network=isolated"),
        "{default_err}"
    );

    // 2. `--yolo` 与 `--permission-mode danger-full-access` 均切到 Full Access (danger-full-access) 并关闭沙盒。
    for args in [
        vec!["--yolo", "/permissions"],
        vec!["--permission-mode", "danger-full-access", "/permissions"],
    ] {
        let output = Cli::spawn(home.path(), &args, &[]).finish();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Permission mode: Full Access (danger-full-access)"),
            "{stderr}"
        );
        assert!(stderr.contains("effective=off, configured=off"), "{stderr}");
        assert!(stderr.contains("network=unrestricted"), "{stderr}");
    }
    assert!(!config_path(home.path(), "permission.toml").exists());
}
