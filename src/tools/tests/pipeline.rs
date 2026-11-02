//! 前台 `bash` 工具的真实执行路径上的管道镜像回归。
//!
//! 这些用例只通过工具结果里的 `pipes` 元数据和 `PipeStore::read` 观察行为，
//! 不断言源码文本或内部 schema 文案。

use super::*;
use std::time::{Duration, Instant};

fn run_bash(tools: &Tools, command: &str) -> Value {
    execute(tools, "bash", json!({"command": command}))
}

fn pipes(result: &Value) -> &Value {
    assert!(
        result["pipes"].is_object(),
        "missing pipes metadata: {result}"
    );
    &result["pipes"]
}

fn pipelines(result: &Value) -> Vec<Value> {
    let list = pipes(result)["pipelines"]
        .as_array()
        .unwrap_or_else(|| panic!("missing pipeline list: {result}"));
    list.clone()
}

fn streams(result: &Value) -> Vec<Value> {
    pipes(result)["streams"]
        .as_array()
        .unwrap_or_else(|| panic!("missing stream list: {result}"))
        .clone()
}

fn stage_status(pipeline: &Value, index: usize) -> Option<u64> {
    pipeline["stages"][index]["status"].as_u64()
}

fn stage_signal(pipeline: &Value, index: usize) -> Option<i64> {
    pipeline["stages"][index]["signal"].as_i64()
}

fn nonzero_stages(result: &Value) -> u64 {
    pipes(result)["nonzero_stages"]
        .as_u64()
        .unwrap_or_else(|| panic!("missing nonzero stage count: {result}"))
}

struct Observed {
    data: Vec<u8>,
    start: u64,
    end: u64,
    open: bool,
    total_bytes: u64,
    prefix_evicted: bool,
}

fn read_stream(tools: &Tools, result: &Value, from: Option<u64>, stream: u64) -> Observed {
    let run_id = pipes(result)["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing run id: {result}"));
    let read = tools
        .pipe_store()
        .read(run_id, stream, from)
        .unwrap_or_else(|error| panic!("reading stream {stream} of run {run_id}: {error}"));
    Observed {
        data: read.data,
        start: read.start,
        end: read.end,
        open: read.open,
        total_bytes: read.total_bytes,
        prefix_evicted: read.prefix_evicted,
    }
}

fn stream_unavailable(tools: &Tools, result: &Value, stream: u64) -> bool {
    let run_id = pipes(result)["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing run id: {result}"));
    tools.pipe_store().read(run_id, stream, None).is_err()
}

#[test]
fn pipeline_upstream_failure_keeps_stage_status_and_stream_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let result = run_bash(
        &tools,
        "{ printf 'hidden\\nvisible\\n'; exit 7; } | tail -n 1",
    );
    // The shell reports the last stage's status; the failed upstream stage is
    // only visible through the recorded stages, never by rewriting `ok`.
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    assert_eq!(result["cancelled"], false, "{result}");
    assert_eq!(result["output"], "visible\n", "{result}");

    let list = pipelines(&result);
    assert_eq!(list.len(), 1, "{result}");
    let pipeline = &list[0];
    assert_eq!(pipeline["stage_count"], 2, "{result}");
    assert_eq!(stage_status(pipeline, 0), Some(7), "{result}");
    assert_eq!(stage_status(pipeline, 1), Some(0), "{result}");
    assert_eq!(nonzero_stages(&result), 1, "{result}");

    let list = streams(&result);
    assert_eq!(list.len(), 1, "{result}");
    let read = read_stream(&tools, &result, None, 1);
    assert!(!read.open, "{result}");
    assert_eq!(read.data, b"hidden\nvisible\n", "{result}");
}

