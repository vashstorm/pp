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
