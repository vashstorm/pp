use super::{
    MAX_INPUT_BYTES, Read, ReadContext, ReadResult,
    image::{ImageInput, ImageReader, is_image},
    selector::Target,
    text::{Page, count_lines, page_text, read_from},
};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, OpenOptions},
    io::{BufReader, Cursor, Read as IoRead},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

pub(super) struct FileReader;
pub(super) struct FileInput {
    pub(super) target: Target,
    pub(super) path: PathBuf,
    pub(super) allow_directory: bool,
}

impl Read for FileReader {
    type Input = FileInput;

    fn read(
        &self,
        FileInput {
            target,
            path: local,
            allow_directory,
        }: Self::Input,
        context: &mut ReadContext<'_, '_>,
    ) -> Result<ReadResult> {
        if allow_directory && fs::metadata(&local)?.is_dir() {
            let mut entries = fs::read_dir(&local)?
                .map(|entry| {
                    let entry = entry?;
                    let directory = entry.file_type()?.is_dir();
                    Ok((!directory, entry.file_name()))
                })
                .collect::<std::io::Result<Vec<_>>>()?;
            entries.sort_unstable();
            let mut listing = String::new();
            for (file, name) in entries {
                listing.push_str(&name.to_string_lossy());
                if !file {
                    listing.push('/');
                }
                listing.push('\n');
            }
            if listing.is_empty() {
                listing.push_str("(empty directory)");
            }
            let ranges = target.resolved_ranges(|| count_lines(Cursor::new(&listing)))?;
            let value = page_text(
                &target,
                context.limits,
                context.mode,
                false,
                ranges,
                |start, count, max| read_from(Cursor::new(&listing), start, count, max),
            )?;
            return Ok(ReadResult {
                value,
                images: Vec::new(),
            });
        }
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
        // 仅可安全编辑的本地文件签发标签；完整读取与分页共享同一份字节。
        let mut snapshot_warning = None;
        if !allow_directory
            && !target.raw
            && matches!(context.mode, crate::config::EditMode::Hashline)
            && let Some((workspace, store)) = context.snapshots.as_mut()
        {
            let safe_target = workspace.target(Path::new(&target.source), false);
            if let Ok(safe_target) = safe_target {
                let key: PathBuf = workspace
                    .components(Path::new(&target.source))?
                    .into_iter()
                    .collect();
                let bytes = safe_target.read_bounded(crate::tools::hashline::MAX_FILE_BYTES)?;
                if bytes.len() <= crate::tools::hashline::MAX_FILE_BYTES {
                    if let Ok(source) = std::str::from_utf8(&bytes) {
                        ensure!(
                            !source.contains('\0'),
                            "read requires text or a supported image"
                        );
                        let lines = crate::tools::hashline::scan(source);
                        let ranges = target.resolved_ranges(|| Ok(lines.len()))?;
                        let mut value = page_text(
                            &target,
                            context.limits,
                            context.mode,
                            true,
                            ranges,
                            |start, count, max| {
                                let mut end = start.saturating_sub(1);
                                let first = end;
                                let mut used = 0;
                                while end < lines.len() && end - first < count {
                                    let length = lines[end].end
                                        - if end == 0 { 0 } else { lines[end].start };
                                    if used + length > max {
                                        break;
                                    }
                                    used += length;
                                    end += 1;
                                }
                                ensure!(
                                    end > first || first >= lines.len(),
                                    "line at offset {start} exceeds read_max_bytes ({max})"
                                );
                                let bytes = if end > first {
                                    bytes[if first == 0 { 0 } else { lines[first].start }
                                        ..lines[end - 1].end]
                                        .to_vec()
                                } else {
                                    Vec::new()
                                };
                                Ok(Page {
                                    bytes,
                                    lines_read: end - first,
                                    next_offset: (end < lines.len()).then_some(end + 1),
                                })
                            },
                        )?;
                        let seen = value["ranges"]
                            .as_array()
                            .expect("分页总是包含 ranges")
                            .iter()
                            .map(|range| {
                                range["start"].as_u64().expect("起始行") as usize
                                    ..range["end"].as_u64().expect("结束行") as usize + 1
                            })
                            .collect();
                        let tag = crate::tools::hashline::file_hash(source);
                        let display = key.to_string_lossy();
                        let encodable = !display.contains(['[', ']', '\r', '\n']);
                        let registered = encodable
                            && store.record(key.clone(), std::sync::Arc::from(source), seen);
                        if registered {
                            let header = format!("[{display}#{tag}]");
                            value["content"] = serde_json::json!(format!(
                                "{header}\n{}",
                                value["content"].as_str().unwrap_or_default()
                            ));
                        } else {
                            value["warning"] = serde_json::json!(
                                "No editable hashline snapshot: ambiguous tag, unsupported path or cache limit; read again before editing."
                            );
                        }
                        return Ok(ReadResult {
                            value,
                            images: Vec::new(),
                        });
                    }
                    snapshot_warning = Some("No editable hashline snapshot: file is not UTF-8.");
                } else {
                    snapshot_warning = Some("No editable hashline snapshot: file exceeds 4 MiB.");
                }
                store.invalidate(&key);
            } else {
                snapshot_warning = Some(
                    "No editable hashline snapshot: path is outside the editable workspace or contains a symlink.",
                );
            }
        }
        let ranges = target.resolved_ranges(|| count_lines(BufReader::new(open(&local)?)))?;
        let mut value = page_text(
            &target,
            context.limits,
            context.mode,
            true,
            ranges,
            |start, count, max| read(&local, start, count, max),
        )?;
        if let Some(warning) = snapshot_warning {
            value["warning"] = serde_json::json!(warning);
        }
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
