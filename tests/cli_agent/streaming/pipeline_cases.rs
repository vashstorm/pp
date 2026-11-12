//! Plan §3 集成回归：真实 CLI 上验证前台/后台管道阶段镜像、pipe:// 只读正文、
//! 瞬时阶段事件不进入持久化，以及恢复会话只保留状态而不恢复正文。
//!
//! 用例只观察工具结果、进程间历史、会话/统计/artifact 文件与终端输出；
//! 不断言提示词、schema 文案或源码文本。

use super::*;
use rusqlite::Connection;
use std::{
    path::Path,
    time::{Duration, Instant},
};

fn assert_pipes(result: &Value) -> &Value {
    assert!(
        result["pipes"].is_object(),
        "missing pipes metadata: {result}"
    );
    &result["pipes"]
}

fn pipeline_run(result: &Value) -> String {
    assert_pipes(result)["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing pipe run id: {result}"))
        .to_owned()
}

fn pipeline_stream_uri(result: &Value, index: usize) -> String {
    assert_pipes(result)["streams"][index]["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("missing stream {index}: {result}"))
        .to_owned()
}

fn session_journal(home: &Path) -> std::path::PathBuf {
    let sessions = config_path(home, "sessions");
    let mut files = std::fs::read_dir(&sessions)
        .unwrap_or_else(|error| panic!("missing sessions directory {sessions:?}: {error}"))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(files.len(), 1, "expected one session journal: {files:?}");
    files.pop().unwrap()
}

fn collect_text(path: &Path, text: &mut String) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            collect_text(&entry.path(), text);
        }
    } else if metadata.is_file() {
        text.push_str(&String::from_utf8_lossy(&std::fs::read(path).unwrap()));
    }
}

fn assert_no_marker(path: &Path, marker: &str) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            assert_no_marker(&entry.path(), marker);
        }
    } else if metadata.is_file() {
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(marker),
            "transient pipe body reached {path:?}"
        );
    }
}

fn assert_stats_have_no_marker(home: &Path, marker: &str) {
    let path = home.join(".pp/stats/stats.db");
    assert!(path.is_file(), "stats database missing: {path:?}");
    let database = Connection::open(&path).unwrap();
    let mut statement = database
        .prepare("SELECT request_json, skills_json FROM request_contexts")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok([
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ])
        })
        .unwrap();
    for row in rows {
        for text in row.unwrap().into_iter().flatten() {
            assert!(
                !text.contains(marker),
                "transient pipe body reached the stats database"
            );
            assert!(
                !text.contains("tool_pipeline_snapshot"),
                "transient pipeline snapshot reached the stats database"
            );
        }
    }
}

