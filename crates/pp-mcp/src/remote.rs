//! HTTP／SSE 传输。工作线程独占网络状态，稳定清理句柄可以随时取消 I/O。

use super::{MESSAGE_LIMIT, RemoteConfig};
use anyhow::{Context, Result, bail, ensure};
use pp_core::cancellation::{self, CancellationToken, Cancelled};
use reqwest::{Method, Response, Url, header::HeaderValue};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc as async_mpsc;

pub(super) struct Worker {
    shutdown: CancellationToken,
    thread: Option<thread::JoinHandle<()>>,
}
impl Worker {
    pub(super) fn stop(&mut self) -> Result<()> {
        self.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("MCP network worker failed"))?;
        }
        Ok(())
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

enum Operation {
    Send(Value),
    Receive,
    Version(String),
}
struct Command {
    operation: Operation,
    deadline: Instant,
    cancellation: CancellationToken,
    reply: mpsc::SyncSender<Result<Value>>,
}

pub(super) struct RemoteClient {
    sender: async_mpsc::Sender<Command>,
}
impl RemoteClient {
    pub(super) fn spawn(config: RemoteConfig, legacy: bool) -> Result<(Self, Worker)> {
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        let (sender, mut receiver) = async_mpsc::channel::<Command>(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| anyhow::anyhow!("starting MCP network worker failed"))?;
        let thread = thread::Builder::new().name("pp-mcp-http".into()).spawn(move || {
            runtime.block_on(async move {
                let mut io = match RemoteIo::new(config, legacy) {
                    Ok(io) => io,
                    Err(_) => return,
                };
                loop {
                    let command = tokio::select! {
                        biased;
                        _ = stop.cancelled() => break,
                        command = receiver.recv() => match command { Some(c) => c, None => break },
                    };
                    let result = tokio::select! {
                        biased;
                        _ = stop.cancelled() => Err(Cancelled.into()),
                        _ = command.cancellation.cancelled() => Err(Cancelled.into()),
                        result = tokio::time::timeout_at(command.deadline.into(), io.perform(command.operation)) => {
                            result.unwrap_or_else(|_| Err(anyhow::anyhow!("MCP request timed out")))
                        }
                    };
                    let failed = result.is_err();
                    if result.as_ref().is_err_and(|e| e.is::<Cancelled>()) {
                        // 只做有界尽力通知；连接关闭并不表示服务端已经停止工作。
                        let _ = tokio::time::timeout(Duration::from_millis(100), io.cancel()).await;
                    }
                    let _ = command.reply.send(result);
                    if failed { break; }
                }
                // 调用方在 send／receive 之间取消时，仍通知已经发送的请求。
                let _ = tokio::time::timeout(Duration::from_millis(100), io.cancel()).await;
                io.stream.take();
                let _ = tokio::time::timeout(Duration::from_millis(100), io.close()).await;
            });
            runtime.shutdown_timeout(Duration::from_millis(100));
        }).map_err(|_| anyhow::anyhow!("starting MCP network worker failed"))?;
        Ok((
            Self { sender },
            Worker {
                shutdown,
                thread: Some(thread),
            },
        ))
    }
    fn exchange(
        &self,
        operation: Operation,
        deadline: Instant,
        token: &CancellationToken,
    ) -> Result<Value> {
        cancellation::check(token)?;
        let (reply, receive) = mpsc::sync_channel(1);
        self.sender
            .try_send(Command {
                operation,
                deadline,
                cancellation: token.clone(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("MCP connection is closed"))?;
        receive
            .recv()
            .map_err(|_| anyhow::anyhow!("MCP network worker exited"))?
    }
    pub(super) fn send(
        &self,
        message: Value,
        deadline: Instant,
        token: &CancellationToken,
    ) -> Result<()> {
        self.exchange(Operation::Send(message), deadline, token)
            .map(|_| ())
    }
    pub(super) fn receive(&self, deadline: Instant, token: &CancellationToken) -> Result<Value> {
        self.exchange(Operation::Receive, deadline, token)
    }
    pub(super) fn set_version(
        &self,
        version: &str,
        deadline: Instant,
        token: &CancellationToken,
    ) -> Result<()> {
        self.exchange(Operation::Version(version.into()), deadline, token)
            .map(|_| ())
    }
}

struct RemoteIo {
    client: reqwest::Client,
    url: Url,
    legacy: bool,
    endpoint: Option<Url>,
    session: Option<HeaderValue>,
    version: Option<String>,
    stream: Option<EventStream>,
    pending: VecDeque<Value>,
    request_id: Option<Value>,
}
impl RemoteIo {
    fn new(config: RemoteConfig, legacy: bool) -> Result<Self> {
        let client = reqwest::Client::builder()
            .default_headers(config.headers)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| anyhow::anyhow!("creating MCP HTTP client failed"))?;
        Ok(Self {
            client,
            url: config.url,
            legacy,
            endpoint: None,
            session: None,
            version: None,
            stream: None,
            pending: VecDeque::new(),
            request_id: None,
        })
    }
    fn request(&self, method: Method, url: Url) -> reqwest::RequestBuilder {
        let mut request = self.client.request(method, url);
        if let Some(session) = &self.session {
            request = request.header("MCP-Session-Id", session);
        }
        if let Some(version) = &self.version {
            request = request.header("MCP-Protocol-Version", version);
        }
        request
    }
    async fn perform(&mut self, operation: Operation) -> Result<Value> {
        match operation {
            Operation::Send(message) => {
                self.send(message).await?;
                Ok(Value::Null)
            }
            Operation::Receive => self.receive().await,
            Operation::Version(version) => {
                self.version = Some(version);
                Ok(Value::Null)
            }
        }
    }
    async fn connect_sse(&mut self) -> Result<()> {
        let response = self
            .request(Method::GET, self.url.clone())
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("MCP HTTP connection failed"))?;
        check_status(&response)?;
        ensure!(
            content_type(&response).eq_ignore_ascii_case("text/event-stream"),
            "invalid MCP HTTP content type"
        );
        let mut stream = EventStream::new(response);
        let event = stream.next().await?;
        ensure!(event.kind == "endpoint", "missing MCP SSE endpoint event");
        let endpoint = self
            .url
            .join(&event.data)
            .map_err(|_| anyhow::anyhow!("invalid MCP SSE endpoint"))?;
        ensure!(
            endpoint.origin() == self.url.origin()
                && endpoint.username().is_empty()
                && endpoint.password().is_none()
                && endpoint.fragment().is_none(),
            "invalid MCP SSE endpoint"
        );
        self.endpoint = Some(endpoint);
        self.stream = Some(stream);
        Ok(())
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let body = message.to_string();
        ensure!(body.len() < MESSAGE_LIMIT, "MCP message exceeds size limit");
        if self.legacy && self.endpoint.is_none() {
            self.connect_sse().await?;
        }
        let is_request = message.get("method").is_some() && message.get("id").is_some();
        if is_request {
            self.request_id = message.get("id").cloned();
        }
        let initializing = message["method"] == "initialize";
        let response = self
            .request(
                Method::POST,
                self.endpoint.as_ref().unwrap_or(&self.url).clone(),
            )
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("MCP HTTP request failed"))?;
        check_status(&response)?;
        if self.legacy {
            return Ok(());
        }
        if !is_request {
            ensure!(
                response.status() == reqwest::StatusCode::ACCEPTED,
                "invalid MCP HTTP notification response"
            );
            ensure!(
                read_body(response).await?.is_empty(),
                "invalid MCP HTTP notification body"
            );
            return Ok(());
        }
        ensure!(
            response.status() == reqwest::StatusCode::OK,
            "invalid MCP HTTP request response"
        );
        if initializing && let Some(session) = response.headers().get("MCP-Session-Id") {
            ensure!(
                !session.is_empty() && session.as_bytes().iter().all(|b| (0x21..=0x7e).contains(b)),
                "invalid MCP session id"
            );
            let mut session = session.clone();
            session.set_sensitive(true);
            self.session = Some(session);
        }
        match content_type(&response) {
            value if value.eq_ignore_ascii_case("application/json") => {
                let message = serde_json::from_slice(&read_body(response).await?)
                    .map_err(|_| anyhow::anyhow!("invalid MCP JSON message"))?;
                self.pending.push_back(message);
            }
            value if value.eq_ignore_ascii_case("text/event-stream") => {
                ensure!(self.stream.is_none(), "unexpected MCP HTTP stream");
                self.stream = Some(EventStream::new(response));
            }
            _ => bail!("invalid MCP HTTP content type"),
        }
        Ok(())
    }
    async fn receive(&mut self) -> Result<Value> {
        let message = if let Some(message) = self.pending.pop_front() {
            message
        } else {
            loop {
                let event = self
                    .stream
                    .as_mut()
                    .context("MCP response unavailable")?
                    .next()
                    .await?;
                if matches!(event.kind.as_str(), "message" | "") {
                    break serde_json::from_str::<Value>(&event.data)
                        .map_err(|_| anyhow::anyhow!("invalid MCP JSON message"))?;
                }
            }
        };
        if message.get("method").is_none() && message.get("id") == self.request_id.as_ref() {
            self.request_id = None;
            if !self.legacy {
                self.stream.take();
            }
        }
        Ok(message)
    }
    async fn cancel(&mut self) -> Result<()> {
        if let Some(id) = self.request_id.take() {
            self.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})).await?;
        }
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        if self.session.is_some() {
            let _ = self.request(Method::DELETE, self.url.clone()).send().await;
        }
        Ok(())
    }
}
fn check_status(response: &Response) -> Result<()> {
    ensure!(
        response.status().is_success(),
        "MCP HTTP status {}",
        response.status().as_u16()
    );
    Ok(())
}
fn content_type(response: &Response) -> &str {
    response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
}
async fn read_body(mut response: Response) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("reading MCP HTTP response failed"))?
    {
        ensure!(
            bytes.len() + chunk.len() < MESSAGE_LIMIT,
            "MCP message exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

struct Event {
    kind: String,
    data: String,
}
struct EventStream {
    response: Response,
    decoder: Decoder,
}
impl EventStream {
    fn new(response: Response) -> Self {
        Self {
            response,
            decoder: Decoder::default(),
        }
    }
    async fn next(&mut self) -> Result<Event> {
        loop {
            if let Some(event) = self.decoder.next()? {
                return Ok(event);
            }
            let chunk = self
                .response
                .chunk()
                .await
                .map_err(|_| anyhow::anyhow!("reading MCP SSE failed"))?
                .context("MCP SSE connection closed")?;
            self.decoder.feed(&chunk);
        }
    }
}

#[derive(Default)]
struct Decoder {
    pending: Vec<u8>,
    offset: usize,
    scanned: usize,
    skip_lf: bool,
    kind: String,
    data: String,
    size: usize,
    started: bool,
}
impl Decoder {
    fn feed(&mut self, bytes: &[u8]) {
        // 每个网络分块只压缩一次，避免多行事件反复移动整个剩余缓冲。
        self.pending.drain(..self.offset);
        self.scanned -= self.offset;
        self.offset = 0;
        self.pending.extend_from_slice(bytes);
    }
    fn next(&mut self) -> Result<Option<Event>> {
        loop {
            if self.skip_lf {
                if self.offset == self.pending.len() {
                    return Ok(None);
                }
                if self.pending[self.offset] == b'\n' {
                    self.offset += 1;
                }
                self.scanned = self.offset;
                self.skip_lf = false;
            }
            let Some(relative) = self.pending[self.scanned..]
                .iter()
                .position(|b| matches!(b, b'\n' | b'\r'))
            else {
                self.scanned = self.pending.len();
                ensure!(
                    self.size + self.pending.len() - self.offset < MESSAGE_LIMIT,
                    "MCP message exceeds size limit"
                );
                return Ok(None);
            };
            let end = self.scanned + relative;
            self.size += end + 1 - self.offset;
            ensure!(self.size < MESSAGE_LIMIT, "MCP message exceeds size limit");
            let line = std::str::from_utf8(&self.pending[self.offset..end])
                .map_err(|_| anyhow::anyhow!("invalid MCP SSE UTF-8"))?;
            let line = if !self.started {
                line.strip_prefix('\u{feff}').unwrap_or(line)
            } else {
                line
            };
            self.started = true;
            self.skip_lf = self.pending[end] == b'\r';
            self.offset = end + 1;
            self.scanned = self.offset;
            if line.is_empty() {
                let kind = std::mem::take(&mut self.kind);
                let mut data = std::mem::take(&mut self.data);
                self.size = 0;
                // 空 data 的 priming 事件与心跳不作为 JSON 交给上层。
                if data.ends_with('\n') {
                    data.pop();
                }
                if !data.is_empty() {
                    return Ok(Some(Event { kind, data }));
                }
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => self.kind = value.into(),
                "data" => {
                    self.data.push_str(value);
                    self.data.push('\n');
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_sse_decoder_handles_chunk_boundaries_multiline_heartbeats_and_utf8() {
        let text = "\u{feff}: 心跳\r\n\r\nevent: message\r\ndata: {\r\ndata: \"text\":\"中文\"}\r\n\r\nid: cursor\ndata:\n\ndata: {}\n\n";
        let mut decoder = Decoder::default();
        let mut events = Vec::new();
        for byte in text.as_bytes() {
            decoder.feed(&[*byte]);
            while let Some(event) = decoder.next().unwrap() {
                events.push(event);
            }
        }
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "message");
        assert_eq!(
            serde_json::from_str::<Value>(&events[0].data).unwrap()["text"],
            "中文"
        );
        assert_eq!(events[1].data, "{}");
    }

    #[test]
    fn mcp_sse_decoder_rejects_oversized_invalid_utf8_and_unbounded_fields() {
        for bytes in [
            vec![b'x'; MESSAGE_LIMIT],
            b"data: \xff\n\n".to_vec(),
            "data: x\n".repeat(MESSAGE_LIMIT / 8 + 1).into_bytes(),
        ] {
            let mut decoder = Decoder {
                pending: bytes,
                ..Decoder::default()
            };
            assert!(decoder.next().is_err());
        }
    }
}
