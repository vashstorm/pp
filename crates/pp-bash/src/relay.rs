//! In-process pipe relay.
//!
//! Each pipe edge creates two pairs of OS pipes inside the pp process:
//!
//! ```text
//! upstream stage ──(pipe A write end)──▶ relay thread ──(pipe B write end)──▶ downstream stage
//! ```
//!
//! The relay thread does three things only: mirror upstream bytes into the temporary ring buffer,
//! forward the same bytes unchanged to the downstream stage, and observe endpoint lifetimes. It does
//! not write to disk, does not touch the main [`OutputSink`](pp_workspace::output::OutputSink), does
//! not call UI callbacks and does not push data into the model channel; so even a very slow downstream
//! only causes bounded backpressure and never stalls the interpreter thread or the supervisor thread.
//!
//! Key semantics:
//! - Mirror first, forward after: no further upstream read happens before the current block is fully
//!   forwarded, so observed bytes are always a prefix of the bytes that actually crossed the edge.
//! - Downstream `POLLERR`/`POLLHUP`/`POLLNVAL` closes the upstream read end immediately without draining.
//! - Before upstream EOF the pending bytes are written out, then the downstream write end is closed.
//! - poll waits at most 50ms, which bounds cooperative stop and run shutdown; every path closes both
//!   ends and the caller joins the thread.

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

/// Smallest block read/forwarded per relay iteration; also the only heap buffer size.
const RELAY_BUFFER: usize = 64 * 1024;
/// poll upper bound, which determines the longest detection latency for cooperative stop and a real
/// write-end EOF.
const POLL_INTERVAL_MS: i32 = 50;
/// How long normal shutdown waits for the relay to observe EOF by itself; after the timeout it switches
/// to cooperative stop.
pub(super) const FINISH_GRACE: Duration = Duration::from_millis(300);

/// Both ends of one pipe edge, handed to the interpreter; the relay handle stays in the registry and is
/// joined at shutdown.
pub(super) struct Edge {
    /// stdin of the downstream stage (pipe B read end).
    pub(super) consumer: OpenFile,
    /// stdout of the upstream stage (pipe A write end).
    pub(super) producer: OpenFile,
    pub(super) relay: Relay,
}

/// Lifetime handle of the relay thread. `done` is set once the stream is sealed so shutdown avoids
/// pointless waiting.
pub(super) struct Relay {
    done: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Relay {
    pub(super) fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Join the relay thread; a panicking thread counts as a capture failure rather than a clean shutdown.
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

/// Create one observable pipe edge.
///
/// A failure leaves no half-registered state: every endpoint and thread is released before returning
/// `Err`.
pub(super) fn spawn(stream: Arc<StreamState>, stop: Arc<AtomicBool>) -> io::Result<Edge> {
    let (upstream_reader, upstream_writer) = io::pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("creating pipe edge: {error}")))?;
    let (downstream_reader, downstream_writer) = io::pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("creating pipe edge: {error}")))?;

    // Only the relay-side endpoints are non-blocking; the interpreter side keeps normal blocking semantics.
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

/// If the relay thread inherited the default SIGPIPE disposition, writing to a closed downstream would
/// kill pp outright.
///
/// The Rust runtime ignores SIGPIPE by default, but it is still blocked inside the thread as a defense:
/// even if the host switches back to the default disposition, a write failure only yields `EPIPE`, and
/// the pending thread-level signal is discarded when the thread exits.
fn block_sigpipe() {
    // SAFETY: `sigset_t` is POD; `pthread_sigmask` is an async-signal-safe thread-local call.
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
    // The current block lives in `buffer[..pending_end]`; everything before `pending_start` is already
    // written downstream.
    let mut pending_start = 0usize;
    let mut pending_end = 0usize;

    loop {
        if stop.load(Ordering::Acquire) {
            return CloseReason::Interrupted;
        }

        if pending_start == pending_end {
            // No pending block: first probe with a zero timeout whether the downstream is still alive. macOS
            // poll only reports HUP/ERR on a write end when POLLOUT is requested, so events=0 cannot be used
            // to probe; the probe timeout is 0, so a normally writable state does not become a busy loop.
            match poll_one(downstream.as_raw_fd(), libc::POLLOUT, 0) {
                Ok(events) if events & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 => {
                    return CloseReason::DownstreamClosed;
                }
                Ok(_) => {}
                Err(()) => return CloseReason::Error,
            }
            // The real blocking point is upstream; the 50ms timeout serves cooperative stop.
            match poll_one(upstream.as_raw_fd(), libc::POLLIN, POLL_INTERVAL_MS) {
                Ok(0) => continue,
                Ok(events) if events & (libc::POLLIN | libc::POLLHUP) != 0 => {}
                Ok(_) => continue,
                Err(()) => return CloseReason::Error,
            }
            match upstream.read(&mut buffer) {
                Ok(0) => {
                    // At EOF there is never a pending block (upstream is re-read only after a block is written).
                    let _ = downstream.flush();
                    return CloseReason::Eof;
                }
                Ok(count) => {
                    // Mirror before forwarding: record the bytes actually observed even when the downstream
                    // consumes only part of them.
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

        // A block is pending: wait until the downstream is writable while watching whether it has closed.
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
            // Reset the cursor once fully written; otherwise keep the remaining range for the next POLLOUT.
            if pending_start == pending_end {
                pending_start = 0;
                pending_end = 0;
            }
        }
    }
}

/// Poll a single fd once; EINTR counts as no event and any other error returns `Err`.
fn poll_one(fd: i32, events: i16, timeout_ms: i32) -> Result<i16, ()> {
    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };
    loop {
        // SAFETY: `pollfd` is a single-element mutable array of the right length and stays valid for the call.
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
    fn pipeline_relay_eof_downstream_close_and_interrupt() {
        {
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
        {
            let stream = detached_stream(2);
            let stop = Arc::new(AtomicBool::new(false));
            let edge = spawn(stream.clone(), stop).expect("spawn relay");
            let (reader, mut writer, mut relay) = split(edge);

            // Once the downstream reader closes, the relay must exit even with no new upstream bytes.
            drop(reader);
            assert!(
                wait_until(|| relay.is_done()),
                "relay must notice a closed downstream without new upstream data"
            );
            relay.join();
            assert_eq!(stream.close_reason(), Some(CloseReason::DownstreamClosed));

            // The upstream read end is released, so later writes hit EPIPE instead of unbounded backpressure.
            let error = writer
                .write_all(b"after close")
                .expect_err("upstream read end is gone");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        }
        {
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

        // Slow downstream: force the relay through the partial write / EAGAIN branch.
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
        // The 1MiB ring easily holds 512KiB, so observed bytes equal forwarded bytes exactly.
        assert_eq!(stream.retained_range().1, TOTAL as u64);
        assert_eq!(stream.retained(), payload);
    }
}
