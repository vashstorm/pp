use super::*;
use crate::subagent::tests::binding;
use std::sync::mpsc;

#[test]
fn killed_subagent_stale_settle_keeps_aborted_status_and_blocks_idle_event() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let agent_id = "a1".to_owned();
    let run_id = "r1".to_owned();
    let mut record = Record {
        id: agent_id.clone(),
        name: "fast".into(),
        parent: "Main".into(),
        generation: 1,
        status: "running".into(),
        spec: Spec {
            model_role: Some(ModelRole::Fast),
            role_route: None,
            definition: "fast".into(),
            prompt: String::new(),
            context: String::new(),
            model: "fixture/main".into(),
            thinking: ThinkLevel::None,
            tools: vec!["read".into()],
            spawns: Vec::new(),
            output_schema: None,
            cwd: dir.path().to_path_buf(),
            depth: 1,
            readonly: true,
            isolated: false,
            cost: None,
        },
        session_file: None,
        registered_ms: Some(now()),
        updated_ms: now(),
    };
    let run = Run {
        id: run_id.clone(),
        agent_id: agent_id.clone(),
        owner: "Main".into(),
        status: "running".into(),
        task: "work".into(),
        result: None,
        consumed: false,
        duration_ms: 0,
        started_ms: now(),
        requests: 0,
        tokens: 0,
        cost: None,
    };
    let token = CancellationToken::new();
    {
        let mut state = lock(&binding.inner.state);
        state.disk.agents.insert(agent_id.clone(), record.clone());
        state.disk.runs.insert(run_id.clone(), run);
        let (sender, _) = mpsc::channel();
        state.live.insert(
            agent_id.clone(),
            Live {
                sender,
                input: InputQueue::default(),
                cancellation: token.clone(),
                generation: 1,
            },
        );
    }

    // kill 将状态设为 aborted 并将 generation 递增到 2。
    binding.control(&agent_id, "kill", None).unwrap();
    assert!(token.is_cancelled());

    // 模拟旧 generation=1 worker 迟到退出调用 settle(..., Some(1))。
    binding
        .settle(
            &run_id,
            json!({"ok": false, "cancelled": true}),
            25,
            Some(1),
        )
        .unwrap();

    let state = lock(&binding.inner.state);
    record = state.disk.agents[&agent_id].clone();
    assert_eq!(record.status, "aborted");
    assert_eq!(record.generation, 2);
    assert_eq!(state.disk.runs[&run_id].status, "cancelled");
    drop(state);

    // 观察日志末尾不得出现将 status 改回 idle 的事件。
    let snap = binding.hub_snapshot();
    let agent = snap
        .agents
        .iter()
        .find(|a| a.record.id == agent_id)
        .unwrap();
    assert_eq!(agent.record.status, "aborted");
}
