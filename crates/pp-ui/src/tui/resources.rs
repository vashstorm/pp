//! Clipboard and resource preparation run on a dedicated thread; replies belong only to
//! the draft that started them.
use anyhow::{Context, Result, ensure};
use pp_config::config::ToolLimits;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use pp_runtime::user_input;
use pp_runtime::user_input::Image;
use pp_runtime::user_input::UserInput;

pub(super) type Bindings = std::collections::BTreeMap<String, std::ops::Range<usize>>;
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver},
    thread,
};

pub(super) enum Operation {
    Prepare {
        source: String,
        text: String,
        images: Vec<Image>,
        limits: ToolLimits,
    },
    Paste(String),
    Clipboard,
    References(Vec<user_input::Reference>),
}

pub(super) enum Output {
    Empty,
    Prepared(UserInput),
    Images(Vec<Image>),
    Text(String),
    References(Vec<(std::ops::Range<usize>, Image)>),
}

pub(super) struct Job {
    pub reply: Receiver<Result<Output>>,
    pub cancellation: CancellationToken,
    pub source: String,
    pub kind: super::PendingKind,
    pub cursor: ratatui_textarea::DataCursor,
    pub replace_paste: bool,
    pub prepare_after: Option<String>,
}

#[derive(Clone)]
struct BindingEdit {
    before: String,
    after: String,
    previous: Bindings,
    next: Bindings,
}

#[derive(Default, Clone)]
pub(super) struct Draft {
    images: Vec<Image>,
    bindings: Bindings,
    undo: Vec<BindingEdit>,
    redo: Vec<BindingEdit>,
}

#[derive(Default)]
pub(super) struct State {
    pub images: Vec<Image>,
    pub history_images: std::collections::BTreeMap<usize, Vec<Image>>,
    pub history_draft: Option<Draft>,
    pub ready: Option<UserInput>,
    pub job: Option<Job>,
    pub next_image: u64,
    pub plan_discussion: bool,
    pub bindings: Bindings,
    undo: Vec<BindingEdit>,
    redo: Vec<BindingEdit>,
    pub epoch: u64,
}

impl State {
    pub fn snapshot_draft(&self) -> Draft {
        Draft {
            images: self.images.clone(),
            bindings: self.bindings.clone(),
            undo: self.undo.clone(),
            redo: self.redo.clone(),
        }
    }

    pub fn restore_draft(&mut self, draft: Draft) {
        self.epoch = self.epoch.wrapping_add(1);
        self.images = draft.images;
        self.bindings = draft.bindings;
        self.undo = draft.undo;
        self.redo = draft.redo;
    }

