//! Translate tmux's xterm modifyOtherKeys encoding into Kitty CSI-u and drop terminal replies.
//!
//! Under `extended-keys on`, tmux reports modified keys following `extended-keys-format`
//! (default `xterm`), e.g. Shift + Enter is `CSI 27;2;13~`; crossterm only parses
//! `CSI 13;2u` and silently discards the rest. Here fd 0 is replaced by a pty slave and a
//! background thread reads the raw terminal and rewrites equivalently; terminals without
//! that encoding are unaffected.
//!
//! Terminal capability replies (APC/DCS/OSC strings, DA1, Kitty keyboard flags, size and status
//! reports) use the same channel, but crossterm emits the parts it cannot parse as key presses;
//! replies arriving outside the probe window also surface after startup. They are dropped whole, except bracketed-paste content.
//! tmux re-encodes the prefix of an unknown reply (`ESC _`/`ESC [`) as an Alt key press; these
//! separators are restored before classification, covering both the `csi-u` and `xterm` extended key formats.
//!
//! A pty is required rather than a pipe: crossterm reads `/dev/tty` when stdin is not a TTY, and a
//! pipe would let it bypass the translation layer.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

/// Buffer cap for a single CSI sequence; over-long input is passed through verbatim so pathological input cannot hold memory without bound.
const MAX_SEQUENCE: usize = 64;
/// Length cap for APC/DCS/OSC: terminal identification, Kitty graphics replies and clipboard replies are all far below it.
const MAX_STRING_SEQUENCE: usize = 4096;
/// Input-thread poll interval, used both to notice complete sequences and to bound shutdown latency.
const POLL_INTERVAL_MS: i32 = 50;
/// Read block size between rescans.
const CHUNK: usize = 8 * 1024;
/// Bracketed-paste markers: the enclosed payload must pass through byte for byte.
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

static INSTALLED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// Real terminal fd: `restore` swaps it back into fd 0 so disabling raw mode applies to the original terminal.
static SAVED: Mutex<Option<File>> = Mutex::new(None);

/// Replace fd 0 with a pty and forward the raw terminal bytes into it through [`Translator`].
///
/// Must be called after `enable_raw_mode` and the one-shot capability probe: the probe reads the raw terminal directly.
pub(super) fn install(pending_reply: Vec<u8>) -> io::Result<()> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // Non-TTY input (redirection, test fixtures) is left as-is for crossterm to handle.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Ok(());
    }
    let mut master = 0;
    let mut slave = 0;
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // The master side is held by the input thread; the slave becomes fd 0 and must be raw, otherwise line buffering eats key presses.
    let master = unsafe { File::from_raw_fd(master) };
    let real = unsafe { libc::dup(libc::STDIN_FILENO) };
    if real < 0 {
        return Err(io::Error::last_os_error());
    }
    let real = unsafe { File::from_raw_fd(real) };
    let forwarded = real.try_clone()?;
    // Register the rollback point first: any later failure can hand the real terminal back to fd 0.
    *SAVED.lock().unwrap_or_else(|error| error.into_inner()) = Some(real);
    if unsafe { libc::dup2(slave, libc::STDIN_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::close(slave) };
    make_raw(libc::STDIN_FILENO)?;
    thread::Builder::new()
        .name("pp-input".into())
        .spawn(move || pump(forwarded, master, pending_reply))?;
    Ok(())
}

/// Disable echo, line buffering and signal-character handling so the writer's bytes reach the reader verbatim.
fn make_raw(fd: libc::c_int) -> io::Result<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::cfmakeraw(&mut termios) };
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Hand the real terminal back to fd 0; the subsequent `disable_raw_mode` therefore applies to the original terminal.
pub(super) fn restore() {
    STOP.store(true, Ordering::SeqCst);
    let saved = SAVED
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some(saved) = saved {
        unsafe { libc::dup2(saved.as_raw_fd(), libc::STDIN_FILENO) };
    }
}

fn pump(mut input: File, mut output: File, pending_reply: Vec<u8>) {
    let mut translator = Translator {
        pending: pending_reply,
        ..Translator::default()
    };
    let mut buffer = [0; CHUNK];
    while !STOP.load(Ordering::SeqCst) {
        let mut fd = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut fd, 1, POLL_INTERVAL_MS) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            // Only a lone Esc is released here; an incomplete reply stays buffered for the next read so a fragment gap cannot leak body text.
            if !write_all(&mut output, &translator.take_pending()) {
                break;
            }
            continue;
        }
        match input.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                if !write_all(&mut output, &translator.push(&buffer[..count])) {
                    break;
                }
                // A lone Esc is released immediately so it cannot pair with the user's next character into Alt; fragments that
                // already carry a CSI/APC prefix keep buffering and are unaffected by this fast path.
                if translator.pending == b"\x1b"
                    && !ready_now(&input)
                    && !write_all(&mut output, &translator.take_pending())
                {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
}

