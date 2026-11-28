// `!command` 行模式回归:本地执行不触碰模型上下文与持久化、双管道流式输出、
// 空命令/失败/超时/工作目录边界,以及 `-p` 与无配置场景绝不执行 bang 输入。
// 复用父模块回环 fixture;等待只用有界轮询、输出与文件栅栏。

use super::streaming::{LiveCli, Pipe};
use super::*;

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum Protocol {
    Chat,
    Responses,
}

// 只有本地 shell 会产生的字样:任何协议的模型请求与会话记录都不得包含。
const FORBIDDEN_MARKERS: [&str; 5] = [
    "out-first",
    "err-first",
    "out-last",
    "local-shell-cli",
    "[shell",
];

impl Protocol {
    fn models(self, server: &TestServer) -> String {
        match self {
            Self::Chat => fixture_models(server),
            Self::Responses => response_models(server, Some("response")),
        }
    }

    fn start(self, server: &TestServer, cwd: &std::path::Path) -> LiveCli {
        LiveCli::new(Cli::start_with(
            cwd,
            None,
            fixture_settings(),
            &self.models(server),
        ))
    }

    fn receive(self, server: &TestServer) -> (CapturedRequest, Value) {
        receive_at(
            server,
            match self {
                Self::Chat => "/v1/chat/completions",
                Self::Responses => "/v1/responses",
            },
        )
    }

    fn final_reply(self, request: CapturedRequest, text: &str) {
        match self {
            Self::Chat => final_reply(request, text),
            Self::Responses => response_final(request, text),
        }
    }

    // 第一条模型任务必须只携带键入的 prompt:没有工具 exchange,也没有任何
    // shell 命令文本或输出混入原生 messages/input。
    fn assert_isolated_prompt(self, body: &Value, prompt: &str) {
        match self {
            Self::Chat => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 2, "chat history polluted: {body}");
                assert_eq!(messages[0]["role"], "system");
                assert_eq!(messages[1]["role"], "user");
                assert_eq!(messages[1]["content"], prompt);
            }
            Self::Responses => {
                let input = body["input"].as_array().unwrap();
                assert_eq!(input.len(), 1, "response input polluted: {body}");
                response_user(&input[0], prompt);
            }
        }
        let wire = body.to_string();
        for marker in FORBIDDEN_MARKERS {
            assert!(
                !wire.contains(marker),
                "native request leaked {marker}: {body}"
            );
        }
    }
}

fn sessions_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

fn session_files(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(sessions_dir(home))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        })
        .unwrap_or_default()
}

fn assert_no_session(home: &std::path::Path) {
    let files = session_files(home);
    assert!(files.is_empty(), "shell-only 交互创建了会话:{files:?}");
}

// 有界轮询直到模型任务把 expected 写进会话 JSONL:文件创建与内容落盘都以
// 可观察结果为栅栏,不用固定 sleep。
fn wait_for_session_record(home: &std::path::Path, expected: &str) -> String {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        let files = session_files(home);
        assert!(files.len() <= 1, "shell 与模型任务各自建了会话:{files:?}");
        if files.len() == 1
            && let Ok(text) = std::fs::read_to_string(&files[0])
            && text.contains(expected)
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "会话始终未记录 {expected:?}:{:?}",
            session_files(home)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_no_shell_leak(text: &str, scope: &str) {
    for marker in FORBIDDEN_MARKERS {
        assert!(
            !text.contains(marker),
            "{scope} 泄漏了本地 shell 内容 {marker}:{text}"
        );
    }
}

// shell-only 交互不产生 Session 通知,也不出现上下文/用量行。
fn assert_no_task_notifications(stderr: &str) {
    for line in ["Session: ", "Context: "] {
        assert!(
            !stderr.contains(line),
            "shell-only 交互产生了 {line:?} 行:{stderr}"
        );
    }
}

// 命令仍卡在 release 栅栏前:两流已各自可见,受控结束状态与被门控的输出都
// 尚未出现,stdout 保持为空。
fn assert_shell_gated(cli: &LiveCli) {
    let stderr = cli.text(Pipe::Stderr);
    assert!(
        !stderr.contains("out-last"),
        "release 未创建就输出了 out-last:{stderr}"
    );
    for status in [
        "shell: exit",
        "shell: failed",
        "shell: cancelled",
        "shell: timed out",
    ] {
        assert!(!stderr.contains(status), "shell 提前结束:{stderr}");
    }
    assert!(cli.text(Pipe::Stdout).is_empty(), "shell 内容写入了 stdout");
    assert_no_task_notifications(&stderr);
}

#[test]
fn bang_shell_cli_is_local_and_streams() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = protocol.start(&server, dir.path());
    assert_no_session(dir.path());
    cli.input("!printf out-first; printf err-first >&2; while [[ ! -e release ]]; do sleep 0.01; done; printf out-last\n");
    // 主输出是 stdout/stderr 合并的有界预览：同一批次内两路字节的相对顺序
    // 由 reader 竞争决定，因此这里只要求两段内容都及时出现。
    cli.wait_for(Pipe::Stderr, "out-first");
    cli.wait_for(Pipe::Stderr, "err-first");
    // 栅栏仍未释放:给“提前完成”留出一个有界观察窗口。
    thread::sleep(Duration::from_millis(200));
    assert_shell_gated(&cli);
    assert!(
        server.try_recv().unwrap().is_none(),
        "shell 执行期间发出了模型请求"
    );
    assert_no_session(dir.path());

    std::fs::write(dir.path().join("release"), b"").unwrap();
    cli.wait_for(Pipe::Stderr, "[shell output] out-last");
    cli.wait_for(Pipe::Stderr, "shell: exit 0");
    let finished = cli.text(Pipe::Stderr);
    assert_no_task_notifications(&finished);
    assert!(
        server.try_recv().unwrap().is_none(),
        "shell 结束触发了模型请求"
    );
    assert_no_session(dir.path());
    assert!(
        cli.text(Pipe::Stdout).is_empty(),
        "shell-only 期间的 stdout 不为空"
    );

    cli.input("ordinary task\n");
    let (request, body) = protocol.receive(&server);
    protocol.assert_isolated_prompt(&body, "ordinary task");
    protocol.final_reply(request, "ordinary-complete");
    cli.wait_for(Pipe::Stdout, "ordinary-complete");
    let recorded = wait_for_session_record(dir.path(), "ordinary task");
    assert_no_shell_leak(&recorded, "会话记录");

    cli.input("/quit\n");
    let output = cli.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{protocol:?}: {stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "ordinary-complete\n",
        "{protocol:?}"
    );
    for (marker, count) in [
        ("out-first", 1),
        ("err-first", 1),
        ("out-last", 1),
        ("shell: running", 1),
        ("shell: exit 0", 1),
    ] {
        assert_eq!(
            stderr.matches(marker).count(),
            count,
            "{protocol:?}: {stderr}"
        );
    }
    let files = session_files(dir.path());
    assert_eq!(files.len(), 1, "{protocol:?}: {files:?}");
    let saved = std::fs::read_to_string(&files[0]).unwrap();
    assert!(saved.contains("ordinary task"), "{protocol:?}: {saved}");
    assert!(saved.contains("ordinary-complete"), "{protocol:?}: {saved}");
    assert_no_shell_leak(&saved, "会话记录");
    no_more(&server);
}
