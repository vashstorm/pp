use super::*;

include!("tests/wait_cases.rs");

pub(super) fn binding(dir: &Path, settings: TaskSettings) -> Binding {
    let binding = Binding::root(
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
    .unwrap();
    binding.set_model_roles(BTreeMap::from([ModelRole::Default, ModelRole::Fast].map(
        |role| {
            (
                role,
                pp_config::config::RoleAssignment {
                    enabled: true,
                    model: "fixture/main".into(),
                    effort: ThinkLevel::None,
                    fallback: None,
                },
            )
        },
    )));
    binding
}

#[test]
fn child_bindings_cannot_spawn_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let root = binding(
        dir.path(),
        TaskSettings {
            max_recursion_depth: 8,
            ..Default::default()
        },
    );
    let args = json!({"context":"no recursive dispatch","tasks":[{"role":"fast","task":"valid child work"}]})
        .to_string();
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        assert!(
            root.definitions(mode)
                .iter()
                .any(|tool| tool.name == "task")
        );
        for depth in [1, 2] {
            let mut child = root.clone();
            child.depth = depth;
            child.owner = format!("a{depth}");
            child.readonly = false;
            child.spawns = vec!["*".into()];
            assert!(child.definitions(mode).is_empty());
            let before = {
                let state = lock(&root.inner.state);
                (state.disk.agents.clone(), state.disk.runs.clone())
            };
            let error = child
                .execute("task", &args, mode == WorkMode::Plan, &Default::default())
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "this agent is not allowed to spawn further subagents"
            );
            let state = lock(&root.inner.state);
            assert_eq!(
                serde_json::to_value((&state.disk.agents, &state.disk.runs)).unwrap(),
                serde_json::to_value(before).unwrap()
            );
        }
    }
    let disabled_dir = tempfile::tempdir().unwrap();
    let disabled = binding(
        disabled_dir.path(),
        TaskSettings {
            max_recursion_depth: 0,
            ..Default::default()
        },
    );
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        assert!(disabled.definitions(mode).is_empty());
        assert!(
            disabled
                .execute("task", &args, mode == WorkMode::Plan, &Default::default())
                .is_err()
        );
    }
    let state = lock(&disabled.inner.state);
    assert!(state.disk.agents.is_empty());
    assert!(state.disk.runs.is_empty());
}

include!("tests/role_cases.rs");

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
fn custom_agents_require_real_definitions_and_never_inherit_the_plan_model() {
    let dir = tempfile::tempdir().unwrap();
    let mut binding = binding(dir.path(), TaskSettings::default());
    binding.model = "fixture/planner".into();
    binding.allow_mcp(
        vec!["mcp__remote__echo".into(), "mcp__remote__fail".into()],
        vec!["mcp__remote__echo".into()],
    );
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert!(
        schema
            .pointer("/properties/tasks/items/properties/agent")
            .is_none()
    );
    let project = dir.path().join(".pp/agents");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("scout.md"), "---\nname: scout\ndescription: configured scout\ntools: [read, rg, glob, lsp, mcp__remote__echo, mcp__remote__fail]\n---\nConfigured investigation prompt.").unwrap();
    let definitions = binding.custom_roles();
    let scout = definitions.iter().find(|def| def.name == "scout").unwrap();
    assert!(scout.source.ends_with(".pp/agents/scout.md"));
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert_eq!(
        schema
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["fast", "scout"])
    );
    for missing in ["task", "reviewer", "invented", ""] {
        assert!(
            binding
                .execute(
                    "task",
                    &json!({"context":"test","tasks":[{"role":missing,"task":"invalid"}]})
                        .to_string(),
                    false,
                    &Default::default()
                )
                .is_err()
        );
    }
    assert!(lock(&binding.inner.state).disk.agents.is_empty());
    binding
        .execute(
            "task",
            &json!({"context":"test","tasks":[{"role":"scout","task":"inspect"}]}).to_string(),
            true,
            &Default::default(),
        )
        .unwrap();
    let state = lock(&binding.inner.state);
    let spec = &state.disk.agents["a1"].spec;
    assert_eq!(spec.model, "fixture/main");
    assert_eq!(spec.model_role, None);
    assert!(spec.role_route.is_none());
    assert_eq!(spec.definition, "scout");
    assert_eq!(spec.prompt, "Configured investigation prompt.");
    assert!(spec.readonly);
    assert!(spec.tools.iter().any(|tool| tool == "mcp__remote__echo"));
    assert!(!spec.tools.iter().any(|tool| tool == "mcp__remote__fail"));
}

