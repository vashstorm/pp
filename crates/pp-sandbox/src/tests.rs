use super::*;

#[test]
fn approved_session_read_off_and_probe_degradation() {
    {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let agent = home.join(".pp/agent");
        let sessions = agent.join("sessions");
        let target = sessions.join("run.subagents");
        let restricted = target.join("private");
        let sibling = sessions.join("other.jsonl");
        let credentials = agent.join("models.yml");
        std::fs::create_dir_all(&restricted).unwrap();
        std::fs::write(&sibling, "other").unwrap();
        std::fs::write(&credentials, "fixture").unwrap();
        let policy = |deny_read| {
            Sandbox::with_probe_for_test(
                SandboxSettings {
                    mode: SandboxMode::WorkspaceWrite,
                    deny_read,
                    ..Default::default()
                },
                &home,
                Some(&home),
                Some(&home),
                ProbeResult::Available,
            )
            .policy()
            .unwrap()
            .unwrap()
        };

        let base = policy(vec![]);
        assert!(base.check_read(&target).is_err());
        assert!(base.session_read_target(&credentials).is_none());
        #[cfg(unix)]
        {
            let link = sessions.join("linked-models.yml");
            std::os::unix::fs::symlink(&credentials, &link).unwrap();
            assert!(base.session_read_target(&link).is_none());
        }
        let approved = base.session_read_target(&target).unwrap();
        let scoped = base.with_approved_session_reads(&[approved]).unwrap();
        assert!(scoped.check_read(&restricted).is_ok());
        assert!(scoped.check_write(&target.join("new.jsonl")).is_err());
        assert!(scoped.check_read(&sibling).is_err());
        assert!(scoped.check_read(&credentials).is_err());
        assert!(base.check_read(&target).is_err());

        for denied in [agent, restricted] {
            let base = policy(vec![denied.to_string_lossy().into_owned()]);
            assert!(base.session_read_target(&target).is_none());
            assert!(
                base.with_approved_session_reads(std::slice::from_ref(&target))
                    .is_err()
            );
        }
    }
    {
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
    {
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
}

#[test]
fn policy_defaults_lexical_fast_path_and_resolved_denials() {
    {
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
        assert!(policy.check_write(&cwd.join(".git/index")).is_err());
        assert!(policy.check_write(&cwd.join(".agents/skill.md")).is_err());
        assert!(!policy.network());
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
    {
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
    {
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
fn command_grants_cwd_inheritance_and_scoped_protections() {
    {
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
    {
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
    {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cwd = root.join("repo");
        let home = root.join("home");
        let worktree_gitdir = root.join("shared-git/worktrees/wt1");
        std::fs::create_dir_all(cwd.join(".agents")).unwrap();
        std::fs::create_dir_all(home.join(".pp/agent")).unwrap();
        std::fs::create_dir_all(&worktree_gitdir).unwrap();
        std::fs::write(
            cwd.join(".git"),
            format!("gitdir: {}\n", worktree_gitdir.display()),
        )
        .unwrap();

        let sbx = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                ..Default::default()
            },
            &cwd,
            Some(&home),
            Some(Path::new("/tmp")),
            ProbeResult::Available,
        );
        let policy = sbx.policy().unwrap().unwrap();
        assert_eq!(
            policy.classify_write(&cwd.join(".agents/notes.md")),
            WriteAccessCheck::NeedsProtectedApproval(cwd.join(".agents"))
        );
        assert_eq!(
            policy.classify_write(&worktree_gitdir.join("index")),
            WriteAccessCheck::NeedsProtectedApproval(worktree_gitdir.clone())
        );
        assert!(matches!(
            policy.classify_write(&home.join(".pp/agent/permission.toml")),
            WriteAccessCheck::ImmutableDenied(_)
        ));

        // 单文件精确批准只放行目标文件，不放行同目录其他文件。
        let approved_file = cwd.join(".agents/notes.md");
        let scoped = policy
            .with_approved_scope(&[], std::slice::from_ref(&approved_file), &[], false)
            .unwrap();
        assert_eq!(
            scoped.classify_write(&approved_file),
            WriteAccessCheck::Allowed
        );
        assert!(matches!(
            scoped.classify_write(&cwd.join(".agents/other.md")),
            WriteAccessCheck::NeedsProtectedApproval(_)
        ));

        // 禁止对 permission.toml 或 deny_read 凭据目录解除保护。
        assert!(
            policy
                .with_approved_scope(&[], &[home.join(".pp/agent/permission.toml")], &[], false)
                .is_err()
        );
        assert!(
            policy
                .with_approved_scope(&[], &[], &[home.join(".ssh")], false)
                .is_err()
        );
    }
    {
        assert!(Policy::is_denied_output(
            "EPERM: operation not permitted, open '/tmp/secret'"
        ));
        assert!(Policy::is_denied_output("Operation not permitted"));
        assert!(Policy::is_denied_output("blocked by sandbox policy"));
        assert!(!Policy::is_denied_output("ENOENT: no such file"));
    }
    {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let cwd = root.join("work");
        let home = root.join("home");
        let ws_file = cwd.join("src/main.rs");
        let outside_home_notes = home.join("Documents/notes.txt");
        let outside_home_docs_dir = home.join("docs");
        let outside_home_doc = outside_home_docs_dir.join("guide.md");
        let ssh_key = home.join(".ssh/id_ed25519");
        let ssh_known = home.join(".ssh/known_hosts");
        let cargo_cred = home.join(".cargo/credentials.toml");
        let cargo_bin = home.join(".cargo/bin/rustc");
        let session_file = home.join(".pp/agent/sessions/s1.jsonl");
        let tmp_file = root.join("shared/tmp.txt");

        for dir in [
            ws_file.parent().unwrap(),
            outside_home_notes.parent().unwrap(),
            &outside_home_docs_dir,
            ssh_key.parent().unwrap(),
            cargo_cred.parent().unwrap(),
            cargo_bin.parent().unwrap(),
            session_file.parent().unwrap(),
            tmp_file.parent().unwrap(),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for file in [
            &ws_file,
            &outside_home_notes,
            &outside_home_doc,
            &ssh_key,
            &ssh_known,
            &cargo_cred,
            &cargo_bin,
            &session_file,
            &tmp_file,
        ] {
            std::fs::write(file, "fixture").unwrap();
        }

        let make = |read_scope| {
            Sandbox::with_probe_for_test(
                SandboxSettings {
                    mode: SandboxMode::WorkspaceWrite,
                    read_scope,
                    allow_read: vec!["~/.ssh/known_hosts".into()],
                    ..Default::default()
                },
                &cwd,
                Some(&home),
                Some(&root.join("tmp")),
                ProbeResult::Available,
            )
            .policy()
            .unwrap()
            .unwrap()
        };

        let ws_policy = make(SandboxReadScope::Workspace);
        let cases = [
            (&ws_file, true, "workspace file allowed"),
            (&cargo_bin, true, "toolchain dir allowed under Workspace"),
            (&ssh_known, true, "explicit allow_read overrides deny_read"),
            (&tmp_file, true, "outside $HOME path allowed"),
            (&ssh_key, false, "credential denied"),
            (
                &cargo_cred,
                false,
                "cargo credentials denied even inside .cargo",
            ),
            (
                &session_file,
                false,
                "session path requires single approval",
            ),
            (
                &outside_home_notes,
                false,
                "$HOME file requires outside approval",
            ),
        ];
        for (path, expected_ok, label) in cases {
            assert_eq!(ws_policy.check_read(path).is_ok(), expected_ok, "{label}");
        }
        assert!(matches!(
            ws_policy.classify_read(&ssh_key),
            ReadAccessCheck::ImmutableDenied(_)
        ));
        assert!(matches!(
            ws_policy.classify_read(&cargo_cred),
            ReadAccessCheck::ImmutableDenied(_)
        ));
        assert_eq!(
            ws_policy.classify_read(&session_file),
            ReadAccessCheck::NeedsSessionApproval(session_file.clone())
        );
        assert_eq!(
            ws_policy.classify_read(&outside_home_notes),
            ReadAccessCheck::NeedsOutsideApproval(outside_home_notes.clone())
        );

        // 批准 $HOME/docs（home_allow_read）只放行该子树，不破坏 deny_read 且拒绝直接放行 .ssh/.pp/agent。
        let approved = ws_policy
            .with_approved_reads(&[], std::slice::from_ref(&outside_home_docs_dir))
            .unwrap();
        assert_eq!(
            approved.classify_read(&outside_home_doc),
            ReadAccessCheck::Allowed
        );
        assert_eq!(
            approved.classify_read(&outside_home_notes),
            ReadAccessCheck::NeedsOutsideApproval(outside_home_notes.clone())
        );
        assert!(matches!(
            approved.classify_read(&ssh_key),
            ReadAccessCheck::ImmutableDenied(_)
        ));
        assert!(
            ws_policy
                .with_approved_reads(&[], &[home.join(".ssh")])
                .is_err()
        );
        assert!(
            ws_policy
                .with_approved_reads(&[], &[home.join(".pp/agent")])
                .is_err()
        );

        // Unrestricted 与 without_home_restriction 放开普通 $HOME 读，但凭据与会话路径仍受阻断。
        let unrestricted = make(SandboxReadScope::Unrestricted);
        assert_eq!(
            unrestricted.classify_read(&outside_home_notes),
            ReadAccessCheck::Allowed
        );
        assert!(matches!(
            unrestricted.classify_read(&ssh_key),
            ReadAccessCheck::ImmutableDenied(_)
        ));
        assert!(matches!(
            unrestricted.classify_read(&session_file),
            ReadAccessCheck::NeedsSessionApproval(_)
        ));
        assert_ne!(ws_policy.fingerprint(), unrestricted.fingerprint());
        assert_ne!(ws_policy.fingerprint(), approved.fingerprint());
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn os_sandbox_blocks_home_python_read_until_approved_or_unrestricted() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let cwd = root.join("work");
    let home = root.join("home");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let ws_file = cwd.join("ok.txt");
    let home_secret = home.join("secret.txt");
    let home_other = home.join("other.txt");
    let ssh_key = home.join(".ssh/id_ed25519");
    std::fs::write(&ws_file, "ws-ok").unwrap();
    std::fs::write(&home_secret, "home-secret").unwrap();
    std::fs::write(&home_other, "home-other").unwrap();
    std::fs::write(&ssh_key, "ssh-secret").unwrap();

    if Sandbox::new(SandboxSettings::default(), &cwd)
        .status()
        .effective
        == EffectiveMode::Off
    {
        return;
    }

    let make = |read_scope| {
        Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                read_scope,
                ..Default::default()
            },
            &cwd,
            Some(&home),
            Some(&root.join("tmp")),
            ProbeResult::Available,
        )
        .policy()
        .unwrap()
        .unwrap()
    };

    let run_py = |policy: &Policy, target: &Path| {
        let args: Vec<std::ffi::OsString> = vec![
            "-c".into(),
            "import sys; print(open(sys.argv[1]).read())".into(),
            target.as_os_str().to_os_string(),
        ];
        let (program, wrapped_args) = policy.wrap_command("python3", &args).unwrap();
        let mut cmd = std::process::Command::new(program);
        cmd.current_dir(&cwd).args(wrapped_args);
        cmd.output().unwrap()
    };

    let ws_policy = make(SandboxReadScope::Workspace);
    assert!(run_py(&ws_policy, &ws_file).status.success());
    let blocked = run_py(&ws_policy, &home_secret);
    assert!(
        !blocked.status.success(),
        "python3 must not read $HOME file by default"
    );

    // 批准单文件后允许该文件，同级 $HOME 文件仍被拦截。
    let approved = ws_policy
        .with_approved_reads(&[], std::slice::from_ref(&home_secret))
        .unwrap();
    let allowed = run_py(&approved, &home_secret);
    assert!(
        allowed.status.success()
            && String::from_utf8_lossy(&allowed.stdout).trim() == "home-secret"
    );
    assert!(!run_py(&approved, &home_other).status.success());

    // Unrestricted 允许 $HOME 普通文件，但凭据仍拦截。
    let unrestricted = make(SandboxReadScope::Unrestricted);
    assert!(run_py(&unrestricted, &home_other).status.success());
    assert!(!run_py(&unrestricted, &ssh_key).status.success());
}
