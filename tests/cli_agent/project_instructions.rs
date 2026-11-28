use super::streaming::{LiveCli, Pipe};
use super::*;
use std::{fs, path::Path};

pub(super) fn receive_for(server: &TestServer, protocol: &str) -> (CapturedRequest, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}

pub(super) fn system_prefix<'a>(body: &'a Value, protocol: &str) -> &'a str {
    if protocol == "completion" {
        body["messages"][0]["content"].as_str().unwrap()
    } else {
        body["instructions"].as_str().unwrap()
    }
}

/// 返回最新送达模型的项目规则块（有动态 `[pp-project-instructions]` developer 增量时取最新一条，否则取静态 system）。
pub(super) fn latest_instructions<'a>(body: &'a Value, protocol: &str) -> &'a str {
    history(body, protocol)
        .iter()
        .rev()
        .find_map(|item| {
            (item["role"].as_str() == Some("developer"))
                .then(|| item["content"].as_str())
                .flatten()
                .filter(|text| text.starts_with("[pp-project-instructions]\n"))
        })
        .unwrap_or_else(|| system_prefix(body, protocol))
}

pub(super) fn system(body: &Value, protocol: &str) -> String {
    let mut combined = system_prefix(body, protocol).to_owned();
    for item in history(body, protocol) {
        if item["role"].as_str() == Some("developer")
            && let Some(text) = item["content"].as_str()
            && text.starts_with("[pp-project-instructions]\n")
        {
            combined.push('\n');
            combined.push_str(text);
        }
    }
    combined
}

fn history<'a>(body: &'a Value, protocol: &str) -> &'a [Value] {
    body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap()
}

pub(super) fn finish_request(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn tool_call(request: CapturedRequest, protocol: &str, id: &str, name: &str, args: Value) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}

pub(super) fn read_project_file(request: CapturedRequest, protocol: &str, path: &str) {
    tool_call(
        request,
        protocol,
        "activate-rules",
        "read",
        json!({"path":path}),
    );
}

pub(super) fn activate_rules(
    server: &TestServer,
    protocol: &str,
    request: CapturedRequest,
) -> (CapturedRequest, Value) {
    read_project_file(request, protocol, ".pp/agent/settings.yml");
    let (request, body) = receive_for(server, protocol);
    assert_tool_ok(&body, protocol, "activate-rules");
    (request, body)
}

fn tool_batch(request: CapturedRequest, protocol: &str, calls: &[(&str, &str, Value)]) {
    if protocol == "completion" {
        calls_reply(
            request,
            Value::Array(
                calls
                    .iter()
                    .map(|(id, name, args)| call(id, name, args.clone()))
                    .collect(),
            ),
        );
    } else {
        response_reply(
            request,
            Value::Array(
                calls
                    .iter()
                    .map(|(id, name, args)| response_call(id, name, args.clone()))
                    .collect(),
            ),
        );
    }
}

fn tool_output(body: &Value, protocol: &str, id: &str) -> Value {
    let item = history(body, protocol)
        .iter()
        .find(|item| {
            item["tool_call_id"] == id
                || item["call_id"] == id && item["type"] == "function_call_output"
        })
        .unwrap();
    if protocol == "completion" {
        result(item, id)
    } else {
        response_result(item, id)
    }
}

fn assert_rules_blocked(body: &Value, protocol: &str, id: &str) {
    let output = tool_output(body, protocol, id);
    assert_eq!(output["ok"], false, "{output}");
    assert!(
        output["error"].as_str().unwrap().contains("Not executed:"),
        "{output}"
    );
}