#[test]
fn custom_agent_discovery_preserves_project_priority_and_disabled_definitions() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".pp/agents");
    let user = dir.path().join("user/agents");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&user).unwrap();
    for (path, label) in [(&project, "project"), (&user, "user")] {
        fs::write(
            path.join("analyst.md"),
            format!("---\nname: analyst\ndescription: {label}\ntools: []\n---\n{label} prompt"),
        )
        .unwrap();
    }
    fs::write(project.join("invalid.md"), "invalid definition").unwrap();
    let binding = binding(
        dir.path(),
        TaskSettings {
            disabled_agents: vec!["hidden".into()],
            ..Default::default()
        },
    );
    fs::write(
        project.join("hidden.md"),
        "---\nname: hidden\ndescription: hidden marker\n---\nHidden prompt",
    )
    .unwrap();
    for name in ["fast", "Default", "plan"] {
        fs::write(
            project.join(format!("{name}.md")),
            format!("---\nname: {name}\ndescription: reserved role\n---\nIgnored prompt"),
        )
        .unwrap();
    }
    let custom = binding.custom_roles();
    assert_eq!(custom.len(), 1);
    assert_eq!(custom[0].name, "analyst");
    assert_eq!(custom[0].description, "project");
    assert_eq!(custom[0].tools, Some(vec![]));
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert_eq!(
        schema
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["fast", "analyst"])
    );
    assert!(
        binding
            .execute(
                "task",
                &json!({"context":"test","tasks":[{"role":"hidden","task":"reject"}]}).to_string(),
                false,
                &Default::default()
            )
            .is_err()
    );
}

#[test]
fn custom_role_uses_model_override_and_works_without_builtin_roles() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".pp/agents");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("analyst.md"), "---\nname: analyst\ndescription: custom model marker\nmodel: fixture/definition\nthinking: high\ntools: [read, rg]\nspawns: []\noutput: {type: object}\n---\nCustom analysis prompt.").unwrap();
    let binding = binding(
        dir.path(),
        TaskSettings {
            agent_models: BTreeMap::from([
                ("analyst".into(), "fixture/override".into()),
                ("ghost".into(), "fixture/ghost".into()),
            ]),
            ..Default::default()
        },
    );
    binding.set_model_roles(BTreeMap::new());
    let schema = binding.definitions(WorkMode::Execute)[0]
        .parameters
        .clone()
        .unwrap();
    assert_eq!(
        schema
            .pointer("/properties/tasks/items/properties/role/enum")
            .unwrap(),
        &json!(["analyst"])
    );
    assert!(
        binding
            .execute(
                "task",
                &json!({"context":"test","tasks":[{"role":"ghost","task":"reject fabricated definition"}]}).to_string(),
                false,
                &Default::default(),
            )
            .is_err()
    );
    binding
        .execute(
            "task",
            &json!({"context":"test","tasks":[{"role":"analyst","task":"custom route"}]})
                .to_string(),
            false,
            &Default::default(),
        )
        .unwrap();
    let state = lock(&binding.inner.state);
    let custom = &state.disk.agents["a1"].spec;
    assert_eq!(custom.model, "fixture/override");
    assert_eq!(custom.thinking, ThinkLevel::High);
    assert_eq!(custom.model_role, None);
    assert!(custom.role_route.is_none());
    assert_eq!(custom.output_schema, Some(json!({"type":"object"})));
    assert_eq!(custom.prompt, "Custom analysis prompt.");
    assert_eq!(custom.tools, vec!["read", "rg"]);
}

