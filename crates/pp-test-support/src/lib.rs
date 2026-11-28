//! Loopback HTTP transport for test fixtures: real TCP, synchronous test threads, async Axum server.
//!
//! The test thread takes [`CapturedRequest`] values in order and writes responses back synchronously;
//! the HTTP server runs on a dedicated OS thread, driving `axum::serve` with a single-threaded runtime
//! so async never leaks to the caller.

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc as async_mpsc, oneshot};

/// Loopback server owner: listener, server thread and request queue all end with it.
pub struct TestServer {
    addr: SocketAddr,
    requests: Mutex<Receiver<CapturedRequest>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    /// Starts on `127.0.0.1:0`; the runtime, listener and router are all ready when it returns.
    pub fn start() -> io::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let (requests, queue) = mpsc::channel();
        let capture = Capture(Arc::new(Mutex::new(requests)));
        let (ready, startup) = mpsc::channel();
        let (shutdown, stop) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("pp-test-support".to_owned())
            .spawn(move || serve(listener, capture, &ready, stop))?;
        match startup.recv() {
            Ok(Ok(())) => Ok(Self {
                addr,
                requests: Mutex::new(queue),
                shutdown: Some(shutdown),
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other(
                    "test transport stopped before it was ready",
                ))
            }
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Waits for the next request; returns `Ok(None)` on timeout and `BrokenPipe` when the server queue disconnects.
    pub fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<CapturedRequest>> {
        let requests = self.lock();
        match requests.recv_timeout(timeout) {
            Ok(request) => Ok(Some(request)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }

    /// Non-blocking receive; returns `Ok(None)` when the queue is empty.
    pub fn try_recv(&self) -> io::Result<Option<CapturedRequest>> {
        let requests = self.lock();
        match requests.try_recv() {
            Ok(request) => Ok(Some(request)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(closed()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Receiver<CapturedRequest>> {
        self.requests
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A captured request; the client sees no response headers until [`respond`](CapturedRequest::respond).
pub struct CapturedRequest {
    method: Method,
    target: String,
    headers: HeaderMap,
    body: Bytes,
    reply: oneshot::Sender<Response>,
}

impl CapturedRequest {
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Raw request-target: not decoded, query preserved.
    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn json(&self) -> serde_json::Result<Value> {
        serde_json::from_slice(&self.body)
    }

    /// Hands back a complete response; on return the server has not yet guaranteed the bytes are on the wire.
    pub fn respond(self, response: Response) -> io::Result<()> {
        self.reply.send(response).map_err(|_| closed())
    }

    /// Responds with a streaming body; each [`BodyWriter::send`] is backpressured by a capacity-1 channel.
    pub fn start_stream(self, status: StatusCode, headers: HeaderMap) -> io::Result<BodyWriter> {
        let (sender, mut receiver) = async_mpsc::channel(1);
        let stream = futures_util::stream::poll_fn(move |context| receiver.poll_recv(context))
            .map(Ok::<Bytes, io::Error>);
        let mut builder = Response::builder().status(status);
        for (name, value) in &headers {
            builder = builder.header(name.clone(), value.clone());
        }
        let response = builder
            .body(Body::from_stream(stream))
            .map_err(io::Error::other)?;
        self.reply.send(response).map_err(|_| closed())?;
        Ok(BodyWriter { sender })
    }

    /// `text/event-stream` response; the frame format is fixed as `data: {payload}\n\n`.
    pub fn start_sse(self) -> io::Result<SseWriter> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/event-stream"),
        );
        Ok(SseWriter {
            writer: self.start_stream(StatusCode::OK, headers)?,
        })
    }
}

/// Streaming response writer; `finish` or Drop closes the body.
pub struct BodyWriter {
    sender: async_mpsc::Sender<Bytes>,
}

impl BodyWriter {
    pub fn send(&mut self, bytes: Bytes) -> io::Result<()> {
        self.sender.blocking_send(bytes).map_err(|_| closed())
    }

    pub fn finish(self) {}
}

/// SSE writer: encodes `data` frames only, adding no event names, heartbeats or terminal markers.
pub struct SseWriter {
    writer: BodyWriter,
}

impl SseWriter {
    pub fn frame(&mut self, data: &str) -> io::Result<()> {
        self.writer.send(Bytes::from(format!("data: {data}\n\n")))
    }

    pub fn event(&mut self, event: &Value) -> io::Result<()> {
        self.frame(&event.to_string())
    }

    pub fn finish(self) {
        self.writer.finish();
    }
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Sender<CapturedRequest>>>);

async fn capture_request(State(capture): State<Capture>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let Ok(body) = axum::body::to_bytes(body, usize::MAX).await else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(String::new, |target| target.as_str().to_owned());
    let (reply, response) = oneshot::channel();
    let captured = CapturedRequest {
        method: parts.method,
        target,
        headers: parts.headers,
        body,
        reply,
    };
    let sent = capture
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .send(captured)
        .is_ok();
    if !sent {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    match response.await {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn serve(
    listener: std::net::TcpListener,
    state: Capture,
    ready: &Sender<io::Result<()>>,
    stop: oneshot::Receiver<()>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = ready.send(Err(error));
                return;
            }
        };
        let router = Router::new().fallback(capture_request).with_state(state);
        if ready.send(Ok(())).is_err() {
            return;
        }
        tokio::select! {
            _ = axum::serve(listener, router) => {}
            _ = stop => {}
        }
    });
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "test transport is closed")
}

#[cfg(test)]
mod tests;