#[test]
fn pipeline_downstream_close_releases_producer_with_real_status() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let started = Instant::now();
    let result = run_bash(&tools, "yes x | head -n 1");
    let elapsed = started.elapsed();
    // Mirroring must not keep draining the producer; the run ends promptly.
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    assert_eq!(result["output"], "x\n", "{result}");
    assert!(
        elapsed < Duration::from_secs(30),
        "pipeline did not terminate promptly: {elapsed:?}"
    );

    let list = pipelines(&result);
    assert_eq!(list.len(), 1, "{result}");
    let pipeline = &list[0];
    assert_eq!(pipeline["stage_count"], 2, "{result}");
    assert_eq!(stage_status(pipeline, 1), Some(0), "{result}");

    // The producer really died from the closed downstream: either the raw
    // signal is preserved or the derived 128+13 status is reported. It is never
    // a fabricated success.
    let producer_status = stage_status(pipeline, 0);
    let producer_signal = stage_signal(pipeline, 0);
    assert!(
        producer_signal == Some(13) || producer_status == Some(141),
        "producer did not report a SIGPIPE termination: {result}"
    );
    if let Some(signal) = producer_signal {
        assert_eq!(signal, 13, "{result}");
    }
    assert!(nonzero_stages(&result) >= 1, "{result}");

    // Observed bytes are a prefix of the producer's output, not a full drain.
    let read = read_stream(&tools, &result, None, 1);
    assert!(!read.open, "{result}");
    assert!(read.data.starts_with(b"x\n"), "{result}");
}

#[test]
fn pipeline_stderr_merge_and_redirect_edges_are_not_conflated() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    // Ordinary stderr stays in the main output and out of the mirrored edge.
    let separate = run_bash(&tools, "printf 'piped' | cat; printf 'E' >&2");
    assert_eq!(separate["ok"], true, "{separate}");
    let output = separate["output"].as_str().unwrap();
    assert!(output.contains("piped"), "{separate}");
    assert!(output.contains('E'), "{separate}");
    let read = read_stream(&tools, &separate, None, 1);
    assert_eq!(read.data, b"piped", "{separate}");
    assert!(!read.open, "{separate}");

    // An explicit merge sends the error through the pipeline.
    let merged = run_bash(&tools, "{ printf 'o'; printf 'e' >&2; } 2>&1 | cat");
    assert_eq!(merged["ok"], true, "{merged}");
    let merged_output = merged["output"].as_str().unwrap();
    assert_eq!(merged_output.len(), 2, "{merged}");
    assert!(merged_output.contains('o'), "{merged}");
    assert!(merged_output.contains('e'), "{merged}");
    let read = read_stream(&tools, &merged, None, 1);
    assert_eq!(read.data, merged_output.as_bytes(), "{merged}");

    // A stage redirect bypasses the edge; the mirror is empty, not fabricated.
    let bypass = run_bash(&tools, "printf 'bypassed' > bypass.txt | cat");
    assert_eq!(bypass["ok"], true, "{bypass}");
    assert_eq!(bypass["output"], "", "{bypass}");
    assert_eq!(
        fs::read_to_string(dir.path().join("bypass.txt")).unwrap(),
        "bypassed"
    );
    let read = read_stream(&tools, &bypass, None, 1);
    assert_eq!(read.data, b"", "{bypass}");
    assert_eq!(read.total_bytes, 0, "{bypass}");
    assert!(!read.open, "{bypass}");
}

