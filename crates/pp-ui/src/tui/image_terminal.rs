//! 启动时同步探测图片协议；不用依赖库的超时读取线程，避免它继续争抢 stdin。
//!
//! 探测只读取命令回复：任何 `ESC` 起始的字节都归入回复序列并整段丢弃，只有探测
//! 窗口内抢先输入的非 `ESC` 字符会转成事件。否则回复的残余字节会被 crossterm
//! 当成按键或粘贴写进输入框。
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::picker::{
    Picker, ProtocolType,
    cap_parser::{Parser, QueryStdioOptions, Response},
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

/// 终端不回复时的探测上限。
const PROBE_WINDOW: Duration = Duration::from_millis(200);
/// 收到回复后的安静窗口：tmux 与真实终端的回复可能分批到达，读到安静为止。
const PROBE_QUIET: Duration = Duration::from_millis(40);
/// 单条回复序列的缓冲上限；超长控制字符串只保留前缀，继续丢弃至结束。
const MAX_SEQUENCE: usize = 1024;

pub(super) fn detect() -> (Picker, Vec<Event>, Vec<u8>) {
    let tmux = std::env::var("TERM")
        .is_ok_and(|term| term.starts_with("tmux") || term.starts_with("screen"));
    let query = Parser::query(tmux, QueryStdioOptions::default());
    let mut parser = Probe::default();
    if io::stdout()
        .write_all(query.as_bytes())
        .and_then(|_| io::stdout().flush())
        .is_ok()
    {
        let deadline = Instant::now() + PROBE_WINDOW;
        let mut quiet_until = deadline;
        // 读到安静窗口为止：tmux 与真实终端会把回复分批送达，中途不能停。
        while let Some(timeout) = quiet_until
            .min(deadline)
            .checked_duration_since(Instant::now())
        {
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
            // 用户开始输入后剩下的字节留给 crossterm，继续读会吞掉按键。
            if count != 1 || parser.push(byte) {
                break;
            }
            quiet_until = Instant::now() + PROBE_QUIET;
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
    // 窗口结束时可能只读到了回复的前半段，由输入线程接着消费，不能丢失前缀。
    let pending_reply = std::mem::take(&mut parser.sequence);
    (picker, parser.input_events(), pending_reply)
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
    /// 每次只读取一个字节；返回 true 表示用户已开始输入，剩余字节留给 crossterm。
    ///
    /// 终端回复一律丢弃：把它们当输入会以按键或粘贴的形式污染输入框。
    fn push(&mut self, byte: u8) -> bool {
        if self.sequence.is_empty() && byte != 0x1b {
            self.input.push(byte);
            return match std::str::from_utf8(&self.input) {
                Ok(_) => true,
                // 多字节字符还没收全，继续读。
                Err(error) if error.error_len().is_none() => false,
                // 非法 UTF-8 只可能是终端噪声。
                Err(_) => {
                    self.input.clear();
                    false
                }
            };
        }
        self.sequence.push(byte);
        super::input_shim::restore_tmux_escape_tail(&mut self.sequence);
        if !reply_complete(&self.sequence) {
            if self.sequence.len() > MAX_SEQUENCE {
                // 超长控制字符串只保留前缀与可能的 ST 起始字节，继续丢弃至结束。
                let escape = byte == 0x1b;
                self.sequence.truncate(2);
                if escape {
                    self.sequence.push(byte);
                }
            }
            return false;
        }
        let mut parser = Parser::new();
        let mut responses = Vec::new();
        for byte in std::mem::take(&mut self.sequence) {
            responses.extend(parser.push(char::from(byte)));
        }
        for response in responses {
            match response {
                Response::Kitty => self.protocol = Some(ProtocolType::Kitty),
                Response::Sixel if self.protocol != Some(ProtocolType::Kitty) => {
                    self.protocol = Some(ProtocolType::Sixel)
                }
                Response::CellSize(Some(font)) if font.0 > 0 && font.1 > 0 => {
                    self.font = Some(font)
                }
                _ => {}
            }
        }
        false
    }

    fn input_events(self) -> Vec<Event> {
        if self.input.is_empty() {
            return Vec::new();
        }
        let key = match self.input.as_slice() {
            b"\r" | b"\n" => Some((KeyCode::Enter, KeyModifiers::NONE)),
            b"\x03" => Some((KeyCode::Char('c'), KeyModifiers::CONTROL)),
            b"\x7f" => Some((KeyCode::Backspace, KeyModifiers::NONE)),
            b"\t" => Some((KeyCode::Tab, KeyModifiers::NONE)),
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

/// `ESC` 起始的终端回复是否已经收全：CSI 以 0x40..=0x7e 收尾，APC／DCS 以 ST 收尾。
fn reply_complete(sequence: &[u8]) -> bool {
    let Some(&last) = sequence.last() else {
        return false;
    };
    if sequence.starts_with(b"\x1b[") {
        return sequence.len() >= 3 && (0x40..=0x7e).contains(&last);
    }
    if sequence.ends_with(b"\x1b\\") {
        return true;
    }
    if sequence.starts_with(b"\x1b]") && last == 0x07 {
        return true;
    }
    // 单字节 `ESC` 与 APC／DCS／OSC 前缀都还要等后续字节。
    sequence.len() == 2 && last != b'_' && last != b'P' && last != b']'
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
    fn image_terminal_probe_decodes_ghostty_replies_reencoded_by_tmux() {
        for replies in [
            &b"\x1b[95;3uGi=31;OK\x1b[92;3u\x1b[91;3u?62;22;52c\x1b[91;3u6;40;20t\x1b[91;3u0n"[..],
            b"\x1b[27;3;95~Gi=31;OK\x1b[27;3;92~\x1b[27;3;91~?62;22;52c\x1b[27;3;91~6;40;20t\x1b[27;3;91~0n",
        ] {
            let mut probe = Probe::default();
            for byte in replies {
                assert!(!probe.push(*byte));
            }
            assert_eq!(probe.protocol, Some(ProtocolType::Kitty));
            assert_eq!(probe.font, Some((20, 40)));
            assert!(probe.input_events().is_empty());
        }
    }

    #[test]
    fn image_terminal_probe_discards_every_reply_and_keeps_reading() {
        // DSR 之后的能力回复仍会到达：既不能停止读取，也不能变成输入。
        let mut probe = Probe::default();
        for byte in b"\x1b[0n\x1b[?62;22;52c\x1b[?95;3u\x1b[6;40;20t\x1bP>|ghostty 1.4.0\x1b\\" {
            assert!(!probe.push(*byte));
        }
        assert_eq!(probe.font, Some((20, 40)));
        assert!(probe.input_events().is_empty());
        // 不完整的回复前缀在窗口结束时同样不能变成输入。
        let mut probe = Probe::default();
        for byte in b"\x1b_Gi=31;OK" {
            assert!(!probe.push(*byte));
        }
        assert!(probe.input_events().is_empty());
    }

    #[test]
    fn image_terminal_probe_discards_osc_and_oversized_control_strings() {
        let mut probe = Probe::default();
        for byte in b"\x1b]11;rgb:0000/0000/0000\x07\x1b_G" {
            assert!(!probe.push(*byte));
        }
        for _ in 0..MAX_SEQUENCE * 3 {
            assert!(!probe.push(b'x'));
            assert!(probe.sequence.len() <= MAX_SEQUENCE);
        }
        for byte in b"\x1b\\\x1b[0n" {
            assert!(!probe.push(*byte));
        }
        assert!(probe.input_events().is_empty());
    }

    #[test]
    fn image_terminal_probe_preserves_typed_unicode_and_control_keys() {
        let mut probe = Probe::default();
        for byte in "图".as_bytes().iter().take(2) {
            assert!(!probe.push(*byte));
        }
        assert!(probe.push("图".as_bytes()[2]));
        assert_eq!(probe.input_events(), vec![Event::Paste("图".into())]);
        let mut probe = Probe::default();
        assert!(probe.push(b'\r'));
        assert_eq!(
            probe.input_events(),
            vec![Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))]
        );
        // 非法 UTF-8 只当噪声：清空后继续等用户输入。
        let mut probe = Probe::default();
        assert!(!probe.push(0x80));
        assert!(probe.push(b'a'));
        assert_eq!(probe.input_events(), vec![Event::Paste("a".into())]);
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
