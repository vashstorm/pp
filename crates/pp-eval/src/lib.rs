//! Persistent, stateful Python and JavaScript REPL kernel host and runtime probes.

mod js_runtime;
mod kernel;
mod runtime;

pub use js_runtime::{JsRuntime, JsRuntimeKind, probe_js};
pub use kernel::{CapturedImage, CellOutcome, CellRequest, Language};
pub use runtime::{PythonRuntime, filter_env, probe_python};

use anyhow::Result;
use kernel::KernelState;
use pp_core::cancellation::{CancellationToken, merge_cleanup_result};
use pp_core::events::AgentEvent;
use pp_workspace::output::OutputSink;
use serde_json::Value;
use std::sync::{Arc, Mutex, MutexGuard};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Persistent Python / JavaScript kernel controller owned by a single agent session; drops the child process group on Drop.
#[derive(Default)]
pub struct Session {
    python: Arc<Mutex<KernelState>>,
    javascript: Arc<Mutex<KernelState>>,
}

impl Session {
    pub fn new() -> Self {
        Self::default()
    }

    fn slot(&self, language: Language) -> &Arc<Mutex<KernelState>> {
        match language {
            Language::Python => &self.python,
            Language::JavaScript => &self.javascript,
        }
    }

    /// Stop and reap the current long-running kernels, keeping the session usable so the next call can start them again.
    pub fn stop(&self) -> Result<()> {
        let mut first_err: Result<()> = Ok(());
        for slot in [&self.python, &self.javascript] {
            let running = {
                let mut state = lock(slot);
                state.restarted_notice = false;
                state.running.take()
            };
            if let Some(kernel) = running {
                let res = kernel.shutdown();
                first_err = merge_cleanup_result(first_err, res, "stopping eval kernel");
            }
        }
        first_err
    }