#[test]
fn task_argument_errors_explain_nesting_without_dispatching_children() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        pp_tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(Arc::new(binding.clone()));
    for arguments in [
        json!({"context":"test","role":"fast","task":"flat task"}).to_string(),
        json!({"context":"test","tasks":[{"role":"fast","task":"valid"}],"role":"fast"})
            .to_string(),
        json!({"context":"test","tasks":[{"task":"missing role"}]}).to_string(),
        json!({"context":"test","tasks":[{"role":"fast","task":"valid","agent":"fast"}]})
            .to_string(),
        "{".to_owned(),
    ] {
        let result = tools.execute(
            "task",
            &arguments,
            "invalid-task",
            pp_tools::ToolPolicy::Execute {
                workflow: &Default::default(),
            },
            &Default::default(),
            &mut |_| {},
        );
        let output: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(output["ok"], false, "{output}");
        let error = output["error"].as_str().unwrap();
        for field in ["context", "tasks[]", "role", "task"] {
            assert!(error.contains(field), "缺少参数层级指引 {field}: {error}");
        }
        let state = lock(&binding.inner.state);
        assert!(state.disk.agents.is_empty());
        assert!(state.disk.runs.is_empty());
        assert_eq!(state.disk.next_id, 0);
        assert_eq!(state.workers, 0);
    }
    let result = binding
        .execute(
            "task",
            r#"{"context":"test","tasks":[{"role":"fast","task":"corrected task"}]}"#,
            false,
            &Default::default(),
        )
        .unwrap();
    assert_eq!(result["tasks"][0]["agent_id"], "a1");
    assert_eq!(result["wait_args"], json!({"run_ids":["r1"]}));
    assert_eq!(
        lock(&binding.inner.state).disk.agents["a1"].spec.definition,
        "fast"
    );
    binding.shutdown().unwrap();
}

