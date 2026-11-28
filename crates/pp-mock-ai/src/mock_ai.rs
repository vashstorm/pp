//! Local Responses service available only in development builds.

mod fixtures;
pub use fixtures::{MockTest, MockTestEvent, MockTestReport, MockTestSnapshot, run_test};

#[cfg(test)]
mod native_tests;
mod protocol;
mod queues;
#[cfg(test)]
mod runner_tests;
mod sse;
#[cfg(test)]
mod tests;

use std::{
    io,
    net::{Ipv4Addr, TcpListener},
    sync::{Arc, OnceLock},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, bail};
use axum::Router;
use pp_core::cancellation::CancellationToken;

static ENDPOINT: OnceLock<MockEndpoint> = OnceLock::new();

/// Shareable service address and cancellation state; does not extend the listener's lifetime.
#[derive(Clone)]
pub struct MockEndpoint {
    port: u16,
    shutdown: CancellationToken,
    queues: Arc<queues::Queues>,
}

impl MockEndpoint {
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn cancel_run(&self, run_id: &str) {
        self.queues.cancel_run(run_id);
    }

    pub fn assert_exhausted(&self, run_id: &str) -> Result<()> {
        self.queues.assert_exhausted(run_id, None)
    }

    fn assert_all_exhausted(&self, run_id: &str, lanes: usize) -> Result<()> {
        self.queues.assert_exhausted(run_id, Some(lanes))
    }
}

pub fn install_endpoint(endpoint: MockEndpoint) -> Result<()> {
    ENDPOINT
        .set(endpoint)
        .map_err(|_| anyhow::anyhow!("mock AI endpoint is already installed"))
}

pub fn current_endpoint() -> Result<MockEndpoint> {
    ENDPOINT
        .get()
        .cloned()
        .context("mock AI endpoint is not installed")
}

/// Sole owner of the listener and the HTTP service thread; Drop cancels immediately without draining connections.
pub struct MockServer {
    endpoint: MockEndpoint,
    thread: Option<JoinHandle<()>>,
}

impl MockServer {
    pub fn start() -> Result<Self> {
        let listener = bind_listener()?;
        let port = listener.local_addr()?.port();
        listener.set_nonblocking(true)?;
        let endpoint = MockEndpoint {
            port,
            shutdown: CancellationToken::new(),
            queues: Arc::new(queues::Queues::default()),
        };
        let (ready, started) = std::sync::mpsc::channel();
        let service = endpoint.clone();
        let thread = thread::Builder::new()
            .name("pp-mock-http".into())
            .spawn(move || serve(listener, service, &ready))
            .context("starting mock AI service thread")?;
        match started.recv() {
            Ok(Ok(())) => {
                tracing::info!(event = "mock_ai_started", pid = std::process::id(), port);
                Ok(Self {
                    endpoint,
                    thread: Some(thread),
                })
            }
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow::anyhow!(
                    "mock AI service stopped before it was ready"
                ))
            }
        }
    }

    pub fn endpoint(&self) -> MockEndpoint {
        self.endpoint.clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.endpoint.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.endpoint.queues.clear();
    }
}

/// A single-threaded runtime carries `axum::serve`; the start result is reported only once ready.
fn serve(
    listener: TcpListener,
    endpoint: MockEndpoint,
    ready: &std::sync::mpsc::Sender<Result<()>>,
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
        let router = Router::new()
            .fallback(queues::handle)
            .with_state(endpoint.clone());
        if ready.send(Ok(())).is_err() {
            return;
        }
        tokio::select! {
            _ = axum::serve(listener, router) => {}
            _ = endpoint.shutdown.cancelled() => {}
        }
    });
    // Parse-only tasks that already started hold no business state; there is no need to wait for them on exit.
    runtime.shutdown_background();
}

fn bind_listener() -> Result<TcpListener> {
    const FIRST: u32 = 32_001;
    const COUNT: u32 = 65_536 - FIRST;
    let mut random = [0; 4];
    getrandom::fill(&mut random).context("choosing mock AI port")?;
    let start = u32::from_ne_bytes(random) % COUNT;
    for offset in 0..COUNT {
        let port = (FIRST + (start + offset) % COUNT) as u16;
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {}
            Err(error) => return Err(error).context("binding mock AI loopback listener"),
        }
    }
    bail!("no available mock AI loopback port in 32001..=65535")
}

#[cfg(test)]
mod integration {
    use super::*;
    use std::{
        io::{Read, Write},
        net::{SocketAddr, TcpStream},
        time::{Duration, Instant},
    };

    #[test]
    fn mock_ai_listener_lifecycle() {
        let first = MockServer::start().unwrap();
        let second = MockServer::start().unwrap();
        let endpoint = first.endpoint();
        assert!(endpoint.port() > 32_000);
        assert_ne!(endpoint.port(), second.endpoint().port());
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port()));
        let mut connection = TcpStream::connect(addr).unwrap();
        connection
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        connection
            .write_all(b"GET /missing HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut reply = String::new();
        connection.read_to_string(&mut reply).unwrap();
        assert!(reply.starts_with("HTTP/1.1 404"), "{reply}");
        drop(connection);
        drop(first);
        assert!(endpoint.shutdown.is_cancelled());
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_err() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "listener survived owner shutdown"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
