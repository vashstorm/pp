//! 把 tmux 的 xterm modifyOtherKeys 编码翻译成 Kitty CSI-u，并丢弃终端回复。
//!
//! tmux 在 `extended-keys on` 下按 `extended-keys-format`（默认 `xterm`）回报带
//! 修饰键的按键，例如 Shift + Enter 是 `CSI 27;2;13~`；crossterm 只解析
//! `CSI 13;2u`，其余序列会被静默丢弃。这里把 fd 0 换成一个伪终端从端，由
//! 后台线程读原始终端并做等价改写；没有该编码的终端不受影响。
//!
//! 终端能力回复（APC／DCS／OSC 字符串、DA1、Kitty 键盘标志、尺寸与设备状态
//! 报告）同样走这条通道，但 crossterm 会把无法解析的部分当成按键输出；探测窗口
//! 之外到达的回复还会在启动后冒出。它们一律整条丢弃，括号粘贴的内容除外。
//! tmux 会把未知回复的 `ESC _`／`ESC [` 等前缀当作 Alt 按键重新编码；分类前先
//! 还原这几个分隔符，同时支持 `csi-u` 和 `xterm` 两种扩展键格式。
//!
//! 必须用伪终端而不是管道：crossterm 在 stdin 不是 TTY 时改读 `/dev/tty`，
//! 管道会让它绕开翻译层。

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

/// 单个 CSI 序列的缓冲上限；超限直接透传，避免病态输入无限占用内存。
const MAX_SEQUENCE: usize = 64;
/// APC／DCS／OSC 的长度上限：终端标识、Kitty 图形回复与剪贴板回复都远小于它。
const MAX_STRING_SEQUENCE: usize = 4096;
/// 输入线程轮询间隔：既用于发现完整序列，也决定退出延迟。
const POLL_INTERVAL_MS: i32 = 50;
/// 重新扫描前的读取块大小。
const CHUNK: usize = 8 * 1024;
/// 括号粘贴标记：内部内容必须逐字节透传。
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

static INSTALLED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// 真实终端 fd：`restore` 用它换回 fd 0，使关闭 raw mode 作用在原始终端上。
static SAVED: Mutex<Option<File>> = Mutex::new(None);

