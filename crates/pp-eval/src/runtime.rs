//! Python 解释器发现、环境变量清洗与轻量探测缓存。

use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

const ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "TMPDIR",
    "TEMP",
    "TMP",
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "VIRTUAL_ENV",
    "PYTHONPATH",
    "BUN_INSTALL",
    "NODE_PATH",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "UV_CACHE_DIR",
    "UV_PYTHON",
];

const ENV_ALLOW_PREFIXES: &[&str] = &["LC_", "XDG_", "PP_"];

const ENV_DENYLIST: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GOOGLE_API_KEY",
    "GEMINI_API_KEY",
    "OPENROUTER_API_KEY",
    "PERPLEXITY_API_KEY",
    "PERPLEXITY_COOKIES",
    "EXA_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "MISTRAL_API_KEY",
    "DEEPSEEK_API_KEY",
    "COHERE_API_KEY",
    "GROQ_API_KEY",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PythonRuntime {
    pub python_path: PathBuf,
    pub env: BTreeMap<String, String>,
    pub venv_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ProbeCacheKey {
    cwd: Option<PathBuf>,
    interpreter: Option<String>,
    path: Option<String>,
    virtual_env: Option<String>,
    conda_prefix: Option<String>,
    home: Option<String>,
}

fn probe_cache() -> &'static Mutex<HashMap<ProbeCacheKey, PythonRuntime>> {
    static CACHE: OnceLock<Mutex<HashMap<ProbeCacheKey, PythonRuntime>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 过滤环境变量，仅保留安全白名单并剥离敏感凭据。
pub fn filter_env<I, K, V>(vars: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: Into<String>,
{
    let mut out = BTreeMap::new();
    for (k, v) in vars {
        let key: String = k.into();
        let val: String = v.into();
        if ENV_DENYLIST.contains(&key.as_str()) {
            continue;
        }
        if ENV_ALLOWLIST.contains(&key.as_str())
            || ENV_ALLOW_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
        {
            out.insert(key, val);
        }
    }
    out
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}

fn apply_venv_env(
    base: &BTreeMap<String, String>,
    venv_path: &Path,
    bin_dir: &Path,
) -> BTreeMap<String, String> {
    let mut env = base.clone();
    env.insert(
        "VIRTUAL_ENV".to_owned(),
        venv_path.to_string_lossy().into_owned(),
    );
    let bin_str = bin_dir.to_string_lossy();
    let new_path = match env.get("PATH") {
        Some(existing) if !existing.is_empty() => format!("{bin_str}:{existing}"),
        _ => bin_str.into_owned(),
    };
    env.insert("PATH".to_owned(), new_path);
    env
}

fn detect_venv_for_python(python_path: &Path) -> Option<(PathBuf, PathBuf)> {
    let bin_dir = python_path.parent()?;
    let venv_dir = bin_dir.parent()?;
    if venv_dir.join("pyvenv.cfg").is_file() {
        Some((venv_dir.to_owned(), bin_dir.to_owned()))
    } else {
        None
    }
}

pub(crate) fn expand_tilde(raw: &str, home: Option<&str>) -> PathBuf {
    if raw == "~"
        && let Some(home) = home
    {
        return PathBuf::from(home);
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

fn resolve_explicit_runtime(
    interpreter: &str,
    cwd: &Path,
    base_env: &BTreeMap<String, String>,
) -> PythonRuntime {
    let home = base_env.get("HOME").map(String::as_str);
    let expanded = expand_tilde(interpreter.trim(), home);
    let python_path = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    if let Some((venv_path, bin_dir)) = detect_venv_for_python(&python_path) {
        let env = apply_venv_env(base_env, &venv_path, &bin_dir);
        PythonRuntime {
            python_path,
            env,
            venv_path: Some(venv_path),
        }
    } else {
        PythonRuntime {
            python_path,
            env: base_env.clone(),
            venv_path: None,
        }
    }
}

fn venv_candidate(
    venv_dir: &Path,
    base_env: &BTreeMap<String, String>,
    seen: &mut HashSet<PathBuf>,
) -> Option<PythonRuntime> {
    let bin_dir = venv_dir.join("bin");
    for name in ["python", "python3"] {
        let candidate = bin_dir.join(name);
        if is_executable_file(&candidate) && seen.insert(candidate.clone()) {
            return Some(PythonRuntime {
                python_path: candidate,
                env: apply_venv_env(base_env, venv_dir, &bin_dir),
                venv_path: Some(venv_dir.to_owned()),
            });
        }
    }
    None
}

fn try_candidates(
    cwd: &Path,
    interpreter: Option<&str>,
    base_env: &BTreeMap<String, String>,
    mut visit: impl FnMut(PythonRuntime) -> bool,
) -> bool {
    if let Some(explicit) = interpreter.map(str::trim).filter(|s| !s.is_empty()) {
        visit(resolve_explicit_runtime(explicit, cwd, base_env));
        return true;
    }

    let mut seen = HashSet::new();
    let mut found = false;

    let mut emit = |runtime: PythonRuntime| -> bool {
        found = true;
        visit(runtime)
    };

    if let Some(venv) = base_env.get("VIRTUAL_ENV").filter(|s| !s.is_empty())
        && let Some(candidate) = venv_candidate(Path::new(venv), base_env, &mut seen)
        && emit(candidate)
    {
        return true;
    }
    if let Some(conda) = base_env.get("CONDA_PREFIX").filter(|s| !s.is_empty())
        && let Some(candidate) = venv_candidate(Path::new(conda), base_env, &mut seen)
        && emit(candidate)
    {
        return true;
    }
    for local in [cwd.join(".venv"), cwd.join("venv")] {
        if local.is_dir()
            && let Some(candidate) = venv_candidate(&local, base_env, &mut seen)
            && emit(candidate)
        {
            return true;
        }
    }
    if let Some(home) = base_env.get("HOME").filter(|s| !s.is_empty()) {
        let managed = PathBuf::from(home).join(".pp/agent/python-env");
        if managed.is_dir()
            && let Some(candidate) = venv_candidate(&managed, base_env, &mut seen)
            && emit(candidate)
        {
            return true;
        }
    }

    if let Some(path_var) = base_env.get("PATH") {
        for bin in ["python3", "python"] {
            for dir in env::split_paths(path_var) {
                let candidate = dir.join(bin);
                if is_executable_file(&candidate)
                    && seen.insert(candidate.clone())
                    && emit(PythonRuntime {
                        python_path: candidate,
                        env: base_env.clone(),
                        venv_path: None,
                    })
                {
                    return true;
                }
            }
        }
    }

    found
}

fn probe_candidate(runtime: &PythonRuntime, cwd: &Path) -> Result<()> {
    let mut cmd = Command::new(&runtime.python_path);
    cmd.args([
        "-S",
        "-c",
        "import sys; raise SystemExit(0 if sys.version_info >= (3, 9) else 1)",
    ])
    .current_dir(cwd)
    .env_clear()
    .envs(&runtime.env)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning {}", runtime.python_path.display()))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut poll = Duration::from_micros(250);
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            bail!(
                "{} exited with status {} (Python >= 3.9 required)",
                runtime.python_path.display(),
                status
            );
        }
        let now = Instant::now();
        if now >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{} probe timed out", runtime.python_path.display());
        }
        thread::sleep(poll.min(deadline - now));
        poll = (poll * 2).min(Duration::from_millis(5));
    }
}

/// 探测可用 Python 3.9+ 运行时；成功结果按环境键缓存。
pub fn probe_python(cwd: &Path, interpreter: Option<&str>) -> Result<PythonRuntime> {
    let base_env = filter_env(env::vars());
    let trimmed_interpreter = interpreter
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let cwd_sensitive = trimmed_interpreter.as_deref().is_some_and(|raw| {
        let home = base_env.get("HOME").map(String::as_str);
        !expand_tilde(raw, home).is_absolute()
    }) || (trimmed_interpreter.is_none()
        && (cwd.join(".venv").is_dir() || cwd.join("venv").is_dir()));
    let key = ProbeCacheKey {
        cwd: cwd_sensitive.then(|| cwd.to_path_buf()),
        interpreter: trimmed_interpreter,
        path: base_env.get("PATH").cloned(),
        virtual_env: base_env.get("VIRTUAL_ENV").cloned(),
        conda_prefix: base_env.get("CONDA_PREFIX").cloned(),
        home: base_env.get("HOME").cloned(),
    };
    if let Ok(guard) = probe_cache().lock()
        && let Some(cached) = guard.get(&key)
        && is_executable_file(&cached.python_path)
    {
        return Ok(cached.clone());
    }

    let mut matched = None;
    let mut failures = Vec::new();
    let any_candidate =
        try_candidates(
            cwd,
            interpreter,
            &base_env,
            |candidate| match probe_candidate(&candidate, cwd) {
                Ok(()) => {
                    matched = Some(candidate);
                    true
                }
                Err(error) => {
                    failures.push(format!("{error:#}"));
                    false
                }
            },
        );
    if let Some(candidate) = matched {
        if let Ok(mut guard) = probe_cache().lock() {
            guard.insert(key, candidate.clone());
        }
        return Ok(candidate);
    }
    if !any_candidate {
        bail!("Python 3.9+ executable not found on PATH; configure eval.interpreter if needed");
    }

    bail!(
        "No working Python 3.9+ interpreter found ({})",
        failures.join("; ")
    )
}
