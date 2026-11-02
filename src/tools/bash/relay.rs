//! 进程内管道中继。
//!
//! 每条管道边在 pp 进程内创建两对 OS pipe：
//!
//! ```text
//! 上游阶段 ──(pipe A 写端)──▶ relay 线程 ──(pipe B 写端)──▶ 下游阶段
//! ```
//!
//! 中继线程只做三件事：把上游字节镜像到临时环形缓存、把同一份字节原样转发给
//! 下游、观测端点生命周期。它不写磁盘、不碰主 [`OutputSink`](crate::tools::output::OutputSink)、
//! 不调用 UI 回调，也不向模型 channel 投递数据；因此即使下游很慢，也只产生有限
//! 背压，不会拖住解释器线程或监督线程。
//!
//! 关键语义：
//! - 先镜像、后转发：当前块被完整转发前不再从上游读取，因此观察到的字节一定
//!   是实际流经该边的字节前缀。
//! - 下游 `POLLERR`/`POLLHUP`/`POLLNVAL` 立即关闭上游读端，不继续排空。
//! - 上游 EOF 前先写完待发送字节，再关闭下游写端。
//! - poll 最长 50ms，用于协作停止与 run 收尾；任何路径都关闭两端并由调用方 join。

use super::streams::{CloseReason, StreamState};
use brush_core::openfiles::OpenFile;
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    io::{self, PipeReader, PipeWriter, Read, Write},
    os::fd::{AsFd, AsRawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// 每次从中继侧读取/转发的最小块；也是唯一的堆缓冲大小。
const RELAY_BUFFER: usize = 64 * 1024;
/// poll 上限，决定协作停止与实际写端 EOF 的最长感知延迟。
const POLL_INTERVAL_MS: i32 = 50;
/// 正常收尾时等待 relay 自行观察到 EOF 的时间；超时后转为协作停止。
pub(super) const FINISH_GRACE: Duration = Duration::from_millis(300);

/// 一条管道边的两端，交给解释器；中继句柄留在 registry，收尾时 join。
pub(super) struct Edge {
    /// 下游阶段的 stdin（pipe B 读端）。
    pub(super) consumer: OpenFile,
    /// 上游阶段的 stdout（pipe A 写端）。
    pub(super) producer: OpenFile,
    pub(super) relay: Relay,
}

/// relay 线程的生命周期句柄。`done` 在流封存后置位，便于收尾时避免无谓等待。
pub(super) struct Relay {
    done: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Relay {
    pub(super) fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// join 中继线程；线程 panic 视为一次捕获失败而不是成功收尾。
    pub(super) fn join(&mut self) {
        let Some(handle) = self.join.take() else {
            return;
        };
        if handle.join().is_err() {
            tracing::warn!(
                event = "pipe_relay_thread_panicked",
                "pipe relay thread panicked"
            );
        }
    }
}

/// 创建一条可观测的管道边。
///
/// 失败时不会留下半登记状态：所有端点与线程都在返回 `Err` 前释放。
pub(super) fn spawn(stream: Arc<StreamState>, stop: Arc<AtomicBool>) -> io::Result<Edge> {
    let (upstream_reader, upstream_writer) = io::pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("creating pipe edge: {error}")))?;
    let (downstream_reader, downstream_writer) = io::pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("creating pipe edge: {error}")))?;

    // 只有中继侧的端点使用非阻塞；解释器侧保持正常的阻塞语义。
    if let Err(error) = nonblocking(&upstream_reader) {
        return Err(io::Error::new(
            error.kind(),
            format!("configuring pipe edge: {error}"),
        ));
    }
    if let Err(error) = nonblocking(&downstream_writer) {
        return Err(io::Error::new(
            error.kind(),
            format!("configuring pipe edge: {error}"),
        ));
    }

    let done = Arc::new(AtomicBool::new(false));
    let thread_done = done.clone();
    let join = thread::Builder::new()
        .name("pp-bash-pipe-relay".into())
        .spawn(move || {
            block_sigpipe();
            let reason = relay_loop(upstream_reader, downstream_writer, &stream, &stop);
            stream.finish(reason);
            thread_done.store(true, Ordering::Release);
        })
        .map_err(|error| io::Error::new(error.kind(), format!("starting pipe relay: {error}")))?;

    Ok(Edge {
        consumer: OpenFile::PipeReader(downstream_reader),
        producer: OpenFile::PipeWriter(upstream_writer),
        relay: Relay {
            done,
            join: Some(join),
        },
    })
}

fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
    let flags = fcntl_getfl(fd).map_err(errno_to_io)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(errno_to_io)
}

