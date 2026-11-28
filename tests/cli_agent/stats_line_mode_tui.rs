// Real-terminal scenario for line-mode `/stats`: stdin is a FIFO (not a TTY) while stdout/stderr share a
// PTY, so the process picks line mode but `stdout` is still a terminal and `/stats` really tries to launch a
// browser.
//
// `/stats` is fed line by line through the FIFO, and the browser stand-in records the URL it was called with
// before exiting non-zero.

/// URL prefix of line-mode `/stats`; the browser failure warning does not contain it.
const URL_MARKER: &str = "pp stats: http://";

// Fixed configuration far from any real provider: this scenario sends no model requests and only needs the
// startup configuration to be valid.
const LINE_MODE_MODELS: &str = "providers:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://127.0.0.1:1/v1\n    models:\n      primary:\n        model: fixture-model\n";

/// Browser stand-in: records the URL it was called with and exits non-zero (browser launch failure).
fn opener_fixture(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let bin = dir.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let script = bin.join(program);
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$1\" >> \"$HOME/opened-url\"\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn fifo(path: &std::path::Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo {}", path.display());
}

/// The line-mode process's stdin is a FIFO: the case delivers command lines on demand, so output can be
/// observed per request.
struct LineModePty {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    // Both ends of the PTY must stay alive until the child exits.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    _slave: Box<dyn portable_pty::SlavePty + Send>,
    lines: mpsc::Sender<String>,
    output: mpsc::Receiver<Vec<u8>>,
    raw: Vec<u8>,
}

impl Drop for LineModePty {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

impl LineModePty {
    /// stdin is redirected to a FIFO: `is_terminal()` is false and the process picks line mode.
    fn start(project: &std::path::Path, home: &std::path::Path, bin: &std::path::Path) -> Self {
        std::fs::write(config_path(home, "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(config_path(home, "models.yml"), LINE_MODE_MODELS).unwrap();
        let input = home.join("stdin.fifo");
        fifo(&input);
        let spawn_guard = super::tui::PTY_SPAWN
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = portable_pty::CommandBuilder::new("/bin/sh");
        command.cwd(project);
        command.env("HOME", home);
        command.env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        command.arg("-c");
        command.arg("exec \"$1\" < \"$2\"");
        command.arg("sh");
        command.arg(env!("CARGO_BIN_EXE_pp"));
        command.arg(&input);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(spawn_guard);
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            use std::io::Read;
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        // Opening the write end blocks until the line-mode process opens the read end, so this thread is also
        // the startup barrier.
        let (lines, queue) = mpsc::channel::<String>();
        let path = input.clone();
        thread::spawn(move || {
            use std::io::Write;
            let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(&path) else {
                return;
            };
            while let Ok(line) = queue.recv() {
                if file.write_all(line.as_bytes()).is_err() || file.flush().is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            _master: pair.master,
            _slave: pair.slave,
            lines,
            output,
            raw: Vec::new(),
        }
    }

    fn send(&self, line: &str) {
        self.lines.send(line.to_owned()).unwrap();
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.raw).into_owned()
    }

    // Use "output appeared" as the barrier rather than sleeping to guess asynchronous completion.
    fn wait_for(&mut self, what: &str, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = self.text();
            if text.contains(needle) {
                return text;
            }
            match self.output.recv_timeout(Duration::from_millis(10)) {
                Ok(bytes) => self.raw.extend_from_slice(&bytes),
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; output so far: {}",
                self.text()
            );
        }
    }

    fn wait_for_urls(&mut self, expected: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.text().matches(URL_MARKER).count() < expected {
            match self.output.recv_timeout(Duration::from_millis(10)) {
                Ok(bytes) => self.raw.extend_from_slice(&bytes),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {expected} stats URLs; output so far: {}",
                self.text()
            );
        }
    }

    fn finish(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "line mode did not exit; output so far: {}",
                self.text()
            );
            thread::sleep(Duration::from_millis(10));
        };
        while let Ok(bytes) = self.output.try_recv() {
            self.raw.extend_from_slice(&bytes);
        }
        assert!(status.success(), "line mode failed: {status}");
        self.text()
    }
}

/// A failure hint from the browser thread must not take over the launcher's reply slot: the two `/stats`
/// calls must each print the address of their own service process.
#[test]
fn stats_line_mode_prints_one_url_per_request_when_the_browser_opener_fails() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let bin = opener_fixture(project.path());
    let mut pty = LineModePty::start(project.path(), home.path(), &bin);

    pty.send("/stats --port 0\n");
    pty.wait_for("the first stats URL", URL_MARKER);
    // The browser thread is independent of the URL reply, so wait until the stand-in really ran before sending
    // the next command.
    let opened_url = home.path().join("opened-url");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&opened_url)
        .is_ok_and(|opened| opened.contains("http://127.0.0.1:"))
    {
        assert!(Instant::now() < deadline, "browser opener never ran");
        thread::sleep(Duration::from_millis(10));
    }
    // The second command forces a different service process with different arguments: the two replies belong
    // to different addresses, so "printed once each" cannot be faked by any repeated output of one of them.
    pty.send("/stats --host 127.0.0.1 --port 0\n");
    pty.send("/quit\n");
    pty.wait_for_urls(2);
    let text = pty.finish();

    let addresses: Vec<&str> = text
        .split(URL_MARKER)
        .skip(1)
        .filter_map(|rest| rest.split('/').next())
        .collect();
    assert_eq!(
        addresses.len(),
        2,
        "each /stats request must print its own address: {text}"
    );
    assert!(
        addresses
            .iter()
            .all(|address| address.starts_with("127.0.0.1:")),
        "every printed address must come from a served launcher reply: {text}"
    );
}