#[test]
fn agents_layered_external_reads_keep_scopes_and_write_limits_both_protocols() {
    let protocol = "response";
    let server = server();
    let parent = tempfile::tempdir().unwrap();
    let cwd = parent.path().join("main");
    let external = parent.path().join("external");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(external.join(".git")).unwrap();
    fs::write(cwd.join("AGENTS.md"), "main-project-rule").unwrap();
    fs::write(external.join("AGENTS.md"), "foreign-project-rule").unwrap();
    fs::write(external.join("code.txt"), "external-content").unwrap();
    fs::write(parent.path().join("AGENTS.md"), "unrelated-parent-rule").unwrap();
    fs::write(parent.path().join("loose.txt"), "loose-content").unwrap();
    let cli = Cli::start_with(
        &cwd,
        Some("inspect external files"),
        fixture_settings(),
        &response_models(&server, Some(protocol)),
    );
    let (request, _) = receive_for(&server, protocol);
    tool_batch(
        request,
        protocol,
        &[
            (
                "external-read",
                "read",
                json!({"path":external.join("code.txt")}),
            ),
            (
                "loose-read",
                "read",
                json!({"path":parent.path().join("loose.txt")}),
            ),
        ],
    );
    let (request, body) = receive_for(&server, protocol);
    assert_tool_ok(&body, protocol, "external-read");
    assert_tool_ok(&body, protocol, "loose-read");
    let text = system(&body, protocol);
    assert!(text.contains("main-project-rule") && text.contains("foreign-project-rule"));
    assert!(!text.contains("unrelated-parent-rule"));
    assert!(text.contains(external.canonicalize().unwrap().to_str().unwrap()));
    tool_call(
        request,
        protocol,
        "escape",
        "write",
        json!({"path":"../external/code.txt","content":"forbidden"}),
    );
    let (request, body) = receive_for(&server, protocol);
    assert_eq!(tool_output(&body, protocol, "escape")["ok"], false);
    assert_eq!(
        fs::read_to_string(external.join("code.txt")).unwrap(),
        "external-content"
    );
    finish_request(request, protocol, "done");
    assert!(cli.finish().status.success());
}

#[test]
fn agents_layered_unseen_rules_block_whole_batch_without_creating_directories_both_protocols() {
    let protocol = "response";
    let server = server();
    let cwd = tempfile::tempdir().unwrap();
    fs::write(cwd.path().join("AGENTS.md"), "unseen-root-rule").unwrap();
    fs::create_dir(cwd.path().join("child")).unwrap();
    fs::write(cwd.path().join("child/AGENTS.md"), "unseen-child-rule").unwrap();
    fs::write(cwd.path().join("child/code.txt"), "before").unwrap();
    let mut cli = start(&server, cwd.path(), protocol);
    cli.input("write files\n");
    let (request, body) = receive_for(&server, protocol);
    assert!(system(&body, protocol).contains("unseen-root-rule"));
    assert!(!system(&body, protocol).contains("unseen-child-rule"));
    tool_batch(
        request,
        protocol,
        &[
            (
                "early-write",
                "write",
                json!({"path":"early.txt","content":"must-not-write"}),
            ),
            ("same-read", "read", json!({"path":"child/code.txt"})),
            (
                "late-write",
                "write",
                json!({"path":"child/new/deep/file.txt","content":"new"}),
            ),
        ],
    );
    let (request, body) = receive_for(&server, protocol);
    for id in ["early-write", "same-read", "late-write"] {
        assert_rules_blocked(&body, protocol, id);
    }
    assert!(system(&body, protocol).contains("unseen-root-rule"));
    assert!(system(&body, protocol).contains("unseen-child-rule"));
    assert!(!cwd.path().join("early.txt").exists());
    assert!(!cwd.path().join("child/new").exists());
    tool_call(
        request,
        protocol,
        "retry-write",
        "write",
        json!({"path":"child/new/deep/file.txt","content":"reconsidered"}),
    );
    let (request, body) = receive_for(&server, protocol);
    assert_tool_ok(&body, protocol, "retry-write");
    assert_eq!(
        fs::read_to_string(cwd.path().join("child/new/deep/file.txt")).unwrap(),
        "reconsidered"
    );
    assert!(!cwd.path().join("early.txt").exists());
    finish_request(request, protocol, "done");
    cli.wait_for(Pipe::Stdout, "done");
    cli.input("/quit\n");
    assert!(cli.finish().status.success());
}

