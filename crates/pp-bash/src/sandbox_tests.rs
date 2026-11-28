mod integration {
    use super::super::*;
    use pp_config::config::{SandboxMode, SandboxSettings};
    use pp_sandbox::{ProbeResult, Sandbox};
    use std::net::TcpListener;
    use std::time::Duration;

    fn runtime_sandbox_available(cwd: &Path) -> bool {
        Sandbox::new(SandboxSettings::default(), cwd)
            .status()
            .effective
            != pp_sandbox::EffectiveMode::Off
    }

    fn test_sandbox(
        cwd: &Path,
        home: &Path,
        mode: SandboxMode,
        network: bool,
    ) -> Arc<pp_sandbox::Policy> {
        let sbx = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode,
                network,
                ..Default::default()
            },
            cwd,
            Some(home),
            Some(Path::new("/tmp")),
            ProbeResult::Available,
        );
        sbx.policy().unwrap().expect("active policy")
    }

    fn run_cmd(cwd: &Path, command: &str, policy: Arc<pp_sandbox::Policy>) -> Result<Value> {
        let mut events = Vec::new();
        run(
            cwd,
            &cwd.join(".pp-cache"),
            command,
            15,
            "test-sandbox",
            &CancellationToken::new(),
            &mut |event| events.push(event),
            OutputSink::new(Default::default(), Default::default()),
            pp_workspace::host_defaults::HostDefaults::test_default(),
            Some(policy),
        )
    }

    #[test]
    fn sandbox_bash_fs_protect_read_deny_and_in_process_guards() {
        {
            let dir = tempfile::tempdir().unwrap();
            let cwd = std::fs::canonicalize(dir.path()).unwrap();
            if !runtime_sandbox_available(&cwd) {
                eprintln!("skipping sandbox_bash test: platform sandbox unavailable");
                return;
            }
            let repo = cwd.join("repo");
            let home = cwd.join("home");
            std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
            std::fs::create_dir_all(repo.join(".pp")).unwrap();
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            std::fs::write(home.join(".ssh/id_ed25519"), b"PRIVATE-KEY").unwrap();

            let policy = test_sandbox(&repo, &home, SandboxMode::WorkspaceWrite, true);
            let backend = policy.backend();

            // 1. Writable inside cwd, and PP_SANDBOX is injected into the interpreter and external child processes
            let ok = run_cmd(
        &repo,
        "printf \"%s|\" \"$PP_SANDBOX\" && /bin/sh -c 'echo ok > inside.txt && printf \"%s\" \"$PP_SANDBOX\"'",
        policy.clone(),
    )
    .unwrap();
            assert_eq!(ok["ok"], true, "{ok}");
            assert_eq!(ok["output"], format!("{backend}|{backend}"));
            assert_eq!(
                std::fs::read_to_string(repo.join("inside.txt")).unwrap(),
                "ok\n"
            );

            // 2. .pp and .git/hooks are protected and not writable; sandbox metadata is returned
            let prot = run_cmd(&repo, "/bin/sh -c 'touch .pp/hooks.yml'", policy.clone()).unwrap();
            assert_eq!(prot["ok"], false, "{prot}");
            assert_eq!(prot["sandbox"]["blocked"], true, "{prot}");

            let git_hook = run_cmd(
                &repo,
                "/bin/sh -c 'touch .git/hooks/pre-commit'",
                policy.clone(),
            )
            .unwrap();
            assert_eq!(git_hook["ok"], false, "{git_hook}");
            assert_eq!(git_hook["sandbox"]["blocked"], true, "{git_hook}");

            // 3. Credential paths under denyRead are not readable
            let secret = run_cmd(
                &repo,
                &format!("/bin/cat {}", home.join(".ssh/id_ed25519").display()),
                policy.clone(),
            )
            .unwrap();
            assert_eq!(secret["ok"], false, "{secret}");
            assert_eq!(secret["sandbox"]["blocked"], true, "{secret}");

            // 4. In an existing git repository .git is sandbox write-protected by default until explicitly approved;
            // after approving .git, git add / git commit work
            let init_status = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(init_status.success());
            let blocked_git = run_cmd(
                &repo,
                "git -c user.name=t -c user.email=t@t add inside.txt",
                policy.clone(),
            )
            .unwrap();
            assert_eq!(blocked_git["ok"], false, "{blocked_git}");
            assert_eq!(blocked_git["sandbox"]["blocked"], true, "{blocked_git}");

            let approved_git_policy = policy
                .with_approved_scope(&[], &[], &[repo.join(".git")], false)
                .unwrap();
            let git_res = run_cmd(
        &repo,
        "git -c user.name=t -c user.email=t@t add inside.txt && git -c user.name=t -c user.email=t@t commit -q -m init",
        approved_git_policy,
    )
    .unwrap();
            assert_eq!(git_res["ok"], true, "{git_res}");
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let cwd = std::fs::canonicalize(dir.path()).unwrap();
            let repo = cwd.join("repo");
            let home = cwd.join("home");
            std::fs::create_dir_all(&repo).unwrap();
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            let key = home.join(".ssh/id_ed25519");
            std::fs::write(&key, b"SECRET_ENV=1\n").unwrap();

            let policy = test_sandbox(&repo, &home, SandboxMode::ReadOnly, true);

            // Redirected write into a read-only workspace (no external command involved)
            let write_redir = run_cmd(&repo, "echo bad > blocked.txt", policy.clone()).unwrap();
            assert_eq!(write_redir["ok"], false, "{write_redir}");
            assert_eq!(write_redir["sandbox"]["blocked"], true, "{write_redir}");
            assert!(
                write_redir["output"]
                    .as_str()
                    .unwrap()
                    .contains("blocked by sandbox policy"),
                "{write_redir}"
            );
            assert!(!repo.join("blocked.txt").exists());

            let err_redir = run_cmd(&repo, "echo bad &> blocked2.txt", policy.clone()).unwrap();
            assert_eq!(err_redir["ok"], false, "{err_redir}");
            assert_eq!(err_redir["sandbox"]["blocked"], true, "{err_redir}");

            // Redirected read of a credential path
            let read_redir = run_cmd(
                &repo,
                &format!("read line < {}", key.display()),
                policy.clone(),
            )
            .unwrap();
            assert_eq!(read_redir["ok"], false, "{read_redir}");
            assert_eq!(read_redir["sandbox"]["blocked"], true, "{read_redir}");

            // source of a credential path
            let src_res = run_cmd(&repo, &format!("source {}", key.display()), policy).unwrap();
            assert_eq!(src_res["ok"], false, "{src_res}");
            assert_eq!(src_res["sandbox"]["blocked"], true, "{src_res}");
        }
    }

    #[test]
    fn sandbox_bash_pipeline_background_cancel_and_network() {
        {
            let dir = tempfile::tempdir().unwrap();
            let cwd = std::fs::canonicalize(dir.path()).unwrap();
            if !runtime_sandbox_available(&cwd) {
                eprintln!("skipping sandbox_bash pipeline test: platform sandbox unavailable");
                return;
            }
            let repo = cwd.join("repo");
            let home = cwd.join("home");
            std::fs::create_dir_all(repo.join(".pp")).unwrap();
            std::fs::create_dir_all(&home).unwrap();

            let policy = test_sandbox(&repo, &home, SandboxMode::WorkspaceWrite, true);

            // Pipelines and exec are both sandbox-constrained
            let pipe_res = run_cmd(
                &repo,
                "printf hi | /bin/sh -c 'cat >/dev/null && touch .pp/x'",
                policy.clone(),
            )
            .unwrap();
            assert_eq!(pipe_res["ok"], false, "{pipe_res}");
            assert_eq!(pipe_res["sandbox"]["blocked"], true, "{pipe_res}");

            let exec_res = run_cmd(&repo, "exec /bin/sh -c 'touch .pp/x'", policy.clone()).unwrap();
            assert_eq!(exec_res["ok"], false, "{exec_res}");
            assert_eq!(exec_res["sandbox"]["blocked"], true, "{exec_res}");

            // Background tasks are sandbox-constrained too
            let manager = TaskManager::default();
            let bg = manager
                .start_with_output(
                    &repo,
                    &repo.join(".pp-cache"),
                    "/bin/sh -c 'touch .pp/bg_blocked'",
                    "test-bg-sbx",
                    &CancellationToken::new(),
                    pp_workspace::host_defaults::HostDefaults::test_default(),
                    OutputSink::new(Default::default(), Default::default()),
                    Some(policy.clone()),
                )
                .unwrap();
            let task_id = bg["task_id"].as_str().unwrap().to_owned();
            manager.task(&task_id).unwrap().join().unwrap();
            let snap = manager.status(&task_id).unwrap();
            assert_eq!(snap["status"], "failed", "{snap}");
            assert_eq!(snap["sandbox"]["blocked"], true, "{snap}");

            // Cancelling a wrapped process group reclaims it cleanly
            let cancel = CancellationToken::new();
            let cancel_clone = cancel.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(120));
                cancel_clone.cancel();
            });
            let cancelled = run(
                &repo,
                &repo.join(".pp-cache"),
                "sleep 30",
                15,
                "test-cancel-sbx",
                &cancel,
                &mut |_| {},
                OutputSink::new(Default::default(), Default::default()),
                pp_workspace::host_defaults::HostDefaults::test_default(),
                Some(policy),
            )
            .unwrap();
            assert_eq!(cancelled["cancelled"], true, "{cancelled}");
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let cwd = std::fs::canonicalize(dir.path()).unwrap();
            if !runtime_sandbox_available(&cwd) {
                eprintln!("skipping sandbox_bash network test: platform sandbox unavailable");
                return;
            }
            let repo = cwd.join("repo");
            let home = cwd.join("home");
            std::fs::create_dir_all(&repo).unwrap();
            std::fs::create_dir_all(&home).unwrap();

            // The listener is owned by the test process and never accepts: under macOS Seatbelt loopback connects
            // successfully and external addresses are refused; under Linux Bubblewrap `--unshare-net` isolates the
            // network namespace so both host loopback and the internet are unreachable.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();

            let policy = test_sandbox(&repo, &home, SandboxMode::WorkspaceWrite, false);
            let expected_loopback_ok = if policy.backend() == "seatbelt" { 1 } else { 0 };
            let script = format!(
                "/usr/bin/python3 -c \"\
import errno, socket, sys; \
loopback = socket.socket(); loopback.settimeout(2.0); loop = loopback.connect_ex(('127.0.0.1', {port})); loopback.close(); \
external = socket.socket(); external.settimeout(2.0); ext = external.connect_ex(('93.184.216.34', 80)); external.close(); \
print('loopback=%d external=%d' % (loop, ext)); \
loop_ok = (loop == 0) if {expected_loopback_ok} else (loop != 0); \
sys.exit(0 if loop_ok and ext != 0 else 3)\""
            );
            let res = run_cmd(&repo, &script, policy).unwrap();
            assert_eq!(res["ok"], true, "{res}");
        }
    }
}