#[test]
fn pipeline_foreground_stage_mirror_reads_into_model_history() {
    const MARKER: &str = "hidden-foreground-secret";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("{MARKER}\nvisible-line\n"),
        )
        .unwrap();
        let cli = protocol.start(&server, dir.path(), Some("inspect pipeline"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "pipeline",
                "bash",
                json!({"command":"{ cat secret.txt; exit 7; } | tail -n 1"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "pipeline");
        assert_eq!(result["output"], "visible-line\n", "{result}");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["timed_out"], false, "{result}");
        assert_eq!(result["cancelled"], false, "{result}");
        assert!(
            !result.to_string().contains(MARKER),
            "stage body leaked into the bash result: {result}"
        );

        let pipes = assert_pipes(&result);
        assert_eq!(pipes["nonzero_stages"], 1, "{result}");
        assert_eq!(pipes["incomplete"], false, "{result}");
        let pipelines = pipes["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 1, "{result}");
        assert_eq!(pipelines[0]["stage_count"], 2, "{result}");
        assert_eq!(pipelines[0]["stages"][0]["status"], 7, "{result}");
        assert_eq!(pipelines[0]["stages"][1]["status"], 0, "{result}");

        let uri = pipeline_stream_uri(&result, 0);
        assert_eq!(
            uri,
            format!("pipe://{}/1", pipeline_run(&result)),
            "{result}"
        );
        let total = format!("{MARKER}\nvisible-line\n").len() as u64;
        assert_eq!(pipes["streams"][0]["open"], false, "{result}");
        assert_eq!(pipes["streams"][0]["total_bytes"], total, "{result}");

        protocol.calls_reply(
            request,
            json!([protocol.call("pipe-read", "read", json!({"path":uri}))]),
        );
        let (request, body) = protocol.receive(&server);
        let read = protocol.tool_result(&body, "pipe-read");
        assert_eq!(read["content"], format!("{MARKER}\nvisible-line\n"));
        assert_eq!(read["pipe"]["total_bytes"], total);
        assert_eq!(read["pipe"]["start"], 0);
        assert_eq!(read["pipe"]["end"], total);
        assert_eq!(read["pipe"]["open"], false);
        assert_eq!(read["pipe"]["prefix_evicted"], false);
        // The read result is now part of the next model request's tool history,
        // while the bash result still carries only bounded metadata.
        assert!(
            body.to_string().contains(MARKER),
            "read content missing from model history: {body}"
        );
        assert_eq!(
            protocol.tool_result(&body, "pipeline")["pipes"]["run_id"],
            pipeline_run(&result)
        );

        protocol.final_reply(request, "foreground pipeline verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("[pipes: pipe://"),
            "missing bounded pipe summary: {stderr}"
        );
        assert!(
            stderr.contains("Non-zero stage results: 1"),
            "missing bounded stage warning: {stderr}"
        );
        assert!(
            !stderr.contains(MARKER),
            "stage body reached the CLI output: {stderr}"
        );
        no_more(&server);
    }
}

#[test]
fn pipeline_foreground_stream_retention_stays_bounded_and_tail_readable() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = protocol.start(&server, dir.path(), Some("bounded pipeline"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "large",
                "bash",
                json!({"command":"awk 'BEGIN { for (i = 1; i <= 300000; i++) print i }' | tail -n 1"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "large");
        assert_eq!(result["output"], "300000\n", "{result}");
        let pipes = assert_pipes(&result);
        let stream = &pipes["streams"][0];
        let total = stream["total_bytes"].as_u64().unwrap();
        assert!(
            total > 1024 * 1024,
            "test stream must exceed the retention cap: {result}"
        );
        let retained_start = stream["retained_start"].as_u64().unwrap();
        assert!(retained_start > 0, "{result}");
        assert_eq!(stream["retained_end"], total, "{result}");
        let uri = pipeline_stream_uri(&result, 0);

        protocol.calls_reply(
            request,
            json!([protocol.call("tail", "read", json!({"path":format!("{uri}:-2")}))]),
        );
        let (request, body) = protocol.receive(&server);
        let read = protocol.tool_result(&body, "tail");
        assert_eq!(read["content"], "299999\n300000\n");
        assert_eq!(read["pipe"]["total_bytes"], total);
        assert_eq!(read["pipe"]["prefix_evicted"], true);
        assert_eq!(read["pipe"]["retained_start"], retained_start);

        protocol.final_reply(request, "bounded pipeline verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        no_more(&server);
    }
}

#[test]
fn pipeline_background_status_stop_share_run_and_allow_live_read() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let command = "{ printf 'ready-line\\n'; while [ ! -f gate ]; do sleep 0.05; done; printf 'done-line\\n'; } | tail -n 1";
        let cli = protocol.start(&server, dir.path(), Some("background pipeline gate"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "start",
                "bash",
                json!({"command":command,"mode":"background"})
            )]),
        );
        let (mut request, body) = protocol.receive(&server);
        let started = protocol.tool_result(&body, "start");
        assert_eq!(started["status"], "running", "{started}");
        let task_id = started["task_id"].as_str().unwrap().to_owned();
        let run = pipeline_run(&started);

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut attempt = 0;
        let (stream_uri, live) = loop {
            assert!(
                Instant::now() < deadline,
                "background pipeline never produced ready bytes"
            );
            assert!(attempt < 300, "too many status polls");
            let status_id = format!("status-{attempt}");
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    &status_id,
                    "bash_task",
                    json!({"task_id":task_id,"action":"status"})
                )]),
            );
            let (next, body) = protocol.receive(&server);
            let status = protocol.tool_result(&body, &status_id);
            assert_eq!(status["status"], "running", "{status}");
            assert_eq!(pipeline_run(&status), run, "{status}");
            assert_eq!(status["output"], "", "{status}");
            request = next;
            if let Some(uri) = assert_pipes(&status)["streams"]
                .get(0)
                .and_then(|stream| stream["uri"].as_str())
            {
                let read_id = format!("live-read-{attempt}");
                protocol.calls_reply(
                    request,
                    json!([protocol.call(&read_id, "read", json!({"path":uri}))]),
                );
                let (next, body) = protocol.receive(&server);
                let read = protocol.tool_result(&body, &read_id);
                request = next;
                if read["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("ready-line"))
                {
                    assert_eq!(read["content"], "ready-line\n", "{read}");
                    assert_eq!(read["pipe"]["open"], true, "{read}");
                    break (uri.to_owned(), read);
                }
            }
            attempt += 1;
        };
        assert_eq!(live["pipe"]["run_id"], run);

        protocol.calls_reply(
            request,
            json!([protocol.call(
                "stop",
                "bash_task",
                json!({"task_id":task_id,"action":"stop"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let stopped = protocol.tool_result(&body, "stop");
        assert_eq!(stopped["status"], "stopped", "{stopped}");
        assert_eq!(stopped["ok"], true, "{stopped}");
        assert_eq!(pipeline_run(&stopped), run, "{stopped}");
        assert_eq!(pipeline_stream_uri(&stopped, 0), stream_uri, "{stopped}");
        assert_eq!(
            assert_pipes(&stopped)["streams"][0]["open"],
            false,
            "{stopped}"
        );
        assert_eq!(stopped["output"], "", "{stopped}");

        protocol.calls_reply(
            request,
            json!([protocol.call("after-stop", "read", json!({"path":stream_uri}))]),
        );
        let (request, body) = protocol.receive(&server);
        let after = protocol.tool_result(&body, "after-stop");
        assert_eq!(after["content"], "ready-line\n", "{after}");
        assert_eq!(after["pipe"]["open"], false, "{after}");
        assert_eq!(after["pipe"]["total_bytes"], "ready-line\n".len() as u64);

        protocol.final_reply(request, "background pipeline verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("task {task_id}: stopped")),
            "{protocol:?}: {stderr}"
        );
        no_more(&server);
    }
}

#[test]
fn pipeline_transient_snapshots_do_not_reach_journal_stats_or_artifacts() {
    const MARKER: &str = "hidden-journal-secret";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("{MARKER}\nvisible-line\n"),
        )
        .unwrap();
        let command = "{ cat secret.txt; : > started; while [ ! -f gate ]; do sleep 0.05; done; printf 'done-line\\n'; } | tail -n 1";
        // Force an artifact spill so the storage check covers a real file.
        let settings = format!(
            "{}tools:\n  artifactSpillThreshold: 1\n",
            fixture_settings()
        );
        let cli =
            protocol.start_with_settings(&server, dir.path(), Some("journal pipeline"), &settings);
        let (request, _) = protocol.receive(&server);
        // A foreground call keeps the host's 50 ms snapshot pump running while
        // the model request is blocked on the tool, so the transient
        // ToolPipelineSnapshot path is actually exercised before persistence.
        let started = dir.path().join("started");
        let gate = dir.path().join("gate");
        let release = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !started.exists() {
                assert!(Instant::now() < deadline, "pipeline never started");
                thread::sleep(Duration::from_millis(5));
            }
            thread::sleep(Duration::from_millis(150));
            std::fs::write(gate, b"").unwrap();
        });
        protocol.calls_reply(
            request,
            json!([protocol.call("pipeline", "bash", json!({"command":command}))]),
        );
        let (request, body) = protocol.receive(&server);
        release.join().unwrap();
        let result = protocol.tool_result(&body, "pipeline");
        assert!(
            result["artifact"]["uri"]
                .as_str()
                .is_some_and(|uri| uri.starts_with("artifact://")),
            "{result}"
        );
        assert!(
            !result.to_string().contains(MARKER),
            "stage body leaked into the spilled result: {result}"
        );
        let run = pipeline_run(&result);

        protocol.final_reply(request, "journal pipeline verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");

        let journal = session_journal(dir.path());
        let text = std::fs::read_to_string(&journal).unwrap();
        assert!(
            !text.contains("tool_pipeline_snapshot"),
            "transient pipeline snapshot reached the journal: {journal:?}"
        );
        assert!(
            !text.contains(MARKER),
            "stage body reached the journal: {journal:?}"
        );
        assert!(
            text.contains(&run),
            "final pipes metadata missing from the journal: {journal:?}"
        );
        let artifacts = journal.with_extension("");
        assert!(
            artifacts.is_dir(),
            "spilled artifact directory missing: {artifacts:?}"
        );
        let mut spilled = String::new();
        collect_text(&artifacts, &mut spilled);
        assert!(
            spilled.contains("done-line"),
            "spilled artifact missing the final output: {spilled:?}"
        );
        assert_no_marker(&artifacts, MARKER);
        assert_stats_have_no_marker(dir.path(), MARKER);
        no_more(&server);
    }
}

