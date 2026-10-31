//! 剪贴板和资源准备在独立线程执行，回复只属于启动它的草稿。
use crate::{
    cancellation::{CancellationToken, check},
    config::ToolLimits,
    user_input::{self, Image, UserInput},
};
use anyhow::{Context, Result, ensure};
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
}

pub(super) enum Output {
    Prepared(UserInput),
    Images(Vec<Image>),
    Text(String),
}

pub(super) struct Job {
    pub reply: Receiver<Result<Output>>,
    pub cancellation: CancellationToken,
    pub source: String,
    pub kind: super::PendingKind,
    pub cursor: ratatui_textarea::DataCursor,
    pub replace_paste: bool,
}

#[derive(Clone)]
struct BindingEdit {
    before: String,
    after: String,
    previous: std::collections::BTreeSet<String>,
    next: std::collections::BTreeSet<String>,
}

#[derive(Default, Clone)]
pub(super) struct Draft {
    images: Vec<Image>,
    live: std::collections::BTreeSet<String>,
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
    pub live: std::collections::BTreeSet<String>,
    undo: Vec<BindingEdit>,
    redo: Vec<BindingEdit>,
}

impl State {
    pub fn snapshot_draft(&self) -> Draft {
        Draft {
            images: self.images.clone(),
            live: self.live.clone(),
            undo: self.undo.clone(),
            redo: self.redo.clone(),
        }
    }

    pub fn restore_draft(&mut self, draft: Draft) {
        self.images = draft.images;
        self.live = draft.live;
        self.undo = draft.undo;
        self.redo = draft.redo;
    }

    pub fn set_images(&mut self, images: Vec<Image>) {
        self.live = images.iter().map(|image| image.marker.clone()).collect();
        self.images = images;
        self.undo.clear();
        self.redo.clear();
    }

    pub fn edited(
        &mut self,
        before: String,
        after: String,
        previous: std::collections::BTreeSet<String>,
        undo: bool,
        redo: bool,
    ) {
        if before == after {
            return;
        }
        if undo {
            if let Some(index) = self.undo.iter().rposition(|edit| edit.before == after) {
                while self.undo.len() > index {
                    let edit = self.undo.pop().expect("撤销记录");
                    self.live = edit.previous.clone();
                    self.redo.push(edit);
                }
                return;
            }
        } else if redo && let Some(index) = self.redo.iter().rposition(|edit| edit.after == after) {
            while self.redo.len() > index {
                let edit = self.redo.pop().expect("重做记录");
                self.live = edit.next.clone();
                self.undo.push(edit);
            }
            return;
        }
        self.live.retain(|marker| after.contains(marker));
        self.undo.push(BindingEdit {
            before,
            after,
            previous,
            next: self.live.clone(),
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
        self.images
            .iter()
            .filter(|image| self.live.contains(&image.marker) && text.contains(&image.marker))
            .cloned()
            .collect()
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
                };
                check(&token)?;
                Ok(output)
            })();
            let _ = sender.send(result);
        })
        .context("无法启动输入资源线程")?;
    Ok(Job {
        reply,
        cancellation,
        source,
        kind,
        cursor,
        replace_paste: false,
    })
}

fn load_paths(paths: Vec<PathBuf>, token: &CancellationToken) -> Result<Vec<Image>> {
    paths
        .iter()
        .map(|path| {
            check(token)?;
            user_input::image_file(path).with_context(|| format!("无法附加图片 {}", path.display()))
        })
        .collect()
}

fn clipboard(cwd: &std::path::Path, token: &CancellationToken) -> Result<Output> {
    // SSH 中的系统剪贴板属于远端机器，不能代替用户终端剪贴板。
    ensure!(
        std::env::var_os("SSH_CONNECTION").is_none() && std::env::var_os("SSH_TTY").is_none(),
        "远程会话请使用终端粘贴图片路径或 @图片路径"
    );
    let mut clipboard = arboard::Clipboard::new().context("系统剪贴板不可用；可使用 @图片路径")?;
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
    // Finder 同时提供文件 URL 和图标位图，优先读取真实文件。
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
                "剪贴板图片超过 3200 万像素限制"
            );
            let rgba = image::RgbaImage::from_raw(
                u32::try_from(image.width)?,
                u32::try_from(image.height)?,
                image.bytes.into_owned(),
            )
            .context("剪贴板图片数据无效")?;
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(rgba).write_to(&mut png, image::ImageFormat::Png)?;
            Ok(Output::Images(vec![user_input::image_bytes(
                "剪贴板",
                png.get_ref(),
            )?]))
        }
        Err(arboard::Error::ContentNotAvailable) => {
            let text = clipboard.text().context("剪贴板没有可用图片或文本")?;
            if let Some(paths) = user_input::pasted_paths(&text, cwd) {
                Ok(Output::Images(load_paths(paths, token)?))
            } else {
                Ok(Output::Text(text))
            }
        }
        Err(error) => Err(error).context("读取剪贴板图片失败；可使用 @图片路径"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Clipboard {
        files: Vec<PathBuf>,
        bitmap: bool,
        text: String,
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
            Ok(self.text.clone())
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
            text: String::new(),
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
            text: "unused".into(),
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
}
