use super::{
    MAX_INPUT_BYTES, Read, ReadContext, ReadResult,
    image::{ImageInput, ImageReader, is_image},
    selector::Target,
    text::{Page, count_lines, page_text, read_from},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::OpenOptions,
    io::{BufReader, Read as IoRead},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(super) struct FileReader;
pub(super) struct FileInput(pub(super) Target);

impl Read for FileReader {
    type Input = FileInput;

    fn read(
        &self,
        FileInput(target): Self::Input,
        context: &mut ReadContext<'_, '_>,
    ) -> Result<ReadResult> {
        let local = context.cwd.join(&target.source);
        let mut file = open(&local)?;
        let mut header = [0; 32];
        let n = file.read(&mut header)?;
        let image_extension = local.extension().and_then(|s| s.to_str()).is_some_and(|s| {
            ["png", "jpg", "jpeg", "webp", "gif"]
                .iter()
                .any(|e| s.eq_ignore_ascii_case(e))
        });
        if is_image(&header[..n]) || image_extension {
            target.validate_image()?;
            let mut bytes = Vec::new();
            open(&local)?
                .take(MAX_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            return ImageReader.read(ImageInput { target, bytes }, context);
        }
        ensure!(
            !header[..n].contains(&0),
            "read requires text or a supported image"
        );
        let ranges = target.resolved_ranges(|| count_lines(BufReader::new(open(&local)?)))?;
        let value = page_text(
            &target,
            context.limits,
            context.mode,
            true,
            ranges,
            |start, count, max| read(&local, start, count, max),
        )?;
        Ok(ReadResult {
            value,
            images: Vec::new(),
        })
    }
}

pub(super) fn open(path: &Path) -> Result<std::fs::File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    ensure!(file.metadata()?.is_file(), "read requires a regular file");
    Ok(file)
}

pub(super) fn read(path: &Path, offset: usize, limit: usize, max_bytes: usize) -> Result<Page> {
    read_from(BufReader::new(open(path)?), offset, limit, max_bytes)
}
