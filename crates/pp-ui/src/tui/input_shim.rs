//! 把 tmux 的 xterm modifyOtherKeys 编码翻译成 Kitty CSI-u。
//!
//! tmux 在 `extended-keys on` 下按 `extended-keys-format`（默认 `xterm`）回报带
//! 修饰键的按键，例如 Shift + Enter 是 `CSI 27;2;13~`；crossterm 只解析
//! `CSI 13;2u`，其余序列会被静默丢弃。这里把 fd 0 换成一个伪终端从端，由
//! 后台线程读原始终端并做等价改写；没有该编码的终端不受影响。
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
/// 输入线程轮询间隔：既用于发现完整序列，也决定退出延迟。
const POLL_INTERVAL_MS: i32 = 50;
/// 重新扫描前的读取块大小。
const CHUNK: usize = 8 * 1024;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// 真实终端 fd：`restore` 用它换回 fd 0，使关闭 raw mode 作用在原始终端上。
static SAVED: Mutex<Option<File>> = Mutex::new(None);

/// 用伪终端替换 fd 0，并把原始终端字节经 [`Translator`] 转发进去。
///
/// 必须在 `enable_raw_mode` 与一次性能力探测之后调用：探测直接读原始终端。
pub(super) fn install() -> io::Result<()> {
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
        .spawn(move || pump(forwarded, master))?;
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

fn pump(mut input: File, mut output: File) {
    let mut translator = Translator::default();
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
            // 兜底：长时间没有后续字节的不完整序列不能让按键无限等待。
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
                // 单独的 Esc 不能等超时：已没有后续字节时立即放行，否则紧接着的
                // 输入会和 ESC 拼成 Alt 组合而被编辑器丢弃。
                if translator.has_pending()
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

/// 增量翻译器：只改写 `CSI 27;mod;code~`，其余字节原样输出。
#[derive(Default)]
pub(super) struct Translator {
    pending: Vec<u8>,
}

impl Translator {
    /// 追加原始字节，返回可以立即写出的结果；末尾不完整的序列留到下一次。
    pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut out = Vec::with_capacity(input.len());
        let mut index = 0;
        while index < input.len() {
            if input[index] != 0x1b {
                let end = input[index..]
                    .iter()
                    .position(|byte| *byte == 0x1b)
                    .map_or(input.len(), |offset| index + offset);
                out.extend_from_slice(&input[index..end]);
                index = end;
                continue;
            }
            match sequence(&input[index..]) {
                Sequence::Complete(end) => {
                    let sequence = &input[index..index + end];
                    match rewrite(sequence) {
                        Some(rewritten) => out.extend_from_slice(&rewritten),
                        None => out.extend_from_slice(sequence),
                    }
                    index += end;
                }
                Sequence::Incomplete => {
                    if input.len() - index <= MAX_SEQUENCE {
                        self.pending.extend_from_slice(&input[index..]);
                    } else {
                        out.extend_from_slice(&input[index..]);
                    }
                    index = input.len();
                }
            }
        }
        out
    }

    /// 取出尚未完成的字节：没有后续输入时它们是独立的 Esc 或非法序列前缀。
    pub(super) fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// 是否还有等待后续字节的序列前缀。
    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

enum Sequence {
    /// 完整序列的字节长度。
    Complete(usize),
    /// 结尾被截断，需要更多字节才能判定。
    Incomplete,
}

/// 返回 `ESC` 起始序列的结束位置；`ESC [ …` 以 0x40..=0x7E 收尾。
fn sequence(bytes: &[u8]) -> Sequence {
    if bytes.len() == 1 {
        return Sequence::Incomplete;
    }
    if bytes[1] != b'[' {
        // 非 CSI：`ESC ESC` 只放行一个，交给下一次扫描，Alt 组合整体透传。
        return Sequence::Complete(if bytes[1] == 0x1b { 1 } else { 2 });
    }
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
