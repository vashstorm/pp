use anyhow::Result;
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::cancellation::CancellationToken;
use pp_llm::http::HttpResponse;
use serde_json::Value;
use std::{borrow::Cow, io::Cursor, path::Path};

mod dedup;
pub(crate) use dedup::{ReadDedupCache, ReadProbe};
mod file;
mod image;
pub use image::decode_image;
mod selector;
mod text;
mod url;

pub use file::open as open_local_file;
use file::{FileInput, FileReader};
pub(super) use selector::{Target, parse_args};
use text::{count_lines, page_text, read_from};
pub(crate) use url::WebCache;
use url::{UrlInput, UrlReader};

pub(super) fn dedup_probe(
    path: &str,
    cwd: &Path,
    workspace: &pp_workspace::save::Workspace,
    mode: EditMode,
    limits: ToolLimits,
) -> Option<ReadProbe> {
    let target = Target::parse(path, cwd).ok()?;
    dedup::build_probe(&target, cwd, workspace, mode, limits)
}

/// Shares selector parsing with the actual read; resource URIs take no part in local project rule discovery.
pub(super) fn local_path(path: &str, cwd: &Path) -> Result<Option<std::path::PathBuf>> {
    Ok(Target::parse(path, cwd)?.local_path(cwd))
}

pub(super) fn is_http_url(path: &str, cwd: &Path) -> Result<bool> {
    Ok(Target::parse(path, cwd)?.is_url())
}

pub(super) fn agent_result(
    uri: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    binding: &dyn crate::ports::SubagentHost,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let mut target = Target::parse(uri, cwd)?;
    let (mut metadata, result) = binding.read_agent_result(&target.source, cancellation)?;
    let Some(result) = result else {
        return Ok(metadata);
    };
    let output = result
        .get("output")
        .or_else(|| result.get("error"))
        .unwrap_or(&result);
    let content = match output {
        Value::String(text) => Cow::Borrowed(text.as_str()),
        value => Cow::Owned(serde_json::to_string_pretty(value)?),
    };
    let source = target.source.clone();
    if !source["agent://".len()..].contains('/')
        && let Some(run_id) = metadata["published"]["id"].as_str()
    {
        // The paged continuation reads the same Run, so a later publish cannot replace the body.
        target.source = format!("{source}/{run_id}");
    }
    let bytes = content.as_bytes();
    let ranges = target.resolved_ranges(|| count_lines(Cursor::new(bytes)))?;
    let page = page_text(&target, limits, mode, false, ranges, |start, count, max| {
        pp_core::cancellation::check(cancellation)?;
        read_from(Cursor::new(bytes), start, count, max)
    })?;
    if let (Some(metadata), Value::Object(page)) = (metadata.as_object_mut(), page) {
        metadata.extend(page);
    }
    metadata["source"] = Value::String(source);
    Ok(metadata)
}

pub(super) fn artifact(
    path: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    store: &pp_workspace::output::Store,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    let target = Target::parse(path, cwd)?;
    let snapshot = store.snapshot(&target.source, cancellation)?;
    let mut cache = WebCache::default();
    let mut fetch =
        |_: &str, _: &CancellationToken| anyhow::bail!("artifact does not support network reads");
    let mut result = FileReader.read(
        FileInput {
            target,
            path: snapshot.path.clone(),
            allow_directory: true,
            approved: None,
        },
        &mut ReadContext {
            limits,
            mode,
            cache: &mut cache,
            fetch: &mut fetch,
            cancellation,
            snapshots: None,
        },
    )?;
    result.value["artifact"] = snapshot.metadata;
    Ok(result)
}

pub(super) fn approved_session(
    target: Target,
    path: std::path::PathBuf,
    workspace: &pp_workspace::save::Workspace,
    limits: ToolLimits,
    mode: EditMode,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    pp_core::cancellation::check(cancellation)?;
    let approved = workspace.external_target(&path, false)?;
    FileReader.read(
        FileInput {
            target,
            path,
            allow_directory: false,
            approved: Some(approved),
        },
        &mut ReadContext {
            limits,
            mode,
            cache: &mut WebCache::default(),
            fetch: &mut |_, _| unreachable!("session files support local reads only"),
            cancellation,
            snapshots: None,
        },
    )
}

pub use pp_workspace::image::ToolImage;

