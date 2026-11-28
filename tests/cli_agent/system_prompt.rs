use super::project_instructions::{finish_request, receive_for, system};
use super::streaming::{LiveCli, Pipe};
use super::*;
use std::{fs, path::Path};

pub(super) const LIVE_TEMPLATE: &str = "PROMPT_MODEL={{model}}\nPROMPT_MODE={{work_mode}}\nPROMPT_TOOLS={{#each tools}}[{{this}}]{{/each}}\n";

pub(super) fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

pub(super) fn assert_live(body: &Value, protocol: &str, mode: &str) {
    let text = system(body, protocol);
    assert!(
        text.contains(&format!(
            "PROMPT_MODEL={}\n",
            body["model"].as_str().unwrap()
        )),
        "{text}"
    );
    assert!(text.contains(&format!("PROMPT_MODE={mode}\n")), "{text}");
    let names: String = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            let name = if protocol == "completion" {
                &tool["function"]["name"]
            } else {
                &tool["name"]
            };
            format!("[{}]", name.as_str().unwrap())
        })
        .collect();
    assert!(text.contains(&format!("PROMPT_TOOLS={names}\n")), "{text}");
}

#[test]
fn system_prompt_sources_stay_fixed_across_tools_new_and_model_switch_both_protocols() {
    let protocol = "response";
    let server = server();
    let cwd = tempfile::tempdir().unwrap();
    write(cwd.path(), ".pp/SYSTEM_TEMPLATE.md", LIVE_TEMPLATE);
    write(cwd.path(), ".pp/APPEND_SYSTEM.md", "fixed append {{model}}");
    let mut models: Value =
        serde_json::from_str(&response_models(&server, Some(protocol))).unwrap();
    models["providers"]["fixture"]["models"]["secondary"] = json!({"model":"second-model"});
    let mut cli = LiveCli::new(Cli::start_with(
        cwd.path(),
        None,
        fixture_settings(),
        &models.to_string(),
    ));
    cli.input("first task\n");
    let (request, _) = receive_for(&server, protocol);
    let (request, body) = super::project_instructions::activate_rules(&server, protocol, request);
    assert_live(&body, protocol, "execute");
    write(cwd.path(), ".pp/APPEND_SYSTEM.md", "changed append");
    write(cwd.path(), "AGENTS.md", "fresh project rule");
    let arguments = json!({"path":".pp/SYSTEM_TEMPLATE.md","content":"{{#if broken}}"});
    if protocol == "completion" {
        calls_reply(request, json!([call("change-source", "write", arguments)]));
    } else {
        response_reply(
            request,
            json!([response_call("change-source", "write", arguments)]),
        );
    }
    let (request, body) = receive_for(&server, protocol);
    assert_live(&body, protocol, "execute");
    let text = system(&body, protocol);
    // An AGENTS.md written after history exists is delivered as a trailing developer-message increment, keeping the
    // system prefix unchanged.
    assert!(body.to_string().contains("fresh project rule"));
    assert_eq!(text.matches("fixed append {{model}}").count(), 1);
    assert!(!text.contains("changed append"));
    finish_request(request, protocol, "first done");
    cli.wait_for(Pipe::Stdout, "first done");
    cli.input("/new\n/model fixture/secondary\nsecond task\n");
    let (request, body) = receive_for(&server, protocol);
    assert_eq!(body["model"], "second-model");
    assert_live(&body, protocol, "execute");
    assert!(system(&body, protocol).contains("fixed append {{model}}"));
    finish_request(request, protocol, "second done");
    assert!(cli.finish().status.success());
    no_more(&server);
}
