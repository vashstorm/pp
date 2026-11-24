use super::*;

#[test]
fn off_mode_skips_the_platform_probe() {
    let dir = tempfile::tempdir().unwrap();

    let mut probes = 0;
    let off = Sandbox::build_with_probe(
        SandboxSettings {
            mode: SandboxMode::Off,
            ..Default::default()
        },
        dir.path(),
        || {
            probes += 1;
            ProbeResult::Unavailable {
                reason: "off mode must not probe".into(),
            }
        },
    );
    assert_eq!(probes, 0, "mode=off 不得调用平台探测");
    let status = off.status();
    assert_eq!(status.configured, SandboxMode::Off);
    assert_eq!(status.effective, EffectiveMode::Off);
    assert_eq!(status.degraded_reason, None);
    assert!(off.policy().unwrap().is_none());

    // 同一个探测入口在启用模式下必须恰好调用一次：跳过逻辑不能变成探测整体失效。
    let mut probes = 0;
    let enabled = Sandbox::build_with_probe(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        dir.path(),
        || {
            probes += 1;
            ProbeResult::Available
        },
    );
    assert_eq!(probes, 1);
    assert_eq!(enabled.status().effective, EffectiveMode::WorkspaceWrite);
}

#[test]
fn probe_degradation_differs_between_auto_and_explicit_modes() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let unavailable = ProbeResult::Unavailable {
        reason: "nested sandbox".into(),
    };

    let auto = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::Auto,
            ..Default::default()
        },
        dir.path(),
        Some(&home),
        None,
        unavailable.clone(),
    );
    assert!(auto.policy().unwrap().is_none());
    let st = auto.status();
    assert_eq!(st.effective, EffectiveMode::Off);
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        assert_eq!(st.degraded_reason.as_deref(), Some("nested sandbox"));
    }

    for explicit in [SandboxMode::WorkspaceWrite, SandboxMode::ReadOnly] {
        let sbx = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: explicit,
                ..Default::default()
            },
            dir.path(),
            Some(&home),
            None,
            unavailable.clone(),
        );
        assert!(
            sbx.policy().is_err(),
            "explicit mode {explicit:?} must fail closed"
        );
    }
}

#[test]
fn policy_sets_expected_defaults_for_workspace_write_and_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let cwd = root.join("repo");
    let home = PathBuf::from("/Users/__pp_sandbox_test_home__");
    std::fs::create_dir_all(&cwd).unwrap();

    let sbx = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            allow_read: vec!["~/.ssh/known_hosts".into()],
            protect: vec![".envrc".into()],
            ..Default::default()
        },
        &cwd,
        Some(&home),
        Some(Path::new("/tmp")),
        ProbeResult::Available,
    );
    let policy = sbx.policy().unwrap().unwrap();
    assert_eq!(policy.mode(), EffectiveMode::WorkspaceWrite);
    assert!(policy.check_write(&cwd.join("src/a.rs")).is_ok());
    assert!(
        policy
            .check_write(&PathBuf::from("/tmp/sbx_test_file"))
            .is_ok()
    );
    assert!(policy.check_write(&cwd.join(".pp/hooks.yml")).is_err());
    assert!(policy.check_write(&cwd.join(".envrc")).is_err());
    assert!(
        policy
            .check_write(&cwd.join(".git/hooks/pre-commit"))
            .is_err()
    );
    assert!(policy.check_write(&home.join(".zshrc")).is_err());
    assert!(policy.check_read(&home.join(".ssh/id_rsa")).is_err());
    assert!(policy.check_read(&home.join(".ssh/known_hosts")).is_ok());
    assert!(
        policy
            .check_read(&home.join(".pp/agent/models.yml"))
            .is_err()
    );
    assert!(
        policy
            .check_read(&home.join(".pp/agent/skills/x/SKILL.md"))
            .is_ok()
    );

    sbx.set_mode_override(Some(SandboxMode::ReadOnly));
    let ro = sbx.policy().unwrap().unwrap();
    assert_eq!(ro.mode(), EffectiveMode::ReadOnly);
    assert!(ro.check_write(&cwd.join("src/a.rs")).is_err());
    assert!(ro.check_write(&cwd.join(".pp-cache/x")).is_ok());
    let ro_scratch = ro.with_extra_writable(&cwd.join(".pp-tmp-123"));
    assert!(ro_scratch.check_write(&cwd.join(".pp-tmp-123/out")).is_ok());
}

#[test]
fn sandbox_lexical_fast_path_skips_canonicalize_for_disjoint_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();

    let sbx = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        &cwd,
        Some(&home),
        Some(&root),
        ProbeResult::Available,
    );
    let policy = sbx.policy().unwrap().unwrap();

    crate::guard::reset_canonicalize_calls();
    assert!(!policy.is_read_denied(&cwd.join("src/main.rs")));
    assert_eq!(
        crate::guard::canonicalize_calls(),
        0,
        "与拒绝根不相交的绝对路径不得触发 canonicalize"
    );

    // 词法上可能命中（拒绝根后代）时必须回退到规范化，且判定仍为拒绝。
    crate::guard::reset_canonicalize_calls();
    assert!(policy.is_read_denied(&home.join(".ssh/id_rsa")));
    assert!(
        crate::guard::canonicalize_calls() > 0,
        "可能命中时必须走 normalize_path"
    );
}

