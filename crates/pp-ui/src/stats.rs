//! 复用当前二进制的 stats 子命令；后台线程独占子进程，退出时回收。

use anyhow::{Context, Result, bail};
use std::{
    io::{BufRead, BufReader, IsTerminal, Read},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

pub(crate) struct Worker {
    requests: Option<Sender<String>>,
    pub(crate) replies: Receiver<Result<String>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(crate) fn new() -> Result<Self> {
        let (requests, receiver) = mpsc::channel::<String>();
        let (sender, replies) = mpsc::channel();
        let interactive = std::io::stdout().is_terminal();
        let thread = thread::Builder::new()
            .name("stats-launcher".into())
            .spawn(move || {
                let mut server: Option<Server> = None;
                while let Ok(arguments) = receiver.recv() {
                    let result = (|| {
                        let args = arguments.split_whitespace().collect::<Vec<_>>();
                        if let Some(active) = &mut server
                            && (active.arguments != arguments
                                || active.child.try_wait()?.is_some())
                        {
                            server = None;
                        }
                        if server.is_none() {
                            server = Some(Server::start(&arguments, &args)?);
                        }
                        let url = &server.as_ref().context("statistics server unavailable")?.url;
                        if interactive
                            && !args.contains(&"--no-open")
                            && open_browser(url.clone(), sender.clone()).is_err()
                        {
                            return Ok(format!(
                                "pp stats: {url}\nwarning: could not start browser opener; open the URL manually"
                            ));
                        }
                        Ok(format!("pp stats: {url}"))
                    })();
                    if sender.send(result).is_err() {
                        break;
                    }
                }
            })
            .context("starting statistics launcher")?;
        Ok(Self {
            requests: Some(requests),
            replies,
            thread: Some(thread),
        })
    }

    pub(crate) fn open(&self, arguments: &str) -> Result<()> {
        self.requests
            .as_ref()
            .context("statistics launcher stopped")?
            .send(arguments.to_owned())
            .map_err(|_| anyhow::anyhow!("statistics launcher stopped"))
    }
}

// 与独立 stats 入口一致：浏览器启动不阻塞服务或退出收尾。
fn open_browser(url: String, replies: Sender<Result<String>>) -> std::io::Result<()> {
    thread::Builder::new()
        .name("stats-browser".into())
        .spawn(move || {
            let program = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let opened = Command::new(program)
                .arg(&url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !opened {
                let _ = replies.send(Ok(format!(
                    "warning: could not open browser; open {url} manually"
                )));
            }
        })
        .map(|_| ())
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.requests.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Server {
    child: Child,
    arguments: String,
    url: String,
}

impl Server {
    fn start(arguments: &str, args: &[&str]) -> Result<Self> {
        let mut command = Command::new(std::env::current_exe().context("locating pp executable")?);
        command.arg("stats").args(args);
        if !args.contains(&"--no-open") {
            command.arg("--no-open");
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("starting pp stats")?;
        let mut server = Self {
            child,
            arguments: arguments.to_owned(),
            url: String::new(),
        };
        let stdout = server
            .child
            .stdout
            .take()
            .context("reading statistics URL")?;
        let mut line = String::new();
        BufReader::new(stdout).take(8192).read_line(&mut line)?;
        if let Some(url) = line.trim().strip_prefix("pp stats: http://") {
            server.url = format!("http://{url}");
            return Ok(server);
        }
        // 先终止再读取有界诊断，避免非服务输出（例如 --help）阻塞收尾。
        let _ = server.child.kill();
        let _ = server.child.wait();
        let mut diagnostic = String::new();
        if let Some(stderr) = server.child.stderr.take() {
            stderr.take(8192).read_to_string(&mut diagnostic)?;
        }
        bail!(
            "could not start pp stats: {}",
            pp_core::text::display_text(diagnostic.trim())
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