fn ready_now(input: &File) -> bool {
    let mut fd = libc::pollfd {
        fd: input.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut fd, 1, 0) > 0 }
}

fn write_all(output: &mut File, bytes: &[u8]) -> bool {
    bytes.is_empty() || output.write_all(bytes).is_ok()
}

/// Incremental translator: rewrites `CSI 27;mod;code~`, drops terminal replies and passes the rest through verbatim.
#[derive(Default)]
pub(super) struct Translator {
    pending: Vec<u8>,
    /// Inside a bracketed paste: the content is immune to sequence classification and rewriting.
    paste: bool,
}

impl Translator {
    /// Append raw bytes and return what can be written immediately; a trailing incomplete sequence waits for the next call.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.paste {
                // Paste content gets no tmux decoding; only the terminator is matched incrementally.
                self.pending.push(byte);
                while !PASTE_END.starts_with(&self.pending) {
                    out.push(self.pending.remove(0));
                }
                if self.pending == PASTE_END {
                    out.append(&mut self.pending);
                    self.paste = false;
                }
                continue;
            }
            if self.pending.is_empty() && byte != 0x1b {
                out.push(byte);
                continue;
            }
            self.pending.push(byte);
            restore_tmux_escape_tail(&mut self.pending);
            while !self.pending.is_empty() {
                match sequence(&self.pending) {
                    Sequence::Complete(end) => {
                        let complete: Vec<_> = self.pending.drain(..end).collect();
                        self.forward(&complete, &mut out);
                    }
                    Sequence::Incomplete => {
                        let string = matches!(self.pending.get(1), Some(b'_' | b'P' | b']'));
                        let limit = if string {
                            MAX_STRING_SEQUENCE
                        } else {
                            MAX_SEQUENCE
                        };
                        if self.pending.len() > limit {
                            if string {
                                // Drop over-long strings within a bound, keeping a possibly fragmented terminator.
                                let tail = self.pending.iter().rposition(|b| *b == 0x1b);
                                if let Some(tail) = tail.filter(|offset| {
                                    *offset >= 2 && self.pending.len() - offset <= MAX_SEQUENCE
                                }) {
                                    self.pending.drain(2..tail);
                                } else {
                                    self.pending.truncate(2);
                                }
                            } else {
                                out.append(&mut self.pending);
                            }
                        }
                        break;
                    }
                }
            }
        }
        out
    }

    /// Handle the paste start marker; other complete sequences are filtered for replies, then rewritten or passed through.
    fn forward(&mut self, sequence: &[u8], out: &mut Vec<u8>) {
        if sequence == PASTE_START {
            self.paste = true;
            out.extend_from_slice(sequence);
            return;
        }
        if terminal_reply(sequence) {
            return;
        }
        match rewrite(sequence) {
            Some(rewritten) => out.extend_from_slice(&rewritten),
            None => out.extend_from_slice(sequence),
        }
    }

    /// Release only a lone Esc; every other prefix keeps buffering until the reply completes or the paste ends.
    pub(super) fn take_pending(&mut self) -> Vec<u8> {
        if !self.paste && self.pending == b"\x1b" {
            std::mem::take(&mut self.pending)
        } else {
            Vec::new()
        }
    }

    /// Whether a sequence prefix is still waiting for further bytes.
    #[cfg(test)]
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// tmux treats the prefix of an unrecognized terminal reply as an Alt key press and re-encodes it per the extended key protocol.
/// Only control-sequence separators are restored; ordinary Alt shortcuts are still left to crossterm.
pub(super) fn restore_tmux_escape_tail(buffer: &mut Vec<u8>) {
    if !matches!(buffer.last(), Some(b'u' | b'~')) {
        return;
    }
    let Some(start) = buffer.iter().rposition(|byte| *byte == 0x1b) else {
        return;
    };
    let Some(code) = tmux_alt_separator(&buffer[start..]) else {
        return;
    };
    buffer.truncate(start);
    buffer.extend_from_slice(&[0x1b, code]);
}

fn tmux_alt_separator(bytes: &[u8]) -> Option<u8> {
    let body = bytes.strip_prefix(b"\x1b[")?;
    let code = if let Some(body) = body.strip_suffix(b"u") {
        std::str::from_utf8(body)
            .ok()?
            .strip_suffix(";3")?
            .parse::<u8>()
            .ok()?
    } else {
        std::str::from_utf8(body.strip_suffix(b"~")?)
            .ok()?
            .strip_prefix("27;3;")?
            .parse::<u8>()
            .ok()?
    };
    matches!(code, b'[' | b'_' | b'P' | b']' | b'\\').then_some(code)
}

