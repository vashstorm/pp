//! Read-only entry for temporary `pipe://` stage streams.
//!
//! Listings return bounded run/stream metadata; a body URI is pinned to one in-memory snapshot and reuses the ordinary read
//! selector/`page_text`/`read_from` paging pipeline. Nothing here writes temp files or issues
//! hashline; the display copy reuses the `OutputSink` `Normalizer`.

use super::{
    ReadResult,
    selector::Target,
    text::{count_lines, page_text, read_from},
};
use anyhow::{Context, Result, ensure};
use pp_bash::PipeStore;
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_workspace::output::Normalizer;
use serde_json::json;
use std::io::Cursor;
use std::path::Path;

pub(crate) const SCHEME: &str = "pipe://";

/// Only paths with the `pipe://` prefix enter this module; ordinary file names never leak in.
pub(crate) fn is_uri(path: &str) -> bool {
    path.starts_with(SCHEME)
}

/// Parse `pipe://<run-id>[/<stream-id>]`, consistent with `streams::parse_uri`.
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

/// Read a `pipe://` listing or body; `limits`/`mode` behave exactly like an ordinary read.
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
    // Run the selector first so ordinary syntax such as `:20`, `:-30` and `:raw` is reused directly.
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

/// Display copy: trims a UTF-8 fragment at the start of the kept range, then reuses the `OutputSink`
/// `Normalizer` for a trailing UTF-8 fragment, ANSI/control sequences and CR.
///
/// Forwarded raw bytes do not pass through here; `:raw` on `page_text` only affects selection/anchors,
/// so terminal control sequences never leak into the body because of `:raw`.
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