#[test]
fn pipeline_restored_session_keeps_metadata_but_body_uri_is_unavailable() {
    const MARKER: &str = "hidden-restore-secret";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("{MARKER}\nvisible-line\n"),
        )
        .unwrap();
        let cli = protocol.start(&server, dir.path(), Some("persist pipeline"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "pipeline",
                "bash",
                json!({"command":"{ cat secret.txt; exit 7; } | tail -n 1"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "pipeline");
        assert_eq!(result["output"], "visible-line\n");
        let run = pipeline_run(&result);
        let uri = pipeline_stream_uri(&result, 0);
        protocol.final_reply(request, "session persisted");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");

        let journal = session_journal(dir.path());
        let id = journal.file_stem().unwrap().to_str().unwrap().to_owned();
        let saved = std::fs::read_to_string(&journal).unwrap();
        assert!(saved.contains(&run), "{journal:?}");
        assert!(!saved.contains(MARKER), "{journal:?}");

        let resumed = LiveCli::new(Cli::spawn(
            dir.path(),
            &["-r", &id, "-p", "resume pipeline"],
            &[],
        ));
        let (request, body) = protocol.receive(&server);
        let restored = protocol.tool_result(&body, "pipeline");
        assert_eq!(pipeline_run(&restored), run, "{restored}");
        assert_eq!(pipeline_stream_uri(&restored, 0), uri, "{restored}");
        assert!(
            !body.to_string().contains(MARKER),
            "restored metadata must not recreate the body: {body}"
        );

        protocol.calls_reply(
            request,
            json!([protocol.call("old-pipe", "read", json!({"path":uri}))]),
        );
        let (request, body) = protocol.receive(&server);
        let old = protocol.tool_result(&body, "old-pipe");
        assert_eq!(old["ok"], false, "{old}");
        assert!(
            old["error"]
                .as_str()
                .is_some_and(|error| error.contains("Temporary pipe output is unavailable")),
            "{old}"
        );
        protocol.final_reply(request, "restored pipeline metadata verified");
        let output = resumed.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        no_more(&server);
    }
}

fn is_subagent_request(body: &Value) -> bool {
    body["instructions"]
        .as_str()
        .or_else(|| body["messages"][0]["content"].as_str())
        .is_some_and(|text| text.contains("You are subagent"))
}

#[test]
fn pipeline_subagent_shares_the_root_pipe_store() {
    const MARKER: &str = "hidden-subagent-secret";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("{MARKER}\nchild-visible\n"),
        )
        .unwrap();
        let cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("subagent pipeline"),
            &task_fixture_settings(),
        );
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "spawn",
                "task",
                json!({"context":"shared root pipe store","tasks":[{"role":"fast","task":"produce child pipeline output"}]})
            )]),
        );
        let mut child_calls = 0;
        let mut child_uri: Option<String> = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                Instant::now() < deadline,
                "subagent pipeline did not settle"
            );
            let (request, body) = protocol.receive(&server);
            if is_subagent_request(&body) {
                if child_calls == 0 {
                    protocol.calls_reply(
                        request,
                        json!([protocol.call(
                            "child-pipeline",
                            "bash",
                            json!({"command":"{ cat secret.txt; } | tail -n 1"})
                        )]),
                    );
                    child_calls = 1;
                } else {
                    let result = protocol.tool_result(&body, "child-pipeline");
                    assert_eq!(result["output"], "child-visible\n", "{result}");
                    assert!(
                        !result.to_string().contains(MARKER),
                        "child stage body leaked into the bash result: {result}"
                    );
                    let uri = pipeline_stream_uri(&result, 0);
                    protocol.final_reply(request, &format!("child pipeline uri {uri}"));
                    child_calls = 2;
                }
                continue;
            }
            if child_uri.is_some() {
                let read = protocol.tool_result(&body, "parent-pipe");
                assert_eq!(read["content"], format!("{MARKER}\nchild-visible\n"));
                assert_eq!(read["pipe"]["total_bytes"], (MARKER.len() + 15) as u64);
                protocol.final_reply(request, "subagent pipeline shared");
                break;
            }
            let wire = body.to_string();
            let Some(position) = wire.find("child pipeline uri pipe://") else {
                protocol.final_reply(request, "waiting for child pipeline");
                continue;
            };
            let start = position + "child pipeline uri ".len();
            let rest = &wire[start..];
            let end = rest
                .find(|character: char| {
                    character == '"' || character == '\\' || character.is_whitespace()
                })
                .unwrap_or(rest.len());
            let uri = rest[..end].to_owned();
            assert!(uri.starts_with("pipe://"), "{wire}");
            child_uri = Some(uri.clone());
            protocol.calls_reply(
                request,
                json!([protocol.call("parent-pipe", "read", json!({"path":uri}))]),
            );
        }
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        no_more(&server);
    }
}

