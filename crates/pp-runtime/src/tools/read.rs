use anyhow::Result;
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::cancellation::CancellationToken;
use pp_llm::http::HttpResponse;
use serde_json::Value;
use std::{borrow::Cow, io::Cursor, path::Path};

mod file;
mod image;
pub(crate) use image::decode_image;
mod pipe;
pub(crate) use pipe::{is_uri as is_pipe_uri, read as read_pipe};
mod selector;
mod text;
mod url;

pub(crate) use file::open as open_local_file;
use file::{FileInput, FileReader};
use selector::Target;
use text::{count_lines, page_text, read_from};
pub(crate) use url::WebCache;
use url::{UrlInput, UrlReader};

pub(super) fn agent_result(
    uri: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    binding: &crate::subagent::Binding,
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
        // 分页继续读取同一次 Run，避免后续发布替换正文。
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
    store: &super::output::Store,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    let target = Target::parse(path, cwd)?;
    let snapshot = store.snapshot(&target.source, cancellation)?;
    let mut cache = WebCache::default();
    let mut fetch = |_: &str, _: &CancellationToken| anyhow::bail!("artifact 不支持网络读取");
    let mut result = FileReader.read(
        FileInput {
            target,
            path: snapshot.path.clone(),
            allow_directory: true,
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

pub(crate) type Fetch<'a> = dyn FnMut(&str, &CancellationToken) -> Result<HttpResponse> + 'a;

pub(crate) struct ToolImage {
    pub(crate) data_url: String,
}

pub(crate) struct ReadResult {
    pub(crate) value: Value,
    pub(crate) images: Vec<ToolImage>,
}

/// 同步读取接口；每种实现声明自己的输入类型，共享工具结果格式。
trait Read {
    type Input;

    fn read(&self, input: Self::Input, context: &mut ReadContext<'_, '_>) -> Result<ReadResult>;
}

/// 每次调用借用运行环境，不持有独立的异步运行时或会话状态。
struct ReadContext<'a, 'fetch> {
    limits: ToolLimits,
    mode: EditMode,
    cache: &'a mut WebCache,
    fetch: &'a mut Fetch<'fetch>,
    cancellation: &'a CancellationToken,
    snapshots: Option<(&'a super::save::Workspace, &'a mut super::hashline::Store)>,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_with_snapshots(
    path: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    skills: &pp_config::skills::SharedSkills,
    cache: &mut WebCache,
    snapshots: Option<(&super::save::Workspace, &mut super::hashline::Store)>,
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
            .map_or_else(|| cwd.join(&target.source), |skill| skill.path.clone());
        FileReader.read(
            FileInput {
                target,
                path,
                allow_directory: skill.is_some(),
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

/// 主聊天的显式本地引用：路径按字面处理，不启用 URL、技能和行段语法。
pub(crate) fn local_reference(
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
            anyhow::ensure!(!probe.contains(&0), "引用文件不是文本或受支持的图片");
            if let Err(error) = std::str::from_utf8(&probe) {
                anyhow::ensure!(
                    size > probe.len() as u64
                        && error.error_len().is_none()
                        && error.valid_up_to() >= probe.len().saturating_sub(3),
                    "引用文本必须使用 UTF-8 编码"
                );
            }
        }
    }
    let target = Target::literal(path.to_string_lossy().into_owned());
    let mut cache = WebCache::default();
    let mut fetch = |_: &str, _: &CancellationToken| anyhow::bail!("本地引用不支持网络读取");
    FileReader.read(
        FileInput {
            target,
            path: path.to_path_buf(),
            allow_directory: true,
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
