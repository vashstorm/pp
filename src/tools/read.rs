use crate::{
    cancellation::CancellationToken,
    config::{EditMode, ToolLimits},
};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

mod file;
mod image;
pub(crate) use image::decode_image;
mod selector;
mod text;
mod url;

pub(crate) use file::open as open_local_file;
use file::{FileInput, FileReader};
use selector::Target;
pub(crate) use url::WebCache;
use url::{UrlInput, UrlReader};

pub(crate) const MAX_INPUT_BYTES: usize = 20 * 1024 * 1024;

pub(crate) struct HttpResponse {
    pub(crate) bytes: Vec<u8>,
    pub(crate) final_url: String,
    pub(crate) content_type: String,
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
    skills: &crate::skills::SharedSkills,
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
    skills: &crate::skills::SharedSkills,
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
        return Err(crate::cancellation::Cancelled.into());
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