// `!` 走内嵌管道：主输出只保留 tail 结果，阶段状态与补读 URI 走有界摘要；
// 行模式 `/pipes` 只列元数据，不打印阶段正文。
#[test]
fn pipeline_bang_shell_keeps_stage_body_out_of_the_local_repl() {
    const UNKNOWN_RUN: &str = "0123456789abcdef0123456789abcdef";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            "hidden-bang-secret\nvisible-line\n",
        )
        .unwrap();
        let mut cli = protocol.start(&server, dir.path(), None);
        cli.input("!{ cat secret.txt; exit 7; } | tail -n 1\n");
        cli.wait_for(Pipe::Stderr, "[shell output] visible-line");
        cli.wait_for(Pipe::Stderr, "shell: exit 0");
        cli.wait_for(Pipe::Stderr, "Non-zero stage results: 1");
        let summary = cli.text(Pipe::Stderr);
        assert!(summary.contains("[pipes: pipe://"), "{summary}");
        cli.input("/pipes\n");
        cli.wait_for(Pipe::Stderr, "Selected run: pipe://");
        cli.wait_for(Pipe::Stderr, "Read a stream with read");
        cli.input(&format!("/pipes {UNKNOWN_RUN}\n"));
        cli.wait_for(Pipe::Stderr, "Temporary pipe output is unavailable");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("hidden-bang-secret"),
            "local shell stage body reached the REPL: {stderr}"
        );
        assert!(
            stderr.contains("Non-zero stage results: 1"),
            "missing bounded stage warning: {stderr}"
        );
        assert!(
            server.try_recv().unwrap().is_none(),
            "local shell pipeline requested a model"
        );
    }
}

