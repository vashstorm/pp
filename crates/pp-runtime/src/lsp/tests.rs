use super::*;
use pp_core::cancellation::CancellationToken;
use serde_json::json;

fn fixture() -> (tempfile::TempDir, Registry) {
    fixture_pull("full")
}

fn fixture_pull(pull: &str) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".pp")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
    let log = dir.path().join("requests.jsonl");
    std::fs::write(dir.path().join(".pp/lsp.json"), json!({"servers": {"fixture": {
        "command":"python3", "args":[script, "--log", log, "--pull", pull], "fileTypes":[".fixture"], "rootMarkers":[".pp"]
    }}}).to_string()).unwrap();
    let config = config::Config::load_with(dir.path(), None).unwrap();
    let registry = Registry::from_config(
        dir.path().to_path_buf(),
        LspSettings::default(),
        Ok(Arc::new(config)),
    );
    (dir, registry)
}

fn ensure(registry: &Registry) -> Arc<Client> {
    registry
        .ensure_client(
            "fixture",
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .unwrap()
}

fn starts(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("requests.jsonl"))
        .unwrap()
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["event"] == "started"
        })
        .count()
}

/// 同一工作区里的多个服务器别名共享 fixture 脚本；只有 `fileTypes` 可不同。
fn fixture_aliases(aliases: &[(&str, &[&str])]) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".pp")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
    let log = dir.path().join("requests.jsonl");
    let mut servers = serde_json::Map::new();
    for (name, file_types) in aliases {
        servers.insert(
            (*name).to_owned(),
            json!({
                "command": "python3",
                "args": [script, "--log", log],
                "fileTypes": file_types,
                "rootMarkers": [".pp"],
            }),
        );
    }
    std::fs::write(
        dir.path().join(".pp/lsp.json"),
        json!({"servers": servers}).to_string(),
    )
    .unwrap();
    let config = config::Config::load_with(dir.path(), None).unwrap();
    let registry = Registry::from_config(
        dir.path().to_path_buf(),
        LspSettings::default(),
        Ok(Arc::new(config)),
    );
    (dir, registry)
}

