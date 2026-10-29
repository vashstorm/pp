use super::*;
fn binding(dir: &Path, settings: Settings) -> Binding {
    Binding::root(
        RootOptions {
            dir: dir.join("state"),
            user: dir.join("user"),
            cwd: dir.to_owned(),
            model: "fixture/main".into(),
            thinking: ThinkLevel::None,
            settings,
            plan_mode: false,
        },
        Arc::new(|_, _| bail!("fixture factory")),
        Arc::new(|_| Ok(())),
    )
    .unwrap()
}
#[test]
fn schema_checks_nested_required_additional_properties_and_combinators() {
    let schema = json!({"type":"object","properties":{"items":{"type":"array","items":{"type":"integer","minimum":1}}},"required":["items"],"additionalProperties":false});
    assert!(validate_output(&schema, &json!({"items":[1,2]})).is_ok());
    for value in [
        json!({}),
        json!({"items":[0]}),
        json!({"items":["x"]}),
        json!({"items":[],"extra":true}),
    ] {
        assert!(validate_output(&schema, &value).is_err());
    }
    assert!(validate_schema(&json!({"$ref":"other"})).is_err());
    assert!(
        validate_output(
            &json!({"oneOf":[{"type":"number"},{"type":"integer"}]}),
            &json!(1)
        )
        .is_err()
    );
}
#[test]
fn discovery_uses_project_then_user_then_bundled_and_skips_bad_files() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".pp/agents");
    let user = dir.path().join("user/agents");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&user).unwrap();
    for (path, label) in [(&project, "project"), (&user, "user")] {
        fs::write(
            path.join("scout.md"),
            format!("---\nname: scout\ndescription: {label}\ntools: []\n---\nRole"),
        )
        .unwrap();
    }
    fs::write(project.join("invalid.md"), "bad").unwrap();
    let roles =
        catalog::discover(dir.path(), &dir.path().join("user"), &Settings::default()).unwrap();
    let scout = roles.iter().find(|d| d.name == "scout").unwrap();
    assert_eq!(scout.description, "project");
    assert_eq!(scout.tools, Some(vec![]));
}
#[test]
fn invalid_batch_has_no_dispatch_or_reserved_ids() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Settings::default());
    let result = binding.execute(
        "task",
        &json!({"context":"test","tasks":[{"task":"valid"},{"agent":"missing","task":"invalid"}]})
            .to_string(),
        false,
        &Default::default(),
    );
    assert!(result.is_err());
    assert!(lock(&binding.inner.state).disk.agents.is_empty());
    assert_eq!(lock(&binding.inner.state).workers, 0);
}
#[test]
fn delivery_is_inspectable_until_ack_and_scoped_to_owner() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Settings::default());
    let run = Run {
        id: "r1".into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: "completed".into(),
        task: "test".into(),
        result: Some(json!({"ok":true,"output":"done"})),
        consumed: false,
        duration_ms: 0,
        requests: 1,
        tokens: 2,
        cost: None,
    };
    lock(&binding.inner.state)
        .disk
        .runs
        .insert("r1".into(), run);
    let result = binding.wait(&[], 0, &Default::default()).unwrap();
    assert!(result.to_string().contains("pp-subagent-result:r1"));
    assert_eq!(binding.deliveries(&[]).len(), 1);
    let mut other = binding.clone();
    other.owner = "other".into();
    assert!(other.deliveries(&[]).is_empty());
    assert!(other.wait(&["r1".into()], 0, &Default::default()).is_err());
    binding.acknowledge(&["r1".into()]).unwrap();
    assert!(binding.deliveries(&[]).is_empty());
}
#[test]
fn cancelled_permit_wait_does_not_leak_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut binding = binding(
        dir.path(),
        Settings {
            max_concurrency: 1,
            ..Default::default()
        },
    );
    binding.owner = "child".into();
    let permit = binding.permit(&Default::default()).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    assert!(binding.permit(&token).is_err());
    drop(permit);
    assert!(binding.permit(&Default::default()).is_ok());
}
#[test]
fn persisted_running_work_becomes_interrupted_without_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Settings::default());
    let run = Run {
        id: "r1".into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: "running".into(),
        task: "test".into(),
        result: None,
        consumed: false,
        duration_ms: 0,
        requests: 0,
        tokens: 0,
        cost: None,
    };
    {
        let mut state = lock(&binding.inner.state);
        state.disk.runs.insert("r1".into(), run);
        binding.inner.save(&state).unwrap();
    }
    let restored = super::tests::binding(dir.path(), Settings::default());
    assert_eq!(
        lock(&restored.inner.state).disk.runs["r1"].status,
        "interrupted"
    );
    assert_eq!(lock(&restored.inner.state).workers, 0);
}
#[test]
fn worktree_preserves_dirty_source_and_conflicts_retain_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap();
        assert!(result.status.success(), "{:?}", result);
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "fixture@example.com"]);
    git(&["config", "user.name", "Fixture"]);
    fs::write(repo.join("file"), "base\n").unwrap();
    git(&["add", "file"]);
    git(&["commit", "-qm", "base"]);
    fs::write(repo.join("file"), "dirty\n").unwrap();
    fs::write(repo.join("new"), [0, 1, 2, 255]).unwrap();
    let artifacts = dir.path().join("artifacts");
    fs::create_dir(&artifacts).unwrap();
    let target = workspace::prepare(&repo, &artifacts, "a1", &Default::default()).unwrap();
    assert_eq!(fs::read(target.join("file")).unwrap(), b"dirty\n");
    assert_eq!(fs::read(target.join("new")).unwrap(), [0, 1, 2, 255]);
    fs::write(target.join("file"), "child\n").unwrap();
    workspace::capture(&artifacts, "a1", true, &Default::default()).unwrap();
    assert_eq!(fs::read(repo.join("file")).unwrap(), b"child\n");
    fs::write(target.join("file"), "another\n").unwrap();
    fs::write(repo.join("file"), "user\n").unwrap();
    assert!(workspace::capture(&artifacts, "a1", true, &Default::default()).is_err());
    assert!(target.exists());
    assert!(artifacts.join("a1.patch").exists());
    assert_eq!(fs::read(repo.join("file")).unwrap(), b"user\n");
}