enum Sequence {
    /// Byte length of the complete sequence.
    Complete(usize),
    /// The tail was truncated and more bytes are needed to decide.
    Incomplete,
}

/// Return the end of the sequence starting at `ESC`: CSI ends at 0x40..=0x7E, APC/DCS at ST,
/// and OSC also accepts BEL.
fn sequence(bytes: &[u8]) -> Sequence {
    if bytes.len() == 1 {
        return Sequence::Incomplete;
    }
    match bytes[1] {
        b'[' => csi(bytes),
        b'_' | b'P' | b']' => control_string(bytes),
        // Release only one byte of `ESC ESC` and leave the rest to the next scan; Alt combinations pass through whole.
        _ => Sequence::Complete(if bytes[1] == 0x1b { 1 } else { 2 }),
    }
}

fn csi(bytes: &[u8]) -> Sequence {
    for (offset, byte) in bytes.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(byte) {
            return Sequence::Complete(offset + 1);
        }
        if !(0x20..=0x3f).contains(byte) {
            // Invalid CSI: release only the ESC and rescan the following bytes.
            return Sequence::Complete(1);
        }
    }
    Sequence::Incomplete
}

fn control_string(bytes: &[u8]) -> Sequence {
    // Only OSC is terminated by BEL; APC and DCS accept ST alone.
    let osc = bytes[1] == b']';
    let mut offset = 2;
    while offset < bytes.len() {
        match bytes[offset] {
            0x07 if osc => return Sequence::Complete(offset + 1),
            0x1b if bytes.get(offset + 1) == Some(&b'\\') => {
                return Sequence::Complete(offset + 2);
            }
            _ => offset += 1,
        }
    }
    Sequence::Incomplete
}

/// Terminal capability replies can only originate from the terminal; clients never need them.
fn terminal_reply(sequence: &[u8]) -> bool {
    match sequence.get(1) {
        // APC/DCS/OSC strings: Kitty graphics replies, XTVERSION, clipboard and palette replies.
        Some(b'_' | b'P' | b']') => true,
        Some(b'[') => {
            let private = sequence.get(2) == Some(&b'?');
            match sequence.last() {
                // DA1 and Kitty keyboard protocol flags.
                Some(b'c' | b'u') => private,
                // Window/cell size and device status reports.
                Some(b't' | b'n') => !private,
                _ => false,
            }
        }
        _ => false,
    }
}