#[test]
fn invalid_batch_has_no_dispatch_or_reserved_ids() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    for invalid in [
        json!({"task":"missing role"}),
        json!({"agent":"fast","task":"legacy selector"}),
        json!({"role":"missing","task":"unknown role"}),
        json!({"role":null,"task":"null role"}),
    ] {
        let result = binding.execute(
            "task",
            &json!({"context":"test","tasks":[{"role":"fast","task":"valid"},invalid]}).to_string(),
            false,
            &Default::default(),
        );
        assert!(result.is_err());
        let state = lock(&binding.inner.state);
        assert!(state.disk.agents.is_empty());
        assert!(state.disk.runs.is_empty());
        assert_eq!(state.disk.next_id, 0);
        assert_eq!(state.workers, 0);
    }
}
#[test]
fn delivery_is_inspectable_until_ack_and_scoped_to_owner() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let run = Run {
        id: "r1".into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: "completed".into(),
        task: "test".into(),
        result: Some(json!({"ok":true,"output":"done"})),
        consumed: false,
        duration_ms: 0,
        started_ms: 0,
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
        TaskSettings {
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
fn run_permit_queues_initialization_and_releases_after_failure_or_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(
        dir.path(),
        TaskSettings {
            max_concurrency: 1,
            ..Default::default()
        },
    );
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    *lock(&binding.inner.factory) = Some(Arc::new(move |record, _| {
        entered_tx.send(record.id.clone())?;
        lock(&release_rx).recv_timeout(Duration::from_secs(5))?;
        bail!("fixture initialization failure")
    }));
    binding
        .execute(
            "task",
            &json!({"context":"verify run permits", "tasks":[
                {"role":"fast","task":"first"}, {"role":"fast","task":"second"}, {"role":"fast","task":"third"}
            ]})
            .to_string(),
            false,
            &Default::default(),
        )
        .unwrap();
    let first = entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let (cancelled, next) = {
        let state = lock(&binding.inner.state);
        assert_eq!(binding.inner.semaphore.available_permits(), 0);
        assert_eq!(
            state
                .disk
                .agents
                .values()
                .filter(|a| a.status == "running")
                .count(),
            1
        );
        let queued = state
            .disk
            .runs
            .values()
            .filter(|r| r.status == "queued")
            .collect::<Vec<_>>();
        assert_eq!(queued.len(), 2);
        assert!(
            queued
                .iter()
                .all(|r| state.disk.agents[&r.agent_id].status == "queued")
        );
        (queued[0].id.clone(), queued[1].agent_id.clone())
    };
    binding
        .cancel_runs(std::slice::from_ref(&cancelled))
        .unwrap();
    binding.reap_runs(std::slice::from_ref(&cancelled)).unwrap();
    assert_eq!(
        lock(&binding.inner.state).disk.runs[&cancelled].status,
        "cancelled"
    );
    assert!(entered_rx.try_recv().is_err());
    release_tx.send(()).unwrap();
    assert_eq!(
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        next
    );
    // 已有 worker 的后续 Run 同样排队，不能绕过当前占用的名额。
    let followup = binding.control(&first, "send", Some("follow-up")).unwrap();
    let followup = followup["run_id"].as_str().unwrap();
    {
        let state = lock(&binding.inner.state);
        assert_eq!(state.disk.runs[followup].status, "queued");
        assert_eq!(state.disk.agents[&first].status, "queued");
        let running = state
            .disk
            .runs
            .values()
            .find(|r| r.agent_id == next)
            .unwrap()
            .id
            .clone();
        drop(state);
        binding.cancel_runs(&[running]).unwrap();
    }
    release_tx.send(()).unwrap();
    assert_eq!(
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        first
    );
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut state = lock(&binding.inner.state);
    while state
        .disk
        .runs
        .values()
        .any(|r| matches!(r.status.as_str(), "queued" | "running"))
    {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .expect("Run 未及时结束");
        state = binding
            .inner
            .changed
            .wait_timeout(state, remaining)
            .unwrap()
            .0;
    }
    assert_eq!(state.disk.runs[followup].status, "failed");
    assert_eq!(
        state
            .disk
            .runs
            .values()
            .filter(|r| r.status == "cancelled")
            .count(),
        2
    );
    drop(state);
    binding.shutdown().unwrap();
    assert_eq!(binding.inner.semaphore.available_permits(), 1);
}

#[test]
fn persisted_running_work_becomes_interrupted_without_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let run = Run {
        id: "r1".into(),
        agent_id: "a1".into(),
        owner: "Main".into(),
        status: "running".into(),
        task: "test".into(),
        result: None,
        consumed: false,
        duration_ms: 0,
        started_ms: 0,
        requests: 0,
        tokens: 0,
        cost: None,
    };
    {
        let mut state = lock(&binding.inner.state);
        state.disk.runs.insert("r1".into(), run);
        binding.inner.save(&state).unwrap();
    }
    let restored = super::tests::binding(dir.path(), TaskSettings::default());
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
    let patches = workspace::capture(&artifacts, "a1", true, &Default::default()).unwrap();
    assert!(!patches.is_empty());
    assert_eq!(fs::read(nested.join("binary")).unwrap(), [0, 255, 9, 8]);
    assert!(!nested.join("deleted").exists());
    assert!(
        workspace::capture(&artifacts, "a1", true, &Default::default())
            .unwrap()
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
                started_ms: 0,
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
        model_role: None,
        role_route: None,
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
            registered_ms: None,
            updated_ms: 0,
        },
    );
    assert!(binding.control("a1", "revive", None).is_err());
    assert_eq!(lock(&binding.inner.state).workers, 0);
}