#[test]
fn worktree_captures_nested_repository_binary_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    fn git(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    fn init(path: &Path) {
        git(path, &["init", "-q"]);
        git(path, &["config", "user.email", "fixture@example.com"]);
        git(path, &["config", "user.name", "Fixture"]);
    }
    init(&repo);
    fs::write(repo.join("root"), "root\n").unwrap();
    git(&repo, &["add", "root"]);
    git(&repo, &["commit", "-qm", "root"]);
    let nested = repo.join("nested");
    fs::create_dir(&nested).unwrap();
    init(&nested);
    fs::write(nested.join("binary"), [0, 255, 1]).unwrap();
    fs::write(nested.join("deleted"), "remove").unwrap();
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-qm", "nested"]);
    let artifacts = dir.path().join("artifacts");
    let target = workspace::prepare(&repo, &artifacts, "a1", &Default::default()).unwrap();
    fs::write(target.join("nested/binary"), [0, 255, 9, 8]).unwrap();
    fs::remove_file(target.join("nested/deleted")).unwrap();
    let captured = workspace::capture(&artifacts, "a1", true, &Default::default()).unwrap();
    assert!(!captured.patches.is_empty());
    assert_eq!(fs::read(nested.join("binary")).unwrap(), [0, 255, 9, 8]);
    assert!(!nested.join("deleted").exists());
    assert!(
        workspace::capture(&artifacts, "a1", true, &Default::default())
            .unwrap()
            .patches
            .is_empty()
    );
}
#[test]
fn cancelled_and_unknown_waits_do_not_consume_other_owners_results() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Default::default());
    let token = CancellationToken::new();
    token.cancel();
    assert!(binding.wait(&[], 60_000, &token).is_err());
    assert!(
        binding
            .wait(&["missing".into()], 0, &Default::default())
            .is_err()
    );
}

#[test]
fn published_result_recovers_without_replaying_an_incomplete_registry_run() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), Default::default());
    {
        let mut state = lock(&binding.inner.state);
        state.disk.runs.insert(
            "r1".into(),
            Run {
                id: "r1".into(),
                agent_id: "a1".into(),
                owner: "Main".into(),
                status: "running".into(),
                task: "task".into(),
                result: None,
                consumed: false,
                duration_ms: 0,
                requests: 0,
                tokens: 0,
                cost: None,
            },
        );
        binding.inner.save(&state).unwrap();
    }
    atomic_json(
        &binding.directory().join("r1.result.json"),
        &json!({"ok":true,"output":"durable result"}),
    )
    .unwrap();
    let restored = super::tests::binding(dir.path(), Default::default());
    let results = restored.deliveries(&[]);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].status, "completed");
    assert_eq!(
        results[0].result.as_ref().unwrap()["output"],
        "durable result"
    );
    assert_eq!(lock(&restored.inner.state).workers, 0);
}
#[test]
fn plan_mode_refuses_to_revive_a_writable_isolated_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let mut binding = binding(dir.path(), Default::default());
    Arc::get_mut(&mut binding.inner).unwrap().plan_mode = true;
    let spec = Spec {
        definition: "task".into(),
        prompt: String::new(),
        context: String::new(),
        model: "fixture/main".into(),
        thinking: ThinkLevel::None,
        tools: vec!["write".into()],
        spawns: vec![],
        output_schema: None,
        cwd: dir.path().into(),
        depth: 1,
        readonly: false,
        isolated: true,
        cost: None,
    };
    lock(&binding.inner.state).disk.agents.insert(
        "a1".into(),
        Record {
            id: "a1".into(),
            name: "isolated".into(),
            parent: "Main".into(),
            generation: 1,
            status: "parked".into(),
            spec,
            session_file: None,
            updated_ms: 0,
        },
    );
    assert!(binding.control("a1", "revive", None).is_err());
    assert_eq!(lock(&binding.inner.state).workers, 0);
}
