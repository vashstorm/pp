use super::*;
use pp_test_support::TestServer;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn agent_persists_eval_state_across_turns_and_resets_on_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = Arc::new(TestServer::start().unwrap());
    let mut agent = runtime_input_agent(dir.path(), "response", &server.addr().to_string(), "");
    if !agent.tools.eval_available() {
        return;
    }

    let worker = Arc::clone(&server);
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let server_thread = std::thread::spawn(move || {
        // 第 1 轮：设置 Python 变量 counter = 41
        let r1 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r1.json().unwrap()).unwrap();
        tool_call_for(
            "response",
            r1,
            "eval",
            r#"{"language":"py","title":"init","code":"counter = 41"}"#,
        );
        let r2 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r2.json().unwrap()).unwrap();
        final_for("response", r2, "turn 1 done", true);

        // 第 2 轮：同一会话读取并递增 counter
        let r3 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r3.json().unwrap()).unwrap();
        tool_call_for(
            "response",
            r3,
            "eval",
            r#"{"language":"py","code":"counter + 1"}"#,
        );
        let r4 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r4.json().unwrap()).unwrap();
        final_for("response", r4, "turn 2 done", true);

        // 第 3 轮：/new 重置后 counter 应触发 NameError
        let r5 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r5.json().unwrap()).unwrap();
        tool_call_for(
            "response",
            r5,
            "eval",
            r#"{"language":"py","code":"counter"}"#,
        );
        let r6 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r6.json().unwrap()).unwrap();
        final_for("response", r6, "after reset", true);
    });

    assert_eq!(
        agent
            .run("set counter", &CancellationToken::new(), None, &mut |_| {})
            .unwrap(),
        RunOutcome::Completed
    );
    assert_eq!(
        agent
            .run("read counter", &CancellationToken::new(), None, &mut |_| {})
            .unwrap(),
        RunOutcome::Completed
    );

    agent.reset();
    assert_eq!(
        agent
            .run("check reset", &CancellationToken::new(), None, &mut |_| {})
            .unwrap(),
        RunOutcome::Completed
    );

    server_thread.join().unwrap();
    let _req1 = rx.recv().unwrap();
    let req2 = rx.recv().unwrap();
    let _req3 = rx.recv().unwrap();
    let req4 = rx.recv().unwrap();
    let _req5 = rx.recv().unwrap();
    let req6 = rx.recv().unwrap();

    let out1 = req2["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let v1: Value = serde_json::from_str(out1).unwrap();
    assert_eq!(v1["ok"], true);

    let out2 = req4["input"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let v2: Value = serde_json::from_str(out2).unwrap();
    assert_eq!(v2["ok"], true);
    assert_eq!(v2["output"], "42\n");

    let out3 = req6["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let v3: Value = serde_json::from_str(out3).unwrap();
    assert_eq!(v3["ok"], false);
    assert!(v3["output"].as_str().unwrap().contains("NameError"));
}

#[test]
fn agent_hides_eval_when_disabled_in_settings() {
    let dir = tempfile::tempdir().unwrap();
    let agent = runtime_settings_agent(
        dir.path(),
        "completion",
        "127.0.0.1:1",
        "eval:\n  enabled: false\n",
    );
    assert!(!agent.tools.eval_available());
    assert!(!agent.session.tool_names().iter().any(|name| name == "eval"));
    assert!(!agent.base_system.contains("Prefer eval over"));
}

#[test]
fn agent_persists_js_eval_state_across_turns_and_resets_on_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = Arc::new(TestServer::start().unwrap());
    let mut agent = runtime_input_agent(dir.path(), "response", &server.addr().to_string(), "");
    if !agent.tools.eval_js_available() {
        return;
    }

    let worker = Arc::clone(&server);
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let server_thread = std::thread::spawn(move || {
        let r1 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r1.json().unwrap()).unwrap();
        tool_call_for(
            "response",
            r1,
            "eval",
            r#"{"language":"js","title":"init","code":"const counter = 41;"}"#,
        );
        let r2 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r2.json().unwrap()).unwrap();
        final_for("response", r2, "turn 1 done", true);

        let r3 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r3.json().unwrap()).unwrap();
        tool_call_for(
            "response",
            r3,
            "eval",
            r#"{"language":"js","code":"counter + 1"}"#,
        );
        let r4 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r4.json().unwrap()).unwrap();
        final_for("response", r4, "turn 2 done", true);

        let r5 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r5.json().unwrap()).unwrap();
        tool_call_for(
            "response",
            r5,
            "eval",
            r#"{"language":"js","code":"counter"}"#,
        );
        let r6 = worker
            .recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap();
        tx.send(r6.json().unwrap()).unwrap();
        final_for("response", r6, "after reset", true);
    });

    assert_eq!(
        agent
            .run("set counter", &CancellationToken::new(), None, &mut |_| {})
            .unwrap(),
        RunOutcome::Completed
    );
    assert_eq!(
        agent
            .run("read counter", &CancellationToken::new(), None, &mut |_| {})
            .unwrap(),
        RunOutcome::Completed
    );

    agent.reset();
    assert_eq!(
        agent
            .run("check reset", &CancellationToken::new(), None, &mut |_| {})
            .unwrap(),
        RunOutcome::Completed
    );

    server_thread.join().unwrap();
    let _req1 = rx.recv().unwrap();
    let req2 = rx.recv().unwrap();
    let _req3 = rx.recv().unwrap();
    let req4 = rx.recv().unwrap();
    let _req5 = rx.recv().unwrap();
    let req6 = rx.recv().unwrap();

    let out1 = req2["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let v1: Value = serde_json::from_str(out1).unwrap();
    assert_eq!(v1["ok"], true, "{v1}");

    let out2 = req4["input"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let v2: Value = serde_json::from_str(out2).unwrap();
    assert_eq!(v2["ok"], true, "{v2}");
    assert_eq!(v2["output"], "42\n");

    let out3 = req6["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap()["output"]
        .as_str()
        .unwrap();
    let v3: Value = serde_json::from_str(out3).unwrap();
    assert_eq!(v3["ok"], false, "{v3}");
    assert!(
        v3["output"].as_str().unwrap().contains("ReferenceError"),
        "{v3}"
    );
}