#[test]
fn pipeline_stage_status_survives_pipefail_bang_and_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let plain = run_bash(&tools, "false | true");
    assert_eq!(plain["ok"], true, "{plain}");
    assert_eq!(plain["exit_code"], 0, "{plain}");
    assert_eq!(plain["output"], "", "{plain}");
    let list = pipelines(&plain);
    assert_eq!(list.len(), 1, "{plain}");
    assert_eq!(stage_status(&list[0], 0), Some(1), "{plain}");
    assert_eq!(stage_status(&list[0], 1), Some(0), "{plain}");
    assert_eq!(list[0]["pipefail"], false, "{plain}");
    assert_eq!(nonzero_stages(&plain), 1, "{plain}");

    let pipefail = run_bash(&tools, "set -o pipefail; false | true");
    assert_eq!(pipefail["ok"], false, "{pipefail}");
    assert_eq!(pipefail["exit_code"], 1, "{pipefail}");
    let list = pipelines(&pipefail);
    assert_eq!(list[0]["pipefail"], true, "{pipefail}");
    assert_eq!(stage_status(&list[0], 0), Some(1), "{pipefail}");

    let bang = run_bash(&tools, "! false | true");
    assert_eq!(bang["ok"], false, "{bang}");
    assert_eq!(bang["exit_code"], 1, "{bang}");
    let list = pipelines(&bang);
    assert_eq!(list[0]["bang"], true, "{bang}");
    assert_eq!(stage_status(&list[0], 0), Some(1), "{bang}");
    assert_eq!(nonzero_stages(&bang), 1, "{bang}");

    let recovered = run_bash(&tools, "false | false || printf 'recovered'");
    assert_eq!(recovered["ok"], true, "{recovered}");
    assert_eq!(recovered["exit_code"], 0, "{recovered}");
    assert_eq!(recovered["output"], "recovered", "{recovered}");
    let list = pipelines(&recovered);
    assert_eq!(stage_status(&list[0], 0), Some(1), "{recovered}");
    assert_eq!(stage_status(&list[0], 1), Some(1), "{recovered}");
    assert_eq!(nonzero_stages(&recovered), 2, "{recovered}");
}

#[test]
fn pipeline_occurrences_cover_loops_functions_and_substitutions() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let result = run_bash(
        &tools,
        r#"for i in 1 2; do printf "$i" | cat; done
f() { printf f | cat; }
f
x=$(printf s | cat)
printf '%s' "$x"
(printf a | cat)
printf b | cat & wait
"#,
    );
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["output"], "12fsab", "{result}");

    let list = pipelines(&result);
    // Two loop iterations, one function call, one command substitution, one
    // subshell and one background pipeline are distinct executions.
    assert_eq!(list.len(), 6, "{result}");
    let mut ids = Vec::new();
    for pipeline in &list {
        let id = pipeline["id"].as_u64().expect("pipeline id");
        assert_eq!(pipeline["stage_count"], 2, "{result}");
        assert_eq!(pipeline["status"], 0, "{result}");
        for index in 0..2 {
            assert_eq!(stage_status(pipeline, index), Some(0), "{result}");
        }
        assert!(!ids.contains(&id), "duplicate occurrence id {id}: {result}");
        ids.push(id);
    }

    // Each occurrence owns a distinct stream id; per-run closed-stream
    // retention keeps only the four newest bodies.
    let list = streams(&result);
    let ids: Vec<u64> = list
        .iter()
        .map(|stream| stream["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, vec![3, 4, 5, 6], "{result}");
    assert_eq!(pipes(&result)["evicted_streams"], 2, "{result}");
    let mut contents = Vec::new();
    for id in &ids {
        contents.push(read_stream(&tools, &result, None, *id).data);
    }
    contents.sort();
    assert_eq!(
        contents,
        vec![b"a".to_vec(), b"b".to_vec(), b"f".to_vec(), b"s".to_vec()],
        "{result}"
    );
}

#[test]
fn pipeline_metadata_stays_bounded_for_many_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let result = run_bash(
        &tools,
        "for i in $(seq 1 40); do printf '%s' \"$i\" | cat > /dev/null; done",
    );
    assert_eq!(result["ok"], true, "{result}");

    let list = pipelines(&result);
    assert_eq!(list.len(), 16, "{result}");
    assert_eq!(pipes(&result)["omitted_pipelines"], 24, "{result}");
    assert_eq!(pipes(&result)["incomplete"], true, "{result}");
    // Retained details are still complete for the recent occurrences.
    for pipeline in &list {
        assert_eq!(stage_status(pipeline, 0), Some(0), "{result}");
        assert_eq!(stage_status(pipeline, 1), Some(0), "{result}");
    }
    // Closed-stream retention also stays bounded.
    assert!(streams(&result).len() <= 4, "{result}");
    assert_eq!(nonzero_stages(&result), 0, "{result}");
}

