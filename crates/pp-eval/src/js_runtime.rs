//! JavaScript runtime discovery (Bun preferred, Node.js fallback) and lightweight probe cache.

use crate::runtime::{PROBE_TIMEOUT, expand_tilde, filter_env, is_executable_file};
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

const JS_PROBE_SCRIPT: &str = r#"
const vm = require("node:vm");
if (typeof vm.createContext !== "function" || typeof vm.runInContext !== "function") {
  process.exit(2);
}
if (typeof Bun !== "undefined") {
  process.stdout.write("bun\n");
  process.exit(0);
}
const major = parseInt(((process.versions && process.versions.node) || "").split(".")[0], 10);
if (!Number.isFinite(major) || major < 18) {
  process.exit(3);
}
process.stdout.write("node\n");
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JsRuntimeKind {
    Bun,
    Node,
}

impl JsRuntimeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bun => "bun",
            Self::Node => "node",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Bun => "Bun",
            Self::Node => "Node.js",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsRuntime {
    pub kind: JsRuntimeKind,
    pub executable: PathBuf,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct JsProbeCacheKey {
    cwd: Option<PathBuf>,
    runtime_override: Option<String>,
    path: Option<String>,
    bun_install: Option<String>,
    home: Option<String>,
}

fn js_probe_cache() -> &'static Mutex<HashMap<JsProbeCacheKey, JsRuntime>> {
    static CACHE: OnceLock<Mutex<HashMap<JsProbeCacheKey, JsRuntime>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn find_on_path(bin: &str, path_var: Option<&str>) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Some(path_var) = path_var else {
        return found;
    };
    for dir in env::split_paths(path_var) {
        let candidate = dir.join(bin);
        if is_executable_file(&candidate) {
            found.push(candidate);
        }
    }
    found
}

fn resolve_explicit_candidate(
    raw: &str,
    cwd: &Path,
    base_env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    let trimmed = raw.trim();
    let home = base_env.get("HOME").map(String::as_str);
    let expanded = expand_tilde(trimmed, home);
    if expanded.is_absolute() {
        return vec![expanded];
    }
    // A bare binary name (such as "node" / "bun") is looked up on PATH directly;
    // a name containing a path separator, or matching a local executable, is resolved against cwd.
    let has_separator = trimmed.contains('/') || trimmed.contains('\\');
    if !has_separator && !trimmed.starts_with('~') {
        let cwd_candidate = cwd.join(&expanded);
        if is_executable_file(&cwd_candidate) {
            return vec![cwd_candidate];
        }
        let path_matches = find_on_path(trimmed, base_env.get("PATH").map(String::as_str));
        if !path_matches.is_empty() {
            return path_matches;
        }
        return vec![cwd_candidate];
    }
    vec![cwd.join(expanded)]
}

fn collect_candidates(
    cwd: &Path,
    runtime_override: Option<&str>,
    base_env: &BTreeMap<String, String>,
) -> Vec<PathBuf> {
    if let Some(explicit) = runtime_override.map(str::trim).filter(|s| !s.is_empty()) {
        return resolve_explicit_candidate(explicit, cwd, base_env);
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut push_if_exec = |candidate: PathBuf| {
        if is_executable_file(&candidate) && seen.insert(candidate.clone()) {
            out.push(candidate);
        }
    };

    // 1. Bun first: $BUN_INSTALL/bin/bun → bun on PATH → $HOME/.bun/bin/bun
    if let Some(bun_install) = base_env.get("BUN_INSTALL").filter(|s| !s.is_empty()) {
        push_if_exec(PathBuf::from(bun_install).join("bin/bun"));
    }
    for candidate in find_on_path("bun", base_env.get("PATH").map(String::as_str)) {
        push_if_exec(candidate);
    }
    if let Some(home) = base_env.get("HOME").filter(|s| !s.is_empty()) {
        push_if_exec(PathBuf::from(home).join(".bun/bin/bun"));
    }

    // 2. Node.js fallback (>= 18): node on PATH
    for candidate in find_on_path("node", base_env.get("PATH").map(String::as_str)) {
        push_if_exec(candidate);
    }

    out
}

fn probe_candidate(
    executable: &Path,
    cwd: &Path,
    env_map: &BTreeMap<String, String>,
) -> Result<JsRuntimeKind> {
    let mut cmd = Command::new(executable);
    cmd.args(["-e", JS_PROBE_SCRIPT])
        .current_dir(cwd)
        .env_clear()
        .envs(env_map)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", executable.display()))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut poll = Duration::from_micros(250);
    loop {
        if let Some(_status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            if output.status.success() {
                let stdout = String::from_utf8_lossy(&output.stdout);
                return match stdout.trim() {
                    "bun" => Ok(JsRuntimeKind::Bun),
                    "node" => Ok(JsRuntimeKind::Node),
                    other => bail!(
                        "{} returned unexpected probe output: {other:?}",
                        executable.display()
                    ),
                };
            }
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            bail!(
                "{} exited with status {} (Bun or Node.js >= 18 with node:vm required){detail}",
                executable.display(),
                output.status
            );
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} probe timed out", executable.display());
        }
        thread::sleep(poll.min(deadline - now));
        poll = (poll * 2).min(Duration::from_millis(5));
    }
}

/// Probe for an available JavaScript runtime (Bun preferred, Node.js >= 18 fallback); successful results are cached per environment key.
pub fn probe_js(cwd: &Path, runtime_override: Option<&str>) -> Result<JsRuntime> {
    let base_env = filter_env(env::vars());
    probe_js_with_env(cwd, runtime_override, base_env, true)
}

pub(crate) fn probe_js_with_env(
    cwd: &Path,
    runtime_override: Option<&str>,
    base_env: BTreeMap<String, String>,
    use_cache: bool,
) -> Result<JsRuntime> {
    let trimmed_override = runtime_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let cwd_sensitive = trimmed_override.as_deref().is_some_and(|raw| {
        let home = base_env.get("HOME").map(String::as_str);
        !expand_tilde(raw, home).is_absolute()
    });
    let key = JsProbeCacheKey {
        cwd: cwd_sensitive.then(|| cwd.to_path_buf()),
        runtime_override: trimmed_override,
        path: base_env.get("PATH").cloned(),
        bun_install: base_env.get("BUN_INSTALL").cloned(),
        home: base_env.get("HOME").cloned(),
    };
    if use_cache
        && let Ok(guard) = js_probe_cache().lock()
        && let Some(cached) = guard.get(&key)
        && is_executable_file(&cached.executable)
    {
        return Ok(cached.clone());
    }

    let candidates = collect_candidates(cwd, runtime_override, &base_env);
    if candidates.is_empty() {
        bail!(
            "Bun or Node.js >= 18 executable not found on PATH; configure eval.js_runtime if needed"
        );
    }

    let mut failures = Vec::new();
    for executable in candidates {
        match probe_candidate(&executable, cwd, &base_env) {
            Ok(kind) => {
                let runtime = JsRuntime {
                    kind,
                    executable,
                    env: base_env,
                };
                if use_cache && let Ok(mut guard) = js_probe_cache().lock() {
                    guard.insert(key, runtime.clone());
                }
                return Ok(runtime);
            }
            Err(error) => {
                failures.push(format!("{error:#}"));
            }
        }
    }

    bail!(
        "No working Bun or Node.js >= 18 runtime found ({})",
        failures.join("; ")
    )
}
