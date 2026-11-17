use super::*;
use pp_config::config::{SandboxMode, SandboxSettings};
use pp_sandbox::{ProbeResult, Sandbox};
use std::net::TcpListener;
use std::time::Duration;

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
    let store = PipeStore::new();
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
        &store,
        Some(policy),
    )
}

#[test]
fn sandbox_bash_enforces_workspace_write_protect_and_read_deny() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let repo = cwd.join("repo");
    let home = cwd.join("home");
    std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
    std::fs::create_dir_all(repo.join(".pp")).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_ed25519"), b"PRIVATE-KEY").unwrap();

    let policy = test_sandbox(&repo, &home, SandboxMode::WorkspaceWrite, true);

    // 1. cwd 内可写且 PP_SANDBOX=seatbelt 注入到解释器及外部子进程
    let ok = run_cmd(
        &repo,
        "printf \"%s|\" \"$PP_SANDBOX\" && /bin/sh -c 'echo ok > inside.txt && printf \"%s\" \"$PP_SANDBOX\"'",
        policy.clone(),
    )
    .unwrap();
    assert_eq!(ok["ok"], true, "{ok}");
    assert_eq!(ok["output"], "seatbelt|seatbelt");
    assert_eq!(
        std::fs::read_to_string(repo.join("inside.txt")).unwrap(),
        "ok\n"
    );

    // 2. .pp 与 .git/hooks 受保护不可写，返回 sandbox 元数据
    let prot = run_cmd(&repo, "/bin/sh -c 'touch .pp/hooks.yml'", policy.clone()).unwrap();
    assert_eq!(prot["ok"], false, "{prot}");
    assert_eq!(prot["sandbox"]["blocked"], true, "{prot}");

    // 3. denyRead 里的凭据路径不可读
    let secret = run_cmd(
        &repo,
        &format!("/bin/cat {}", home.join(".ssh/id_ed25519").display()),
        policy.clone(),
    )
    .unwrap();
    assert_eq!(secret["ok"], false, "{secret}");
    assert_eq!(secret["sandbox"]["blocked"], true, "{secret}");

    // 4. 在已有 git 仓库中 git add / git commit 正常工作（只读 .git/config/.git/hooks 不影响提交）
    let init_status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&repo)
        .status()
        .unwrap();
    assert!(init_status.success());
    let git_res = run_cmd(
        &repo,
        "git -c user.name=t -c user.email=t@t add inside.txt && git -c user.name=t -c user.email=t@t commit -q -m init",
        policy,
    )
    .unwrap();
    assert_eq!(git_res["ok"], true, "{git_res}");
}

#[test]
fn sandbox_bash_redirects_and_source_blocked_by_in_process_guard() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let repo = cwd.join("repo");
    let home = cwd.join("home");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    let key = home.join(".ssh/id_ed25519");
    std::fs::write(&key, b"SECRET_ENV=1\n").unwrap();

    let policy = test_sandbox(&repo, &home, SandboxMode::ReadOnly, true);

    // 重定向写只读工作区（不触发外部命令）
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

    // 重定向读凭据路径
    let read_redir = run_cmd(
        &repo,
        &format!("read line < {}", key.display()),
        policy.clone(),
    )
    .unwrap();
    assert_eq!(read_redir["ok"], false, "{read_redir}");
    assert_eq!(read_redir["sandbox"]["blocked"], true, "{read_redir}");

    // source 凭据路径
    let src_res = run_cmd(&repo, &format!("source {}", key.display()), policy).unwrap();
    assert_eq!(src_res["ok"], false, "{src_res}");
    assert_eq!(src_res["sandbox"]["blocked"], true, "{src_res}");
}

#[test]
fn sandbox_bash_pipeline_exec_background_and_cancellation_reclaim() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let repo = cwd.join("repo");
    let home = cwd.join("home");
    std::fs::create_dir_all(repo.join(".pp")).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    let policy = test_sandbox(&repo, &home, SandboxMode::WorkspaceWrite, true);

    // 管道与 exec 均受沙盒约束
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

    // 后台任务同样受沙盒约束
    let manager = TaskManager::default();
    let store = PipeStore::new();
    let bg = manager
        .start_with_output(
            &repo,
            &repo.join(".pp-cache"),
            "/bin/sh -c 'touch .pp/bg_blocked'",
            "test-bg-sbx",
            &CancellationToken::new(),
            pp_workspace::host_defaults::HostDefaults::test_default(),
            &store,
            OutputSink::new(Default::default(), Default::default()),
            Some(policy.clone()),
        )
        .unwrap();
    let task_id = bg["task_id"].as_str().unwrap().to_owned();
    manager.task(&task_id).unwrap().join().unwrap();
    let snap = manager.status(&task_id).unwrap();
    assert_eq!(snap["status"], "failed", "{snap}");
    assert_eq!(snap["sandbox"]["blocked"], true, "{snap}");

    // 取消被包装进程组可干净回收
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
        &store,
        Some(policy),
    )
    .unwrap();
    assert_eq!(cancelled["cancelled"], true, "{cancelled}");
}

#[test]
fn sandbox_bash_network_false_blocks_external_but_allows_loopback() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(dir.path()).unwrap();
    let repo = cwd.join("repo");
    let home = cwd.join("home");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&home).unwrap();

    // 监听器只由测试进程持有、不 accept：loopback 建连在内核完成握手即算成功，
    // 避免无上界阻塞的 accept 线程在沙盒内进程未启动时永久挂住。
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let policy = test_sandbox(&repo, &home, SandboxMode::WorkspaceWrite, false);
    // connect_ex 以 errno 代替异常：loopback 必须建连成功（0），
    // 外部地址必须被策略拒绝（EPERM/EACCES）。整条命令受 run 的 15s 看门狗约束。
    let script = format!(
        "/usr/bin/python3 -c \"\
import errno, socket, sys; \
loopback = socket.socket(); loopback.settimeout(2.0); loop = loopback.connect_ex(('127.0.0.1', {port})); loopback.close(); \
external = socket.socket(); external.settimeout(2.0); ext = external.connect_ex(('93.184.216.34', 80)); external.close(); \
print('loopback=%d external=%d' % (loop, ext)); \
sys.exit(0 if loop == 0 and ext in (errno.EPERM, errno.EACCES) else 3)\""
    );
    let res = run_cmd(&repo, &script, policy).unwrap();
    assert_eq!(res["ok"], true, "{res}");
}