#[test]
fn pipeline_stream_ring_reports_tail_offsets_beyond_one_mebibyte() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let result = run_bash(&tools, "head -c 1200000 /dev/zero | cat | wc -c");
    assert_eq!(result["ok"], true, "{result}");
    assert!(
        result["output"].as_str().unwrap().contains("1200000"),
        "{result}"
    );

    let list = streams(&result);
    assert_eq!(list.len(), 2, "{result}");
    assert_eq!(nonzero_stages(&result), 0, "{result}");

    // The request clamps to the retained tail and reports the covered prefix.
    let tail = read_stream(&tools, &result, Some(0), 1);
    assert!(!tail.open, "{result}");
    assert_eq!(tail.total_bytes, 1_200_000, "{result}");
    assert_eq!(tail.end, 1_200_000, "{result}");
    assert_eq!(tail.start, 1_200_000 - 1024 * 1024, "{result}");
    assert!(tail.prefix_evicted, "{result}");
    assert_eq!(tail.data.len() as u64, 1024 * 1024, "{result}");

    // A request inside the retained window is served without prefix loss.
    let window = read_stream(&tools, &result, Some(1_100_000), 1);
    assert_eq!(window.start, 1_100_000, "{result}");
    assert_eq!(window.end, 1_200_000, "{result}");
    assert!(!window.prefix_evicted, "{result}");
    assert_eq!(window.data.len(), 100_000, "{result}");
}

#[test]
fn pipeline_closed_streams_evict_and_old_uris_stay_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    let result = run_bash(
        &tools,
        "for i in 1 2 3 4 5 6; do printf '%s' \"$i\" | cat > /dev/null; done",
    );
    assert_eq!(result["ok"], true, "{result}");

    let list = streams(&result);
    let ids: Vec<u64> = list
        .iter()
        .map(|stream| stream["id"].as_u64().unwrap())
        .collect();
    // Only the most recent four closed streams survive; older ids are gone.
    assert_eq!(ids, vec![3, 4, 5, 6], "{result}");
    assert_eq!(pipes(&result)["evicted_streams"], 2, "{result}");
    assert!(stream_unavailable(&tools, &result, 1), "{result}");
    assert!(stream_unavailable(&tools, &result, 2), "{result}");
    assert_eq!(read_stream(&tools, &result, None, 6).data, b"6", "{result}");

    // A valid empty stream is distinguishable from an evicted one.
    let empty = run_bash(&tools, "printf '' | cat");
    assert_eq!(empty["ok"], true, "{empty}");
    assert_eq!(empty["output"], "", "{empty}");
    let read = read_stream(&tools, &empty, None, 1);
    assert_eq!(read.data, b"", "{empty}");
    assert_eq!(read.total_bytes, 0, "{empty}");
    assert!(!read.open, "{empty}");

    // The earlier run's evicted ids never resolve again, and a new run's stream
    // id is scoped to that run, so old bytes cannot leak into it.
    assert!(stream_unavailable(&tools, &result, 1), "{result}");
    assert_eq!(read_stream(&tools, &empty, None, 1).data, b"", "{empty}");
    assert_eq!(read_stream(&tools, &result, None, 6).data, b"6", "{result}");
}

