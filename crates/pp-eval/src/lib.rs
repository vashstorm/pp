//! 持久化、有状态 Python REPL 内核宿主与运行期探针。

mod kernel;
mod runtime;

pub use kernel::{CapturedImage, CellOutcome, CellRequest};
pub use runtime::{PythonRuntime, filter_env, probe_python};

use anyhow::Result;
use kernel::KernelState;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_workspace::output::OutputSink;
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 单个 Agent 会话独占的持久 Python 内核控制器；Drop 时自动回收子进程组。
#[derive(Default)]
pub struct Session {
    state: Arc<Mutex<KernelState>>,
}

/// 独立清理句柄，供外层（如任务收尾或会话切换）在必要时触发回收。
#[derive(Clone)]
pub struct EvalCleanup {
    state: Arc<Mutex<KernelState>>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cleanup(&self) -> EvalCleanup {
        EvalCleanup {
            state: Arc::clone(&self.state),
        }
    }

    /// 停止并回收当前常驻内核，保留会话对象可供下次调用重新拉起。
    pub fn stop(&self) -> Result<()> {
        let running = {
            let mut state = lock(&self.state);
            state.restarted_notice = false;
            state.running.take()
        };
        if let Some(kernel) = running {
            kernel.shutdown()?;
        }
        Ok(())
    }