fn assert_tool_ok(body: &Value, protocol: &str, id: &str) {
    let item = history(body, protocol)
        .iter()
        .find(|item| {
            item["tool_call_id"] == id
                || item["call_id"] == id && item["type"] == "function_call_output"
        })
        .unwrap();
    let result = if protocol == "completion" {
        result(item, id)
    } else {
        response_result(item, id)
    };
    assert_eq!(result["ok"], true, "{result}");
}

fn start(server: &TestServer, cwd: &Path, protocol: &str) -> LiveCli {
    LiveCli::new(Cli::start_with(
        cwd,
        None,
        fixture_settings(),
        &response_models(server, Some(protocol)),
    ))
}

fn journal(cwd: &Path) -> std::path::PathBuf {
    let entries: Vec<_> = fs::read_dir(cwd.join(".pp/agent/sessions"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1);
    entries[0].clone()
}

#[test]
fn agents_refreshes_after_tool_create_edit_and_delete_both_protocols() {
    let protocol = "response";
    let server = server();
    let cwd = tempfile::tempdir().unwrap();
    let mut cli = start(&server, cwd.path(), protocol);
    cli.input("maintain rules\n");
    let (request, body) = receive_for(&server, protocol);
    assert!(!system(&body, protocol).contains("[Project instructions:"));
    tool_call(
        request,
        protocol,
        "create",
        "write",
        json!({"path":"AGENTS.md","content":"agents-created-marker"}),
    );
    let (request, body) = receive_for(&server, protocol);
    assert_tool_ok(&body, protocol, "create");
    assert_eq!(
        system(&body, protocol)
            .matches("agents-created-marker")
            .count(),
        1
    );
    tool_call(
        request,
        protocol,
        "update",
        "edit",
        json!({"path":"AGENTS.md","edits":[{"oldText":"agents-created-marker","newText":"agents-updated-marker"}]}),
    );
    let (request, body) = receive_for(&server, protocol);
    assert_tool_ok(&body, protocol, "update");
    let latest = latest_instructions(&body, protocol);
    assert!(!latest.contains("agents-created-marker"));
    assert_eq!(latest.matches("agents-updated-marker").count(), 1);
    tool_call(
        request,
        protocol,
        "delete",
        "bash",
        json!({"command":"rm AGENTS.md"}),
    );
    let (request, body) = receive_for(&server, protocol);
    assert_tool_ok(&body, protocol, "delete");
    assert!(latest_instructions(&body, protocol).contains("[Project instructions removed:"));
    let final_system = latest_instructions(&body, protocol).to_owned();
    finish_request(request, protocol, "rules maintained");
    cli.input("/quit\n");
    assert!(cli.finish().status.success());
    let records = records(&journal(cwd.path()));
    let changes: Vec<_> = records
        .iter()
        .filter(|record| record["customType"] == "pp.instructions")
        .collect();
    // 已有历史后的创建/更新/删除均走尾部 developer 消息增量送达，不改写首条 system 前缀。
    assert!(changes.is_empty(), "{changes:?}");
    let append_key = if protocol == "completion" {
        "messages"
    } else {
        "items"
    };
    let dynamic_updates: Vec<_> = records
        .iter()
        .filter(|record| record["customType"] == "pp.native_append")
        .filter_map(|record| record["data"][append_key].as_array())
        .flatten()
        .filter_map(|item| {
            (item["role"].as_str() == Some("developer"))
                .then(|| item["content"].as_str())
                .flatten()
                .filter(|text| text.starts_with("[pp-project-instructions]\n"))
        })
        .collect();
    assert_eq!(dynamic_updates.len(), 3, "{dynamic_updates:?}");
    assert_eq!(*dynamic_updates.last().unwrap(), final_system);
    let state = records
        .iter()
        .find(|record| record["customType"] == "pp.session_state")
        .unwrap();
    assert!(
        !state["data"]["baseSystem"]
            .as_str()
            .unwrap()
            .contains("agents-created-marker")
    );
}
