use super::*;
use std::os::unix::fs::PermissionsExt;

fn write(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn write_executable(path: &Path) {
    write(path, "#!/bin/sh\nexit 0\n");
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

/// 只包含一个自定义服务器的项目配置，避免依赖真实语言服务器二进制。
fn custom_project(dir: &Path, extra: &str) {
    write(
        &dir.join(".pp").join("lsp.json"),
        &format!(
            r#"{{"servers":{{"fake":{{"command":"fake-lsp","fileTypes":[".fake"],"rootMarkers":["fake.toml"]{extra}}}}}}}"#
        ),
    );
    write(&dir.join("fake.toml"), "");
    write(&dir.join("package.json"), "{}");
    write_executable(&dir.join("node_modules").join(".bin").join("fake-lsp"));
}

#[test]
fn lsp_builtin_defaults_parse_all_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::load_with(dir.path(), None).unwrap();
    assert_eq!(config.definitions.len(), 53);
    let names: Vec<&str> = config
        .definitions
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    // 内置顺序沿用上游 defaults.json。
    assert_eq!(names[0], "rust-analyzer");
    assert_eq!(names[1], "tlaplus");
    assert_eq!(names[names.len() - 1], "sourcekit-lsp");
    let typescript = config
        .definitions
        .iter()
        .find(|server| server.name == "typescript-language-server")
        .unwrap();
    assert_eq!(typescript.init_options["hostInfo"], "pp");
    assert!(config.servers.is_empty(), "no project marker at startup");
}

#[test]
fn lsp_project_layer_replaces_whole_settings_and_args() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.json");
    write(
        &user,
        r#"{"servers":{"rust-analyzer":{"args":["--user"],"settings":{"rust-analyzer":{"a":1}}}}}"#,
    );
    write(
        &dir.path().join(".pp").join("lsp.json"),
        r#"{"servers":{"rust-analyzer":{"args":["--project"],"settings":{"rust-analyzer":{"b":2}}}}}"#,
    );
    let config = Config::load_with(dir.path(), Some(&user)).unwrap();
    let server = config
        .definitions
        .iter()
        .find(|entry| entry.name == "rust-analyzer")
        .unwrap();
    assert_eq!(server.args, vec!["--project"]);
    // settings 整体替换，不与内置 `checkOnSave:false` 深合并。
    assert_eq!(
        server.settings,
        serde_json::json!({"rust-analyzer": {"b": 2}})
    );
}

#[test]
fn lsp_invalid_config_reports_source_and_field_without_values() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".pp").join("lsp.json"),
        r#"{"servers":{"bad":{"command":"s3cr3t-command","unknownField":true}}}"#,
    );
    let error = Config::load_with(dir.path(), None).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("lsp.json"), "{message}");
    assert!(message.contains("servers.bad.unknownField"), "{message}");
    assert!(!message.contains("s3cr3t-command"), "{message}");
}

#[test]
fn lsp_wrapped_and_flat_forms_are_both_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.json");
    write(
        &user,
        r#"{"servers":{"rust-analyzer":{"disabled":true}},"idleTimeoutMs":2500}"#,
    );
    let config = Config::load_with(dir.path(), Some(&user)).unwrap();
    assert_eq!(config.idle_timeout_ms, Some(2500));
    assert!(
        config
            .definitions
            .iter()
            .find(|entry| entry.name == "rust-analyzer")
            .unwrap()
            .disabled
    );

    write(
        &user,
        r#"{"rust-analyzer":{"disabled":true},"idleTimeoutMs":0}"#,
    );
    let config = Config::load_with(dir.path(), Some(&user)).unwrap();
    assert_eq!(config.idle_timeout_ms, Some(0));
}

#[test]
fn lsp_wrapped_form_rejects_flat_server_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.json");
    write(
        &user,
        r#"{"servers":{},"extra":{"command":"x","fileTypes":[".x"],"rootMarkers":["x"]}}"#,
    );
    let error = Config::load_with(dir.path(), Some(&user)).unwrap_err();
    assert!(format!("{error:#}").contains("unknown top-level field `extra`"));
}

#[test]
fn lsp_duplicate_keys_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.json");
    write(
        &user,
        r#"{"servers":{"rust-analyzer":{"args":[]},"rust-analyzer":{"args":[]}}}"#,
    );
    let error = Config::load_with(dir.path(), Some(&user)).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("duplicate key"), "{message}");
    assert!(message.contains("user.json"), "{message}");
}

#[test]
fn lsp_runtime_only_fields_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.json");
    write(
        &user,
        r#"{"servers":{"rust-analyzer":{"resolvedCommand":"/bin/true"}}}"#,
    );
    let error = Config::load_with(dir.path(), Some(&user)).unwrap_err();
    assert!(format!("{error:#}").contains("runtime-only field"));
}

#[test]
fn lsp_new_server_requires_complete_definition() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user.json");
    write(&user, r#"{"servers":{"mine":{"command":"mine"}}}"#);
    let error = Config::load_with(dir.path(), Some(&user)).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("servers.mine.fileTypes"), "{message}");
}