fn errno_to_io(error: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(error.raw_os_error())
}

/// 中继线程若继承默认 SIGPIPE 处置，写已关闭的下游会直接杀死 pp。
///
/// Rust 运行时默认忽略 SIGPIPE，但这里仍在线程内阻塞它作为防御：即使宿主将来
/// 改回默认处置，写失败也只会得到 `EPIPE`，而挂起的线程级信号随线程退出丢弃。
fn block_sigpipe() {
    // SAFETY: `sigset_t` 是 POD；`pthread_sigmask` 是 async-signal-safe 的线程本地调用。
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

fn relay_loop(
    mut upstream: PipeReader,
    mut downstream: PipeWriter,
    stream: &StreamState,
    stop: &AtomicBool,
) -> CloseReason {
    let mut buffer = vec![0u8; RELAY_BUFFER];
    // 当前块在 `buffer[..pending_end]` 中；`pending_start` 之前的部分已写入下游。
    let mut pending_start = 0usize;
    let mut pending_end = 0usize;

    loop {
        if stop.load(Ordering::Acquire) {
            return CloseReason::Interrupted;
        }

        if pending_start == pending_end {
            // 无待转发块：先用零超时探测下游是否仍存活。macOS 的 poll 只在请求
            // POLLOUT 时才报告写端的 HUP/ERR，因此不能用 events=0 探测；同时本次
            // 探测超时为 0，不会让正常可写状态变成忙循环。
            match poll_one(downstream.as_raw_fd(), libc::POLLOUT, 0) {
                Ok(events) if events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 => {
                    return CloseReason::DownstreamClosed;
                }
                Ok(_) => {}
                Err(()) => return CloseReason::Error,
            }
            // 真正的阻塞点是上游；50ms 超时用于协作停止。
            match poll_one(upstream.as_raw_fd(), libc::POLLIN, POLL_INTERVAL_MS) {
                Ok(0) => continue,
                Ok(events) if events & (libc::POLLIN | libc::POLLHUP) != 0 => {}
                Ok(_) => continue,
                Err(()) => return CloseReason::Error,
            }
            match upstream.read(&mut buffer) {
                Ok(0) => {
                    // EOF 时不会有待发送块（只有块写完后才重新读上游）。
                    let _ = downstream.flush();
                    return CloseReason::Eof;
                }
                Ok(count) => {
                    // 先镜像再转发：即使下游只消费了一部分，也记录实际观察到的字节。
                    stream.observe(&buffer[..count]);
                    pending_start = 0;
                    pending_end = count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return CloseReason::Error,
            }
            continue;
        }

        // 有待发送块：等下游可写，同时观察它是否已经关闭。
        let events = match poll_one(downstream.as_raw_fd(), libc::POLLOUT, POLL_INTERVAL_MS) {
            Ok(events) => events,
            Err(()) => return CloseReason::Error,
        };
        if events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            return CloseReason::DownstreamClosed;
        }
        if events & libc::POLLOUT != 0 {
            match flush_pending(&mut downstream, &buffer[..pending_end], &mut pending_start) {
                Ok(Forward::Complete) => {
                    pending_start = 0;
                    pending_end = 0;
                }
                Ok(Forward::Blocked) => {}
                Ok(Forward::Closed) => return CloseReason::DownstreamClosed,
                Err(_) => return CloseReason::Error,
            }
            // 完全写出后复位游标；未写出时保留剩余区间等待下一轮 POLLOUT。
            if pending_start == pending_end {
                pending_start = 0;
                pending_end = 0;
            }
        }
    }
}

/// 对单个 fd 做一次 poll；EINTR 视为无事件，其余错误返回 `Err`。
fn poll_one(fd: i32, events: i16, timeout_ms: i32) -> Result<i16, ()> {
    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        // SAFETY: `pollfd` 是长度正确的单元素可变数组，调用期间一直有效。
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(());
        }
        return Ok(if ready == 0 { 0 } else { pollfd.revents });
    }
}

enum Forward {
    Complete,
    Blocked,
    Closed,
}

fn flush_pending(writer: &mut PipeWriter, buffer: &[u8], start: &mut usize) -> io::Result<Forward> {
    while *start < buffer.len() {
        match writer.write(&buffer[*start..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pipe relay wrote zero bytes",
                ));
            }
            Ok(count) => *start += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(Forward::Blocked);
            }
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {
                return Ok(Forward::Closed);
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Forward::Complete)
}

