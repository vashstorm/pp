use super::*;
include!("effort_defaults.rs");
include!("mcp_tui.rs");
include!("shell_tui.rs");
include!("cancel_tui.rs");
include!("cancel_mcp_tui.rs");
include!("resume_tui.rs");
include!("resume_boundaries.rs");
include!("context_tui.rs");
include!("project_instructions_tui.rs");
include!("diff_blocks_tui.rs");
include!("changes_tui.rs");
include!("transcript_blocks_tui.rs");
include!("ask_tui.rs");
include!("subagents_tui.rs");
include!("task_test_tui.rs");
include!("auth_tui.rs");
include!("plan_tui.rs");
include!("todo_tui.rs");
include!("plan_list_tui.rs");
include!("plan_review_tui.rs");
include!("tree_tui.rs");
include!("runtime_input_tui.rs");
include!("runtime_responsiveness_tui.rs");
include!("scroll_stream_tui.rs");
include!("runtime_roles_tui.rs");
include!("menu_tui.rs");
include!("native_tui.rs");
include!("sqlite_tui.rs");
use portable_pty::{CommandBuilder, MasterPty, PtySize, SlavePty, native_pty_system};
use std::{
    io::Read,
    path::Path,
    sync::mpsc::{self, Receiver},
};

const DEADLINE: Duration = Duration::from_secs(5);
// portable-pty sets CLOEXEC after openpty; do not spawn another child in that window.
pub(super) static PTY_SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn input_is_ready(screen: &vt100::Screen) -> bool {
    screen
        .rows(0, screen.size().1)
        .any(|line| line.starts_with('❯'))
}

struct Pty {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    master: Box<dyn MasterPty + Send>,
    // Keep the slave open until termios has been checked after the child exits.
    slave: Option<Box<dyn SlavePty + Send>>,
    writer: Option<Box<dyn Write + Send>>,
    output: Receiver<Vec<u8>>,
    reader: Option<thread::JoinHandle<()>>,
    parser: vt100::Parser,
    parse_pending: Vec<u8>,
    raw: Vec<u8>,
    original_flags: [u128; 4],
}

fn terminal_flags(master: &dyn MasterPty) -> [u128; 4] {
    let termios = master.get_termios().expect("PTY must expose termios");
    [
        u128::from(termios.input_flags.bits()),
        u128::from(termios.output_flags.bits()),
        u128::from(termios.control_flags.bits()),
        u128::from(termios.local_flags.bits()),
    ]
}

