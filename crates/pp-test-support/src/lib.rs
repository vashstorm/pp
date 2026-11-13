//! 测试夹具用的回环 HTTP 传输：真实 TCP、同步测试线程、异步 Axum 服务。
//!
//! 测试线程按顺序领取 [`CapturedRequest`] 并同步写回响应；HTTP 服务跑在独立的
//! OS 线程里，用单线程 runtime 承载 `axum::serve`，不把 async 泄漏给调用方。

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

/// 回环服务 owner：监听器、服务线程与请求队列都随它一起结束。
pub struct TestServer {
    addr: SocketAddr,
    requests: Mutex<Receiver<CapturedRequest>>,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    /// 在 `127.0.0.1:0` 上启动，返回时 runtime／listener／router 均已就绪。
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

    /// 等待下一个请求；超时返回 `Ok(None)`，服务端队列断开返回 `BrokenPipe`。
    pub fn recv_timeout(&self, timeout: Duration) -> io::Result<Option<CapturedRequest>> {
        let requests = self.lock();
        match requests.recv_timeout(timeout) {
            Ok(request) => Ok(Some(request)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err(closed()),
        }
    }

    /// 非阻塞领取，队列为空返回 `Ok(None)`。
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

/// 捕获到的请求；未 [`respond`](CapturedRequest::respond) 前客户端收不到响应头。
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

    /// 原始 request-target：不解码、不丢 query。
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

    /// 交还完整响应；返回时服务端尚未保证网络字节已写出。
    pub fn respond(self, response: Response) -> io::Result<()> {
        self.reply.send(response).map_err(|_| closed())
    }

    /// 以流式 body 响应；每次 [`BodyWriter::send`] 受容量 1 的通道背压。
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

    /// `text/event-stream` 响应，帧格式固定为 `data: {payload}\n\n`。
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

/// 流式响应写端；`finish` 或 Drop 关闭 body。
pub struct BodyWriter {
    sender: async_mpsc::Sender<Bytes>,
}

impl BodyWriter {
    pub fn send(&mut self, bytes: Bytes) -> io::Result<()> {
        self.sender.blocking_send(bytes).map_err(|_| closed())
    }

    pub fn finish(self) {}
}

/// SSE 写端：只编码 `data` 帧，不添加事件名、心跳或终止标记。
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