    /// Execute a single Python / JavaScript cell; lazily start the matching kernel or rebuild it when `reset` is set.
    pub fn execute(
        &self,
        req: CellRequest<'_>,
        cancellation: &CancellationToken,
        sink: OutputSink,
        emit: &mut dyn FnMut(AgentEvent),
        bridge: &mut dyn FnMut(&str, &Value) -> Result<Value>,
    ) -> Result<CellOutcome> {
        let mut state = lock(self.slot(req.language));
        kernel::run_cell(&mut state, req, cancellation, sink, emit, bridge)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod js_cases;

#[cfg(test)]
mod integration {
    use super::*;
    use anyhow::bail;
    use base64::Engine as _;
    use pp_config::config::OutputSettings;
    use pp_workspace::output::Store;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::{Duration, Instant};

    pub(super) struct Harness {
        pub(super) work: tempfile::TempDir,
        pub(super) cache: tempfile::TempDir,
        pub(super) store: Store,
        pub(super) settings: OutputSettings,
        pub(super) session: Session,
    }

    impl Harness {
        pub(super) fn new(spill_threshold: usize) -> Self {
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

        #[allow(clippy::too_many_arguments)]
        pub(super) fn run_with_lang(
            &self,
            language: Language,
            interpreter: Option<&str>,
            code: &str,
            reset: bool,
            timeout_seconds: u64,
            cancellation: &CancellationToken,
            bridge: &mut dyn FnMut(&str, &Value) -> Result<Value>,
        ) -> CellOutcome {
            self.session
                .execute(
                    CellRequest {
                        language,
                        cwd: self.work.path(),
                        cache_dir: self.cache.path(),
                        interpreter,
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

        pub(super) fn run(&self, code: &str, reset: bool, timeout_seconds: u64) -> CellOutcome {
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
            self.run_with_lang(
                Language::Python,
                None,
                code,
                reset,
                timeout_seconds,
                cancellation,
                bridge,
            )
        }
    }

    #[test]
    fn python_env_state_isolation_and_duplex_bridge() {
        {
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
        {
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
        {
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

            // Confirm the embedded kernel is still healthy and can read later variables, and that the previous cell's raw fd output does not leak into the next cell.
            let next = h.run("eof", false, 10);
            assert_eq!(next.value["ok"], true);
            assert_eq!(next.value["output"], "True\n");
        }
        {
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
    }

    #[test]
    fn python_signals_timeouts_magic_images_and_spill() {
        {
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

            // The kernel survives the SIGINT soft interrupt, and the variable kept still exists.
            let after = h.run("kept + 1", false, 10);
            assert_eq!(after.value["ok"], true);
            assert_eq!(after.value["output"], "100\n");
        }
        {
            let h = Harness::new(65_536);

            let start = Instant::now();
            // Ignoring SIGINT simulates being stuck in a C extension or a blocking child process, forcing the kill escalation after timeout.
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
        {
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

            // Build a real 1x1 PNG and trigger capture through print(omp_image_dict).
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

            // Output beyond the spill threshold is automatically spilled to an artifact:// file.
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
                        language: Language::Python,
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

        // cwd write + bridge round trip + PP_SANDBOX all work (control FDs are not polluted)
        let ok = run_with_policy(
            "import os\nopen('ok.txt', 'w').write('hello')\nres = tool.read(path='ok.txt')\nprint(os.environ.get('PP_SANDBOX'), res['ok'])",
            Some(ws_policy.clone()),
        );
        assert_eq!(ok.value["ok"], true, "{}", ok.value);
        assert_eq!(ok.value["output"], format!("{backend} True\n"));

        // Writing a protected path and reading a credentials path are blocked and carry sandbox metadata
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

        // Switching the sandbox mode restarts the kernel automatically
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

        // Changing the policy stack under the same mode (e.g. allow_read / permission overlay) also restarts it
        sbx.set_permission_overlay(pp_sandbox::SandboxOverlay {
            read_only: vec![home.join("overlay_ro")],
            ..Default::default()
        });
        let overlay_policy = sbx.policy().unwrap().unwrap();
        let overlay_res = run_with_policy("print(2 + 2)", Some(overlay_policy));
        assert_eq!(overlay_res.value["ok"], true, "{}", overlay_res.value);
        assert!(
            overlay_res.value["notice"]
                .as_str()
                .unwrap_or_default()
                .contains("sandbox policy changed"),
            "{}",
            overlay_res.value
        );
    }

    #[test]
    fn python_idle_flood_bridge_budget_and_cleanup_seal() {
        {
            let h = Harness::new(65_536);

            // 1. A background thread writing fd 2 with no active cell must not self-deadlock through the loopback.
            let idle_err = h.run(
                r#"import os, threading, time
def bg():
    time.sleep(0.03)
    os.write(2, b'idle-fd2-note\n')
threading.Thread(target=bg, daemon=True).start()
print('before-idle')"#,
                false,
                10,
            );
            assert_eq!(idle_err.value["ok"], true, "{}", idle_err.value);
            std::thread::sleep(std::time::Duration::from_millis(120));
            let after_idle = h.run("print('after-idle')", false, 10);
            assert_eq!(after_idle.value["ok"], true, "{}", after_idle.value);
            assert_eq!(after_idle.value["output"], "after-idle\n");

            // 2. For a timeout that responds normally to SIGINT, timed_out is true and cancelled is false.
            let timed = h.run("import time\ntime.sleep(5)", false, 1);
            assert_eq!(timed.value["ok"], false, "{}", timed.value);
            assert_eq!(timed.value["timed_out"], true, "{}", timed.value);
            assert_eq!(timed.value["cancelled"], false, "{}", timed.value);

            // 3. A background coroutine sends > 512 frames to fill the channel, then reset; shutdown must not deadlock.
            let flood = h.run(
                r#"import asyncio
async def spam():
    await asyncio.sleep(0.02)
    for i in range(600):
        print(f'spam-{i}')
asyncio.get_running_loop().create_task(spam())
print('scheduled')"#,
                false,
                10,
            );
            assert_eq!(flood.value["ok"], true, "{}", flood.value);
            std::thread::sleep(std::time::Duration::from_millis(120));
            let reset_ok = h.run("print('reset-ok')", true, 10);
            assert_eq!(reset_ok.value["ok"], true, "{}", reset_ok.value);
            assert_eq!(reset_ok.value["output"], "reset-ok\n");
        }
        {
            let h = Harness::new(65_536);

            // The ~2.4s of wall clock spent in 4 host bridges must all be exempt from the compute budget;
            // the following pure Python sleep(3) then exceeds the 2s compute allowance and must be interrupted at the limit.
            let res = h.run_with(
                r#"
import time
for _ in range(4):
    tool.slow({})
time.sleep(3)
print('never')
"#,
                false,
                2,
                &CancellationToken::new(),
                &mut |name, _args| {
                    assert_eq!(name, "slow");
                    std::thread::sleep(Duration::from_millis(600));
                    Ok(json!({"ok": true}))
                },
            );
            assert_eq!(res.value["timed_out"], true, "{}", res.value);
            assert_eq!(res.value["ok"], false, "{}", res.value);
            assert_eq!(res.value["cancelled"], false, "{}", res.value);
            assert_eq!(res.value["kernel_restarted"], Value::Null, "{}", res.value);

            // Timeout takes the soft-interrupt path: the kernel survives and later cells keep running.
            let after = h.run("print('alive')", false, 10);
            assert_eq!(after.value["ok"], true, "{}", after.value);
            assert_eq!(after.value["output"], "alive\n");
        }
        {
            let h = Harness::new(64);

            // Inject one in-loop kernel reap failure: cleanup fails after the artifact exists,
            // but resources must still be sealed before the error surfaces (idempotent finish), without faking cell success.
            lock(&h.session.python).fail_next_shutdown_for_test();

            let result = h.session.execute(
            CellRequest {
                language: Language::Python,
                cwd: h.work.path(),
                cache_dir: h.cache.path(),
                interpreter: None,
                code: "print('A' * 200)\nimport signal, time\nsignal.signal(signal.SIGINT, signal.SIG_IGN)\ntime.sleep(30)",
                call_id: "call-cleanup",
                timeout_seconds: 1,
                reset: false,
                sandbox: None,
            },
            &CancellationToken::new(),
            OutputSink::new(h.settings, h.store.clone()),
            &mut |_| {},
            &mut |_, _| unreachable!(),
        );
            let err = result
                .err()
                .expect("cleanup failure must surface instead of fake success");
            assert!(
                err.downcast_ref::<pp_core::cancellation::CleanupError>()
                    .is_some(),
                "{err:#}"
            );
            assert!(format!("{err:#}").contains("injected for test"), "{err:#}");

            // The live capture has been detached and can be read back in full from disk: equivalent to asserting live_is_empty_for_test(),
            // and the capture metadata has been sealed and published (complete is true only once meta.json is published).
            let snapshot = h
                .store
                .snapshot("artifact://0", &CancellationToken::new())
                .unwrap();
            assert_eq!(
                snapshot.metadata["capturing"], false,
                "{}",
                snapshot.metadata
            );
            assert_eq!(snapshot.metadata["complete"], true, "{}", snapshot.metadata);
            let text = fs::read_to_string(snapshot.path).unwrap();
            assert_eq!(text, format!("{}\n", "A".repeat(200)));
        }
    }
}
