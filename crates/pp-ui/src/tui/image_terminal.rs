//! Probe the image protocol synchronously at startup; no dependency timeout reader thread that would keep competing for stdin.
//!
//! The probe only reads command replies: every byte starting with `ESC` joins a reply sequence and is discarded whole,
//! and only non-`ESC` characters typed early inside the probe window become events. Otherwise leftover reply bytes
//! would be written into the input box by crossterm as key presses or paste.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui_image::picker::{
    Picker, ProtocolType,
    cap_parser::{Parser, QueryStdioOptions, Response},
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

/// Probe upper bound when the terminal does not reply.
const PROBE_WINDOW: Duration = Duration::from_millis(200);
/// Quiet window after a reply: tmux and real terminals may deliver replies in batches, so read until it goes quiet.
const PROBE_QUIET: Duration = Duration::from_millis(40);
/// Buffer cap for one reply sequence; an over-long control string keeps only its prefix and keeps discarding to the end.
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
        // Read until the quiet window: tmux and real terminals deliver replies in batches, so stopping early is not allowed.
        while let Some(timeout) = quiet_until
            .min(deadline)
            .checked_duration_since(Instant::now())
        {
            let mut fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            // poll and read use the same raw fd, so there is neither a background reader nor a stdio read-ahead buffer.
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
            // Once the user starts typing, the remaining bytes belong to crossterm; reading further would swallow key presses.
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
        // This constructor only configures font size and environment hints without reading stdin; the old active query leaked a reader thread.
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
    // When the window ends only the first half of a reply may have been read; the input thread consumes the rest, so the prefix must not be lost.
    let pending_reply = std::mem::take(&mut parser.sequence);
    (picker, parser.input_events(), pending_reply)
}

fn select_protocol(
    fallback: ProtocolType,
    detected: Option<ProtocolType>,
    iterm2: bool,
    incompatible_kitty: bool,
) -> ProtocolType {
    // A successful Kitty query does not mean the Unicode placeholders this component relies on are supported; iTerm2 uses its native protocol.
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
    /// Read one byte at a time; returning true means the user has started typing and the rest belongs to crossterm.
    ///
    /// Terminal replies are always dropped: treating them as input would pollute the input box as key presses or paste.
    fn push(&mut self, byte: u8) -> bool {
        if self.sequence.is_empty() && byte != 0x1b {
            self.input.push(byte);
            return match std::str::from_utf8(&self.input) {
                Ok(_) => true,
                // The multi-byte character is not complete yet; keep reading.
                Err(error) if error.error_len().is_none() => false,
                // Invalid UTF-8 can only be terminal noise.
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
                // An over-long control string keeps only its prefix and a possible ST start byte, discarding the rest to the end.
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

/// Whether the `ESC`-started terminal reply is complete: CSI ends at 0x40..=0x7e, APC/DCS at ST.
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
    // A lone `ESC` and an APC/DCS/OSC prefix still wait for further bytes.
    sequence.len() == 2 && last != b'_' && last != b'P' && last != b']'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_protocol_and_negative_replies() {
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

        let mut probe = Probe::default();
        for byte in b"\x1b[?1;2c\x1b_Gi=31;EINVAL\x1b\\\x1b[6;20;10t" {
            assert!(!probe.push(*byte));
        }
        assert_eq!(probe.font, Some((10, 20)));
        assert!(probe.input_events().is_empty());
    }

    #[test]
    fn probe_kitty_ghostty_tmux_and_oversized() {
        let mut probe = Probe::default();
        for byte in b"\x1b_Gi=31;OK\x1b\\\x1b[6;20;10t" {
            assert!(!probe.push(*byte));
        }
        assert_eq!(
            (probe.protocol, probe.font),
            (Some(ProtocolType::Kitty), Some((10, 20)))
        );
        assert!(probe.push(b'x'));
        assert_eq!(probe.input_events(), vec![Event::Paste("x".into())]);

        for replies in [
            &b"\x1b[95;3uGi=31;OK\x1b[92;3u\x1b[91;3u?62;22;52c\x1b[91;3u6;40;20t\x1b[91;3u0n"[..],
            b"\x1b[27;3;95~Gi=31;OK\x1b[27;3;92~\x1b[27;3;91~?62;22;52c\x1b[27;3;91~6;40;20t\x1b[27;3;91~0n",
        ] {
            let mut ghost = Probe::default();
            for byte in replies {
                assert!(!ghost.push(*byte));
            }
            assert_eq!((ghost.protocol, ghost.font), (Some(ProtocolType::Kitty), Some((20, 40))));
            assert!(ghost.input_events().is_empty());
        }

        let mut dsr = Probe::default();
        for byte in b"\x1b[0n\x1b[?62;22;52c\x1b[?95;3u\x1b[6;40;20t\x1bP>|ghostty 1.4.0\x1b\\" {
            assert!(!dsr.push(*byte));
        }
        assert_eq!(dsr.font, Some((20, 40)));
        assert!(dsr.input_events().is_empty());

        let mut partial = Probe::default();
        for byte in b"\x1b_Gi=31;OK" {
            assert!(!partial.push(*byte));
        }
        assert!(partial.input_events().is_empty());

        let mut osc = Probe::default();
        for byte in b"\x1b]11;rgb:0000/0000/0000\x07\x1b_G" {
            assert!(!osc.push(*byte));
        }
        for _ in 0..MAX_SEQUENCE * 3 {
            assert!(!osc.push(b'x'));
            assert!(osc.sequence.len() <= MAX_SEQUENCE);
        }
        for byte in b"\x1b\\\x1b[0n" {
            assert!(!osc.push(*byte));
        }
        assert!(osc.input_events().is_empty());
    }
}
