//! 行模式 `/pipes`：有界的 run／流列表与补读 URI，不输出阶段正文。

use pp_runtime::tools;

/// 行模式 `/pipes`：有界的 run/流列表与补读 URI，不输出阶段正文。
pub(crate) fn cli_pipes_list(store: &tools::PipeStore, run: Option<&str>) -> String {
    const MAX_RUNS: usize = 8;
    const MAX_STREAMS: usize = 8;
    let runs = store
        .list()
        .into_iter()
        .map(|info| info.to_json())
        .collect::<Vec<_>>();
    if runs.is_empty() {
        return "No temporary pipe runs in this session.\n".to_owned();
    }
    let selected = match run {
        Some(id) => match store.snapshot(id) {
            Ok(info) => info.to_json(),
            Err(error) => return format!("{}\n", error.message()),
        },
        None => runs[0].clone(),
    };
    let mut text = format!("Pipe runs: {} (newest first)\n", runs.len());
    for info in runs.iter().take(MAX_RUNS) {
        text.push_str(&format!("  {}\n", pipes_run_line(info)));
    }
    if runs.len() > MAX_RUNS {
        text.push_str(&format!("  … {} older runs\n", runs.len() - MAX_RUNS));
    }
    let uri = selected["uri"].as_str().unwrap_or_default();
    text.push_str(&format!("Selected run: {uri}\n"));
    let streams = selected["streams"].as_array().cloned().unwrap_or_default();
    if streams.is_empty() {
        text.push_str("  no captured streams\n");
    } else {
        for stream in streams.iter().take(MAX_STREAMS) {
            let pipeline = stream["pipeline"].as_u64().unwrap_or_default();
            let producer = stream["producer_stage"].as_u64().unwrap_or_default();
            let consumer = stream["consumer_stage"].as_u64().unwrap_or_default();
            let state = if stream["open"] == true {
                "open"
            } else {
                "closed"
            };
            let retained_start = stream["retained_start"].as_u64().unwrap_or_default();
            let retained_end = stream["retained_end"].as_u64().unwrap_or_default();
            let total = stream["total_bytes"].as_u64().unwrap_or_default();
            text.push_str(&format!(
                "  {} · pipeline {pipeline} stage {producer}→{consumer} · {state} · bytes {retained_start}..{retained_end} of {total}\n",
                stream["uri"].as_str().unwrap_or_default()
            ));
        }
        if streams.len() > MAX_STREAMS {
            text.push_str(&format!(
                "  … {} more streams\n",
                streams.len() - MAX_STREAMS
            ));
        }
        text.push_str(
            "Read a stream with read(path=\"pipe://<run>/<stream>\"); bodies are not printed here.\n",
        );
    }
    text
}

fn pipes_run_line(info: &serde_json::Value) -> String {
    let mut line = format!(
        "{} · {} · {}",
        info["uri"].as_str().unwrap_or_default(),
        info["owner"].as_str().unwrap_or("-"),
        info["state"].as_str().unwrap_or("unknown"),
    );
    if let Some(code) = info["exit_code"].as_i64() {
        line.push_str(&format!(" · exit {code}"));
    }
    let pipelines = info["pipelines"].as_array().map_or(0, Vec::len);
    let omitted = info["omitted_pipelines"].as_u64().unwrap_or(0);
    line.push_str(&format!(" · pipelines {pipelines}"));
    if omitted > 0 {
        line.push_str(&format!(" (+{omitted} omitted)"));
    }
    let nonzero = info["nonzero_stages"].as_u64().unwrap_or(0);
    if nonzero > 0 {
        line.push_str(&format!(" · Non-zero stage results: {nonzero}"));
    }
    if info["incomplete"] == true || info["capture_error"].is_string() {
        line.push_str(" · stage output incomplete");
    }
    line
}
