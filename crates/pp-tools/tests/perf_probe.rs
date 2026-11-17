//! release 性能探针：默认忽略，只在手工采集基线时运行。
//!
//! ```sh
//! cargo nextest run --release --locked -p pp-tools --run-ignored only -E 'test(perf_probe)'
//! ```
//!
//! 夹具由 `docs/perf.md` 记录的脚本在 `$PERF_FIXTURES` 下生成；未设置时跳过。

use pp_config::config::EditMode;
use pp_core::cancellation::CancellationToken;
use pp_core::plan::WorkflowState;
use pp_tools::{ToolPolicy, Tools};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn fixtures() -> Option<PathBuf> {
    std::env::var_os("PERF_FIXTURES").map(PathBuf::from)
}

fn tools(cwd: &Path) -> Tools {
    let mut tools = Tools::with_mcp(
        cwd.to_path_buf(),
        EditMode::Replace,
        std::sync::Arc::new(pp_mcp::McpManager::empty()),
    )
    .unwrap();
    tools.configure_eval(&pp_config::config::EvalSettings::default());
    tools
}

fn run(tools: &Tools, name: &str, args: Value) -> Value {
    let workflow = WorkflowState::default();
    let token = CancellationToken::new();
    let execution = tools.execute(
        name,
        &args.to_string(),
        "probe-call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |_| {},
    );
    serde_json::from_str(&execution.output).unwrap_or(Value::Null)
}

fn report(label: &str, runs: &[f64]) {
    println!(
        "probe {label} ms={}",
        runs.iter()
            .map(|v| format!("{v:.1}"))
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[values.len() / 2]
}

fn bench(label: &str, mut f: impl FnMut()) {
    let mut samples = Vec::new();
    for _ in 0..3 {
        let start = Instant::now();
        f();
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report(label, &samples);
    println!("probe {label} median_ms={:.1}", median(samples));
}

#[test]
#[ignore = "release perf probe"]
fn perf_probe_tool_paths() {
    let Some(root) = fixtures() else {
        println!("probe skipped: PERF_FIXTURES not set");
        return;
    };
    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().to_path_buf();
    std::fs::copy(root.join("edit3k.txt"), work.join("edit3k.txt")).unwrap();
    let tools = tools(&work);
    bench("glob_1k", || {
        let out = run(
            &tools,
            "glob",
            json!({"pattern": "*.txt", "path": root.join("d1000")}),
        );
        assert_eq!(out["ok"], true, "{out}");
    });
    bench("glob_10k", || {
        let out = run(
            &tools,
            "glob",
            json!({"pattern": "*.txt", "path": root.join("d10000")}),
        );
        assert_eq!(out["ok"], true, "{out}");
    });
    bench("glob_50k", || {
        let out = run(
            &tools,
            "glob",
            json!({"pattern": "*.txt", "path": root.join("d50000")}),
        );
        assert_eq!(out["ok"], true, "{out}");
    });
    bench("rg_10k", || {
        let out = run(
            &tools,
            "rg",
            json!({"pattern": "needle", "path": root.join("d10000")}),
        );
        assert_eq!(out["ok"], true, "{out}");
    });
    bench("rg_50k", || {
        let out = run(
            &tools,
            "rg",
            json!({"pattern": "needle", "path": root.join("d50000")}),
        );
        assert_eq!(out["ok"], true, "{out}");
    });
    bench("read_5k", || {
        let out = run(
            &tools,
            "read",
            json!({"path": root.join("big5k.txt").to_string_lossy()}),
        );
        assert_eq!(out["ok"], true, "{out}");
    });
    bench("edit_3k", || {
        let text = format!("line 1500 value{}", "");
        let out = run(
            &tools,
            "edit",
            json!({"path": "edit3k.txt", "edits": [{"oldText": text, "newText": "line 1500 value patched"}]}),
        );
        assert_eq!(out["ok"], true, "{out}");
        let restore = run(
            &tools,
            "edit",
            json!({"path": "edit3k.txt", "edits": [{"oldText": "line 1500 value patched", "newText": text}]}),
        );
        assert_eq!(restore["ok"], true, "{restore}");
    });
    bench("bash_100k", || {
        let out = run(&tools, "bash", json!({"command": "seq 1 100000"}));
        assert_eq!(out["ok"], true, "{out}");
    });
    let _ = tools.stop_background_tasks();
}