#[test]
fn pipeline_raw_bytes_survive_chunking_and_do_not_pollute_main_output() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    // A UTF-8 code point split across two producer writes arrives byte-exact.
    let chunked = run_bash(
        &tools,
        "sh -c 'printf \"\\303\"; sleep 0.05; printf \"\\251\"' | cat",
    );
    assert_eq!(chunked["ok"], true, "{chunked}");
    assert_eq!(chunked["output"], "é", "{chunked}");
    let read = read_stream(&tools, &chunked, None, 1);
    assert_eq!(read.data, "é".as_bytes(), "{chunked}");

    // ANSI/OSC control strings stay raw in the mirror but never reach the UI text.
    let ansi = run_bash(&tools, "printf '\\033[31mred\\033[0m' | cat");
    assert_eq!(ansi["ok"], true, "{ansi}");
    assert_eq!(ansi["output"], "red", "{ansi}");
    let read = read_stream(&tools, &ansi, None, 1);
    assert_eq!(read.data, b"\x1b[31mred\x1b[0m", "{ansi}");

    let osc = run_bash(&tools, "printf '\\033]0;title\\007body' | cat");
    assert_eq!(osc["ok"], true, "{osc}");
    assert_eq!(osc["output"], "body", "{osc}");
    let read = read_stream(&tools, &osc, None, 1);
    assert_eq!(read.data, b"\x1b]0;title\x07body", "{osc}");

    // Binary bytes including NUL are mirrored unchanged on every edge.
    let binary = run_bash(&tools, "printf 'a\\000b' | cat | wc -c");
    assert_eq!(binary["ok"], true, "{binary}");
    assert!(binary["output"].as_str().unwrap().contains('3'), "{binary}");
    assert_eq!(
        read_stream(&tools, &binary, None, 1).data,
        b"a\0b",
        "{binary}"
    );
    assert_eq!(
        read_stream(&tools, &binary, None, 2).data,
        b"a\0b",
        "{binary}"
    );

    // The main result only carries the last stage's output, never a copy of the
    // upstream stage's bytes.
    let mapped = run_bash(&tools, "printf 'first' | cat | tr a-z A-Z");
    assert_eq!(mapped["ok"], true, "{mapped}");
    assert_eq!(mapped["output"], "FIRST", "{mapped}");
    assert_eq!(
        read_stream(&tools, &mapped, None, 1).data,
        b"first",
        "{mapped}"
    );
}

#[test]
fn pipeline_host_setup_failure_finalizes_run_without_leaking_running_state() {
    struct Restore {
        root: std::path::PathBuf,
        moved: std::path::PathBuf,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.root);
            let _ = fs::rename(&self.moved, &self.root);
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_owned();
    let moved = root.with_extension("moved");
    let tools = Tools::new(root.clone(), EditMode::Replace).unwrap();

    // Keep the already-open workspace descriptor valid while making the startup
    // path unusable for the command's scratch directory.
    fs::rename(&root, &moved).unwrap();
    fs::write(&root, b"not a directory").unwrap();
    let _restore = Restore {
        root: root.clone(),
        moved,
    };

    let result = run_bash(&tools, "printf never");
    assert_eq!(result["ok"], false, "{result}");
    assert!(result["error"].is_string(), "{result}");
    assert!(result.get("pipes").is_none(), "{result}");

    let store = tools.pipe_store();
    let runs = store.list();
    assert_eq!(runs.len(), 1, "{result}");
    assert_eq!(runs[0].owner, "test-call");
    let snapshot = store.snapshot(&runs[0].run_id).unwrap().to_json();
    assert_eq!(snapshot["state"], "interrupted", "{snapshot}");
    assert_eq!(snapshot["exit_code"], Value::Null, "{snapshot}");
    assert_eq!(snapshot["incomplete"], false, "{snapshot}");
}

