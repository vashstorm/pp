use super::*;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

const SKILL_NAME: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub(super) fn install(home: &std::path::Path, folder: &str, name: &str) -> std::path::PathBuf {
    let file = config_path(home, "skills").join(folder).join("SKILL.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    let body = (0..20)
        .map(|index| format!("secret-skill-line-{index:02}\n"))
        .collect::<String>();
    std::fs::write(
        &file,
        format!("---\nname: {name}\ndescription: Rust 测试指导\n---\n{body}"),
    )
    .unwrap();
    file
}
pub(super) fn send_skill_read(request: CapturedRequest, protocol: &str, name: &str) {
    let path = format!("skill://{}", utf8_percent_encode(name, SKILL_NAME));
    send_read(request, protocol, &path);
}
fn send_read(request: CapturedRequest, protocol: &str, path: &str) {
    if protocol == "response" {
        response_reply(
            request,
            json!([response_call("load", "read", json!({"path":path}))]),
        );
    } else {
        calls_reply(request, json!([call("load", "read", json!({"path":path}))]));
    }
}
fn finish_request(request: CapturedRequest, protocol: &str) {
    if protocol == "response" {
        response_final(request, "ok");
    } else {
        final_reply(request, "ok");
    }
}
fn loaded(body: &Value, protocol: &str) -> Value {
    if protocol == "response" {
        let item = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        response_result(item, "load")
    } else {
        let item = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|item| item["role"] == "tool")
            .unwrap();
        result(item, "load")
    }
}
fn assert_skill_read(output: &Value, uri: &str, file: &std::path::Path) {
    assert_eq!(output["ok"], true, "{output}");
    assert_eq!(output["source"], uri);
    assert_eq!(
        output["path"],
        file.canonicalize().unwrap().to_string_lossy().as_ref()
    );
    assert_eq!(
        output["base_dir"],
        file.parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(output["truncated"], false);
    let expected = std::fs::read_to_string(file).unwrap();
    let content = output["content"].as_str().unwrap();
    for line in expected.lines() {
        assert!(content.contains(line), "missing {line:?} in {output}");
    }
    assert_eq!(output["lines_read"], expected.lines().count());
}
fn system(body: &Value, protocol: &str) -> String {
    if protocol == "response" {
        body["instructions"].as_str().unwrap().into()
    } else {
        body["messages"][0]["content"].as_str().unwrap().into()
    }
}

pub(super) fn install_source(home: &std::path::Path, root: &str, name: &str) {
    let file = home.join(root).join(name).join("SKILL.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        file,
        format!("---\nname: {name}\ndescription: {name} guidance\n---\n{name} body\n"),
    )
    .unwrap();
}

#[test]
fn skills_load_on_demand_disable_persist_and_reset_in_both_protocols() {
    let protocol = "response";
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let file = install(home.path(), "rust", "rust");
    let path = if protocol == "response" {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    };
    let mut cli = Cli::start_with(
        home.path(),
        None,
        "# 保留配置注释\nthink_level: high\nread_max_lines: 2\nread_max_bytes: 8\n",
        &response_models(&server, Some(protocol)),
    );
    cli.input("first\n");
    let (request, body) = receive_at(&server, path);
    assert!(system(&body, protocol).contains("Rust 测试指导"));
    assert!(!body.to_string().contains("secret-skill-line"));
    send_skill_read(request, protocol, "rust");
    let (request, body) = receive_at(&server, path);
    let output = loaded(&body, protocol);
    assert_skill_read(&output, "skill://rust", &file);
    finish_request(request, protocol);
    cli.input("/skills disable rust\nsecond\n");
    let (request, body) = receive_at(&server, path);
    assert!(!system(&body, protocol).contains("Rust 测试指导"));
    assert!(body.to_string().contains("secret-skill-line-19"));
    send_skill_read(request, protocol, "rust");
    let (request, body) = receive_at(&server, path);
    assert_eq!(loaded(&body, protocol)["ok"], false);
    assert!(body.to_string().contains("secret-skill-line-19"));
    finish_request(request, protocol);
    cli.input("/new\nthird\n");
    let (request, body) = receive_at(&server, path);
    assert!(!body.to_string().contains("secret-skill-line"));
    assert!(!system(&body, protocol).contains("Rust 测试指导"));
    send_skill_read(request, protocol, "rust");
    let (request, body) = receive_at(&server, path);
    assert_eq!(loaded(&body, protocol)["ok"], false);
    assert!(!body.to_string().contains("secret-skill-line"));
    finish_request(request, protocol);
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok\nok\nok\n");
    let settings = std::fs::read_to_string(config_path(home.path(), "settings.yml")).unwrap();
    assert!(settings.contains("# 保留配置注释"));
    assert!(settings.contains("read_max_lines: 2"));
    assert!(settings.contains("read_max_bytes: 8"));
    assert!(!settings.contains("think_level"));
    let database = rusqlite::Connection::open_with_flags(
        config_path(home.path(), "agent.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let think_level: String = database
        .query_row(
            "SELECT think_level FROM preferences WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(think_level, "high");
    let mut cli = Cli::start_with_existing_files(home.path(), None);
    cli.input("restart\n");
    let (request, body) = receive_at(&server, path);
    assert!(!system(&body, protocol).contains("Rust 测试指导"));
    send_skill_read(request, protocol, "rust");
    let (request, body) = receive_at(&server, path);
    assert_eq!(loaded(&body, protocol)["ok"], false);
    finish_request(request, protocol);
    cli.input("/quit\n");
    assert!(cli.finish().status.success());
}
