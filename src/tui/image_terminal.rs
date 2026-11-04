//! 启动时同步探测图片协议；不用依赖库的超时读取线程，避免它继续争抢 stdin。
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::picker::{
    Picker, ProtocolType,
    cap_parser::{Parser, QueryStdioOptions, Response},
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

pub(super) fn detect() -> (Picker, Vec<Event>) {
    let tmux = std::env::var("TERM")
        .is_ok_and(|term| term.starts_with("tmux") || term.starts_with("screen"));
    let query = Parser::query(tmux, QueryStdioOptions::default());
    let mut parser = Probe::default();
    if io::stdout()
        .write_all(query.as_bytes())
        .and_then(|_| io::stdout().flush())
        .is_ok()
    {
        let deadline = Instant::now() + Duration::from_millis(200);
        while let Some(timeout) = deadline.checked_duration_since(Instant::now()) {
            let mut fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // poll 与 read 使用同一个原始 fd，既没有后台读取者，也没有 stdio 预读缓冲。
            let ready = unsafe {
                libc::poll(
                    &mut fd,
                    1,
                    timeout.as_millis().max(1).min(i32::MAX as u128) as i32,
                )
            };
            if ready <= 0 || fd.revents & libc::POLLIN == 0 {
                break;
            }
            let mut byte = 0u8;
            let count = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
            if count != 1 || parser.push(byte) {
                break;
            }
        }
    }
    let mut font = parser.font;
    if font.is_none() {
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
            && size.ws_row > 0
        {
            let cell = (size.ws_xpixel / size.ws_col, size.ws_ypixel / size.ws_row);
            if cell.0 > 0 && cell.1 > 0 {
                font = Some(cell);
            }
        }
    }
    let mut picker = if let Some(font) = font {
        // 该构造器只配置字号和环境提示，不读取 stdin；旧版本的主动查询会泄漏读取线程。
        #[allow(deprecated)]
        let picker = Picker::from_fontsize(font);
        picker
    } else {
        Picker::halfblocks()
    };
    if font.is_some() {
        let iterm2 = std::env::var("TERM_PROGRAM").is_ok_and(|term| term.contains("iTerm"))
            || std::env::var("LC_TERMINAL").is_ok_and(|term| term.contains("iTerm"))
            || std::env::var("ITERM_SESSION_ID").is_ok_and(|id| !id.is_empty());
        let incompatible_kitty = std::env::var_os("WEZTERM_EXECUTABLE").is_some()
            || std::env::var_os("KONSOLE_VERSION").is_some();
        picker.set_protocol_type(select_protocol(
            picker.protocol_type(),
            parser.protocol,
            iterm2,
            incompatible_kitty,
        ));
    }
    (picker, parser.input_events())
}

fn select_protocol(
    fallback: ProtocolType,
    detected: Option<ProtocolType>,
    iterm2: bool,
    incompatible_kitty: bool,
) -> ProtocolType {
    // Kitty 查询成功不代表支持本组件依赖的 Unicode 占位符，iTerm2 使用原生协议。
    if iterm2 {
        return ProtocolType::Iterm2;
    }
    detected
        .filter(|protocol| !(*protocol == ProtocolType::Kitty && incompatible_kitty))
        .unwrap_or(fallback)
}

#[derive(Default)]
struct Probe {
    sequence: Vec<u8>,
    input: Vec<u8>,
    protocol: Option<ProtocolType>,
    font: Option<(u16, u16)>,
}

impl Probe {
    /// 每次只读取一个字节；收到 DSR 后立刻停止，之后的输入完整留给 crossterm。
    fn push(&mut self, byte: u8) -> bool {
        if self.sequence.is_empty() && byte != 0x1b {
            self.input.push(byte);
            // 保留用户抢先输入的首个 UTF-8 字符，后续字节仍留在终端输入队列。
            return std::str::from_utf8(&self.input).is_ok();
        }
        self.sequence.push(byte);
        let complete = (self.sequence.starts_with(b"\x1b[")
            && self.sequence.len() >= 3
            && (0x40..=0x7e).contains(&byte))
            || self.sequence.ends_with(b"\x1b\\")
            || (self.sequence.len() == 2 && byte != b'[' && byte != b'_');
        if !complete && self.sequence.len() <= 1024 {
            return false;
        }
        let mut parser = Parser::new();
        let mut responses = Vec::new();
        for byte in &self.sequence {
            responses.extend(parser.push(char::from(*byte)));
        }
        if responses.is_empty() {
            // 不支持图片的终端也会回复普通 DA 或 Kitty 错误，这些仍属于探测响应。
            if (self.sequence.starts_with(b"\x1b[?") && byte == b'c')
                || self.sequence.starts_with(b"\x1b_G")
                || (self.sequence.starts_with(b"\x1b[6;") && byte == b't')
            {
                self.sequence.clear();
                return false;
            }
            self.input.append(&mut self.sequence);
            return true;
        }
        self.sequence.clear();
        for response in responses {
            match response {
                Response::Kitty => self.protocol = Some(ProtocolType::Kitty),
                Response::Sixel if self.protocol != Some(ProtocolType::Kitty) => {
                    self.protocol = Some(ProtocolType::Sixel)
                }
                Response::CellSize(Some(font)) if font.0 > 0 && font.1 > 0 => {
                    self.font = Some(font)
                }
                Response::Status => return true,
                _ => {}
            }
        }
        false
    }

    fn input_events(mut self) -> Vec<Event> {
        self.input.append(&mut self.sequence);
        if self.input.is_empty() {
            return Vec::new();
        }
        let key = match self.input.as_slice() {
            b"\r" | b"\n" => Some((KeyCode::Enter, KeyModifiers::NONE)),
            b"\x03" => Some((KeyCode::Char('c'), KeyModifiers::CONTROL)),
            b"\x7f" => Some((KeyCode::Backspace, KeyModifiers::NONE)),
            b"\t" => Some((KeyCode::Tab, KeyModifiers::NONE)),
            b"\x1b" => Some((KeyCode::Esc, KeyModifiers::NONE)),
            b"\x1b[A" => Some((KeyCode::Up, KeyModifiers::NONE)),
            b"\x1b[B" => Some((KeyCode::Down, KeyModifiers::NONE)),
            b"\x1b[C" => Some((KeyCode::Right, KeyModifiers::NONE)),
            b"\x1b[D" => Some((KeyCode::Left, KeyModifiers::NONE)),
            _ => None,
        };
        if let Some((code, modifiers)) = key {
            vec![Event::Key(KeyEvent::new(code, modifiers))]
        } else {
            vec![Event::Paste(
                String::from_utf8_lossy(&self.input).into_owned(),
            )]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_terminal_iterm2_uses_native_protocol_even_after_kitty_reply() {
        for detected in [None, Some(ProtocolType::Kitty), Some(ProtocolType::Sixel)] {
            assert_eq!(
                select_protocol(ProtocolType::Halfblocks, detected, true, false),
                ProtocolType::Iterm2
            );
        }
        assert_eq!(
            select_protocol(
                ProtocolType::Halfblocks,
                Some(ProtocolType::Kitty),
                false,
                false
            ),
            ProtocolType::Kitty
        );
        assert_eq!(
            select_protocol(ProtocolType::Iterm2, Some(ProtocolType::Kitty), false, true),
            ProtocolType::Iterm2
        );
    }

    #[test]
    fn image_terminal_probe_detects_protocol_font_and_retains_early_input() {
        let mut probe = Probe::default();
        for byte in b"\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t" {
            assert!(!probe.push(*byte));
        }
        assert_eq!(probe.protocol, Some(ProtocolType::Kitty));
        assert_eq!(probe.font, Some((10, 20)));
        assert!(probe.push(b'x'));
        assert_eq!(probe.input_events(), vec![Event::Paste("x".into())]);
    }

    #[test]
    fn image_terminal_probe_stops_at_status_and_preserves_unicode_and_escape() {
        let mut probe = Probe::default();
        for byte in b"\x1b[0" {
            assert!(!probe.push(*byte));
        }
        assert!(probe.push(b'n'));
        assert!(probe.input_events().is_empty());
        let mut probe = Probe::default();
        for byte in "图".as_bytes().iter().take(2) {
            assert!(!probe.push(*byte));
        }
        assert!(probe.push("图".as_bytes()[2]));
        assert_eq!(probe.input_events(), vec![Event::Paste("图".into())]);
    }

    #[test]
    fn image_terminal_probe_ignores_negative_capability_replies() {
        let mut probe = Probe::default();
        for byte in b"\x1b[?1;2c\x1b_Gi=31;EINVAL\x1b\\\x1b[6;20;10t" {
            assert!(!probe.push(*byte));
        }
        assert_eq!(probe.font, Some((10, 20)));
        assert!(probe.input_events().is_empty());
    }
}