#[test]
fn pipeline_background_not_found_reported_once_without_phantom() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

    // A background pipeline that cannot start any process must run its stages
    // exactly once: no second logical execution and no half-finished
    // occurrence left behind by the fast external path.
    let result = run_bash(&tools, "zzz | yyy & wait");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    let output = result["output"].as_str().unwrap();
    assert_eq!(output.matches("zzz").count(), 1, "{result}");
    assert_eq!(output.matches("yyy").count(), 1, "{result}");
    assert_eq!(output.lines().count(), 2, "{result}");

    let list = pipelines(&result);
    assert_eq!(
        list.len(),
        1,
        "the failed spawn must not execute the pipeline twice: {result}"
    );
    let pipeline = &list[0];
    assert_eq!(pipeline["stage_count"], 2, "{result}");
    assert_eq!(pipeline["status"], 127, "{result}");
    assert_eq!(stage_status(pipeline, 0), Some(127), "{result}");
    assert_eq!(stage_status(pipeline, 1), Some(127), "{result}");
    for index in 0..2 {
        assert_ne!(
            pipeline["stages"][index]["state"], "running",
            "no stage may stay unfinished: {result}"
        );
    }
    assert_eq!(nonzero_stages(&result), 2, "{result}");

    // The edge exists, carried no bytes, and is closed instead of half-open.
    let read = read_stream(&tools, &result, None, 1);
    assert_eq!(read.total_bytes, 0, "{result}");
    assert_eq!(read.data, b"", "{result}");
    assert!(!read.open, "{result}");
}

/// Runs a live three-stage pipeline whose producer is defined by
/// `command_for(gate)` and verifies that both intermediate edges carry the
/// first line while the producer is still blocked on the gate. The final `tail`
/// keeps its output until EOF, so the main result stays empty during that
/// window.
fn assert_live_three_stage_streams(command_for: impl FnOnce(&std::path::Path) -> String) {
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("gate");
    let command = command_for(&gate);

    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let store = tools.pipe_store();
    let runner = std::thread::spawn(move || run_bash(&tools, &command));

    let deadline = Instant::now() + Duration::from_secs(10);
    let run_id = loop {
        if let Some(run) = store.list().into_iter().next() {
            break run.run_id;
        }
        assert!(Instant::now() < deadline, "bash run did not start in time");
        std::thread::sleep(Duration::from_millis(5));
    };

    // While the producer waits on the gate, the first line must already be
    // visible on both edges. This fails if a logical stage is executed to
    // completion during pipeline assembly: then `cat`/`tail` start only after
    // the producer finishes and the second edge stays empty.
    let (first, second) = loop {
        let first = store.read(&run_id, 1, None);
        let second = store.read(&run_id, 2, None);
        if let (Ok(first), Ok(second)) = (first, second) {
            if second.total_bytes >= 4 {
                break (first, second);
            }
        }
        assert!(
            Instant::now() < deadline,
            "edge 2 never received the first line before the gate"
        );
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(first.data, b"one\n");
    assert_eq!(second.data, b"one\n");
    assert!(first.open && second.open);

    fs::write(&gate, b"").unwrap();
    let result = runner.join().unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["output"], "two\n", "{result}");

    let list = pipelines(&result);
    assert_eq!(list.len(), 1, "{result}");
    assert_eq!(list[0]["stage_count"], 3, "{result}");
    for index in 0..3 {
        assert_eq!(stage_status(&list[0], index), Some(0), "{result}");
    }

    let first = store.read(&run_id, 1, None).expect("edge 1");
    assert_eq!(first.data, b"one\ntwo\n");
    assert!(!first.open);
    let second = store.read(&run_id, 2, None).expect("edge 2");
    assert_eq!(second.data, b"one\ntwo\n");
    assert!(!second.open);
}

#[test]
fn pipeline_live_three_stage_compound_streams_both_edges_before_gate() {
    assert_live_three_stage_streams(|gate| {
        format!(
            "{{ printf 'one\\n'; while [ ! -f '{gate}' ]; do sleep 0.02; done; printf 'two\\n'; }} | cat | tail -n 1",
            gate = gate.display()
        )
    });
}

#[test]
fn pipeline_live_three_stage_function_streams_both_edges_before_gate() {
    assert_live_three_stage_streams(|gate| {
        format!(
            "live() {{ printf 'one\\n'; while [ ! -f '{gate}' ]; do sleep 0.02; done; printf 'two\\n'; }}; live | cat | tail -n 1",
            gate = gate.display()
        )
    });
}
