use super::*;

#[test]
fn artifact_bash_failure_read_grep_and_next_page_keep_uri() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    tools.artifacts.bind(dir.path(), "root").unwrap();
    let text = (1..=10000)
        .map(|i| format!("line-{i:05}: diagnostic text\n"))
        .collect::<String>();
    fs::write(dir.path().join("input.log"), &text).unwrap();
    let result = execute(&tools, "bash", json!({"command":"cat input.log; exit 7"}));
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["ok"], false);
    let uri = result["artifact"]["uri"].as_str().unwrap();
    assert!(!result["output"].as_str().unwrap().contains("line-05000"));
    tools.limits.read_max_lines = 1;
    let page = execute(
        &tools,
        "read",
        json!({"path":format!("{uri}:raw:5000-5001")}),
    );
    assert_eq!(page["content"], "line-05000: diagnostic text\n");
    let next = page["next_path"].as_str().unwrap();
    assert!(next.starts_with(uri));
    assert!(
        execute(&tools, "read", json!({"path":next}))["content"]
            .as_str()
            .unwrap()
            .contains("05001")
    );
    let found = execute(&tools, "grep", json!({"path":uri,"pattern":"line-05000"}));
    assert_eq!(found["details"]["matchCount"], 1, "{found}");
    assert_eq!(found["details"]["scopePath"], uri);
    assert_eq!(found["details"]["fileMatches"][0]["path"], uri);
    assert!(!found["content"].as_str().unwrap().contains('#'));
    for mode in ["files_with_matches", "count"] {
        let found = execute(
            &tools,
            "grep",
            json!({"path":uri,"pattern":"LINE-05000","-i":true,"output_mode":mode}),
        );
        assert_eq!(found["details"]["fileMatches"][0]["path"], uri);
        assert_eq!(found["details"]["scopePath"], uri);
        assert_eq!(found["artifact"]["uri"], uri);
        assert!(!found["content"].as_str().unwrap().contains(".bash.log"));
        if mode == "files_with_matches" {
            assert_eq!(found["content"], format!("{uri}\n"));
        } else {
            assert_eq!(found["details"]["totalMatchCount"], 1);
            assert_eq!(found["details"]["countComplete"], true);
        }
    }
    assert!(
        execute(&tools, "read", json!({"path":format!("{uri}:-1")}))["content"]
            .as_str()
            .unwrap()
            .contains("10000")
    );
    for name in ["write", "edit"] {
        let value = execute(
            &tools,
            name,
            if name == "write" {
                json!({"path":uri,"content":"overwrite"})
            } else {
                json!({"input":format!("[{uri}#0000]\nPUT 1.=1:\n+overwrite")})
            },
        );
        assert_eq!(value["ok"], false);
    }
}

#[test]
fn artifact_grep_reads_after_four_mib_and_warns_for_multiline() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.artifacts.bind(dir.path(), "root").unwrap();
    let mut sink = output::OutputSink::new(Default::default(), tools.artifacts.clone());
    let block = "line of padding\n".repeat(4096);
    for _ in 0..80 {
        sink.append(OutputStream::Stdout, block.as_bytes());
    }
    sink.append(OutputStream::Stderr, b"UNIQUE_AFTER_FOUR_MIB\n");
    sink.finish();
    let uri = sink.value()["artifact"]["uri"].as_str().unwrap().to_owned();
    let found = execute(
        &tools,
        "grep",
        json!({"path":uri,"pattern":"UNIQUE_AFTER_FOUR_MIB"}),
    );
    assert_eq!(found["details"]["matchCount"], 1, "{found}");
    let multi = execute(
        &tools,
        "grep",
        json!({"path":uri,"pattern":"UNIQUE_AFTER_FOUR_MIB\\n"}),
    );
    assert!(multi["content"].as_str().unwrap().contains("first 4 MiB"));
    for pattern in ["UNIQUE_AFTER_FOUR_MIB", "UNIQUE_AFTER_FOUR_MIB\\n"] {
        let count = execute(
            &tools,
            "grep",
            json!({"path":uri,"pattern":pattern,"output_mode":"count"}),
        );
        let complete = !pattern.ends_with("\\n");
        assert_eq!(count["details"]["countComplete"], complete, "{count}");
        assert_eq!(
            count["details"]["totalMatchCount"],
            usize::from(complete),
            "{count}"
        );
    }
}

#[test]
fn artifact_background_live_read_is_stable_and_stop_seals_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.artifacts.bind(dir.path(), "root").unwrap();
    tools.limits.output.artifact_spill_threshold = 12;
    let started = execute(
        &tools,
        "bash",
        json!({"mode":"background","command":"printf 'initial-log-output\\n'; while [ ! -e release ]; do sleep 0.01; done; printf 'after-release\\n'; sleep 60"}),
    );
    let id = started["task_id"].as_str().unwrap();
    let status = background_wait(&tools, id, |v| v["artifact"]["uri"].is_string());
    let uri = status["artifact"]["uri"].as_str().unwrap().to_owned();
    assert!(status["notice"].as_str().unwrap().contains("snapshot"));
    assert!(
        status["notice"]
            .as_str()
            .unwrap()
            .contains(&format!("{uri}:-80"))
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("root/0.bash.log")).unwrap(),
        "initial-log-output\n"
    );
    assert_eq!(fs::read_dir(dir.path().join("root")).unwrap().count(), 1);
    let read = execute(&tools, "read", json!({"path":format!("{uri}:raw")}));
    assert_eq!(read["content"], "initial-log-output\n");
    assert_eq!(read["artifact"]["capturing"], true);
    fs::write(dir.path().join("release"), "go").unwrap();
    background_wait(&tools, id, |v| v["total_lines"] == 2);
    let found = execute(
        &tools,
        "grep",
        json!({"path":uri,"pattern":"after-release"}),
    );
    assert_eq!(found["details"]["matchCount"], 1, "{found}");
    let stopped = execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"}));
    assert_eq!(stopped["artifact"]["uri"], uri);
    assert_eq!(stopped["artifact"]["complete"], true);
    assert_eq!(stopped["artifact"]["capturing"], false);
    assert!(
        stopped["notice"]
            .as_str()
            .unwrap()
            .contains("Full saved output")
    );
    assert_eq!(fs::read_dir(dir.path().join("root")).unwrap().count(), 2);
    assert_eq!(
        execute(&tools, "read", json!({"path":format!("{uri}:raw")}))["content"],
        "initial-log-output\nafter-release\n"
    );
    assert_eq!(
        execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"})),
        stopped
    );
}
