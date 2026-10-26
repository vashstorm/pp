use crate::{
    cancellation::CancellationToken,
    config::{EditMode, ToolLimits},
};
use anyhow::Result;
use serde_json::Value;
use std::path::Path;

mod file;
mod image;
mod selector;
mod text;
mod url;

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

pub(super) struct ReadResult {
    pub(super) value: Value,
    pub(super) images: Vec<ToolImage>,
}

/// 同步读取接口；每种实现声明自己的输入类型，共享工具结果格式。
trait Read {
    type Input;

    fn read(&self, input: Self::Input, context: &mut ReadContext<'_, '_>) -> Result<ReadResult>;
}

/// 每次调用借用运行环境，不持有独立的异步运行时或会话状态。
struct ReadContext<'a, 'fetch> {
    cwd: &'a Path,
    limits: ToolLimits,
    mode: EditMode,
    cache: &'a mut WebCache,
    fetch: &'a mut Fetch<'fetch>,
    cancellation: &'a CancellationToken,
}

pub(super) fn run(
    path: &str,
    cwd: &Path,
    limits: ToolLimits,
    mode: EditMode,
    cache: &mut WebCache,
    fetch: &mut Fetch<'_>,
    cancellation: &CancellationToken,
) -> Result<ReadResult> {
    let target = Target::parse(path, cwd)?;
    let mut context = ReadContext {
        cwd,
        limits,
        mode,
        cache,
        fetch,
        cancellation,
    };
    if target.is_url() {
        UrlReader.read(UrlInput(target), &mut context)
    } else {
        FileReader.read(FileInput(target), &mut context)
    }
}

#[cfg(test)]
mod tests;
