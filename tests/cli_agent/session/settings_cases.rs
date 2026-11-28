use super::*;

#[test]
fn session_settings_persist_across_model_switch_and_restore() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let models = json!({"providers":{
        "a": slash_provider(&server, "a", "completion", json!({
            "primary": {"model":"a-one"},"secondary": {"model":"a-two"}})),
        "b": slash_provider(&server, "b", "response", json!({"primary": {"model":"b-one"}}))}})
    .to_string();
    configure(home.path(), r#"{"default_model":"a/primary"}"#, &models);

    let mut cli = start(home.path(), project.path(), &[]);
    cli.input("first\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "a-one");
    Api::Chat.assert_level(&body, "none");
    final_reply(request, "first answer");
    cli.input("/effort max\n/model a/secondary\n");
    cli.wait_for(Pipe::Err, "Current model: a/secondary (a-two)");
    cli.input("second\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "a-two");
    Api::Chat.assert_level(&body, "max");
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["first", "second"]);
    assert_eq!(assistants, ["first answer"]);
    final_reply(request, "second answer");
    cli.input("/quit\n");
    let first_output = cli.finish();
    assert!(
        first_output.status.success(),
        "{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let id = only_session_id(home.path());
    let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
    let same_protocol = records(&path);
    assert_session_contract(&same_protocol, &id, project.path());
    let saved_request = rebuild_request(&same_protocol);
    assert_eq!(saved_request["model"], "a-two");
    Api::Chat.assert_level(&saved_request, "max");
    let same_protocol_messages = canonical_messages(&same_protocol);
    assert_eq!(same_protocol_messages.len(), 4);
    let mut cli = start(home.path(), launch.path(), &["-r", &id]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
    no_more(&server);
    // 新设计：手动覆盖 a 只在内存，重启后回到配置默认 b（settings 的 a/primary）；
    // checkpoint 的 modelReference 只作审计，恢复时按实际生效模型补一条审计记录，
    // 已保存的原生历史保持不变。
    let resumed = records(&path);
    assert_eq!(&resumed[..same_protocol.len()], same_protocol.as_slice());
    let audit = &resumed[same_protocol.len()..];
    assert_eq!(
        audit.len(),
        2,
        "restore must append one audit checkpoint pair"
    );
    assert_eq!(audit[0]["type"], "model_change");
    assert_eq!(audit[0]["model"], "a/primary");
    assert_eq!(audit[1]["customType"], "pp.session_state");
    assert_eq!(audit[1]["data"]["modelReference"], "a/primary");
    assert_eq!(audit[1]["data"]["thinkLevel"], "max");
    assert_eq!(audit[1]["data"]["session"]["request"]["model"], "a-one");
    assert_eq!(canonical_messages(&resumed), same_protocol_messages);
    cli.input("/model b/primary\n");
    cli.wait_for(Pipe::Err, "Current model: b/primary (b-one)");
    cli.input("third\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "b-one");
    Api::Responses.assert_level(&body, "max");
    let (users, assistants) = Api::Responses.turns(&body);
    assert_eq!(users, ["first", "second", "third"]);
    assert_eq!(assistants, ["first answer", "second answer"]);
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("coding assistant")
    );
    response_final(request, "third answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        [first_output.stdout, output.stdout].concat(),
        b"first answer\nsecond answer\nthird answer\n"
    );

    let saved = records(&path);
    assert_session_contract(&saved, &id, project.path());
    assert_eq!(&saved[..same_protocol.len()], same_protocol.as_slice());
    let messages = canonical_messages(&saved);
    assert_eq!(messages.len(), 6);
    assert_eq!(
        &messages[..same_protocol_messages.len()],
        same_protocol_messages.as_slice()
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "user")
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .map(|message| message["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a-one", "a-two", "b-one"]
    );
    // 审计轨迹：首次默认、手动切换、恢复回 b、再次手动切换。
    assert_eq!(
        saved
            .iter()
            .filter(|record| record["type"] == "model_change")
            .map(|record| record["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a/primary", "a/secondary", "a/primary", "b/primary"]
    );
    assert!(saved.iter().any(
        |record| record["type"] == "thinking_level_change" && record["thinkingLevel"] == "max"
    ));
    assert_eq!(
        saved
            .iter()
            .filter(|record| record["customType"] == "pp.session_state")
            .count(),
        4
    );

    // 再次重启仍回到 b（a/primary），保存的 Responses checkpoint 被转换回
    // completion 原生历史；模型与档位来自 resolve()，不再沿用上次手动切换。
    let cli = start(
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "after restore"],
    );
    let (request, body) = receive_at(&server, "/v1/chat/completions");
    assert_eq!(body["model"], "a-one");
    assert_eq!(body["reasoning_effort"], "max");
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["first", "second", "third", "after restore"]);
    assert_eq!(
        assistants,
        ["first answer", "second answer", "third answer"]
    );
    let system = body["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("coding assistant"),
        "restored system prompt: {system}"
    );
    final_reply(request, "restored answer");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"restored answer\n");
    let restored = records(&path);
    assert_session_contract(&restored, &id, project.path());
    assert_eq!(&restored[..saved.len()], saved.as_slice());
    // 恢复先补审计 checkpoint（当前生效模型），再追加本次新消息。
    let audit = &restored[saved.len()..];
    assert_eq!(audit[0]["type"], "model_change");
    assert_eq!(audit[0]["model"], "a/primary");
    assert_eq!(audit[1]["customType"], "pp.session_state");
    assert_eq!(audit[1]["data"]["modelReference"], "a/primary");
    assert_eq!(audit[1]["data"]["thinkLevel"], "max");
    assert_eq!(audit[1]["data"]["session"]["protocol"], "completion");
    assert_eq!(audit[1]["data"]["session"]["request"]["model"], "a-one");
    let restored_messages = canonical_messages(&restored);
    assert_eq!(&restored_messages[..messages.len()], messages.as_slice());
    assert_eq!(restored_messages.len(), 8);
    assert_eq!(restored_messages[6]["role"], "user");
    assert_eq!(restored_messages[6]["content"], "after restore");
    assert_eq!(restored_messages[7]["model"], "a-one");

    // Switching settings without an active session writes nothing.
    let idle = tempfile::tempdir().unwrap();
    configure(idle.path(), r#"{"default_model":"a/primary"}"#, &models);
    let mut cli = start(idle.path(), project.path(), &[]);
    cli.input("/effort high\n/model b/primary\n/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!sessions_dir(idle.path()).exists());
    no_more(&server);
}