impl Pty {
    fn start(server: &Server, cwd: &Path) -> Self {
        std::fs::write(config_path(cwd, "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(config_path(cwd, "models.yml"), fixture_models(server)).unwrap();
        Self::start_with_existing_files(cwd)
    }

    fn start_with_existing_files(cwd: &Path) -> Self {
        Self::start_with_args(cwd, cwd, &[])
    }

    fn start_with_args(cwd: &Path, home: &Path, args: &[&str]) -> Self {
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pp"));
        command.cwd(cwd);
        command.env("HOME", home);
        for arg in args {
            command.arg(arg);
        }
        Self::spawn(command)
    }

    // 持久化写失败：bash 的 ulimit -f 以 1024 字节为单位，忽略 SIGXFSZ 后写入返回 EFBIG。
    fn start_with_file_limit(cwd: &Path, home: &Path, blocks: u64, args: &[&str]) -> Self {
        let mut command = CommandBuilder::new("bash");
        command.cwd(cwd);
        command.env("HOME", home);
        command.arg("-c");
        command.arg(format!("trap '' XFSZ; ulimit -f {blocks}; exec \"$@\""));
        command.arg("--");
        command.arg(env!("CARGO_BIN_EXE_pp"));
        for arg in args {
            command.arg(arg);
        }
        Self::spawn(command)
    }

    fn spawn(mut command: CommandBuilder) -> Self {
        let spawn_guard = PTY_SPAWN.lock().unwrap_or_else(|error| error.into_inner());
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 30,
                cols: 100,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let original_flags = terminal_flags(pair.master.as_ref());
        command.env("TERM", "xterm-256color");
        // Exercise ANSI colors even when the test runner opts out of colors.
        command.env_remove("NO_COLOR");
        let writer = pair.master.take_writer().unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(spawn_guard);
        let (sender, output) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut buffer = [0; 8192];
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
        let mut pty = Self {
            child: Some(child),
            master: pair.master,
            slave: Some(pair.slave),
            writer: Some(writer),
            output,
            reader: Some(reader),
            parser: vt100::Parser::new(30, 100, 10_000),
            parse_pending: Vec::new(),
            raw: Vec::new(),
            original_flags,
        };
        pty.wait_for("主屏幕、括号粘贴和 SGR 鼠标模式", |screen| {
            !screen.alternate_screen()
                && screen.bracketed_paste()
                && screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
                && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
        });
        assert_ne!(
            terminal_flags(pty.master.as_ref()),
            pty.original_flags,
            "TUI did not enter raw mode\n{}",
            pty.diagnostics()
        );
        pty
    }

    fn input(&mut self, text: &str) {
        let writer = self.writer.as_mut().unwrap();
        writer.write_all(text.as_bytes()).unwrap();
        writer.flush().unwrap();
    }

    /// P/S/Q/R/T 按平台发送 Option 或 Ctrl；O 始终使用 Ctrl。
    fn function_key(&mut self, character: char) {
        let bytes: &str = if cfg!(target_os = "macos") {
            match character {
                'p' => "\x1bp",
                's' => "\x1bs",
                'o' => "\x0f",
                'q' => "\x1bq",
                'r' => "\x1br",
                't' => "\x1bt",
                _ => panic!("unsupported function key character: {character}"),
            }
        } else {
            match character {
                'p' => "\x10",
                's' => "\x13",
                'o' => "\x0f",
                'q' => "\x11",
                'r' => "\x12",
                't' => "\x14",
                _ => panic!("unsupported function key character: {character}"),
            }
        };
        self.input(bytes);
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        self.drain();
        let previous_output = self.raw.len();
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.parser.screen_mut().set_size(rows, cols);
        let deadline = Instant::now() + DEADLINE;
        loop {
            self.drain();
            if self.raw.len() > previous_output {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not redraw after resizing to {cols}x{rows}\n{}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn draft(&mut self, expected: &str) {
        self.wait_for(&format!("preserved input draft {expected:?}"), |screen| {
            // 输入框按物理行定位；contents() 会合并软换行，resize 后
            // 可能把正文与提示符拼在同一逻辑行，造成假超时。
            let lines: Vec<_> = screen.rows(0, screen.size().1).collect();
            // 主输入框的空白不能误判为搜索面板已关闭，须等待焦点恢复。
            if lines
                .get(usize::from(screen.cursor_position().0))
                .is_some_and(|line| line.starts_with(''))
            {
                return false;
            }
            // 上方菜单也使用箭头，最后一个提示符才是主输入框。
            let Some(start) = lines.iter().rposition(|line| line.starts_with('❯')) else {
                return false;
            };
            lines[start..]
                .iter()
                .take_while(|line| !line.starts_with('─'))
                .any(|line| line.trim_start_matches('❯').trim() == expected)
        });
    }

    fn model_search(&mut self, expected: &str) {
        self.wait_for("独立模型搜索框内容和焦点", |screen| {
            let text = screen.contents();
            let (row, _) = screen.cursor_position();
            !screen.hide_cursor()
                && text
                    .lines()
                    .nth(usize::from(row))
                    .is_some_and(|line| line.trim_end() == format!(" {expected}").trim_end())
                && text.lines().any(|line| line.trim_end() == "❯")
        });
    }

    fn ready_model(&mut self, expected: &str) {
        self.wait_for("空闲输入框和所选模型", |screen| {
            let text = screen.contents();
            input_is_ready(screen)
                && text
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains(expected))
        });
    }

    fn ready(&mut self) {
        self.wait_for("空闲输入框", |screen| {
            input_is_ready(screen)
                && !screen
                    .contents()
                    .lines()
                    .any(|line| line.trim_start().starts_with("✻ ") && !line.contains(" · done "))
        });
    }

    fn completed(&mut self) {
        self.wait_for("task completion committed", |screen| {
            input_is_ready(screen) && screen.contents().contains("✻ Completed")
        });
    }

    fn ready_effort(&mut self, expected: &str) {
        let (glyph, label) = match expected {
            "no think" => ("\u{f0766}", "off"),
            "low" => ("\u{f0a9f}", "low"),
            "medium" => ("\u{f0aa1}", "med"),
            "high" => ("\u{f0aa3}", "hig"),
            "xhigh" => ("\u{f0aa5}", "xhi"),
            "max" => ("\u{f0238}", "max"),
            _ => panic!("未知的思考级别：{expected}"),
        };
        self.wait_for("空闲输入框和思考级别", |screen| {
            input_is_ready(screen)
                && screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains(&format!("{glyph} {label} ·")))
        });
    }

    fn drain(&mut self) {
        while let Ok(bytes) = self.output.try_recv() {
            self.raw.extend_from_slice(&bytes);
            self.parse_pending.extend_from_slice(&bytes);
            // vt100 0.16 不实现 ED3；在测试适配层补上原生历史清理语义。
            let purge = b"\x1b[3J";
            while let Some(index) = self
                .parse_pending
                .windows(purge.len())
                .position(|part| part == purge)
            {
                self.parser.process(&self.parse_pending[..index]);
                let state = self.parser.screen().state_formatted();
                let (rows, columns) = self.parser.screen().size();
                self.parser = vt100::Parser::new(rows, columns, 10_000);
                self.parser.process(&state);
                self.parse_pending.drain(..index + purge.len());
            }
            let hold = (1..purge.len())
                .rev()
                .find(|count| self.parse_pending.ends_with(&purge[..*count]))
                .unwrap_or(0);
            let count = self.parse_pending.len() - hold;
            self.parser.process(&self.parse_pending[..count]);
            self.parse_pending.drain(..count);
        }
    }

    fn native_history(&mut self) -> (String, Vec<u8>) {
        self.drain();
        let mut screen = self.parser.screen().clone();
        screen.set_scrollback(usize::MAX);
        let maximum = screen.scrollback();
        let mut raw = Vec::new();
        let (_, columns) = screen.size();
        for offset in (1..=maximum).rev() {
            screen.set_scrollback(offset);
            raw.extend(screen.rows_formatted(0, columns).next().unwrap());
            raw.extend_from_slice(b"\r\n");
        }
        screen.set_scrollback(0);
        for row in screen.rows_formatted(0, columns) {
            raw.extend(row);
            raw.extend_from_slice(b"\r\n");
        }
        let mut parser = vt100::Parser::new(1000, columns, 0);
        parser.process(&raw);
        (parser.screen().contents(), raw)
    }

    fn diagnostics(&self) -> String {
        format!(
            "visible screen:\n{}\nraw terminal bytes (escaped): {:?}",
            self.parser.screen().contents(),
            String::from_utf8_lossy(&self.raw)
        )
    }

    fn wait_for(&mut self, description: &str, condition: impl Fn(&vt100::Screen) -> bool) {
        self.wait_for_timeout(description, DEADLINE, condition);
    }

    fn wait_for_timeout(
        &mut self,
        description: &str,
        timeout: Duration,
        condition: impl Fn(&vt100::Screen) -> bool,
    ) {
        let deadline = Instant::now() + timeout;
        loop {
            self.drain();
            if condition(self.parser.screen()) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}\n{}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn text(&mut self, text: &str) {
        self.wait_for(text, |screen| screen.contents().contains(text));
    }

    /// 当前可见屏幕文本（先排空已到达的输出）。
    fn contents(&mut self) -> String {
        self.drain();
        self.parser.screen().contents()
    }

    fn wait_for_raw(&mut self, expected: &[u8]) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            self.drain();
            if self
                .raw
                .windows(expected.len())
                .any(|bytes| bytes == expected)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "等待终端控制序列 {:?} 超时\n{}",
                String::from_utf8_lossy(expected),
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn request(&mut self, server: &Server) -> (Request, Value) {
        // Reuse the HTTP fixture, adding the PTY's diagnostics on its deadline.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receive(server))) {
            Ok(request) => request,
            Err(_) => {
                self.drain();
                panic!("model request failed\n{}", self.diagnostics());
            }
        }
    }

    fn request_at(&mut self, server: &Server, path: &str) -> (Request, Value) {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receive_at(server, path))) {
            Ok(request) => request,
            Err(_) => {
                self.drain();
                panic!("model request failed at {path}\n{}", self.diagnostics());
            }
        }
    }

    fn finish(mut self) {
        let deadline = Instant::now() + DEADLINE;
        let status = loop {
            self.drain();
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "TUI did not exit within five seconds\n{}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(10));
        };
        self.child.as_mut().unwrap().wait().unwrap();
        self.child.take();
        assert_eq!(status.exit_code(), 0, "{status}\n{}", self.diagnostics());
        assert!(status.success(), "{status}\n{}", self.diagnostics());
        self.wait_for("terminal mode restoration", |screen| {
            !screen.alternate_screen()
                && !screen.bracketed_paste()
                && !screen.hide_cursor()
                && screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None
                && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Default
        });
        assert_eq!(
            terminal_flags(self.master.as_ref()),
            self.original_flags,
            "terminal flags were not restored\n{}",
            self.diagnostics()
        );
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.writer.take();
        self.slave.take();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

fn streaming_delta(stream: &mut SseReply, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(chat_chunk(json!({"role":"assistant","content":text}), None));
    } else {
        stream.event(json!({
            "type":"response.output_text.delta", "sequence_number":0,
            "item_id":"msg-live", "output_index":0, "content_index":0,
            "delta":text, "logprobs":[]
        }));
    }
}

fn streaming_complete(stream: &mut SseReply, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(chat_chunk(json!({}), Some("stop")));
        stream.frame("[DONE]");
    } else {
        stream.event(json!({
            "type":"response.completed", "sequence_number":1,
            "response":response_body(json!([
                response_message("msg-live", json!([response_text(text)]))
            ]))
        }));
    }
}

fn label_color(screen: &vt100::Screen, label: &str) -> vt100::Color {
    for row in 0..30 {
        let line: String = (0..100)
            .filter_map(|col| screen.cell(row, col))
            .map(|cell| cell.contents())
            .collect();
        if line.contains(label) {
            return screen.cell(row, 2).unwrap().fgcolor();
        }
    }
    panic!("missing label {label}: {}", screen.contents());
}

fn tui_session_is_locked(home: &Path, id: &str) -> bool {
    let Ok(file) = std::fs::File::open(home.join(".pp/run/sessions").join(format!("{id}.lock")))
    else {
        return false;
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => false,
        Err(error) if error == rustix::io::Errno::WOULDBLOCK => true,
        Err(error) => panic!("检查会话锁失败：{error}"),
    }
}

fn notification_bounds(
    screen: &vt100::Screen,
    title: &str,
    color: vt100::Color,
) -> Option<(u16, u16, u16, u16)> {
    let (rows, cols) = screen.size();
    let right = cols.checked_sub(1)?;
    let corner = |row, col, symbol| {
        screen
            .cell(row, col)
            .is_some_and(|cell| cell.contents() == symbol && cell.fgcolor() == color)
    };
    for top in 0..rows {
        if !corner(top, 0, "╭") || !corner(top, right, "╮") {
            continue;
        }
        let heading: String = (0..cols)
            .filter_map(|col| screen.cell(top, col))
            .map(|cell| cell.contents())
            .collect();
        if !heading.replace(' ', "").contains(title) {
            continue;
        }
        for bottom in top + 1..rows {
            if corner(bottom, 0, "╰")
                && corner(bottom, right, "╯")
                && (top + 1..bottom).all(|row| corner(row, 0, "│") && corner(row, right, "│"))
            {
                return Some((top, 0, bottom, right));
            }
        }
    }
    None
}

const USER_BACKGROUND: vt100::Color = vt100::Color::Rgb(48, 42, 63);
const ASSISTANT_BACKGROUND: vt100::Color = vt100::Color::Rgb(35, 39, 50);

fn background_color(screen: &vt100::Screen, text: &str) -> vt100::Color {
    for row in 0..100 {
        let line: String = (0..150)
            .filter_map(|col| screen.cell(row, col))
            .map(|cell| cell.contents())
            .collect();
        if line.contains(text) {
            return screen.cell(row, 0).unwrap().bgcolor();
        }
    }
    panic!("missing text {text}: {}", screen.contents());
}

fn tip_columns(screen: &vt100::Screen, row: u16, columns: std::ops::Range<u16>) -> String {
    columns
        .map(|col| {
            let contents = screen.cell(row, col).unwrap().contents();
            if contents.is_empty() { " " } else { contents }
        })
        .collect()
}

fn assert_colored_tip_key(
    screen: &vt100::Screen,
    row: u16,
    columns: std::ops::Range<u16>,
    key: &str,
    accent: vt100::Color,
    key_field: u16,
) {
    for (offset, character) in key.chars().enumerate() {
        let col = columns.start + u16::try_from(offset).unwrap();
        let cell = screen.cell(row, col).unwrap();
        assert_eq!(
            cell.contents(),
            character.to_string(),
            "row {row}, col {col}"
        );
        assert_eq!(cell.fgcolor(), accent, "row {row}, col {col}");
        assert!(cell.bold(), "row {row}, col {col}");
        assert_eq!(cell.bgcolor(), vt100::Color::Default);
    }
    assert_eq!(
        tip_columns(screen, row, columns.start..columns.start + key_field),
        format!("{key:<width$}", width = usize::from(key_field)),
    );
    let description_start = columns.start + key_field;
    assert!(
        !screen
            .cell(row, description_start)
            .unwrap()
            .contents()
            .trim()
            .is_empty(),
        "description must start after the {key_field}-column key region",
    );
    for col in description_start..columns.end {
        let cell = screen.cell(row, col).unwrap();
        if !cell.contents().trim().is_empty() {
            assert_eq!(cell.fgcolor(), vt100::Color::Idx(7), "row {row}, col {col}");
            assert!(!cell.bold(), "row {row}, col {col}");
        }
    }
}

fn api_path(protocol: &str) -> &'static str {
    if protocol == "completion" {
        "/v1/chat/completions"
    } else {
        "/v1/responses"
    }
}

