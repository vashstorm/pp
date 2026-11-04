//! `pipe://` 临时阶段流的只读入口。
//!
//! 列表中列出 run/流的有界元数据；正文 URI 固定一次内存快照，复用普通 read
//! 的 selector/`page_text`/`read_from` 分页管线。这里不落临时文件、不签发
//! hashline；展示副本复用 `OutputSink` 的 `Normalizer`。

use super::{
    ReadResult,
    selector::Target,
    text::{count_lines, page_text, read_from},
};
use crate::tools::PipeStore;
use crate::tools::output::Normalizer;
use anyhow::{Context, Result, ensure};
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use serde_json::json;
use std::io::Cursor;
use std::path::Path;

pub(crate) const SCHEME: &str = "pipe://";

/// 只有 `pipe://` 前缀的路径进入本模块；普通文件名不会误入。
pub(crate) fn is_uri(path: &str) -> bool {
    path.starts_with(SCHEME)
}

/// 解析 `pipe://<run-id>[/<stream-id>]`，与 `streams::parse_uri` 保持一致。
fn parts(uri: &str) -> Option<(&str, Option<u64>)> {
    let rest = uri.strip_prefix(SCHEME)?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    match rest.split_once('/') {
        None => Some((rest, None)),
        Some((run, stream)) => {
            if run.is_empty() || stream.is_empty() {
                return None;
            }
            stream.parse().ok().map(|stream| (run, Some(stream)))
        }
    }
}

/// 读取 `pipe://` 列表或正文；`limits`/`mode` 与普通 read 完全一致。
pub(crate) fn read(
    uri: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    store: &PipeStore,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    ensure!(!uri.trim().is_empty(), "path must not be blank");
    cancellation::check(cancellation)?;
    // 先经过 selector，让 `:20`、`:-30`、`:raw` 等普通语法直接复用。
    let target = Target::parse(uri, cwd)?;
    ensure!(
        is_uri(&target.source),
        "pipe reads require a pipe:// URI or list URI"
    );
    let (run_id, stream) = parts(&target.source)
        .context("invalid pipe URI; expected pipe://<run-id>[/<stream-id>]")?;
    if stream.is_none() {
        let run = store
            .snapshot(run_id)
            .map_err(|error| anyhow::anyhow!(error))?;
        let mut value = run.to_json();
        value["ok"] = json!(true);
        value["source"] = json!(target.source);
        value["kind"] = json!("pipe-run");
        return Ok(ReadResult {
            value,
            images: Vec::new(),
        });
    }
    let stream_id = stream.expect("checked above");
    let slice = store
        .read(run_id, stream_id, None)
        .map_err(|error| anyhow::anyhow!(error))?;
    cancellation::check(cancellation)?;
    let text = display(&slice.data, slice.start > 0);
    let ranges = target.resolved_ranges(|| count_lines(Cursor::new(text.as_bytes())))?;
    let mut value = page_text(&target, limits, mode, false, ranges, |start, count, max| {
        read_from(Cursor::new(text.as_bytes()), start, count, max)
    })?;
    value["kind"] = json!("pipe-stream");
    value["pipe"] = json!({
        "run_id": run_id,
        "stream": stream_id,
        "start": slice.start,
        "end": slice.end,
        "retained_start": slice.start,
        "retained_end": slice.end,
        "total_bytes": slice.total_bytes,
        "open": slice.open,
        "revision": slice.revision,
        "prefix_evicted": slice.prefix_evicted || slice.start > 0,
    });
    Ok(ReadResult {
        value,
        images: Vec::new(),
    })
}

/// 展示副本：裁剪保留区间起始处的 UTF-8 残片，再复用 `OutputSink` 的
/// `Normalizer` 处理 UTF-8 末尾残片、ANSI/控制串与 CR。
///
/// 转发出去的原字节不经过这里；`page_text` 的 `:raw` 只影响选择/锚点，
/// 因此终端控制串不会因 `:raw` 泄漏到正文。
fn display(bytes: &[u8], fragment: bool) -> String {
    let bytes = if fragment {
        let mut skip = 0;
        while skip < bytes.len() && bytes[skip] & 0b1100_0000 == 0b1000_0000 {
            skip += 1;
        }
        &bytes[skip..]
    } else {
        bytes
    };
    let mut normalizer = Normalizer::default();
    let mut text = normalizer.feed(bytes, false);
    text.push_str(&normalizer.feed(&[], true));
    text
}
