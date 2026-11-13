//! 统计服务的 Axum 传输层：监听、Host/Origin 策略、静态资源与同步工作端桥接。
//!
//! SQLite 与配置文件读写只属于 `server.rs` 的调用线程；这里只把原始
//! request-target 与已校验的风格选择交给同步工作端并等待一次性响应。

use std::{
    io,
    net::SocketAddr,
    sync::mpsc as std_mpsc,
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, anyhow};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
};
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

/// 内嵌字体的精确 request-target。
const FONT: &str = "/fonts/InterVariable.woff2";
/// 内嵌字体的响应类型。
const FONT_TYPE: &str = "font/woff2";
/// 所有响应共用的安全策略，保持既有字面量。
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; frame-ancestors 'none'";
/// 同步工作端不可用时的固定响应体，与 `dispatch` 共用同一份字面量。
const UNAVAILABLE_TYPE: &str = "application/json";

pub(super) const STYLE_PATH: &str = "/api/stats/style";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StyleUpdate {
    style: pp_config::config::StatsStyle,
}

/// 同步工作端的一次请求：原始 request-target、可选风格写入与一次性响应槽。
pub(super) struct Query {
    pub(super) target: String,
    pub(super) style: Option<pp_config::config::StatsStyle>,
    pub(super) reply: oneshot::Sender<(u16, &'static str, String)>,
}

/// 监听器与 runtime 的唯一 owner；Drop 立即取消并回收服务线程。
pub(super) struct Transport {
    address: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    result: std_mpsc::Receiver<io::Result<()>>,
}

impl Transport {
    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }

    /// 收尾服务线程并返回 `axum::serve` 的结果，不吞后台错误。
    pub(super) fn join(mut self) -> Result<()> {
        self.stop();
        match self.result.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(error).context("serving statistics"),
            Err(_) => Err(anyhow!("statistics server stopped without a result")),
        }
    }

    fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 绑定、启动单线程 runtime 与 router，就绪后才返回可用地址。
pub(super) fn start(address: SocketAddr) -> Result<(Transport, mpsc::Receiver<Query>)> {
    let listener = std::net::TcpListener::bind(address).context("starting statistics server")?;
    let address = listener
        .local_addr()
        .context("statistics listener unavailable")?;
    listener
        .set_nonblocking(true)
        .context("configuring statistics listener")?;
    let mut hosts = vec![address.to_string()];
    if address.ip().is_loopback() {
        hosts.push(format!("localhost:{}", address.port()));
    }
    let origins = hosts.iter().map(|host| format!("http://{host}")).collect();
    let (queries, requests) = mpsc::channel(32);
    let shared = Shared {
        queries,
        hosts,
        origins,
    };
    let (shutdown, stop) = oneshot::channel();
    let (results, result) = std_mpsc::channel();
    let (ready, started) = std_mpsc::channel();
    let thread = thread::Builder::new()
        .name("pp-stats-http".into())
        .spawn(move || serve(listener, shared, &ready, stop, results))
        .context("starting statistics HTTP thread")?;
    match started.recv() {
        Ok(Ok(())) => Ok((
            Transport {
                address,
                shutdown: Some(shutdown),
                thread: Some(thread),
                result,
            },
            requests,
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error).context("starting statistics server")
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow!("statistics server stopped before it was ready"))
        }
    }
}

#[derive(Clone)]
struct Shared {
    queries: mpsc::Sender<Query>,
    hosts: Vec<String>,
    origins: Vec<String>,
}

async fn handle(State(shared): State<Shared>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let target = parts
        .uri
        .path_and_query()
        .map_or_else(String::new, |target| target.as_str().to_owned());
    if !permitted(&shared, &parts.headers) {
        return json(
            StatusCode::FORBIDDEN,
            json!({"error":"allowed origin required"}),
        );
    }
    let mut style = None;
    if target == STYLE_PATH && parts.method == Method::POST {
        // 风格偏好是唯一写入口，要求显式同源 JSON，拒绝跨站表单与无 Origin 写入。
        if !parts.headers.contains_key(header::ORIGIN) {
            return json(
                StatusCode::FORBIDDEN,
                json!({"error":"allowed origin required"}),
            );
        }
        let is_json = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            });
        if !is_json {
            return json(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                json!({"error":"JSON required"}),
            );
        }
        let bytes = match to_bytes(body, 1024).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return json(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    json!({"error":"invalid style request body"}),
                );
            }
        };
        let update = match serde_json::from_slice::<StyleUpdate>(&bytes) {
            Ok(update) => update,
            Err(_) => {
                return json(
                    StatusCode::BAD_REQUEST,
                    json!({"error":"invalid statistics style"}),
                );
            }
        };
        style = Some(update.style);
    } else if parts.method != Method::GET {
        return json(
            StatusCode::METHOD_NOT_ALLOWED,
            json!({"error":"GET required"}),
        );
    }
    if target == FONT {
        return resource(
            StatusCode::OK,
            FONT_TYPE,
            Body::from(include_bytes!("../fonts/InterVariable.woff2").as_slice()),
        );
    }
    if let Some((kind, text)) = super::static_text(&target) {
        return resource(StatusCode::OK, kind, Body::from(text));
    }
    let (reply, response) = oneshot::channel();
    if shared
        .queries
        .send(Query {
            target,
            style,
            reply,
        })
        .await
        .is_err()
    {
        return unavailable();
    }
    match response.await {
        Ok((status, kind, body)) => resource(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            kind,
            Body::from(body),
        ),
        Err(_) => unavailable(),
    }
}

/// Host 必须等于实际绑定地址；Origin 可缺省，存在时只允许同源。
fn permitted(shared: &Shared, headers: &HeaderMap) -> bool {
    let host = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
        .unwrap_or_default();
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|origin| origin.to_str().ok());
    shared.hosts.iter().any(|allowed| allowed == host)
        && origin.is_none_or(|origin| shared.origins.iter().any(|allowed| allowed == origin))
}

fn unavailable() -> Response {
    resource(
        StatusCode::SERVICE_UNAVAILABLE,
        UNAVAILABLE_TYPE,
        Body::from(super::DATABASE_UNAVAILABLE),
    )
}

fn json(status: StatusCode, body: serde_json::Value) -> Response {
    resource(status, UNAVAILABLE_TYPE, Body::from(body.to_string()))
}

fn resource(status: StatusCode, kind: &str, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&format!("{kind}; charset=utf-8")).expect("static media type"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    response
}

fn serve(
    listener: std::net::TcpListener,
    shared: Shared,
    ready: &std_mpsc::Sender<Result<()>>,
    stop: oneshot::Receiver<()>,
    results: std_mpsc::Sender<io::Result<()>>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready.send(Err(error.into()));
            return;
        }
    };
    runtime.block_on(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = ready.send(Err(error.into()));
                return;
            }
        };
        let router = Router::new().fallback(handle).with_state(shared);
        if ready.send(Ok(())).is_err() {
            return;
        }
        let result = tokio::select! {
            result = axum::serve(listener, router) => result,
            _ = stop => Ok(()),
        };
        let _ = results.send(result);
    });
}