fn saved_session_id(home: &Path) -> String {
    let dir = home.join(".pp/agent/sessions");
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".jsonl"))
        .map(|name| name.trim_end_matches(".jsonl").to_owned())
        .collect();
    ids.sort();
    assert_eq!(
        ids.len(),
        1,
        "expected exactly one saved session in {dir:?}"
    );
    ids.pop().unwrap()
}

// 驱动一个带 bash+read 工具批次与会话，退出后返回保存的 session ID。
fn create_tool_session(server: &Server, home: &Path, protocol: &str) -> String {
    std::fs::write(config_path(home, "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    let path = api_path(protocol);
    let mut pty = Pty::start_with_existing_files(home);
    pty.ready();
    pty.input("remember tui-marker\r");
    let (request, _) = receive_at(server, path);
    let command = "printf 'tool-out' > tui-read.txt";
    if protocol == "completion" {
        calls_reply(
            request,
            json!([
                call("b1", "bash", json!({"command":command})),
                call("r1", "read", json!({"path":"tui-read.txt"})),
            ]),
        );
    } else {
        response_reply(
            request,
            json!([
                response_call("b1", "bash", json!({"command":command})),
                response_call("r1", "read", json!({"path":"tui-read.txt"})),
            ]),
        );
    }
    let (request, _) = receive_at(server, path);
    if protocol == "completion" {
        final_reply(request, "tui-remembered");
    } else {
        response_final(request, "tui-remembered");
    }
    pty.text("tui-remembered");
    pty.ready();
    // 会话标识不再产生常规提醒。
    assert!(!pty.parser.screen().contents().contains("Session:"));
    pty.input("/quit\r");
    pty.wait_for("会话提示不进入退出后的终端历史", |screen| {
        !screen.alternate_screen()
            && screen.contents().contains("tui-remembered")
            && !screen.contents().contains("Session:")
    });
    pty.finish();
    no_more(server);
    saved_session_id(home)
}

#[path = "tui/file_command_cases.rs"]
mod file_command_cases;
#[path = "tui/file_picker_cases.rs"]
mod file_picker_cases;
#[path = "tui/input_editing_cases.rs"]
mod input_editing_cases;
#[path = "tui/input_effort_cases.rs"]
mod input_effort_cases;
#[path = "tui/model_picker_cases.rs"]
mod model_picker_cases;
#[path = "tui/model_switch_cases.rs"]
mod model_switch_cases;
#[path = "tui/plan_compaction_cases.rs"]
mod plan_compaction_cases;
#[path = "tui/plan_execution_cases.rs"]
mod plan_execution_cases;
#[path = "tui/plan_lifecycle_cases.rs"]
mod plan_lifecycle_cases;
#[path = "tui/plan_mode_cases.rs"]
mod plan_mode_cases;
#[path = "tui/plan_validation_cases.rs"]
mod plan_validation_cases;
#[path = "tui/session_cases.rs"]
mod session_cases;
#[path = "tui/skill_cases.rs"]
mod skill_cases;
#[path = "tui/startup_cases.rs"]
mod startup_cases;
#[path = "tui/streaming_cases.rs"]
mod streaming_cases;
#[path = "tui/terminal_cleanup_cases.rs"]
mod terminal_cleanup_cases;
#[path = "tui/transcript_cases.rs"]
mod transcript_cases;