    /// 执行单个 Python 单元格；需要时惰性启动或按 `reset` 重建内核。
    pub fn execute(
        &self,
        req: CellRequest<'_>,
        cancellation: &CancellationToken,
        sink: OutputSink,
        emit: &mut dyn FnMut(AgentEvent),
        bridge: &mut dyn FnMut(&str, &Value) -> Result<Value>,
    ) -> Result<CellOutcome> {
        let mut state = lock(&self.state);
        kernel::run_cell(&mut state, req, cancellation, sink, emit, bridge)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

impl EvalCleanup {
    pub fn shutdown(&self) -> Result<()> {
        let running = {
            let mut state = lock(&self.state);
            state.closed = true;
            state.running.take()
        };
        if let Some(kernel) = running {
            kernel.shutdown()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use base64::Engine as _;
    use pp_config::config::OutputSettings;
    use pp_workspace::output::Store;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::{Duration, Instant};

    struct Harness {
        work: tempfile::TempDir,
        cache: tempfile::TempDir,
        store: Store,
        settings: OutputSettings,
        session: Session,
    }

    impl Harness {
        fn new(spill_threshold: usize) -> Self {
            let work = tempfile::tempdir().unwrap();
            let cache = tempfile::tempdir().unwrap();
            let store = Store::default();
            store.bind(cache.path(), "eval-test").unwrap();
            let settings = OutputSettings {
                artifact_spill_threshold: spill_threshold,
                artifact_head_bytes: 256,
                output_max_columns: 768,
                artifact_max_bytes: 1024 * 1024,
            };
            Self {
                work,
                cache,
                store,
                settings,
                session: Session::new(),
            }
        }

        fn run(&self, code: &str, reset: bool, timeout_seconds: u64) -> CellOutcome {
            self.run_with(
                code,
                reset,
                timeout_seconds,
                &CancellationToken::new(),
                &mut |_, _| unreachable!(),
            )
        }

        fn run_with(
            &self,
            code: &str,
            reset: bool,
            timeout_seconds: u64,
            cancellation: &CancellationToken,
            bridge: &mut dyn FnMut(&str, &Value) -> Result<Value>,
        ) -> CellOutcome {
            self.session
                .execute(
                    CellRequest {
                        cwd: self.work.path(),
                        cache_dir: self.cache.path(),
                        interpreter: None,
                        code,
                        call_id: "call-1",
                        timeout_seconds,
                        reset,
                        sandbox: None,
                    },
                    cancellation,
                    OutputSink::new(self.settings, self.store.clone()),
                    &mut |_| {},
                    bridge,
                )
                .unwrap()
        }
    }

    #[test]
    fn filter_env_strips_provider_secrets() {
        let mut vars = BTreeMap::new();
        vars.insert("PATH", "/usr/bin");
        vars.insert("HOME", "/tmp/home");
        vars.insert("LC_ALL", "C.UTF-8");
        vars.insert("OPENAI_API_KEY", "sk-secret");
        vars.insert("ANTHROPIC_API_KEY", "sk-ant-secret");
        vars.insert("GEMINI_API_KEY", "gm-secret");
        let filtered = filter_env(vars);
        assert_eq!(filtered.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(filtered.get("LC_ALL").map(String::as_str), Some("C.UTF-8"));
        assert!(!filtered.contains_key("OPENAI_API_KEY"));
        assert!(!filtered.contains_key("ANTHROPIC_API_KEY"));
        assert!(!filtered.contains_key("GEMINI_API_KEY"));
    }

    #[test]
    fn persistent_state_top_level_await_echo_and_reset() {
        let h = Harness::new(65_536);

        let first = h.run(
            "import asyncio\nx = 20\nawait asyncio.sleep(0.01)\nx + 22",
            false,
            10,
        );
        assert_eq!(first.value["ok"], true);
        assert_eq!(first.value["execution_count"], 1);
        assert_eq!(first.value["output"], "42\n");

        let second = h.run("x * 2", false, 10);
        assert_eq!(second.value["ok"], true);
        assert_eq!(second.value["execution_count"], 2);
        assert_eq!(second.value["output"], "40\n");

        let reset = h.run("x", true, 10);
        assert_eq!(reset.value["ok"], false);
        assert!(
            reset.value["output"]
                .as_str()
                .unwrap()
                .contains("NameError")
        );
    }

    #[test]
    fn stdin_is_devnull_and_raw_fd_stdout_does_not_corrupt_frames() {
        let h = Harness::new(65_536);

        let out = h.run(
            "import os, sys\ntry:\n    input('prompt: ')\n    eof = False\nexcept EOFError:\n    eof = True\nos.write(1, b'raw-fd-line\\n')\nprint('eof=', eof)",
            false,
            10,
        );
        assert_eq!(out.value["ok"], true);
        let text = out.value["output"].as_str().unwrap();
        assert!(text.contains("raw-fd-line"), "{text}");
        assert!(text.contains("eof= True"), "{text}");

        // 确认内嵌内核仍健康可读后续变量，且上一个 cell 的 raw fd 输出不会泄漏到下一个 cell。
        let next = h.run("eof", false, 10);
        assert_eq!(next.value["ok"], true);
        assert_eq!(next.value["output"], "True\n");
    }

    #[test]
    fn duplex_tool_bridge_supports_sync_and_await_and_errors() {
        let h = Harness::new(65_536);
        let token = CancellationToken::new();

        let res = h.run_with(
            r#"
a = tool.read(path="alpha.txt")
b = await tool.rg({"pattern": "hello"})
try:
    tool.write(path="bad.txt", content="x")
    caught = None
except ToolError as exc:
    caught = str(exc)
print(a["content"], b["matches"], caught)
"#,
            false,
            10,
            &token,
            &mut |name, args| match name {
                "read" => Ok(json!({"ok": true, "content": format!("file:{}", args["path"].as_str().unwrap())})),
                "rg" => Ok(json!({"ok": true, "matches": 3})),
                "write" => Ok(json!({"ok": false, "error": "denied by policy"})),
                other => bail!("unexpected tool {other}"),
            },
        );
        assert_eq!(res.value["ok"], true, "{}", res.value);
        assert_eq!(res.value["output"], "file:alpha.txt 3 denied by policy\n");
    }

    #[test]
    fn sigint_interrupts_loop_while_preserving_kernel_state() {
        let h = Harness::new(65_536);

        let setup = h.run("kept = 99", false, 10);
        assert_eq!(setup.value["ok"], true);

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel_clone.cancel();
        });

        let interrupted = h.run_with(
            "import time\nwhile True:\n    time.sleep(0.05)",
            false,
            10,
            &cancel,
            &mut |_, _| unreachable!(),
        );
        assert_eq!(interrupted.value["ok"], false);
        assert_eq!(interrupted.value["cancelled"], true);

        // 内核被 SIGINT 软中断后仍存活，变量 kept 仍然存在。
        let after = h.run("kept + 1", false, 10);
        assert_eq!(after.value["ok"], true);
        assert_eq!(after.value["output"], "100\n");
    }

    #[test]
    fn timeout_escalation_kills_uninterruptible_sleep_and_reports_restart() {
        let h = Harness::new(65_536);

        let start = Instant::now();
        // 屏蔽 SIGINT 模拟卡在 C 扩展或子进程阻塞，逼迫超时后升级强杀进程组。
        let timed = h.run(
            "import signal, time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\ntime.sleep(10)",
            false,
            1,
        );
        assert!(start.elapsed() < Duration::from_secs(7));
        assert_eq!(timed.value["ok"], false);
        assert_eq!(timed.value["timed_out"], true);
        assert_eq!(timed.value["kernel_restarted"], true);

        let recovered = h.run("1 + 2", false, 10);
        assert_eq!(recovered.value["ok"], true);
        assert_eq!(recovered.value["output"], "3\n");
        assert!(
            recovered.value["notice"]
                .as_str()
                .unwrap()
                .contains("Kernel restarted"),
            "{}",
            recovered.value
        );
    }

    #[test]
    fn load_magic_and_image_capture_and_artifact_spill() {
        let h = Harness::new(128);

        fs::write(
            h.work.path().join("helper.py"),
            "def greet():\n    return 'hi-from-load'\n",
        )
        .unwrap();
        let loaded = h.run("%load helper.py\ngreet()", false, 10);
        assert_eq!(loaded.value["ok"], true, "{}", loaded.value);
        assert!(
            loaded.value["output"]
                .as_str()
                .unwrap()
                .contains("hi-from-load")
        );

        // 构造真实 1x1 PNG 图片通过 print(omp_image_dict) 触发捕获。
        let mut png = Vec::new();
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let img_res = h.run(
            &format!("print({{'type':'image','mimeType':'image/png','data':'{b64}'}})"),
            false,
            10,
        );
        assert_eq!(img_res.value["ok"], true);
        assert_eq!(img_res.images.len(), 1);
        assert_eq!(img_res.images[0].bytes, png);

        // 输出超过 spill threshold 自动落盘 artifact://。
        let spill = h.run("print('A' * 512)", false, 10);
        assert_eq!(spill.value["ok"], true);
        assert_eq!(spill.value["truncated"], true);
        assert!(
            spill.value["artifact"]["uri"]
                .as_str()
                .unwrap()
                .starts_with("artifact://")
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn sandbox_eval_enforces_fs_preserves_control_fds_and_restarts_on_mode_change() {
        let h = Harness::new(65_536);
        let work_canon = fs::canonicalize(h.work.path()).unwrap();
        if pp_sandbox::Sandbox::new(pp_config::config::SandboxSettings::default(), &work_canon)
            .status()
            .effective
            == pp_sandbox::EffectiveMode::Off
        {
            eprintln!("skipping sandbox_eval test: platform sandbox unavailable");
            return;
        }
        fs::create_dir_all(work_canon.join(".pp")).unwrap();
        let home = work_canon.join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/id_ed25519"), b"SECRET").unwrap();

        let sbx = pp_sandbox::Sandbox::with_probe_for_test(
            pp_config::config::SandboxSettings {
                mode: pp_config::config::SandboxMode::WorkspaceWrite,
                ..Default::default()
            },
            &work_canon,
            Some(&home),
            Some(std::path::Path::new("/tmp")),
            pp_sandbox::ProbeResult::Available,
        );
        let ws_policy = sbx.policy().unwrap().unwrap();
        let backend = ws_policy.backend();

        let run_with_policy = |code: &str, policy: Option<Arc<pp_sandbox::Policy>>| {
            h.session
                .execute(
                    CellRequest {
                        cwd: &work_canon,
                        cache_dir: h.cache.path(),
                        interpreter: None,
                        code,
                        call_id: "eval-sbx",
                        timeout_seconds: 10,
                        reset: false,
                        sandbox: policy,
                    },
                    &CancellationToken::new(),
                    OutputSink::new(h.settings, h.store.clone()),
                    &mut |_| {},
                    &mut |name, args| Ok(json!({"ok": true, "echo": [name, args]})),
                )
                .unwrap()
        };

        // cwd 写入 + 桥接往返 + PP_SANDBOX 均正常（控制 FD 未污染）
        let ok = run_with_policy(
            "import os\nopen('ok.txt', 'w').write('hello')\nres = tool.read(path='ok.txt')\nprint(os.environ.get('PP_SANDBOX'), res['ok'])",
            Some(ws_policy.clone()),
        );
        assert_eq!(ok.value["ok"], true, "{}", ok.value);
        assert_eq!(ok.value["output"], format!("{backend} True\n"));

        // 写受保护路径与读凭据路径被拦截，并带 sandbox 元数据
        let blocked_write = run_with_policy(
            "open('.pp/hooks.yml', 'w').write('bad')",
            Some(ws_policy.clone()),
        );
        assert_eq!(blocked_write.value["ok"], false, "{}", blocked_write.value);
        assert_eq!(
            blocked_write.value["sandbox"]["blocked"], true,
            "{}",
            blocked_write.value
        );

        let blocked_read = run_with_policy(
            &format!("open({:?}).read()", home.join(".ssh/id_ed25519")),
            Some(ws_policy),
        );
        assert_eq!(blocked_read.value["ok"], false, "{}", blocked_read.value);
        assert_eq!(
            blocked_read.value["sandbox"]["blocked"], true,
            "{}",
            blocked_read.value
        );

        // 切换沙盒模式自动重启内核
        sbx.set_mode_override(Some(pp_config::config::SandboxMode::ReadOnly));
        let ro_policy = sbx.policy().unwrap().unwrap();
        let ro_res = run_with_policy("print(1 + 1)", Some(ro_policy));
        assert_eq!(ro_res.value["ok"], true, "{}", ro_res.value);
        assert!(
            ro_res.value["notice"]
                .as_str()
                .unwrap_or_default()
                .contains("sandbox mode changed"),
            "{}",
            ro_res.value
        );
    }
}
