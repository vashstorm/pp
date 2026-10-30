use super::*;

#[test]
fn auth_model_list_and_switch_support_mixed_protocols_and_legacy_cache() {
    use std::os::unix::fs::PermissionsExt;

    for provider in ["github-copilot", "command-code"] {
        let dir = tempfile::tempdir().unwrap();
        let auth_path = config_path(dir.path(), "auth.yml");
        let cache_path = config_path(dir.path(), "models-sub.yml");
        std::fs::write(&auth_path, format!("credentials:\n  {provider}:\n    generation: fixture\n    access_token: private-fixture\n")).unwrap();
        std::fs::write(&cache_path, format!("providers:\n  {provider}:\n    auth: {provider}\n    generation: fixture\n    protocol: completion\n    models:\n      response-model:\n        model: response-model\n        protocol: response\n      chat-model:\n        model: chat-model\n        protocol: completion\n      legacy-model:\n        model: legacy-model\n")).unwrap();
        for path in [&auth_path, &cache_path] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut cli = Cli::start_with_existing_files(dir.path(), None);
        cli.input(&format!("/model\n/model {provider}/response-model\n/model\n/model {provider}/legacy-model\n/model\n/quit\n"));
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(&format!("{provider}/chat-model (chat-model) [completion]")));
        assert!(stderr.contains(&format!(
            "{provider}/response-model (response-model) [response]"
        )));
        assert!(stderr.contains(&format!("* {provider}/response-model")));
        assert!(stderr.contains(&format!(
            "* {provider}/legacy-model (legacy-model) [completion]"
        )));
        assert!(!stderr.contains("error:"), "{stderr}");
        assert!(!stderr.contains("private-fixture"));
    }
}

#[test]
fn auth_old_cli_is_removed_and_legacy_credentials_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = config_path(dir.path(), "auth.json");
    std::fs::write(&path, "legacy-private-token-invalid-json").unwrap();
    for args in [
        vec!["auth", "login", "glm"],
        vec!["auth", "logout", "glm"],
        vec!["auth", "status"],
    ] {
        let output = Cli::spawn(dir.path(), &args, &[]).finish();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("legacy-private-token"));
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "legacy-private-token-invalid-json"
    );
    assert!(!config_path(dir.path(), "auth.yml").exists());
}

#[test]
fn auth_slash_line_mode_stays_local_without_model_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_pp"))
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/login\n/logout\n/login extra\n/logout extra\n/quit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("/login and /logout require the TUI"));
    assert!(stderr.contains("usage: /login"));
    assert!(stderr.contains("usage: /logout"));
    assert!(!config_path(dir.path(), "auth.yml").exists());
    assert!(!config_path(dir.path(), "models-sub.yml").exists());
}