#[test]
fn lsp_discovery_requires_root_marker_and_executable() {
    let dir = tempfile::tempdir().unwrap();
    custom_project(dir.path(), "");
    let config = Config::load_with(dir.path(), None).unwrap();
    let discovered = config
        .servers
        .iter()
        .find(|server| server.name == "fake")
        .expect("custom server discovered");
    assert_eq!(
        discovered.resolved_command.as_deref(),
        Some(
            dir.path()
                .join("node_modules")
                .join(".bin")
                .join("fake-lsp")
                .as_path()
        )
    );
    // 未命中 marker 的内置服务器不会被发现。
    assert!(
        config
            .servers
            .iter()
            .all(|server| server.name != "rust-analyzer")
    );
}

#[test]
fn lsp_discovery_skips_disabled_and_missing_binary() {
    let dir = tempfile::tempdir().unwrap();
    custom_project(dir.path(), r#","disabled":true"#);
    let config = Config::load_with(dir.path(), None).unwrap();
    assert!(config.servers.is_empty(), "disabled server must not start");

    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".pp").join("lsp.json"),
        r#"{"servers":{"fake":{"command":"missing-binary-xyz","fileTypes":[".fake"],"rootMarkers":["fake.toml"]}}}"#,
    );
    write(&dir.path().join("fake.toml"), "");
    let config = Config::load_with(dir.path(), None).unwrap();
    assert!(
        config.servers.is_empty(),
        "missing binary must not be discovered"
    );
}

#[test]
fn lsp_glob_root_marker_reads_only_direct_children() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".pp").join("lsp.json"),
        r#"{"servers":{"fake":{"command":"fake-lsp","fileTypes":[".fake"],"rootMarkers":["*.fake"]}}}"#,
    );
    write(&dir.path().join("package.json"), "{}");
    write_executable(
        &dir.path()
            .join("node_modules")
            .join(".bin")
            .join("fake-lsp"),
    );
    let config = Config::load_with(dir.path(), None).unwrap();
    assert!(config.servers.is_empty(), "no matching direct child");

    write(&dir.path().join("demo.fake"), "");
    let config = Config::load_with(dir.path(), None).unwrap();
    assert_eq!(config.servers.len(), 1);

    // 只有子目录里的同名文件不算命中（不递归）。
    let nested = tempfile::tempdir().unwrap();
    write(
        &nested.path().join(".pp").join("lsp.json"),
        r#"{"servers":{"fake":{"command":"fake-lsp","fileTypes":[".fake"],"rootMarkers":["*.fake"]}}}"#,
    );
    write(&nested.path().join("package.json"), "{}");
    write_executable(
        &nested
            .path()
            .join("node_modules")
            .join(".bin")
            .join("fake-lsp"),
    );
    write(&nested.path().join("sub").join("demo.fake"), "");
    let config = Config::load_with(nested.path(), None).unwrap();
    assert!(config.servers.is_empty(), "glob markers never recurse");
}

#[test]
fn lsp_file_type_matching_prefers_non_linters_and_keeps_order() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join(".pp").join("lsp.json"),
        r#"{"servers":{
            "zeta":{"command":"fake-lsp","fileTypes":[".fake"],"rootMarkers":["fake.toml"]},
            "alpha":{"command":"fake-lsp","fileTypes":["fake"],"rootMarkers":["fake.toml"]},
            "lint":{"command":"fake-lsp","fileTypes":[".fake"],"rootMarkers":["fake.toml"],"isLinter":true}
        }}"#,
    );
    write(&dir.path().join("fake.toml"), "");
    write(&dir.path().join("package.json"), "{}");
    write_executable(
        &dir.path()
            .join("node_modules")
            .join(".bin")
            .join("fake-lsp"),
    );
    let config = Config::load_with(dir.path(), None).unwrap();
    let names: Vec<&str> = config
        .servers_for_file(&dir.path().join("demo.fake"))
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    assert_eq!(names, vec!["zeta", "alpha", "lint"]);
    // 完整 basename 也能匹配。
    write(
        &dir.path().join(".pp").join("lsp.json"),
        r#"{"servers":{"exact":{"command":"fake-lsp","fileTypes":["Makefile"],"rootMarkers":["fake.toml"]}}}"#,
    );
    let config = Config::load_with(dir.path(), None).unwrap();
    assert_eq!(
        config.servers_for_file(&dir.path().join("Makefile")).len(),
        1
    );
    assert!(
        config
            .servers_for_file(&dir.path().join("other"))
            .is_empty()
    );
}

#[test]
fn lsp_executable_symlink_launcher_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    custom_project(dir.path(), "");
    let real = dir.path().join("real-lsp");
    write_executable(&real);
    let link = dir
        .path()
        .join("node_modules")
        .join(".bin")
        .join("fake-lsp");
    fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let config = Config::load_with(dir.path(), None).unwrap();
    assert_eq!(config.servers.len(), 1);
}