pub type Fetch<'a> = dyn FnMut(&str, &CancellationToken) -> Result<HttpResponse> + 'a;

pub struct ReadResult {
    pub value: Value,
    pub images: Vec<ToolImage>,
}

/// Synchronous read interface; each implementation declares its own input type but shares the tool result format.
trait Read {
    type Input;

    fn read(&self, input: Self::Input, context: &mut ReadContext<'_, '_>) -> Result<ReadResult>;
}

/// Each call borrows the run environment and holds no separate async runtime or session state.
struct ReadContext<'a, 'fetch> {
    limits: ToolLimits,
    mode: EditMode,
    cache: &'a mut WebCache,
    fetch: &'a mut Fetch<'fetch>,
    cancellation: &'a CancellationToken,
    snapshots: Option<(
        &'a pp_workspace::save::Workspace,
        &'a mut super::hashline::Store,
    )>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_snapshots(
    path: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    skills: &pp_config::skills::SharedSkills,
    cache: &mut WebCache,
    snapshots: Option<(&pp_workspace::save::Workspace, &mut super::hashline::Store)>,
    fetch: &mut Fetch<'_>,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    let target = Target::parse(path, cwd)?;
    let skill = if target.source.starts_with("skill://") {
        Some(
            skills
                .lock()
                .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
                .resolve_uri(&target.source)?,
        )
    } else {
        None
    };
    let limits = if skill.is_some() {
        ToolLimits {
            read_max_bytes: usize::MAX,
            read_max_lines: usize::MAX,
            ..limits
        }
    } else {
        limits
    };
    let mut context = ReadContext {
        limits,
        mode,
        cache,
        fetch,
        cancellation,
        snapshots,
    };
    let mut result = if target.is_url() {
        UrlReader.read(UrlInput(target), &mut context)?
    } else {
        let path = skill
            .as_ref()
            .map(|skill| skill.path.clone())
            .or_else(|| target.local_path(cwd))
            .unwrap_or_else(|| cwd.join(&target.source));
        FileReader.read(
            FileInput {
                target,
                path,
                allow_directory: skill.is_some(),
                approved: None,
            },
            &mut context,
        )?
    };
    if let Some(skill) = skill {
        result.value["path"] = serde_json::to_value(skill.path)?;
        result.value["base_dir"] = serde_json::to_value(skill.base_dir)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    path: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    skills: &pp_config::skills::SharedSkills,
    cache: &mut WebCache,
    fetch: &mut Fetch<'_>,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    run_with_snapshots(
        path,
        cwd,
        limits,
        mode,
        skills,
        cache,
        None,
        fetch,
        cancellation,
    )
}

/// Explicit local reference from the main chat: the path is literal, with URL, skill and line-range syntax disabled.
pub fn local_reference(
    path: &Path,
    limits: ToolLimits,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    if cancellation.is_cancelled() {
        return Err(pp_core::cancellation::Cancelled.into());
    }
    if path.is_file() {
        use std::io::Read as _;
        let mut probe = Vec::new();
        let file = file::open(path)?;
        let size = file.metadata()?.len();
        file.take(limits.read_max_bytes.saturating_add(4) as u64)
            .read_to_end(&mut probe)?;
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let image_path = ["png", "jpg", "jpeg", "webp", "gif"]
            .iter()
            .any(|e| extension.eq_ignore_ascii_case(e));
        if !image_path && !image::is_image(&probe) {
            anyhow::ensure!(
                !probe.contains(&0),
                "referenced file is not text or a supported image"
            );
            if let Err(error) = std::str::from_utf8(&probe) {
                anyhow::ensure!(
                    size > probe.len() as u64
                        && error.error_len().is_none()
                        && error.valid_up_to() >= probe.len().saturating_sub(3),
                    "referenced text must be UTF-8 encoded"
                );
            }
        }
    }
    let target = Target::literal(path.to_string_lossy().into_owned());
    let mut cache = WebCache::default();
    let mut fetch = |_: &str, _: &CancellationToken| {
        anyhow::bail!("local references do not support network reads")
    };
    FileReader.read(
        FileInput {
            target,
            path: path.to_path_buf(),
            allow_directory: true,
            approved: None,
        },
        &mut ReadContext {
            limits,
            mode: EditMode::Replace,
            cache: &mut cache,
            fetch: &mut fetch,
            cancellation,
            snapshots: None,
        },
    )
}