    pub fn set_images(&mut self, images: Vec<Image>) {
        for image in &images {
            if let Some(number) = image
                .marker
                .rsplit_once('#')
                .and_then(|(_, number)| number.parse::<u64>().ok())
            {
                self.next_image = self.next_image.max(number);
            }
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.bindings.clear();
        self.images = images;
        self.undo.clear();
        self.redo.clear();
    }

    /// Restores position bindings only for trusted attachments coming from persistent
    /// history or pending messages.
    pub fn bind_text(&mut self, text: &str) {
        self.bindings = self
            .images
            .iter()
            .filter_map(|image| {
                image
                    .source_range
                    .as_ref()
                    .map_or_else(
                        || text.find(&image.marker),
                        |range| {
                            (text.get(range.clone()) == Some(image.marker.as_str()))
                                .then_some(range.start)
                        },
                    )
                    .map(|start| (image.marker.clone(), start..start + image.marker.len()))
            })
            .collect();
    }

    pub fn restore_input(&mut self, mut images: Vec<Image>, mut text: String) -> String {
        // Legacy history is converted only when a draft is recalled; on-disk records and
        // protocol history stay unchanged.
        for image in &mut images {
            if let Some(number) = image
                .marker
                .strip_prefix("[图片 ")
                .and_then(|s| s.strip_suffix(']'))
                && let Ok(number) = number.parse::<u64>()
            {
                let marker = format!("{} #{number}", super::attachment_edit::ICON);
                text = text.replacen(&image.marker, &marker, 1);
                image.marker = marker;
                self.next_image = self.next_image.max(number);
            }
        }
        self.set_images(images);
        self.bind_text(&text);
        text
    }

    /// A single user edit may call several textarea primitives, so undo must return to the
    /// same transaction boundary.
    pub fn undo_input(
        &mut self,
        input: &mut ratatui_textarea::TextArea<'static>,
        redo: bool,
    ) -> bool {
        let stack = if redo { &mut self.redo } else { &mut self.undo };
        let Some(edit) = stack.pop() else {
            return if redo { input.redo() } else { input.undo() };
        };
        let target = if redo { &edit.after } else { &edit.before };
        for _ in 0..1000 {
            let changed = if redo { input.redo() } else { input.undo() };
            if input.lines().join("\n") == *target {
                break;
            }
            if !changed {
                break;
            }
        }
        if input.lines().join("\n") != *target {
            // Restore the whole transaction even when the native history capacity runs out;
            // a half attachment must never be left behind.
            *input = super::editor();
            input.insert_str(target);
        }
        self.bindings = if redo {
            edit.next.clone()
        } else {
            edit.previous.clone()
        };
        if redo {
            self.undo.push(edit);
        } else {
            self.redo.push(edit);
        }
        true
    }

    pub fn edited(
        &mut self,
        before: String,
        after: String,
        previous: Bindings,
        range: Option<(usize, usize, usize)>,
    ) {
        if before == after && range.is_none() {
            return;
        }
        self.shift_bindings(&before, &after, &previous, range);
        self.record(before, after, previous);
    }

    pub fn shift_bindings(
        &mut self,
        before: &str,
        after: &str,
        previous: &Bindings,
        range: Option<(usize, usize, usize)>,
    ) {
        let (start, end, inserted_end) =
            range.unwrap_or_else(|| super::attachment_edit::changed_range(before, after));
        let delta = inserted_end as isize - end as isize;
        let mut next = Bindings::new();
        for (marker, range) in previous {
            let range = if range.end <= start {
                range.clone()
            } else if range.start >= end {
                range.start.saturating_add_signed(delta)..range.end.saturating_add_signed(delta)
            } else {
                continue;
            };
            if after.get(range.clone()) == Some(marker.as_str()) {
                next.insert(marker.clone(), range);
            }
        }
        for (marker, range) in &self.bindings {
            if !previous.contains_key(marker) && after.get(range.clone()) == Some(marker.as_str()) {
                next.insert(marker.clone(), range.clone());
            }
        }
        self.bindings = next;
    }

    pub fn record(&mut self, before: String, after: String, previous: Bindings) {
        if before == after && previous == self.bindings {
            return;
        }
        self.undo.push(BindingEdit {
            before,
            after,
            previous,
            next: self.bindings.clone(),
        });
        if self.undo.len() > 100 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    pub fn cancel(&mut self) {
        self.plan_discussion = false;
        if let Some(job) = self.job.take() {
            job.cancellation.cancel();
        }
    }

    pub fn active_images(&self, text: &str) -> Vec<Image> {
        let mut images: Vec<_> = self
            .images
            .iter()
            .filter_map(|image| {
                let range = self.bindings.get(&image.marker)?;
                (text.get(range.clone()) == Some(image.marker.as_str())).then(|| {
                    let mut image = image.clone();
                    image.source_range = Some(range.clone());
                    (range.start, image)
                })
            })
            .collect();
        images.sort_by_key(|(start, _)| *start);
        images.into_iter().map(|(_, image)| image).collect()
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub(super) fn start(
    operation: Operation,
    cwd: PathBuf,
    source: String,
    kind: super::PendingKind,
    cursor: ratatui_textarea::DataCursor,
) -> Result<Job> {
    let (sender, reply) = mpsc::channel();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    thread::Builder::new()
        .name("pp-input".into())
        .spawn(move || {
            let result = (|| {
                check(&token)?;
                let output = match operation {
                    Operation::Prepare {
                        source,
                        text,
                        images,
                        limits,
                    } => Output::Prepared(user_input::prepare(
                        source, &text, images, &cwd, limits, &token,
                    )?),
                    Operation::Paste(text) => {
                        if let Some(paths) = user_input::pasted_paths(&text, &cwd) {
                            Output::Images(load_paths(paths, &token)?)
                        } else {
                            Output::Text(text)
                        }
                    }
                    Operation::Clipboard => clipboard(&cwd, &token)?,
                    Operation::References(references) => {
                        let mut replacements = Vec::new();
                        for reference in references {
                            check(&token)?;
                            let path = user_input::resolve(
                                reference.path.as_deref().expect("image path"),
                                &cwd,
                            )?;
                            replacements.push((
                                reference.start..reference.end,
                                user_input::image_file(&path)?,
                            ));
                        }
                        Output::References(replacements)
                    }
                };
                check(&token)?;
                Ok(output)
            })();
            let _ = sender.send(result);
        })
        .context("starting the input resource thread")?;
    Ok(Job {
        reply,
        cancellation,
        source,
        kind,
        cursor,
        replace_paste: false,
        prepare_after: None,
    })
}

fn load_paths(paths: Vec<PathBuf>, token: &CancellationToken) -> Result<Vec<Image>> {
    paths
        .iter()
        .map(|path| {
            check(token)?;
            user_input::image_file(path)
                .with_context(|| format!("cannot attach image {}", path.display()))
        })
        .collect()
}

fn clipboard(cwd: &std::path::Path, token: &CancellationToken) -> Result<Output> {
    // The system clipboard in an SSH session belongs to the remote machine and cannot
    // stand in for the user's terminal clipboard.
    ensure!(
        std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none(),
        "in a remote session, paste an image path or @path from the terminal"
    );
    let mut clipboard =
        arboard::Clipboard::new().context("system clipboard unavailable; use @path")?;
    read_clipboard(&mut clipboard, cwd, token)
}

trait ClipboardSource {
    fn files(&mut self) -> std::result::Result<Vec<PathBuf>, arboard::Error>;
    fn image(&mut self) -> std::result::Result<arboard::ImageData<'static>, arboard::Error>;
    fn text(&mut self) -> std::result::Result<String, arboard::Error>;
}

impl ClipboardSource for arboard::Clipboard {
    fn files(&mut self) -> std::result::Result<Vec<PathBuf>, arboard::Error> {
        self.get().file_list()
    }
    fn image(&mut self) -> std::result::Result<arboard::ImageData<'static>, arboard::Error> {
        self.get_image()
    }
    fn text(&mut self) -> std::result::Result<String, arboard::Error> {
        self.get_text()
    }
}

fn read_clipboard(
    clipboard: &mut impl ClipboardSource,
    cwd: &std::path::Path,
    token: &CancellationToken,
) -> Result<Output> {
    // Finder provides both file URLs and an icon bitmap, so prefer the real files.
    if let Ok(paths) = clipboard.files() {
        let images: Vec<_> = paths
            .into_iter()
            .filter(|p| user_input::image_extension(p))
            .collect();
        if !images.is_empty() {
            return Ok(Output::Images(load_paths(images, token)?));
        }
    }
    check(token)?;
    match clipboard.image() {
        Ok(image) => {
            ensure!(
                image.width > 0
                    && image.height > 0
                    && (image.width as u64)
                        .checked_mul(image.height as u64)
                        .is_some_and(|n| n <= 32_000_000),
                "clipboard image exceeds the 32-megapixel limit"
            );
            let rgba = image::RgbaImage::from_raw(
                u32::try_from(image.width)?,
                u32::try_from(image.height)?,
                image.bytes.into_owned(),
            )
            .context("clipboard image data is invalid")?;
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(rgba).write_to(&mut png, image::ImageFormat::Png)?;
            Ok(Output::Images(vec![user_input::image_bytes(
                "clipboard",
                png.get_ref(),
            )?]))
        }
        Err(arboard::Error::ContentNotAvailable) => {
            let text = match clipboard.text() {
                Ok(text) => text,
                Err(arboard::Error::ContentNotAvailable) => return Ok(Output::Empty),
                Err(error) => return Err(error).context("failed to read clipboard text"),
            };
            if let Some(paths) = user_input::pasted_paths(&text, cwd) {
                Ok(Output::Images(load_paths(paths, token)?))
            } else {
                Ok(Output::Text(text))
            }
        }
        Err(error) => Err(error).context("failed to read clipboard image; use @path"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Clipboard {
        files: Vec<PathBuf>,
        bitmap: bool,
        text: std::result::Result<String, arboard::Error>,
        image_reads: usize,
    }
    impl ClipboardSource for Clipboard {
        fn files(&mut self) -> std::result::Result<Vec<PathBuf>, arboard::Error> {
            Ok(self.files.clone())
        }
        fn image(&mut self) -> std::result::Result<arboard::ImageData<'static>, arboard::Error> {
            self.image_reads += 1;
            if self.bitmap {
                Ok(arboard::ImageData {
                    width: 1,
                    height: 1,
                    bytes: vec![255, 0, 0, 255].into(),
                })
            } else {
                Err(arboard::Error::ContentNotAvailable)
            }
        }
        fn text(&mut self) -> std::result::Result<String, arboard::Error> {
            std::mem::replace(&mut self.text, Err(arboard::Error::ContentNotAvailable))
        }
    }

    #[test]
    fn resource_clipboard_prefers_real_file_over_finder_icon_and_keeps_multiple_images() {
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = ["one.png", "two.png"]
            .iter()
            .map(|name| {
                let path = dir.path().join(name);
                image::DynamicImage::new_rgb8(2, 3).save(&path).unwrap();
                path
            })
            .collect();
        let mut source = Clipboard {
            files: paths,
            bitmap: true,
            text: Ok(String::new()),
            image_reads: 0,
        };
        let Output::Images(images) =
            read_clipboard(&mut source, dir.path(), &CancellationToken::new()).unwrap()
        else {
            panic!()
        };
        assert_eq!(images.len(), 2);
        assert_eq!(source.image_reads, 0);
        assert!(images.iter().all(|image| image.summary.contains("2 x 3")));
    }

    #[test]
    fn resource_clipboard_bitmap_and_text_fallback_share_normalization() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = Clipboard {
            files: Vec::new(),
            bitmap: true,
            text: Ok("unused".into()),
            image_reads: 0,
        };
        let Output::Images(images) =
            read_clipboard(&mut source, dir.path(), &CancellationToken::new()).unwrap()
        else {
            panic!()
        };
        assert_eq!(images.len(), 1);
        assert!(images[0].data_url.starts_with("data:image/png;base64,"));
        source.bitmap = false;
        let Output::Text(text) =
            read_clipboard(&mut source, dir.path(), &CancellationToken::new()).unwrap()
        else {
            panic!()
        };
        assert_eq!(text, "unused");
    }

    #[test]
    fn resource_clipboard_unavailable_content_is_empty_but_other_errors_propagate() {
        let dir = tempfile::tempdir().unwrap();
        for error in [
            arboard::Error::ContentNotAvailable,
            arboard::Error::ClipboardOccupied,
            arboard::Error::ConversionFailure,
        ] {
            let unavailable = matches!(error, arboard::Error::ContentNotAvailable);
            let expected = std::mem::discriminant(&error);
            let mut source = Clipboard {
                files: Vec::new(),
                bitmap: false,
                text: Err(error),
                image_reads: 0,
            };
            let result = read_clipboard(&mut source, dir.path(), &CancellationToken::new());
            if unavailable {
                assert!(matches!(result, Ok(Output::Empty)));
            } else {
                let actual = result
                    .err()
                    .expect("clipboard failures must still propagate");
                assert_eq!(
                    std::mem::discriminant(actual.downcast_ref::<arboard::Error>().unwrap()),
                    expected
                );
            }
        }
    }
}