fn spec_json(cost: Value) -> Value {
    json!({
        "definition": "task", "prompt": "", "context": "", "model": "m",
        "thinking": "none", "tools": [], "spawns": [], "output_schema": null,
        "cwd": ".", "depth": 1, "readonly": false, "isolated": false,
        "cost": cost,
    })
}

#[test]
fn legacy_registry_with_null_cache_write_still_loads() {
    // 旧版本把缺省的 cacheWrite 写成 null，/resume 必须仍能读取子 Agent 注册表。
    let spec: Spec = serde_json::from_value(spec_json(
        json!({"input":0.3,"cacheRead":0.006,"cacheWrite":null,"output":1.2}),
    ))
    .unwrap();
    let cost = spec.cost.unwrap();
    assert_eq!(cost.cache_write, None);
    assert_eq!(cost.output, 1.2);
    // 其余字段仍严格校验：缺必填项依旧报错。
    assert!(
        serde_json::from_value::<Spec>(spec_json(json!({"input":1,"cacheWrite":null,"output":1})))
            .is_err()
    );
}

#[test]
fn persisted_cost_omits_unknown_cache_write_and_round_trips() {
    let spec: Spec = serde_json::from_value(spec_json(
        json!({"input":0.3,"cacheRead":0.006,"output":1.2}),
    ))
    .unwrap();
    let written = serde_json::to_value(&spec).unwrap();
    assert!(written["cost"].get("cacheWrite").is_none());
    let again: Spec = serde_json::from_value(written).unwrap();
    assert_eq!(again.cost.unwrap().cache_write, None);
    let priced: Spec = serde_json::from_value(spec_json(
        json!({"input":1,"cacheRead":1,"cacheWrite":2.5,"output":1}),
    ))
    .unwrap();
    assert_eq!(
        serde_json::to_value(&priced).unwrap()["cost"]["cacheWrite"],
        2.5
    );
}

#[test]
fn run_cost_follows_the_authoritative_unknown_cache_rules() {
    use pp_core::context::TokenUsage;
    let priced = pp_config::models::ModelCost {
        input: 2.0,
        cache_read: 0.2,
        cache_write: Some(3.0),
        output: 8.0,
    };
    let no_write_price = pp_config::models::ModelCost {
        cache_write: None,
        ..priced
    };
    let usage = TokenUsage {
        input_tokens: 1000,
        output_tokens: 100,
        cached_tokens: Some(200),
        cache_write_tokens: Some(100),
        reasoning_tokens: None,
    };
    // 700 未缓存 + 200 缓存读 + 100 缓存写 + 100 输出。
    let total = accumulate_cost(Some(0.001), Some(priced), usage).unwrap();
    assert!((total - 0.00354).abs() < 1e-12);
    // 未知缓存读、缺费率、已未知的小计都不能被补零。
    assert!(
        accumulate_cost(
            Some(0.0),
            Some(priced),
            TokenUsage {
                cached_tokens: None,
                ..usage
            }
        )
        .is_none()
    );
    assert!(accumulate_cost(Some(0.0), None, usage).is_none());
    assert!(accumulate_cost(None, Some(priced), usage).is_none());
    // 有独立写入价时未知写入量仍未定价；无写入价的费率把写入并入输入价。
    assert!(
        accumulate_cost(
            Some(0.0),
            Some(priced),
            TokenUsage {
                cache_write_tokens: None,
                ..usage
            }
        )
        .is_none()
    );
    // 无写入价时未知写入量按 0 计：其余 800 输入按输入价与缓存读价计算。
    let merged = accumulate_cost(
        Some(0.0),
        Some(no_write_price),
        TokenUsage {
            cache_write_tokens: None,
            ..usage
        },
    )
    .unwrap();
    assert!((merged - 0.00244).abs() < 1e-12);
    // 缓存读写超过总输入时检查溢出，不产生负数金额。
    assert!(
        accumulate_cost(
            Some(0.0),
            Some(priced),
            TokenUsage {
                input_tokens: 10,
                ..usage
            }
        )
        .is_none()
    );
}