#[cfg(test)]
mod tests {
    use super::super::streams::detached_stream;
    use super::*;
    use std::time::{Duration, Instant};

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn split(edge: Edge) -> (PipeReader, PipeWriter, Relay) {
        let Edge {
            consumer,
            producer,
            relay,
        } = edge;
        let OpenFile::PipeReader(reader) = consumer else {
            panic!("consumer must be a pipe reader");
        };
        let OpenFile::PipeWriter(writer) = producer else {
            panic!("producer must be a pipe writer");
        };
        (reader, writer, relay)
    }

    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn pipeline_relay_forwards_bytes_and_propagates_eof() {
        let stream = detached_stream(1);
        let stop = Arc::new(AtomicBool::new(false));
        let edge = spawn(stream.clone(), stop).expect("spawn relay");
        let (mut reader, mut writer, mut relay) = split(edge);

        writer
            .write_all(b"hello pipeline\n")
            .expect("write upstream");
        drop(writer);

        let mut forwarded = Vec::new();
        reader.read_to_end(&mut forwarded).expect("read downstream");

        assert_eq!(forwarded, b"hello pipeline\n");
        assert!(wait_until(|| relay.is_done()), "relay must observe EOF");
        relay.join();
        assert_eq!(stream.close_reason(), Some(CloseReason::Eof));
        assert_eq!(stream.retained(), b"hello pipeline\n".to_vec());
    }

    #[test]
    fn pipeline_relay_releases_upstream_when_downstream_closes() {
        let stream = detached_stream(2);
        let stop = Arc::new(AtomicBool::new(false));
        let edge = spawn(stream.clone(), stop).expect("spawn relay");
        let (reader, mut writer, mut relay) = split(edge);

        // 下游 reader 关闭后，中继在没有任何新上游字节的情况下也必须退出。
        drop(reader);
        assert!(
            wait_until(|| relay.is_done()),
            "relay must notice a closed downstream without new upstream data"
        );
        relay.join();
        assert_eq!(stream.close_reason(), Some(CloseReason::DownstreamClosed));

        // 上游读端已释放，后续写入获得 EPIPE，而不是无限背压。
        let error = writer
            .write_all(b"after close")
            .expect_err("upstream read end is gone");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn pipeline_relay_interrupt_keeps_observed_bytes() {
        let stream = detached_stream(3);
        let stop = Arc::new(AtomicBool::new(false));
        let edge = spawn(stream.clone(), stop.clone()).expect("spawn relay");
        let (_reader, mut writer, mut relay) = split(edge);

        writer
            .write_all(b"observed before cancel")
            .expect("write upstream");
        assert!(
            wait_until(|| stream.retained_range().1 == b"observed before cancel".len() as u64),
            "relay must mirror the block before it is consumed"
        );

        stop.store(true, Ordering::Release);
        assert!(
            wait_until(|| relay.is_done()),
            "relay must stop cooperatively"
        );
        relay.join();
        assert_eq!(stream.close_reason(), Some(CloseReason::Interrupted));
        assert_eq!(stream.retained(), b"observed before cancel".to_vec());
    }

    #[test]
    fn pipeline_relay_handles_large_payload_with_slow_consumer() {
        let stream = detached_stream(4);
        let stop = Arc::new(AtomicBool::new(false));
        let edge = spawn(stream.clone(), stop).expect("spawn relay");
        let (mut reader, mut writer, mut relay) = split(edge);

        const TOTAL: usize = 512 * 1024;
        let payload: Vec<u8> = (0..TOTAL).map(|index| (index % 251) as u8).collect();
        let produced = payload.clone();
        let producer = thread::spawn(move || {
            writer.write_all(&produced).expect("write large payload");
            drop(writer);
        });

        // 慢下游：强制 relay 经历 partial write / EAGAIN 分支。
        let mut forwarded = Vec::with_capacity(TOTAL);
        let mut chunk = [0u8; 7919];
        loop {
            let count = reader.read(&mut chunk).expect("read downstream");
            if count == 0 {
                break;
            }
            forwarded.extend_from_slice(&chunk[..count]);
            thread::sleep(Duration::from_millis(1));
        }
        producer.join().expect("producer thread");

        assert_eq!(forwarded.len(), TOTAL);
        assert_eq!(forwarded, payload);
        assert!(wait_until(|| relay.is_done()));
        relay.join();
        assert_eq!(stream.close_reason(), Some(CloseReason::Eof));
        // 1MiB 环足够容纳 512KiB，观察到的字节与转发字节完全一致。
        assert_eq!(stream.retained_range().1, TOTAL as u64);
        assert_eq!(stream.retained(), payload);
    }
}