#[test]
fn sandbox_resolved_denial_keeps_symlink_and_tmp_alias_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();

    let alias_root = std::env::temp_dir().join("pp-sandbox-lexical-alias");
    let sbx = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            deny_read: vec![alias_root.to_string_lossy().into_owned()],
            ..Default::default()
        },
        &cwd,
        Some(&home),
        Some(&root),
        ProbeResult::Available,
    );
    let policy = sbx.policy().unwrap().unwrap();

    let link = cwd.join("secret-link");
    std::os::unix::fs::symlink(home.join(".ssh"), &link).unwrap();
    let dangling = cwd.join("dangling-link");
    std::os::unix::fs::symlink(home.join(".ssh/id_rsa"), &dangling).unwrap();
    let relative = cwd.join("relative-link");
    std::os::unix::fs::symlink("../home/.ssh", &relative).unwrap();
    assert!(policy.is_read_denied_resolved(&link));
    assert!(policy.is_read_denied_resolved(&dangling));
    assert!(policy.is_read_denied_resolved(&relative));
    assert!(!policy.is_read_denied_resolved(&cwd.join("src/main.rs")));

    // 系统别名（macOS `/tmp` ↔ `/private/tmp`、TMPDIR 的 `/var` ↔ `/private/var`）
    // 不得因词法不相交而被放行。
    assert!(policy.is_read_denied(&alias_root.join("id_rsa")));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_guards_match_case_insensitively_across_protected_and_denied_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let cwd = root.join("repo");
    let home = root.join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), "").unwrap();

    let sbx = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            ..Default::default()
        },
        &cwd,
        Some(&home),
        Some(&root),
        ProbeResult::Available,
    );
    let policy = sbx.policy().unwrap().unwrap();

    // 未创建的文件通过大写变体也不得绕过 protect
    assert!(policy.check_write(&cwd.join(".MCP.JSON")).is_err());
    assert!(policy.check_write(&cwd.join(".PP/hooks.yml")).is_err());

    // 读拒绝词法快路径不得因大小写差异跳过规范化放行
    assert!(policy.is_read_denied(&home.join(".SSH/id_ed25519")));
    assert!(policy.check_read(&home.join(".SSH/id_ed25519")).is_err());
    assert!(policy.can_contain_denied_read(&home.join(".SSH")));
}

#[test]
fn command_sandbox_grant_only_extends_requested_paths_and_network() {
    #[cfg(unix)]
    let fixture = tempfile::tempdir_in("/tmp").unwrap();
    #[cfg(not(unix))]
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let work = root.join("work");
    let approved = root.join("approved");
    let unrelated = root.join("unrelated");
    let protected = root.join("protected");
    for dir in [&work, &approved, &unrelated, &protected] {
        std::fs::create_dir(dir).unwrap();
    }
    let sandbox = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::WorkspaceWrite,
            network: false,
            ..Default::default()
        },
        &work,
        None,
        None,
        ProbeResult::Available,
    );
    let mut base = (*sandbox.policy().unwrap().unwrap()).clone();
    base.writable = vec![work];
    base.protect.push(protected.clone());
    assert!(base.resolve_extra_writable(&[root]).is_err());
    assert!(
        base.resolve_extra_writable(std::slice::from_ref(&protected))
            .is_err()
    );
    let request = base
        .resolve_extra_writable(std::slice::from_ref(&approved))
        .unwrap();
    assert!(!request.is_empty());
    let granted = base.with_command_grant(&request, true).unwrap();
    assert!(granted.check_write(&approved.join("output")).is_ok());
    assert!(granted.check_write(&unrelated.join("output")).is_err());
    assert!(granted.check_write(&protected.join("output")).is_err());
    assert!(granted.network());
    assert!(!base.network());
    assert!(base.check_write(&approved.join("output")).is_err());
}

#[test]
fn for_cwd_shares_mode_override_and_configured_mode_with_parent() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let parent_cwd = root.join("parent");
    let child_cwd = root.join("child");
    std::fs::create_dir_all(&parent_cwd).unwrap();
    std::fs::create_dir_all(&child_cwd).unwrap();
    let parent = Sandbox::with_probe_for_test(
        SandboxSettings {
            mode: SandboxMode::Off,
            ..Default::default()
        },
        &parent_cwd,
        None,
        None,
        ProbeResult::Available,
    );
    parent.set_mode_override(Some(SandboxMode::WorkspaceWrite));
    let child = parent.for_cwd(&child_cwd);
    assert_eq!(child.status().effective, EffectiveMode::WorkspaceWrite);
    assert_eq!(child.policy().unwrap().unwrap().cwd(), child_cwd);

    // `/setting` 保存新配置会清除共享覆盖并更新配置模式，已有子 Agent 必须跟随新配置而非退回 off。
    parent.update_settings_mode(SandboxMode::ReadOnly);
    assert_eq!(parent.status().effective, EffectiveMode::ReadOnly);
    assert_eq!(child.status().effective, EffectiveMode::ReadOnly);
    assert_eq!(child.policy().unwrap().unwrap().cwd(), child_cwd);

    parent.set_mode_override(Some(SandboxMode::Off));
    assert_eq!(child.status().effective, EffectiveMode::Off);
    parent.set_mode_override(None);
    assert_eq!(child.status().effective, EffectiveMode::ReadOnly);
}
