use super::*;
use crate::config::{SettingPatch, read_settings, update_general_setting};

#[test]
fn sandbox_settings_defaults_to_auto_and_network_disabled() {
    let settings = Settings::default();
    assert_eq!(settings.tools.sandbox.mode, SandboxMode::Auto);
    assert!(!settings.tools.sandbox.network);
}

#[test]
fn sandbox_settings_parse_and_reject_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    std::fs::write(
        &path,
        r#"
tools:
  sandbox:
    mode: workspace-write
    network: false
    writable:
      - ~/work/cache
      - /opt/tools
    denyRead:
      - ~/.netrc
    allowRead:
      - ~/.ssh/known_hosts
    protect:
      - .envrc
"#,
    )
    .unwrap();
    let settings = read_settings(&path).unwrap();
    assert_eq!(settings.tools.sandbox.mode, SandboxMode::WorkspaceWrite);
    assert!(!settings.tools.sandbox.network);
    assert_eq!(
        settings.tools.sandbox.writable,
        vec!["~/work/cache", "/opt/tools"]
    );
    assert_eq!(settings.tools.sandbox.deny_read, vec!["~/.netrc"]);
    assert_eq!(
        settings.tools.sandbox.allow_read,
        vec!["~/.ssh/known_hosts"]
    );
    assert_eq!(settings.tools.sandbox.protect, vec![".envrc"]);

    std::fs::write(
        &path,
        "tools:\n  sandbox:\n    mode: workspace-write\n    extraFlag: true\n",
    )
    .unwrap();
    assert!(read_settings(&path).is_err());
}

#[test]
fn sandbox_settings_validate_paths_table() {
    let cases = [
        ("tools:\n  sandbox:\n    writable: [relative/dir]\n", false),
        ("tools:\n  sandbox:\n    writable: [\"~/\"]\n", false),
        (
            "tools:\n  sandbox:\n    writable: [\"~/../escape\"]\n",
            false,
        ),
        ("tools:\n  sandbox:\n    denyRead: [\"  \"]\n", false),
        (
            "tools:\n  sandbox:\n    allowRead: [\"~user/secret\"]\n",
            false,
        ),
        ("tools:\n  sandbox:\n    protect: [/abs/path]\n", false),
        ("tools:\n  sandbox:\n    protect: [~/home]\n", false),
        ("tools:\n  sandbox:\n    protect: [../escape]\n", false),
        ("tools:\n  sandbox:\n    protect: [\".\"]\n", false),
        ("tools:\n  sandbox:\n    protect: [.git/hooks]\n", true),
        (
            "tools:\n  sandbox:\n    writable: [~/.cargo]\n    allowRead: [~/.ssh]\n",
            true,
        ),
    ];

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    for (yaml, valid) in cases {
        std::fs::write(&path, yaml).unwrap();
        let outcome = read_settings(&path);
        assert_eq!(outcome.is_ok(), valid, "yaml={yaml:?} -> {outcome:?}");
    }
}

#[test]
fn sandbox_mode_patch_persists_and_from_str_parses() {
    assert_eq!("auto".parse::<SandboxMode>().unwrap(), SandboxMode::Auto);
    assert_eq!(
        "workspace-write".parse::<SandboxMode>().unwrap(),
        SandboxMode::WorkspaceWrite
    );
    assert_eq!(
        "read-only".parse::<SandboxMode>().unwrap(),
        SandboxMode::ReadOnly
    );
    assert_eq!("off".parse::<SandboxMode>().unwrap(), SandboxMode::Off);
    assert!("invalid".parse::<SandboxMode>().is_err());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    std::fs::write(&path, "# keep\ntools:\n  sandbox:\n    network: true\n").unwrap();

    let saved =
        update_general_setting(&path, SettingPatch::SandboxMode(SandboxMode::ReadOnly)).unwrap();
    assert_eq!(saved.tools.sandbox.mode, SandboxMode::ReadOnly);
    assert!(saved.tools.sandbox.network);
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("# keep\n")
    );
}

#[test]
fn permission_mode_parses_new_names_yolo_alias_and_rejects_persisted_setting() {
    assert_eq!(PermissionMode::default(), PermissionMode::WorkspaceWrite);
    for (raw, expected) in [
        ("workspace-write", PermissionMode::WorkspaceWrite),
        ("auto_review", PermissionMode::AutoReview),
        ("danger-full-access", PermissionMode::DangerFullAccess),
        ("yolo", PermissionMode::DangerFullAccess),
        (" YOLO ", PermissionMode::DangerFullAccess),
        ("read-only", PermissionMode::ReadOnly),
    ] {
        assert_eq!(
            raw.parse::<PermissionMode>().unwrap(),
            expected,
            "raw={raw}"
        );
    }
    for legacy in ["always-ask", "accept-edits", "unknown"] {
        assert!(legacy.parse::<PermissionMode>().is_err(), "legacy={legacy}");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    std::fs::write(&path, "tools:\n  permissionMode: workspace-write\n").unwrap();
    let err = format!("{:#}", read_settings(&path).unwrap_err());
    assert!(err.contains("tools.permissionMode was removed"), "{err}");
}
