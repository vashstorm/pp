//! 图片只在后台解码、缩放与编码；主线程复用完整缓冲，避免消耗 Kitty 的首次传输状态。
use super::attachment_edit::COLOR;
use crate::user_input::Image;
use anyhow::{Context, Result};
use base64::Engine;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Position, Rect},
    style::Style,
    text::Line,
    widgets::{Block, BorderType, Clear, Paragraph, Widget},
};
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType},
};
use std::{
    collections::{HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const DELAY: Duration = Duration::from_millis(200);
const CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Key {
    source: usize,
    width: u16,
    height: u16,
}

struct Request {
    key: Key,
    source: Arc<str>,
    picker: Picker,
}
struct Rendered {
    buffer: Buffer,
    bytes: usize,
    graphics: bool,
}
struct Reply {
    key: Key,
    source: Arc<str>,
    result: Result<Rendered>,
}
struct Entry {
    key: Key,
    _source: Arc<str>,
    rendered: Option<Rendered>,
}

struct Worker {
    requests: Option<SyncSender<Request>>,
    replies: Receiver<Reply>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn new() -> Result<Self> {
        let (requests, incoming) = mpsc::sync_channel::<Request>(8);
        let (outgoing, replies) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new()
            .name("pp-images".into())
            .spawn(move || {
                while let Ok(request) = incoming.recv() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let result =
                        encode(&request.source, &request.picker, request.key).or_else(|_| {
                            let mut picker = request.picker.clone();
                            picker.set_protocol_type(ProtocolType::Halfblocks);
                            encode(&request.source, &picker, request.key)
                        });
                    if outgoing
                        .send(Reply {
                            key: request.key,
                            source: request.source,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .context("无法启动图片预览线程")?;
        Ok(Self {
            requests: Some(requests),
            replies,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.requests.take();
        // 先释放有界回复接收端，避免工作线程在发送完成结果时阻塞退出。
        let (_, empty) = mpsc::channel();
        drop(std::mem::replace(&mut self.replies, empty));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn encode(source: &str, picker: &Picker, key: Key) -> Result<Rendered> {
    let (_, payload) = source.split_once(',').context("无效的图片快照")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .context("无效的图片编码")?;
    let image = image::load_from_memory(&bytes).context("无法读取图片快照")?;
    let area = Rect::new(0, 0, key.width, key.height);
    let protocol = picker.new_protocol(
        image,
        area,
        Resize::Scale(Some(image::imageops::FilterType::Triangle)),
    )?;
    let mut buffer = Buffer::empty(protocol.area());
    ratatui_image::Image::new(&protocol).render(buffer.area, &mut buffer);
    prepare_graphics(&mut buffer);
    let bytes = buffer
        .content
        .iter()
        .map(|cell| cell.symbol().len() + std::mem::size_of_val(cell))
        .sum();
    Ok(Rendered {
        buffer,
        bytes,
        graphics: picker.protocol_type() != ProtocolType::Halfblocks,
    })
}

fn prepare_graphics(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol().contains('\x1b') {
            if cell.symbol().starts_with("\x1bPtmux;\x1b\x1b_G") {
                // ratatui-image 10.0.0 将全部 Kitty 分块包在同一条 tmux 透传中，
                // 大图会超过 tmux 的控制串上限；让每个分块独立透传，保留末尾占位符。
                let symbol = cell.symbol().replace(
                    "\x1b\x1b\\\x1b\x1b_G",
                    "\x1b\x1b\\\x1b\\\x1bPtmux;\x1b\x1b_G",
                );
                cell.set_symbol(&symbol);
            }
            // 协议控制串占据一个锚点单元，不能把转义序列按普通文字计算宽度。
            cell.set_diff_option(ratatui::buffer::CellDiffOption::ForcedWidth(
                std::num::NonZeroU16::MIN,
            ));
        }
    }
}

fn identity(image: &Image) -> usize {
    Arc::as_ptr(&image.data_url) as *const () as usize
}

pub(super) struct View {
    picker: Picker,
    worker: Worker,
    cache: VecDeque<Entry>,
    pending: HashSet<Key>,
    bytes: usize,
    pub graphics: Vec<Rect>,
    hits: Vec<(Rect, Image)>,
    area: Rect,
    attachments: Rect,
    popup: Option<Rect>,
    hover: Option<(usize, Instant)>,
    open: bool,
    first: usize,
    visible: usize,
    count: usize,
    pub suspended: bool,
}

impl View {
    pub fn new() -> Result<Self> {
        Ok(Self {
            picker: Picker::halfblocks(),
            worker: Worker::new()?,
            cache: VecDeque::new(),
            pending: HashSet::new(),
            bytes: 0,
            graphics: Vec::new(),
            hits: Vec::new(),
            area: Rect::default(),
            attachments: Rect::default(),
            popup: None,
            hover: None,
            open: false,
            first: 0,
            visible: 0,
            count: 0,
            suspended: false,
        })
    }

    /// 只在读取首个终端事件之前配置一次，缓存中不会混用不同协议。
    pub fn configure(&mut self, picker: Picker) {
        self.picker = picker;
    }

    pub fn close(&mut self) -> bool {
        let changed = self.hover.is_some() || self.open;
        self.hover = None;
        self.open = false;
        self.popup = None;
        changed
    }

    pub fn poll(&mut self, now: Instant, available: bool) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.worker.replies.try_recv() {
            self.pending.remove(&reply.key);
            let rendered = reply.result.ok();
            let bytes = rendered.as_ref().map_or(0, |image| image.bytes);
            while !self.cache.is_empty()
                && (self.cache.len() >= 64 || self.bytes + bytes > CACHE_BYTES)
            {
                if let Some(entry) = self.cache.pop_front() {
                    self.bytes -= entry.rendered.as_ref().map_or(0, |image| image.bytes);
                }
            }
            self.bytes += bytes;
            self.cache.push_back(Entry {
                key: reply.key,
                _source: reply.source,
                rendered,
            });
            changed = true;
        }
        if !available {
            return self.close() || changed;
        }
        if !self.open
            && self
                .hover
                .is_some_and(|(_, since)| now.saturating_duration_since(since) >= DELAY)
        {
            self.open = true;
            changed = true;
        }
        changed
    }

    pub fn begin_frame(&mut self, area: Rect) {
        if self.area != area {
            self.close();
            self.area = area;
        }
        self.graphics.clear();
        self.hits.clear();
        self.attachments = Rect::default();
        self.popup = None;
    }

    pub fn mouse(&mut self, mouse: MouseEvent, now: Instant) -> (bool, bool) {
        let point = Position::new(mouse.column, mouse.row);
        if self.suspended {
            return (false, self.close());
        }
        if mouse.kind == MouseEventKind::Moved {
            if self.popup.is_some_and(|area| area.contains(point)) {
                return (true, false);
            }
            let source = self
                .hits
                .iter()
                .find(|(area, _)| area.contains(point))
                .map(|(_, image)| identity(image));
            if source == self.hover.map(|(source, _)| source) {
                return (source.is_some(), false);
            }
            self.close();
            self.hover = source.map(|source| (source, now));
            return (source.is_some(), true);
        }
        if self.attachments.contains(point) {
            let direction = match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft => Some(false),
                MouseEventKind::ScrollDown | MouseEventKind::ScrollRight => Some(true),
                MouseEventKind::Down(MouseButton::Left) if mouse.column == self.attachments.x => {
                    Some(false)
                }
                MouseEventKind::Down(MouseButton::Left)
                    if mouse.column.saturating_add(1) == self.attachments.right() =>
                {
                    Some(true)
                }
                _ => None,
            };
            if let Some(forward) = direction {
                self.first = if forward {
                    (self.first + 1).min(self.count.saturating_sub(self.visible))
                } else {
                    self.first.saturating_sub(1)
                };
                self.close();
                return (true, true);
            }
        }
        (false, false)
    }

    fn draw_image(&mut self, frame: &mut Frame, image: &Image, area: Rect) {
        if self.suspended || area.width == 0 || area.height == 0 {
            return;
        }
        let key = Key {
            source: identity(image),
            width: area.width,
            height: area.height,
        };
        if let Some(entry) = self.cache.iter().find(|entry| entry.key == key) {
            if let Some(rendered) = &entry.rendered {
                let source = &rendered.buffer;
                let target = Rect::new(
                    area.x + area.width.saturating_sub(source.area.width) / 2,
                    area.y + area.height.saturating_sub(source.area.height) / 2,
                    source.area.width,
                    source.area.height,
                );
                for y in 0..target.height {
                    for x in 0..target.width {
                        frame.buffer_mut()[(target.x + x, target.y + y)] =
                            source[(source.area.x + x, source.area.y + y)].clone();
                    }
                }
                if rendered.graphics {
                    self.graphics.push(target);
                }
            } else {
                frame.render_widget(
                    Paragraph::new("预览不可用").alignment(Alignment::Center),
                    area,
                );
            }
        } else {
            if !self.pending.contains(&key)
                && self.worker.requests.as_ref().is_some_and(|sender| {
                    sender
                        .try_send(Request {
                            key,
                            source: image.data_url.clone(),
                            picker: self.picker.clone(),
                        })
                        .is_ok()
                })
            {
                self.pending.insert(key);
            }
            frame.render_widget(Paragraph::new("…").alignment(Alignment::Center), area);
        }
    }

    pub fn cards(&mut self, frame: &mut Frame, area: Rect, images: &[Image]) {
        self.attachments = area;
        self.count = images.len();
        if images.is_empty() || area.width == 0 || area.height == 0 {
            self.close();
            return;
        }
        let navigation = images.len() > usize::from((area.width / 25).max(1));
        let margin = u16::from(navigation && area.width >= 6);
        let width = area.width.saturating_sub(margin * 2);
        self.visible = usize::from((width.saturating_add(1) / 25).max(1)).min(images.len());
        self.first = self.first.min(images.len().saturating_sub(self.visible));
        let card_width = width.min(24);
        let shown: Vec<_> = images
            .iter()
            .skip(self.first)
            .take(self.visible)
            .cloned()
            .collect();
        for (index, image) in shown.iter().enumerate() {
            let card = Rect::new(
                area.x + margin + index as u16 * 25,
                area.y,
                card_width,
                area.height,
            );
            self.hits.push((card, image.clone()));
            card_frame(frame, card, image);
            if card.width > 2 && card.height > 2 {
                self.draw_image(
                    frame,
                    image,
                    Rect::new(card.x + 1, card.y + 1, card.width - 2, card.height - 2),
                );
            }
        }
        if navigation && margin > 0 {
            let y = area.y + area.height / 2;
            frame.render_widget(
                Line::styled(
                    if self.first > 0 { "‹" } else { " " },
                    Style::default().fg(COLOR),
                ),
                Rect::new(area.x, y, 1, 1),
            );
            frame.render_widget(
                Line::styled(
                    if self.first + self.visible < images.len() {
                        "›"
                    } else {
                        " "
                    },
                    Style::default().fg(COLOR),
                ),
                Rect::new(area.right() - 1, y, 1, 1),
            );
        }
        if self.hover.is_some_and(|(source, _)| {
            !self.hits.iter().any(|(_, image)| identity(image) == source)
        }) {
            self.close();
        }
    }

    pub fn preview(&mut self, frame: &mut Frame, available: Rect) {
        if !self.open || self.suspended || available.width < 8 || available.height < 4 {
            return;
        }
        let Some((source, _)) = self.hover else {
            return;
        };
        let Some((card, image)) = self
            .hits
            .iter()
            .find(|(_, image)| identity(image) == source)
            .cloned()
        else {
            return;
        };
        let width = available
            .width
            .saturating_sub(2)
            .clamp(8, 96)
            .min(available.width);
        let height = available.height.min(32);
        let x = card
            .x
            .min(available.right().saturating_sub(width))
            .max(available.x);
        let area = Rect::new(x, available.bottom() - height, width, height);
        frame.render_widget(Clear, area);
        card_frame(frame, area, &image);
        self.draw_image(
            frame,
            &image,
            Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 2),
        );
        // 将浮层与卡片间的空隙也作为命中区域，鼠标可连续移入大图。
        self.popup = Some(Rect::new(
            area.x,
            area.y,
            area.width,
            card.y.saturating_sub(area.y),
        ));
    }
}

fn card_frame(frame: &mut Frame, area: Rect, image: &Image) {
    let dimensions = image.dimensions();
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(COLOR))
        .title_top(Line::from(format!(" {} ", image.marker)).centered())
        .title_bottom(Line::from(format!(" {dimensions} ")).centered());
    frame.render_widget(block, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};

    fn fixture() -> Image {
        let image = image::DynamicImage::ImageRgb8(image::ImageBuffer::from_fn(40, 20, |x, y| {
            image::Rgb([(x * 6) as u8, (y * 12) as u8, 120])
        }));
        let mut bytes = std::io::Cursor::new(Vec::new());
        image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
        let mut image = crate::user_input::image_bytes("fixture", bytes.get_ref()).unwrap();
        image.marker = "\u{f03e} #1".into();
        image
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn image_preview_hover_delay_popup_scroll_resize_and_modal_hide() {
        let mut view = View::new().unwrap();
        let images: Vec<_> = (0..6)
            .map(|index| {
                let mut image = fixture();
                image.marker = format!("\u{f03e} #{}", index + 1);
                image
            })
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let draw = |view: &mut View, terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| {
                    view.begin_frame(frame.area());
                    view.cards(frame, Rect::new(0, 20, 100, 8), &images);
                    view.preview(frame, Rect::new(0, 0, 100, 20));
                })
                .unwrap();
        };
        draw(&mut view, &mut terminal);
        assert_eq!(view.hits.len(), 3);
        let now = Instant::now();
        assert!(view.mouse(mouse(MouseEventKind::Moved, 3, 22), now).0);
        view.poll(now + Duration::from_millis(199), true);
        assert!(!view.open);
        view.poll(now + DELAY, true);
        draw(&mut view, &mut terminal);
        assert!(view.open);
        let popup = view.popup.unwrap();
        assert!(popup.bottom() <= 20);
        assert!(
            view.mouse(mouse(MouseEventKind::Moved, popup.x + 2, popup.y + 2), now)
                .0
        );
        assert!(view.open);
        assert!(view.mouse(mouse(MouseEventKind::ScrollDown, 3, 22), now).0);
        assert_eq!(view.first, 1);
        assert!(!view.open);
        draw(&mut view, &mut terminal);
        view.mouse(mouse(MouseEventKind::Moved, 3, 22), now);
        view.poll(now + DELAY, true);
        view.begin_frame(Rect::new(0, 0, 20, 10));
        assert!(!view.open);
        draw(&mut view, &mut terminal);
        view.mouse(mouse(MouseEventKind::Moved, 3, 22), now);
        view.poll(now + DELAY, false);
        assert!(!view.open);
    }

    #[test]
    fn image_preview_encodes_all_protocols_and_preserves_reusable_transmission() {
        let image = fixture();
        for protocol in [
            ProtocolType::Halfblocks,
            ProtocolType::Kitty,
            ProtocolType::Iterm2,
            ProtocolType::Sixel,
        ] {
            let mut picker = Picker::halfblocks();
            picker.set_protocol_type(protocol);
            let rendered = encode(
                &image.data_url,
                &picker,
                Key {
                    source: identity(&image),
                    width: 20,
                    height: 6,
                },
            )
            .unwrap();
            assert!(rendered.buffer.area.width <= 20 && rendered.buffer.area.height <= 6);
            let text: String = rendered
                .buffer
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            match protocol {
                ProtocolType::Halfblocks => assert!(text.contains('▀') || text.contains('▄')),
                ProtocolType::Kitty => assert!(text.contains("\x1b_G") && text.contains("i=")),
                ProtocolType::Iterm2 => assert!(text.contains("1337;File=")),
                ProtocolType::Sixel => assert!(text.contains("\x1bP")),
            }
            assert_eq!(rendered.graphics, protocol != ProtocolType::Halfblocks);
            assert_eq!(rendered.buffer, rendered.buffer.clone());
        }
    }

    #[test]
    fn image_preview_large_kitty_tmux_transmission_uses_bounded_passthroughs() {
        use ratatui_image::protocol::{Protocol, kitty::Kitty};

        let area = Rect::new(0, 0, 90, 20);
        let protocol = Protocol::Kitty(
            Kitty::new(image::DynamicImage::new_rgb8(884, 368), area, 123, true).unwrap(),
        );
        let mut buffer = Buffer::empty(area);
        ratatui_image::Image::new(&protocol).render(area, &mut buffer);
        let original = buffer[(0, 0)].symbol().to_owned();
        prepare_graphics(&mut buffer);
        let symbol = buffer[(0, 0)].symbol();
        let mut remaining = symbol;
        let mut unwrapped = String::new();
        let mut chunks = 0;
        while let Some(body) = remaining.strip_prefix("\x1bPtmux;") {
            let (chunk, rest) = body.split_once("\x1b\x1b\\\x1b\\").unwrap();
            assert!(chunk.len() < 8192, "tmux 透传块过大：{} 字节", chunk.len());
            assert!(chunk.starts_with("\x1b\x1b_G"));
            unwrapped.push_str(chunk);
            unwrapped.push_str("\x1b\x1b\\");
            chunks += 1;
            remaining = rest;
        }
        assert!(chunks > 1);
        unwrapped.push_str(remaining);
        let original = original.strip_prefix("\x1bPtmux;").unwrap();
        let (transmission, placeholders) = original.split_once("\x1b\x1b\\\x1b\\").unwrap();
        assert_eq!(unwrapped, format!("{transmission}\x1b\x1b\\{placeholders}"));
        assert!(remaining.contains('\u{10EEEE}'));
        assert!(symbol.contains("i=123,a=T,U=1"));
        assert!(symbol.contains("m=0;"));
    }

    #[test]
    fn image_preview_worker_cache_and_late_reply_never_restore_removed_attachment() {
        let mut view = View::new().unwrap();
        let image = fixture();
        let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| {
                view.begin_frame(frame.area());
                view.cards(frame, Rect::new(0, 10, 40, 8), std::slice::from_ref(&image));
            })
            .unwrap();
        assert_eq!(view.pending.len(), 1);
        terminal
            .draw(|frame| {
                view.begin_frame(frame.area());
                view.cards(frame, Rect::new(0, 10, 40, 8), &[]);
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !view.pending.is_empty() && Instant::now() < deadline {
            view.poll(Instant::now(), true);
            thread::yield_now();
        }
        assert!(view.pending.is_empty());
        assert!(view.hits.is_empty());
        assert!(!view.open);
        assert!(!view.cache.is_empty());
        assert!(view.cache[0].rendered.is_some());
    }
}