fn ensure_name(registry: &Registry, name: &str) -> Arc<Client> {
    registry
        .ensure_client(
            name,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .unwrap()
}

#[test]
fn lsp_registry_alias_identity_starts_once() {
    let (dir, registry) =
        fixture_aliases(&[("alias-a", &[".fixture"]), ("alias-b", &[".fixture"])]);
    let mut cleanup = registry.cleanup();
    let first = ensure_name(&registry, "alias-a");
    let second = ensure_name(&registry, "alias-b");
    // 名称不参与身份：同初始化的别名共享同一个客户端与进程。
    assert!(Arc::ptr_eq(&first, &second));
    assert_eq!(starts(dir.path()), 1);
    assert!(Arc::ptr_eq(&registry.client("alias-b").unwrap(), &first));
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_alias_coverage_unions_file_types() {
    let (dir, registry) = fixture_aliases(&[("alias-a", &[".fixture"]), ("alias-b", &[".extra"])]);
    let mut cleanup = registry.cleanup();
    let client = ensure_name(&registry, "alias-a");
    let shared = ensure_name(&registry, "alias-b");
    assert!(Arc::ptr_eq(&client, &shared));
    assert_eq!(starts(dir.path()), 1);
    // 共享客户端保存的是 alias-a 的定义；alias-b 的额外类型必须由 registry 补上。
    assert!(
        !client
            .definition
            .supports_file(&dir.path().join("source.extra"))
    );
    assert!(registry.covers(&client, &dir.path().join("source.extra")));
    assert!(registry.covers(&client, &dir.path().join("source.fixture")));
    assert!(!registry.covers(&client, &dir.path().join("source.txt")));
    // 真实 fixture 走 alias-b 的单文件诊断仍复用同一进程。
    let path = dir.path().join("source.extra");
    std::fs::write(&path, "wrong\n").unwrap();
    let report = diagnostics::collect(
        &registry,
        &path,
        "test",
        "owner",
        false,
        Instant::now() + Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(report.servers.len(), 1);
    assert_eq!(report.servers[0].server, "alias-b");
    assert!(report.has_issues(), "{report:?}");
    assert_eq!(starts(dir.path()), 1);
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_reload_alias_rename_reuses_process() {
    let (dir, registry) =
        fixture_aliases(&[("alias-a", &[".fixture"]), ("alias-b", &[".fixture"])]);
    let mut cleanup = registry.cleanup();
    let client = ensure_name(&registry, "alias-a");
    let mut candidate = (*registry.config().unwrap()).clone();
    for definition in candidate
        .servers
        .iter_mut()
        .chain(candidate.definitions.iter_mut())
    {
        if definition.name == "alias-a" {
            definition.name = "alias-c".to_owned();
        }
    }
    // 身份仍在候选配置里（只是换了别名），reload 不能回收旧进程。
    assert!(
        registry
            .reload_candidate(Arc::new(candidate))
            .unwrap()
            .is_empty()
    );
    assert!(registry.client("alias-a").is_none());
    let renamed = ensure_name(&registry, "alias-c");
    assert!(Arc::ptr_eq(&client, &renamed));
    assert_eq!(starts(dir.path()), 1);
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_superseded_initialization_reports_failed_status() {
    let (dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let barrier = dir.path().join("initialize");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&barrier)
            .status()
            .unwrap()
            .success()
    );
    let mut candidate = (*registry.config().unwrap()).clone();
    for definition in candidate
        .servers
        .iter_mut()
        .chain(candidate.definitions.iter_mut())
    {
        if definition.name == "fixture" {
            definition
                .args
                .extend(["--barrier-dir".into(), dir.path().display().to_string()]);
        }
    }
    registry
        .reload_candidate(Arc::new(candidate.clone()))
        .unwrap();
    std::thread::scope(|scope| {
        let initialization = scope.spawn(|| {
            registry.ensure_client(
                "fixture",
                Instant::now() + Duration::from_secs(10),
                &CancellationToken::new(),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let log = dir.path().join("requests.jsonl");
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let reached = text
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .any(|entry| entry["barrier"] == "initialize");
            if reached {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "fixture never reached initialize barrier"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // 配置未变但 generation 必须递增：已经初始化中的客户端要被回收，
        // 状态不能永远停在 initializing。
        registry.reload_candidate(Arc::new(candidate)).unwrap();
        {
            use std::io::Write as _;
            let mut release = std::fs::OpenOptions::new()
                .write(true)
                .open(&barrier)
                .unwrap();
            release.write_all(b"go").unwrap();
        }
        let error = match initialization.join().unwrap() {
            Ok(_) => panic!("initialization must be superseded"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("superseded"), "{error:#}");
        match registry.state("fixture") {
            ClientState::Failed(_) => {}
            other => panic!("unexpected state after superseded init: {other:?}"),
        }
    });
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_cleanup_guard_allows_start() {
    let (dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let client = ensure(&registry);
    assert!(!client.is_closed());
    assert_eq!(starts(dir.path()), 1);
    cleanup.stop().unwrap();
    assert!(client.is_closed());
}

#[test]
fn lsp_registry_concurrent_identity_starts_once() {
    let (dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let clients = std::thread::scope(|scope| {
        let jobs: Vec<_> = (0..4).map(|_| scope.spawn(|| ensure(&registry))).collect();
        jobs.into_iter()
            .map(|job| job.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert!(
        clients
            .iter()
            .all(|client| Arc::ptr_eq(client, &clients[0]))
    );
    assert_eq!(starts(dir.path()), 1);
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_cancelled_keeps_typed_error() {
    let (_dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let error = match registry.ensure_client(
        "fixture",
        Instant::now() + Duration::from_secs(5),
        &cancellation,
    ) {
        Ok(_) => panic!("cancelled initialization succeeded"),
        Err(error) => error,
    };
    assert!(error.is::<pp_core::cancellation::Cancelled>(), "{error:#}");
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_unrelated_reload_reuses_process() {
    let (dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let original = ensure(&registry);
    let mut candidate = (*registry.config().unwrap()).clone();
    candidate.idle_timeout_ms = Some(60_000);
    assert!(
        registry
            .reload_candidate(Arc::new(candidate))
            .unwrap()
            .is_empty()
    );
    let current = ensure(&registry);
    assert!(Arc::ptr_eq(&original, &current));
    assert_eq!(starts(dir.path()), 1);
    cleanup.stop().unwrap();
}

#[test]
fn lsp_registry_idle_maintenance_reaps_without_new_call() {
    let (_dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let mut candidate = (*registry.config().unwrap()).clone();
    candidate.idle_timeout_ms = Some(1);
    registry.reload_candidate(Arc::new(candidate)).unwrap();
    let client = ensure(&registry);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !client.is_closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        client.is_closed(),
        "idle client remained alive without another tool call"
    );
    cleanup.stop().unwrap();
}

#[test]
fn lsp_settings_timeout_boundaries_and_unknown_fields() {
    let disabled: LspSettings = serde_json::from_value(json!({"formatOnWrite":false})).unwrap();
    disabled.validate().unwrap();
    let enabled: LspSettings = serde_json::from_value(json!({"formatOnWrite":true})).unwrap();
    assert!(
        enabled
            .validate()
            .unwrap_err()
            .to_string()
            .contains("action=format")
    );
    for timeout in [4, 301] {
        let settings: LspSettings =
            serde_json::from_value(json!({"requestTimeout":timeout})).unwrap();
        assert!(settings.validate().is_err());
    }
    for timeout in [5, 300] {
        let settings: LspSettings =
            serde_json::from_value(json!({"requestTimeout":timeout})).unwrap();
        settings.validate().unwrap();
    }
    for input in [
        json!({"requestTimeout":5.5}),
        json!({"shared":true}),
        json!({"maxTimeout":10}),
    ] {
        assert!(serde_json::from_value::<LspSettings>(input).is_err());
    }
}

#[test]
fn lsp_registry_stop_during_initialization_reaps_process() {
    let (dir, registry) = fixture();
    let mut cleanup = registry.cleanup();
    let barrier = dir.path().join("initialize");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&barrier)
            .status()
            .unwrap()
            .success()
    );
    let mut candidate = (*registry.config().unwrap()).clone();
    for definition in candidate
        .servers
        .iter_mut()
        .chain(candidate.definitions.iter_mut())
    {
        if definition.name == "fixture" {
            definition
                .args
                .extend(["--barrier-dir".into(), dir.path().display().to_string()]);
        }
    }
    registry.reload_candidate(Arc::new(candidate)).unwrap();
    std::thread::scope(|scope| {
        let initialization = scope.spawn(|| {
            registry.ensure_client(
                "fixture",
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let log = dir.path().join("requests.jsonl");
        let pid = loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let barrier = text
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find(|entry| entry["barrier"] == "initialize");
            if let Some(entry) = barrier {
                break entry["pid"].as_u64().unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "fixture never reached initialize barrier"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        cleanup.stop().unwrap();
        assert!(initialization.join().unwrap().is_err());
        assert!(
            !std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    });
}

#[test]
fn lsp_diagnostics_malformed_pull_never_clean() {
    for pull in ["malformed", "missing"] {
        let (dir, registry) = fixture_pull(pull);
        let mut cleanup = registry.cleanup();
        let path = dir.path().join("source.fixture");
        std::fs::write(&path, "wrong\n").unwrap();
        let client = ensure(&registry);
        let report = client.collect_diagnostics(
            &path,
            Instant::now() + Duration::from_secs(2),
            &CancellationToken::new(),
        );
        assert!(
            report.is_err(),
            "malformed target report must fail, got {report:?}"
        );
        cleanup.stop().unwrap();
    }
}