/// `CSI 27;mod;code~` → `CSI code;mod u`; other sequences return `None` and pass through.
fn rewrite(bytes: &[u8]) -> Option<Vec<u8>> {
    let body = bytes.strip_prefix(b"\x1b[")?.strip_suffix(b"~")?;
    let mut parts = std::str::from_utf8(body).ok()?.split(';');
    if parts.next()? != "27" {
        return None;
    }
    let modifier = parts.next()?.parse::<u16>().ok()?;
    let code = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(2..=16).contains(&modifier) {
        return None;
    }
    // xterm's Meta bit is 8, Kitty's Meta bit is 32 (bit 8 means Super).
    let modifier = if modifier >= 9 {
        modifier + 24
    } else {
        modifier
    };
    Some(format!("\x1b[{code};{modifier}u").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn translated(chunks: &[&[u8]]) -> Vec<u8> {
        let mut translator = Translator::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(translator.push(chunk));
        }
        out.extend(translator.take_pending());
        out
    }

    #[test]
    fn translator_rewrites_modify_other_keys_and_preserves_malformed() {
        assert_eq!(translated(&[b"\x1b[27;2;13~"]), b"\x1b[13;2u");
        assert_eq!(translated(&[b"\x1b[27;3;13~"]), b"\x1b[13;3u");
        assert_eq!(translated(&[b"\x1b[27;5;99~"]), b"\x1b[99;5u");
        assert_eq!(translated(&[b"\x1b[27;2;9~"]), b"\x1b[9;2u");
        assert_eq!(translated(&[b"\x1b[27;10;65~"]), b"\x1b[65;34u");
        assert_eq!(translated(&[b"\x1b[27;2;233~"]), b"\x1b[233;2u");
        assert_eq!(translated(&[b"ab\xc3\xa9c"]), b"ab\xc3\xa9c");
        assert_eq!(translated(&[b"\x1b[A\x1b\r"]), b"\x1b[A\x1b\r");
        assert_eq!(
            translated(&[b"\x1b[200~paste\x1b[201~"]),
            b"\x1b[200~paste\x1b[201~"
        );
        assert_eq!(translated(&[b"\x1b[<0;4;5M"]), b"\x1b[<0;4;5M");
        assert_eq!(translated(&[b"\x1b[27;", b"2;", b"13~"]), b"\x1b[13;2u");
        assert_eq!(translated(&[b"\x1b"]), b"\x1b");
        assert_eq!(translated(&[b"\x1b\x1b"]), b"\x1b\x1b");
        for sequence in [
            &b"\x1b[27;~"[..],
            b"\x1b[27;2~",
            b"\x1b[27;1;13~",
            b"\x1b[27;2;13;4~",
            b"\x1b[4;2;13~",
        ] {
            assert_eq!(translated(&[sequence]), sequence, "{sequence:?}");
        }
    }

    #[test]
    fn translator_holds_incomplete_sequences_and_strings() {
        let mut translator = Translator::default();
        assert!(translator.push(b"\x1b").is_empty());
        assert!(translator.has_pending());
        assert_eq!(translator.take_pending(), b"\x1b");
        assert!(!translator.has_pending());
        assert!(translator.push(b"\x1b[27;").is_empty());
        assert_eq!(translator.push(b"2;13~"), b"\x1b[13;2u");
        assert_eq!(translator.push(b"ok\x1b[A"), b"ok\x1b[A");
        assert!(!translator.has_pending());

        let mut osc = Translator::default();
        assert!(osc.push(b"\x1b]11;rgb:").is_empty() && osc.has_pending());
        assert!(osc.push(b"0000/0000/0000\x07").is_empty() && !osc.has_pending());
        let mut capped = Translator::default();
        let overflow = vec![b'x'; MAX_STRING_SEQUENCE];
        assert!(capped.push(b"\x1b_G").is_empty());
        for _ in 0..3 {
            assert!(capped.push(&overflow).is_empty());
            assert!(
                capped.pending.len() <= MAX_STRING_SEQUENCE && capped.take_pending().is_empty()
            );
        }
        assert!(capped.push(b"\x1b").is_empty());
        assert_eq!(capped.push(b"\\ok"), b"ok");
    }

    #[test]
    fn translator_drops_terminal_replies_and_keeps_paste() {
        for reply in [
            &b"\x1b_Gi=31;OK\x1b\\"[..],
            b"\x1b[?62;22;52c",
            b"\x1b[?95;3u",
            b"\x1b[6;40;20t",
            b"\x1b[0n",
            b"\x1bP>|ghostty 1.4.0\x1b\\",
            b"\x1b]11;rgb:0000/0000/0000\x07",
        ] {
            assert!(translated(&[reply]).is_empty(), "{reply:?}");
            for split in 2..reply.len() {
                let mut translator = Translator::default();
                assert!(translator.push(&reply[..split]).is_empty());
                assert!(translator.take_pending().is_empty());
                assert!(translator.push(&reply[split..]).is_empty());
                assert_eq!(translator.push(b"x"), b"x");
            }
        }
        assert_eq!(translated(&[b"\x1b_Gi=31;OK\x1b\\a\x1b[?62;22;52c"]), b"a");

        for replies in [
            &b"\x1b[95;3uGi=31;OK\x1b[92;3u\x1b[91;3u?62;22;52c\x1b[91;3u6;40;20t\x1b[91;3u0n"[..],
            b"\x1b[27;3;95~Gi=31;OK\x1b[27;3;92~\x1b[27;3;91~?62;22;52c\x1b[27;3;91~6;40;20t\x1b[27;3;91~0n",
        ] {
            assert!(translated(&[replies]).is_empty());
            let chunks: Vec<_> = replies.chunks(1).collect();
            assert!(translated(&chunks).is_empty());
        }
        assert_eq!(
            translated(&[b"\x1b[112;3ux\x1b[27;3;113~"]),
            b"\x1b[112;3ux\x1b[113;3u"
        );

        let paste = b"\x1b[200~\x1b_Gi=1;OK\x1b\\\x1b[?62;22;52c\x1b[27;2;13~\x1b[201~";
        assert_eq!(translated(&[paste]), paste);
        let paste = b"\x1b[200~\x1b_Gunfinished\x1b]also unfinished\x1b[201~";
        for split in 0..paste.len() {
            assert_eq!(translated(&[&paste[..split], &paste[split..]]), paste);
        }
        let paste = b"\x1b[200~\x1b[95;3uGi=31;OK\x1b[92;3u\x1b[201~";
        assert_eq!(translated(&paste.chunks(1).collect::<Vec<_>>()), paste);
    }
}