/// 用伪终端替换 fd 0，并把原始终端字节经 [`Translator`] 转发进去。
///
/// 必须在 `enable_raw_mode` 与一次性能力探测之后调用：探测直接读原始终端。
pub(super) fn install(pending_reply: Vec<u8>) -> io::Result<()> {
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // 非 TTY 输入（重定向、测试夹具）保持原样，交给 crossterm 自行处理。
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
    // 主端由输入线程持有；从端成为 fd 0，必须 raw，否则行缓冲会吃掉按键。
    let master = unsafe { File::from_raw_fd(master) };
    let real = unsafe { libc::dup(libc::STDIN_FILENO) };
    if real < 0 {
        return Err(io::Error::last_os_error());
    }
    let real = unsafe { File::from_raw_fd(real) };
    let forwarded = real.try_clone()?;
    // 先登记回滚点：之后的任何失败都能把真实终端还给 fd 0。
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

/// 关闭回显、行缓冲与信号字符处理，使写入端的字节原样到达读取端。
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

/// 把真实终端还给 fd 0；随后的 `disable_raw_mode` 因此作用于原始终端。
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
            // 只放行独立 Esc；不完整回复保留到下次读取，不能因分片间隔泄漏正文。
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
                // 独立 Esc 立即放行，避免与用户随后输入的字符拼成 Alt；已经收到
                // CSI／APC 等前缀的分片继续缓冲，不受这个快捷路径影响。
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

/// 增量翻译器：改写 `CSI 27;mod;code~`，丢弃终端回复，其余字节原样输出。
#[derive(Default)]
pub(super) struct Translator {
    pending: Vec<u8>,
    /// 括号粘贴内部：内容不受序列分类与改写影响。
    paste: bool,
}

impl Translator {
    /// 追加原始字节，返回可以立即写出的结果；末尾不完整的序列留到下一次。
    pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &byte in bytes {
            if self.paste {
                // 粘贴内容不做 tmux 解码，只增量匹配结束标记。
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
                                // 有界丢弃超长字符串，保留可能分片的结束序列。
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

    /// 处理粘贴起始标记；其他完整序列过滤回复后改写或透传。
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

    /// 仅放行独立 Esc，其余前缀继续缓冲，直到回复完整或粘贴结束。
    pub(super) fn take_pending(&mut self) -> Vec<u8> {
        if !self.paste && self.pending == b"\x1b" {
            std::mem::take(&mut self.pending)
        } else {
            Vec::new()
        }
    }

    /// 是否还有等待后续字节的序列前缀。
    #[cfg(test)]
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// tmux 会把不认识的终端回复前缀当作 Alt 按键，并按扩展键协议重新编码。
/// 只还原控制序列分隔符，普通 Alt 快捷键仍交给 crossterm。
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
    /// 完整序列的字节长度。
    Complete(usize),
    /// 结尾被截断，需要更多字节才能判定。
    Incomplete,
}

/// 返回 `ESC` 起始序列的结束位置：CSI 以 0x40..=0x7E 收尾，APC／DCS 以 ST 收尾，
/// OSC 还接受 BEL。
fn sequence(bytes: &[u8]) -> Sequence {
    if bytes.len() == 1 {
        return Sequence::Incomplete;
    }
    match bytes[1] {
        b'[' => csi(bytes),
        b'_' | b'P' | b']' => control_string(bytes),
        // `ESC ESC` 只放行一个，交给下一次扫描，Alt 组合整体透传。
        _ => Sequence::Complete(if bytes[1] == 0x1b { 1 } else { 2 }),
    }
}

fn csi(bytes: &[u8]) -> Sequence {
    for (offset, byte) in bytes.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(byte) {
            return Sequence::Complete(offset + 1);
        }
        if !(0x20..=0x3f).contains(byte) {
            // 非法 CSI：只放行 ESC，后续字节重新扫描。
            return Sequence::Complete(1);
        }
    }
    Sequence::Incomplete
}

fn control_string(bytes: &[u8]) -> Sequence {
    // 只有 OSC 用 BEL 收尾；APC 与 DCS 只认 ST。
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

/// 终端能力回复只可能由终端发出，客户端永远不需要它们。
fn terminal_reply(sequence: &[u8]) -> bool {
    match sequence.get(1) {
        // APC／DCS／OSC 字符串：Kitty 图形回复、XTVERSION、剪贴板与配色回复。
        Some(b'_' | b'P' | b']') => true,
        Some(b'[') => {
            let private = sequence.get(2) == Some(&b'?');
            match sequence.last() {
                // DA1 与 Kitty 键盘协议标志。
                Some(b'c' | b'u') => private,
                // 窗口／单元格尺寸与设备状态报告。
                Some(b't' | b'n') => !private,
                _ => false,
            }
        }
        _ => false,
    }
}

/// `CSI 27;mod;code~` → `CSI code;mod u`；其他序列返回 `None` 原样输出。
fn rewrite(bytes: &[u8]) -> Option<Vec<u8>> {
    let body = bytes.strip_prefix(b"\x1b[")?.strip_suffix(b"~")?;
    let mut parts = std::str::from_utf8(body).ok()?.split(';');
    if parts.next()? != "27" {
        return None;
    }
    let modifier = parts.next()?.parse::<u16>().ok()?;
    let code = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(2..=8).contains(&modifier) {
        return None;
    }
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
    fn translator_rewrites_modify_other_keys_into_csi_u() {
        // tmux 默认格式：Shift + Enter、Alt + Enter、Ctrl + C、Shift + Tab。
        assert_eq!(translated(&[b"\x1b[27;2;13~"]), b"\x1b[13;2u");
        assert_eq!(translated(&[b"\x1b[27;3;13~"]), b"\x1b[13;3u");
        assert_eq!(translated(&[b"\x1b[27;5;99~"]), b"\x1b[99;5u");
        assert_eq!(translated(&[b"\x1b[27;2;9~"]), b"\x1b[9;2u");
        // 多字节键码保持完整。
        assert_eq!(translated(&[b"\x1b[27;2;233~"]), b"\x1b[233;2u");
    }

    #[test]
    fn translator_keeps_other_bytes_and_split_sequences_intact() {
        assert_eq!(translated(&[b"ab\xc3\xa9c"]), b"ab\xc3\xa9c");
        assert_eq!(translated(&[b"\x1b[A\x1b\r"]), b"\x1b[A\x1b\r");
        assert_eq!(
            translated(&[b"\x1b[200~paste\x1b[201~"]),
            b"\x1b[200~paste\x1b[201~"
        );
        assert_eq!(translated(&[b"\x1b[<0;4;5M"]), b"\x1b[<0;4;5M");
        // 序列跨读取边界时仍能完整改写。
        assert_eq!(translated(&[b"\x1b[27;", b"2;", b"13~"]), b"\x1b[13;2u");
        // 单独按下的 Esc 在前缀超时后放行，连续 Esc 不被合并。
        assert_eq!(translated(&[b"\x1b"]), b"\x1b");
        assert_eq!(translated(&[b"\x1b\x1b"]), b"\x1b\x1b");
    }

    #[test]
    fn translator_holds_incomplete_sequences_for_the_next_read() {
        let mut translator = Translator::default();
        assert!(translator.push(b"\x1b").is_empty());
        assert!(translator.has_pending());
        assert_eq!(translator.take_pending(), b"\x1b");
        assert!(!translator.has_pending());
        assert!(translator.push(b"\x1b[27;").is_empty());
        assert_eq!(translator.push(b"2;13~"), b"\x1b[13;2u");
        // 完整序列与普通字节不进入待完成缓冲。
        assert_eq!(translator.push(b"ok\x1b[A"), b"ok\x1b[A");
        assert!(!translator.has_pending());
    }

    #[test]
    fn translator_drops_terminal_replies() {
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
            // 输入线程会在两次读取间检查待处理字节，分片不能因此被提前放行。
            for split in 2..reply.len() {
                let mut translator = Translator::default();
                assert!(translator.push(&reply[..split]).is_empty());
                assert!(translator.take_pending().is_empty());
                assert!(translator.push(&reply[split..]).is_empty());
                assert_eq!(translator.push(b"x"), b"x");
            }
        }
        // 回复之间的真实按键仍然透传。
        assert_eq!(translated(&[b"\x1b_Gi=31;OK\x1b\\a\x1b[?62;22;52c"]), b"a");
    }

    #[test]
    fn translator_drops_ghostty_replies_reencoded_by_tmux() {
        // 从 Ghostty＋tmux（extended-keys-format csi-u）实际采集的回复。
        for replies in [
            &b"\x1b[95;3uGi=31;OK\x1b[92;3u\x1b[91;3u?62;22;52c\x1b[91;3u6;40;20t\x1b[91;3u0n"[..],
            b"\x1b[27;3;95~Gi=31;OK\x1b[27;3;92~\x1b[27;3;91~?62;22;52c\x1b[27;3;91~6;40;20t\x1b[27;3;91~0n",
        ] {
            assert!(translated(&[replies]).is_empty());
            let chunks: Vec<_> = replies.chunks(1).collect();
            assert!(translated(&chunks).is_empty());
        }
        // 普通 Alt 快捷键与真实按键不会被当成终端回复。
        assert_eq!(
            translated(&[b"\x1b[112;3ux\x1b[27;3;113~"]),
            b"\x1b[112;3ux\x1b[113;3u"
        );
    }

    #[test]
    fn translator_passes_bracketed_paste_content_through() {
        let paste = b"\x1b[200~\x1b_Gi=1;OK\x1b\\\x1b[?62;22;52c\x1b[27;2;13~\x1b[201~";
        assert_eq!(translated(&[paste]), paste);
        // 粘贴正文含未闭合控制字符串、结束标记跨读取时，同样逐字节保留。
        let paste = b"\x1b[200~\x1b_Gunfinished\x1b]also unfinished\x1b[201~";
        for split in 0..paste.len() {
            assert_eq!(translated(&[&paste[..split], &paste[split..]]), paste);
        }
        let paste = b"\x1b[200~\x1b[95;3uGi=31;OK\x1b[92;3u\x1b[201~";
        assert_eq!(translated(&paste.chunks(1).collect::<Vec<_>>()), paste);
    }

    #[test]
    fn translator_holds_control_strings_until_their_terminator() {
        let mut translator = Translator::default();
        assert!(translator.push(b"\x1b]11;rgb:").is_empty());
        assert!(translator.has_pending());
        assert!(translator.push(b"0000/0000/0000\x07").is_empty());
        assert!(!translator.has_pending());
        // 超长的未闭合字符串不无限占用内存，也不能把尾部泄漏成输入。
        let mut translator = Translator::default();
        let overflow = vec![b'x'; MAX_STRING_SEQUENCE];
        assert!(translator.push(b"\x1b_G").is_empty());
        for _ in 0..3 {
            assert!(translator.push(&overflow).is_empty());
            assert!(translator.pending.len() <= MAX_STRING_SEQUENCE);
            assert!(translator.take_pending().is_empty());
        }
        assert!(translator.push(b"\x1b").is_empty());
        assert_eq!(translator.push(b"\\ok"), b"ok");
    }

    #[test]
    fn translator_passes_malformed_modify_other_keys_through() {
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
}