#[test]
fn pipeline_new_session_rejects_old_pipe_uris() {
    const MARKER: &str = "hidden-new-session-secret";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("{MARKER}\nvisible-line\n"),
        )
        .unwrap();
        let mut cli = protocol.start(&server, dir.path(), None);
        cli.input("first pipeline task\n");
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "pipeline",
                "bash",
                json!({"command":"{ cat secret.txt; } | tail -n 1"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "pipeline");
        let uri = pipeline_stream_uri(&result, 0);
        protocol.final_reply(request, "first pipeline done");
        cli.wait_for(Pipe::Stdout, "first pipeline done");

        cli.input("/new\nsecond pipeline task\n");
        let (request, body) = protocol.receive(&server);
        assert!(
            !body.to_string().contains(&pipeline_run(&result)),
            "/new must not carry the old pipe run into the new root: {body}"
        );
        protocol.calls_reply(
            request,
            json!([protocol.call("old-pipe", "read", json!({"path":uri}))]),
        );
        let (request, body) = protocol.receive(&server);
        let old = protocol.tool_result(&body, "old-pipe");
        assert_eq!(old["ok"], false, "{old}");
        assert!(
            old["error"]
                .as_str()
                .is_some_and(|error| error.contains("Temporary pipe output is unavailable")),
            "{old}"
        );
        protocol.final_reply(request, "new session verified");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(MARKER),
            "old pipe body leaked into the new session"
        );
        no_more(&server);
    }
}
