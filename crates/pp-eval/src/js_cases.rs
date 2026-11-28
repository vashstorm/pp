mod integration {
    use super::super::*;
    use crate::js_runtime::probe_js_with_env;
    use anyhow::bail;
    use base64::Engine as _;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::super::integration::Harness;

    fn resolve_real_exec(bin: &PathBuf) -> PathBuf {
        let out = Command::new(bin)
            .args(["-e", "process.stdout.write(process.execPath)"])
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let p = PathBuf::from(String::from_utf8_lossy(&o.stdout).trim());
                if p.is_file() { p } else { bin.clone() }
            }
            _ => bin.clone(),
        }
    }

    fn available_js_runtimes() -> Vec<(JsRuntimeKind, String)> {
        let cwd = std::env::current_dir().unwrap();
        let mut out = Vec::new();
        for (kind, candidate) in [(JsRuntimeKind::Bun, "bun"), (JsRuntimeKind::Node, "node")] {
            if let Ok(rt) = probe_js(&cwd, Some(candidate))
                && rt.kind == kind
            {
                out.push((kind, rt.executable.to_string_lossy().into_owned()));
            }
        }
        out
    }

    fn write_exec_script(path: &std::path::Path, body: &str) {
        fs::write(path, body).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn js_probe_state_stdin_and_duplex_bridge() {
        {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path();
            let empty_home = cwd.join("empty-home");
            let bin_dir = cwd.join("bin");
            fs::create_dir_all(&empty_home).unwrap();
            fs::create_dir_all(&bin_dir).unwrap();

            // 1. With an empty PATH and empty HOME, the probe fails and reports a hint.
            let mut empty_env = BTreeMap::new();
            empty_env.insert("HOME".to_owned(), empty_home.to_string_lossy().into_owned());
            empty_env.insert("PATH".to_owned(), String::new());
            let err = probe_js_with_env(cwd, None, empty_env.clone(), false).unwrap_err();
            assert!(format!("{err:#}").contains("not found"), "{err:#}");

            let bun_rt = probe_js(cwd, Some("bun")).ok();
            let node_rt = probe_js(cwd, Some("node")).ok();

            if let (Some(bun), Some(node)) = (&bun_rt, &node_rt) {
                let real_bun = resolve_real_exec(&bun.executable);
                let real_node = resolve_real_exec(&node.executable);

                let link_dir = cwd.join("both-bin");
                fs::create_dir_all(&link_dir).unwrap();
                let node_only_dir = cwd.join("node-only");
                fs::create_dir_all(&node_only_dir).unwrap();
                symlink(&real_bun, link_dir.join("bun")).unwrap();
                symlink(&real_node, link_dir.join("node")).unwrap();
                symlink(&real_node, node_only_dir.join("node")).unwrap();

                // 2. When both bun and node exist (even with the node directory earlier on PATH), Bun wins by default.
                let mut both_env = BTreeMap::new();
                both_env.insert("HOME".to_owned(), empty_home.to_string_lossy().into_owned());
                both_env.insert(
                    "PATH".to_owned(),
                    format!("{}:{}", node_only_dir.display(), link_dir.display()),
                );
                let picked = probe_js_with_env(cwd, None, both_env.clone(), false).unwrap();
                assert_eq!(picked.kind, JsRuntimeKind::Bun);

                // 3. An explicit override="node" forces Node.js.
                let forced = probe_js_with_env(cwd, Some("node"), both_env, false).unwrap();
                assert_eq!(forced.kind, JsRuntimeKind::Node);

                // 4. With only node present, fall back to Node.js; a broken bun candidate is skipped automatically.
                let broken_bun_dir = cwd.join("broken-bun");
                fs::create_dir_all(&broken_bun_dir).unwrap();
                write_exec_script(&broken_bun_dir.join("bun"), "#!/bin/sh\nexit 1\n");
                let mut fallback_env = BTreeMap::new();
                fallback_env.insert("HOME".to_owned(), empty_home.to_string_lossy().into_owned());
                fallback_env.insert(
                    "PATH".to_owned(),
                    format!("{}:{}", broken_bun_dir.display(), node_only_dir.display()),
                );
                let fallback = probe_js_with_env(cwd, None, fallback_env, false).unwrap();
                assert_eq!(fallback.kind, JsRuntimeKind::Node);

                // 5. $BUN_INSTALL/bin/bun wins over node on PATH.
                let bun_install = cwd.join("bun-install");
                fs::create_dir_all(bun_install.join("bin")).unwrap();
                symlink(&real_bun, bun_install.join("bin/bun")).unwrap();
                let mut bun_install_env = BTreeMap::new();
                bun_install_env
                    .insert("HOME".to_owned(), empty_home.to_string_lossy().into_owned());
                bun_install_env.insert(
                    "PATH".to_owned(),
                    node_only_dir.to_string_lossy().into_owned(),
                );
                bun_install_env.insert(
                    "BUN_INSTALL".to_owned(),
                    bun_install.to_string_lossy().into_owned(),
                );
                let from_bun_install =
                    probe_js_with_env(cwd, None, bun_install_env, false).unwrap();
                assert_eq!(from_bun_install.kind, JsRuntimeKind::Bun);
            }

            if let Some(node) = &node_rt {
                let real_node = resolve_real_exec(&node.executable);
                let node_only = cwd.join("just-node");
                fs::create_dir_all(&node_only).unwrap();
                symlink(&real_node, node_only.join("node")).unwrap();
                let mut env = BTreeMap::new();
                env.insert("HOME".to_owned(), empty_home.to_string_lossy().into_owned());
                env.insert("PATH".to_owned(), node_only.to_string_lossy().into_owned());
                let picked = probe_js_with_env(cwd, None, env, false).unwrap();
                assert_eq!(picked.kind, JsRuntimeKind::Node);
            }
        }
        {
            for (kind, exe) in available_js_runtimes() {
                let h = Harness::new(65_536);

                let first = h.run_with_lang(
            Language::JavaScript,
            Some(&exe),
            "const x = 20;\nlet { a, b: c = 2 } = { a: 20 };\nawait new Promise(r => setTimeout(r, 15));\nx + a + c",
            false,
            10,
            &CancellationToken::new(),
            &mut |_, _| unreachable!(),
        );
                assert_eq!(first.value["ok"], true, "{}", first.value);
                assert_eq!(first.value["language"], "js");
                assert_eq!(first.value["runtime"], kind.as_str());
                assert_eq!(first.value["execution_count"], 1);
                assert_eq!(first.value["output"], "42\n");

                // Redeclaring const / class across cells is not an error, and the previous cell's variables are reused.
                let second = h.run_with_lang(
            Language::JavaScript,
            Some(&exe),
            "const x = 100;\nclass Box { constructor(v) { this.v = v; } }\nnew Box(x + a).v",
            false,
            10,
            &CancellationToken::new(),
            &mut |_, _| unreachable!(),
        );
                assert_eq!(second.value["ok"], true, "{}", second.value);
                assert_eq!(second.value["execution_count"], 2);
                assert_eq!(second.value["output"], "120\n");

                let reset = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "typeof x",
                    true,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(reset.value["ok"], true, "{}", reset.value);
                assert_eq!(reset.value["execution_count"], 1);
                assert_eq!(reset.value["output"], "'undefined'\n");
            }
        }
        {
            for (_kind, exe) in available_js_runtimes() {
                let h = Harness::new(65_536);

                let out = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    r#"
const fs = require("node:fs");
let eof = false;
try {
  const buf = Buffer.alloc(16);
  const n = fs.readSync(0, buf, 0, 16, null);
  eof = (n === 0);
} catch {
  eof = true;
}
fs.writeSync(1, "raw-fd-line\n");
console.log("eof=", eof);
globalThis.keptFlag = eof;
"#,
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(out.value["ok"], true, "{}", out.value);
                let text = out.value["output"].as_str().unwrap();
                assert!(text.contains("raw-fd-line"), "{text}");
                assert!(text.contains("eof= true"), "{text}");

                let next = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "keptFlag",
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(next.value["ok"], true, "{}", next.value);
                assert_eq!(next.value["output"], "true\n");
            }
        }
        {
            for (_kind, exe) in available_js_runtimes() {
                let h = Harness::new(65_536);
                let token = CancellationToken::new();

                let res = h.run_with_lang(
            Language::JavaScript,
            Some(&exe),
            r#"
const a = tool.read({ path: "alpha.txt" });
const b = await tool.rg({ pattern: "hello" });
let caught = null;
try {
  tool.write({ path: "bad.txt", content: "x" });
} catch (exc) {
  caught = exc instanceof ToolError ? exc.message : String(exc);
}
console.log(a.content, b.matches, caught);
"#,
            false,
            10,
            &token,
            &mut |name, args| match name {
                "read" => {
                    Ok(json!({"ok": true, "content": format!("file:{}", args["path"].as_str().unwrap())}))
                }
                "rg" => Ok(json!({"ok": true, "matches": 3})),
                "write" => Ok(json!({"ok": false, "error": "denied by policy"})),
                other => bail!("unexpected tool {other}"),
            },
        );
                assert_eq!(res.value["ok"], true, "{}", res.value);
                assert_eq!(res.value["output"], "file:alpha.txt 3 denied by policy\n");
            }
        }
    }

    #[test]
    fn js_signals_timeout_images_and_spill() {
        {
            for (_kind, exe) in available_js_runtimes() {
                let h = Harness::new(65_536);

                let setup = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "const kept = 99;",
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(setup.value["ok"], true, "{}", setup.value);

                for code in [
                    "while (true) {}",
                    "await new Promise(resolve => setTimeout(resolve, 10000))",
                ] {
                    let cancel = CancellationToken::new();
                    let cancel_clone = cancel.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(120));
                        cancel_clone.cancel();
                    });

                    let interrupted = h.run_with_lang(
                        Language::JavaScript,
                        Some(&exe),
                        code,
                        false,
                        10,
                        &cancel,
                        &mut |_, _| unreachable!(),
                    );
                    assert_eq!(interrupted.value["ok"], false, "{}", interrupted.value);
                    assert_eq!(
                        interrupted.value["cancelled"], true,
                        "{}",
                        interrupted.value
                    );
                }

                let after = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "kept + 1",
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(after.value["ok"], true, "{}", after.value);
                assert_eq!(after.value["output"], "100\n");
            }
        }
        {
            for (_kind, exe) in available_js_runtimes() {
                let h = Harness::new(65_536);

                let start = Instant::now();
                // A synchronous child-process sleep blocks the JS main thread so breakOnSigint cannot be handled, forcing the kill escalation.
                let timed = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    r#"require("node:child_process").spawnSync("sleep", ["10"])"#,
                    false,
                    1,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert!(start.elapsed() < Duration::from_secs(7));
                assert_eq!(timed.value["ok"], false, "{}", timed.value);
                assert_eq!(timed.value["timed_out"], true, "{}", timed.value);
                assert_eq!(timed.value["kernel_restarted"], true, "{}", timed.value);

                let recovered = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "1 + 2",
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(recovered.value["ok"], true, "{}", recovered.value);
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
        }
        {
            let mut png = Vec::new();
            image::DynamicImage::new_rgba8(2, 2)
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .unwrap();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&png);

            for (_kind, exe) in available_js_runtimes() {
                let h = Harness::new(128);

                let img_res = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    &format!(
                        "console.log({{ type: 'image', mimeType: 'image/png', data: '{b64}' }})"
                    ),
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(img_res.value["ok"], true, "{}", img_res.value);
                assert_eq!(img_res.images.len(), 1);
                assert_eq!(img_res.images[0].bytes, png);

                let spill = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "console.log('A'.repeat(512))",
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(spill.value["ok"], true, "{}", spill.value);
                assert_eq!(spill.value["truncated"], true, "{}", spill.value);
                assert!(
                    spill.value["artifact"]["uri"]
                        .as_str()
                        .unwrap()
                        .starts_with("artifact://")
                );
            }
        }
    }

    #[test]
    fn js_bridge_budget_and_python_js_isolation() {
        {
            for (_kind, exe) in available_js_runtimes() {
                let h = Harness::new(65_536);

                let res = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    r#"
for (let i = 0; i < 4; i++) {
  tool.slow({});
}
while (true) {}
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

                let after = h.run_with_lang(
                    Language::JavaScript,
                    Some(&exe),
                    "console.log('alive')",
                    false,
                    10,
                    &CancellationToken::new(),
                    &mut |_, _| unreachable!(),
                );
                assert_eq!(after.value["ok"], true, "{}", after.value);
                assert_eq!(after.value["output"], "alive\n");
            }
        }
        {
            let cwd = std::env::current_dir().unwrap();
            if probe_python(&cwd, None).is_err() || available_js_runtimes().is_empty() {
                return;
            }
            let h = Harness::new(65_536);

            let py1 = h.run("shared_val = 10\nshared_val", false, 10);
            assert_eq!(py1.value["output"], "10\n");

            let js1 = h.run_with_lang(
                Language::JavaScript,
                None,
                "const shared_val = 20; shared_val",
                false,
                10,
                &CancellationToken::new(),
                &mut |_, _| unreachable!(),
            );
            assert_eq!(js1.value["output"], "20\n");

            // Only the JS kernel is reset; Python kernel variables are unaffected.
            let js_reset = h.run_with_lang(
                Language::JavaScript,
                None,
                "typeof shared_val",
                true,
                10,
                &CancellationToken::new(),
                &mut |_, _| unreachable!(),
            );
            assert_eq!(js_reset.value["output"], "'undefined'\n");

            let py2 = h.run("shared_val + 5", false, 10);
            assert_eq!(py2.value["output"], "15\n");
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn sandbox_js_eval_enforces_fs_and_restarts_on_mode_change() {
        for (_kind, exe) in available_js_runtimes() {
            let h = Harness::new(65_536);
            let work_canon = fs::canonicalize(h.work.path()).unwrap();
            if pp_sandbox::Sandbox::new(pp_config::config::SandboxSettings::default(), &work_canon)
                .status()
                .effective
                == pp_sandbox::EffectiveMode::Off
            {
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
                Some(PathBuf::from("/tmp").as_path()),
                pp_sandbox::ProbeResult::Available,
            );
            let ws_policy = sbx.policy().unwrap().unwrap();
            let backend = ws_policy.backend();

            let run_with_policy = |code: &str, policy: Option<Arc<pp_sandbox::Policy>>| {
                h.session
                    .execute(
                        CellRequest {
                            language: Language::JavaScript,
                            cwd: &work_canon,
                            cache_dir: h.cache.path(),
                            interpreter: Some(&exe),
                            code,
                            call_id: "eval-js-sbx",
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

            let ok = run_with_policy(
                r#"
const fs = require("node:fs");
fs.writeFileSync("ok.txt", "hello");
const res = tool.read({ path: "ok.txt" });
console.log(process.env.PP_SANDBOX, res.ok);
"#,
                Some(ws_policy.clone()),
            );
            assert_eq!(ok.value["ok"], true, "{}", ok.value);
            assert_eq!(ok.value["output"], format!("{backend} true\n"));

            let blocked_write = run_with_policy(
                r#"require("node:fs").writeFileSync(".pp/hooks.yml", "bad")"#,
                Some(ws_policy.clone()),
            );
            assert_eq!(blocked_write.value["ok"], false, "{}", blocked_write.value);
            assert_eq!(
                blocked_write.value["sandbox"]["blocked"], true,
                "{}",
                blocked_write.value
            );

            let blocked_read = run_with_policy(
                &format!(
                    "require('node:fs').readFileSync({})",
                    serde_json::to_string(&home.join(".ssh/id_ed25519")).unwrap()
                ),
                Some(ws_policy),
            );
            assert_eq!(blocked_read.value["ok"], false, "{}", blocked_read.value);
            assert_eq!(
                blocked_read.value["sandbox"]["blocked"], true,
                "{}",
                blocked_read.value
            );

            sbx.set_mode_override(Some(pp_config::config::SandboxMode::ReadOnly));
            let ro_policy = sbx.policy().unwrap().unwrap();
            let ro_res = run_with_policy("console.log(1 + 1)", Some(ro_policy));
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
}
