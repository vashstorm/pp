//! Git worktree、脏基线和嵌套仓库补丁；冲突保留所有恢复产物。
use super::{atomic_json, lock};
use crate::cancellation::{self, CancellationToken};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs,
    io::Read,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::Duration,
};

#[derive(Serialize, Deserialize)]
struct Lease {
    source: PathBuf,
    target: PathBuf,
    baseline: String,
    relative: PathBuf,
    #[serde(default)]
    nested: Vec<String>,
}
pub(crate) struct Change {
    pub path: PathBuf,
    pub before: Option<Vec<u8>>,
    pub after: Option<Vec<u8>>,
}
#[derive(Default)]
pub(crate) struct Capture {
    pub patches: Vec<PathBuf>,
    pub changes: Vec<Change>,
}
struct Git<'a> {
    cwd: &'a Path,
    token: &'a CancellationToken,
}
impl Git<'_> {
    fn run(&self, args: &[&OsStr]) -> Result<Vec<u8>> {
        cancellation::check(self.token)?;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(self.cwd)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let stdout = child.stdout.take().context("Git stdout unavailable")?;
        let stderr = child.stderr.take().context("Git stderr unavailable")?;
        let output = thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(16 * 1024 * 1024)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let errors = thread::spawn(move || {
            let mut bytes = Vec::new();
            stderr
                .take(1024 * 1024)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let status = loop {
            if self.token.is_cancelled() {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                let _ = output.join();
                let _ = errors.join();
                return Err(crate::cancellation::Cancelled.into());
            }
            if let Some(status) = child.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let output = output
            .join()
            .map_err(|_| anyhow::anyhow!("failed to read Git output"))??;
        let errors = errors
            .join()
            .map_err(|_| anyhow::anyhow!("failed to read Git errors"))??;
        ensure!(
            status.success(),
            "Git isolation operation failed: {}",
            String::from_utf8_lossy(&errors)
        );
        Ok(output)
    }
    fn text(&self, args: &[&str]) -> Result<String> {
        Ok(
            String::from_utf8(self.run(&args.iter().map(OsStr::new).collect::<Vec<_>>())?)?
                .trim()
                .to_owned(),
        )
    }
    fn tree(&self) -> Result<String> {
        self.text(&["add", "-A"])?;
        self.text(&["write-tree"])
    }
}
fn metadata(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.workspace.json"))
}
pub(crate) fn prepare(
    source: &Path,
    dir: &Path,
    id: &str,
    token: &CancellationToken,
) -> Result<PathBuf> {
    prepare_at(
        source,
        dir,
        id,
        &dir.join(format!("{id}.worktree")),
        token,
        0,
    )
}
fn prepare_at(
    source: &Path,
    dir: &Path,
    id: &str,
    target: &Path,
    token: &CancellationToken,
    depth: usize,
) -> Result<PathBuf> {
    ensure!(depth <= 8, "nested repository depth exceeds the limit");
    let meta = metadata(dir, id);
    if meta.exists() {
        let lease: Lease = serde_json::from_slice(&fs::read(meta)?)?;
        ensure!(lease.target.is_dir(), "isolation directory is missing");
        return Ok(lease.target.join(lease.relative));
    }
    let source = source.canonicalize()?;
    let root = PathBuf::from(
        Git {
            cwd: &source,
            token,
        }
        .text(&["rev-parse", "--show-toplevel"])?,
    )
    .canonicalize()?;
    let relative = source.strip_prefix(&root)?.to_owned();
    fs::create_dir_all(dir)?;
    let artifacts = dir.canonicalize()?;
    let nested = find_nested(&root, &artifacts)?;
    let git = Git { cwd: &root, token };
    if target.exists() {
        ensure!(
            !dir.join(format!("{id}.session.json")).exists(),
            "isolation baseline lost; workspace preserved, refusing to overwrite existing subagent results"
        );
        let actual = Git { cwd: target, token }.text(&["rev-parse", "--show-toplevel"])?;
        ensure!(
            PathBuf::from(actual).canonicalize()? == target.canonicalize()?,
            "isolation target is not an independent worktree"
        );
    } else {
        git.run(&[
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            target.as_os_str(),
            "HEAD".as_ref(),
        ])?;
    }
    let paths = git.run(&[
        "ls-files".as_ref(),
        "-z".as_ref(),
        "--cached".as_ref(),
        "--others".as_ref(),
        "--exclude-standard".as_ref(),
    ])?;
    let mut total = 0u64;
    for bytes in paths.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        cancellation::check(token)?;
        let path = Path::new(OsStr::from_bytes(bytes));
        ensure!(
            path.is_relative()
                && !path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir)),
            "invalid Git path"
        );
        let src = root.join(path);
        let dst = target.join(path);
        if src.starts_with(&artifacts) || nested.iter().any(|p| src.starts_with(p)) {
            continue;
        }
        match fs::symlink_metadata(&src) {
            Ok(meta) if meta.is_file() => {
                total = total.saturating_add(meta.len());
                ensure!(
                    total <= 1024 * 1024 * 1024,
                    "workspace snapshot exceeds 1 GiB"
                );
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                if dst.is_symlink() {
                    fs::remove_file(&dst)?;
                }
                let mut input = fs::File::open(&src)?;
                let mut output = fs::File::create(&dst)?;
                loop {
                    cancellation::check(token)?;
                    let count = std::io::copy(
                        &mut std::io::Read::by_ref(&mut input).take(65536),
                        &mut output,
                    )?;
                    if count == 0 {
                        break;
                    }
                }
                fs::set_permissions(dst, meta.permissions())?;
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                if dst.is_file() || dst.is_symlink() {
                    fs::remove_file(&dst)?;
                }
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if dst.is_file() || dst.is_symlink() {
                    fs::remove_file(dst)?;
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut children = Vec::new();
    for (index, repo) in nested.iter().enumerate() {
        let child_id = format!("{id}-nested-{index}");
        let child_target = target.join(repo.strip_prefix(&root)?);
        if child_target.is_dir()
            && !metadata(dir, &child_id).exists()
            && !child_target.join(".git").exists()
        {
            fs::remove_dir(&child_target).with_context(|| {
                format!(
                    "nested repository target is not an empty directory: {}",
                    child_target.display()
                )
            })?;
        }
        if let Some(parent) = child_target.parent() {
            fs::create_dir_all(parent)?;
        }
        prepare_at(repo, dir, &child_id, &child_target, token, depth + 1)?;
        children.push(child_id);
    }
    let baseline = Git { cwd: target, token }.tree()?;
    atomic_json(
        &meta,
        &Lease {
            source: root,
            target: target.to_owned(),
            baseline,
            relative: relative.clone(),
            nested: children,
        },
    )?;
    Ok(target.join(relative))
}
fn find_nested(root: &Path, artifacts: &Path) -> Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_dir()
                || entry.file_name() == ".git"
                || path.starts_with(artifacts)
            {
                continue;
            }
            if path.join(".git").exists() {
                repos.push(path);
                continue;
            }
            // 使用 Git ignore 规则避免扫描 target、node_modules 等构建目录。
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["check-ignore", "-q"])
                .arg(&path)
                .status()?;
            if !output.success() {
                dirs.push(path);
            }
        }
    }
    repos.sort();
    Ok(repos)
}
struct Prepared {
    meta: PathBuf,
    lease: Lease,
    next: String,
    patch: PathBuf,
    paths: Vec<PathBuf>,
    already_applied: bool,
}
fn collect(dir: &Path, id: &str, token: &CancellationToken, out: &mut Vec<Prepared>) -> Result<()> {
    let meta = metadata(dir, id);
    if !meta.exists() {
        return Ok(());
    }
    let lease: Lease = serde_json::from_slice(&fs::read(&meta)?)?;
    for child in &lease.nested {
        collect(dir, child, token, out)?;
    }
    let git = Git {
        cwd: &lease.target,
        token,
    };
    let next = git.tree()?;
    let bytes = git.run(&[
        "diff".as_ref(),
        "--binary".as_ref(),
        lease.baseline.as_ref(),
        next.as_ref(),
    ])?;
    if bytes.is_empty() {
        return Ok(());
    }
    let names = git.run(&[
        "diff".as_ref(),
        "--name-only".as_ref(),
        "-z".as_ref(),
        lease.baseline.as_ref(),
        next.as_ref(),
    ])?;
    let paths = names
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| PathBuf::from(OsString::from(OsStr::from_bytes(b))))
        .collect();
    let patch = dir.join(format!("{id}.patch"));
    fs::write(&patch, bytes)?;
    out.push(Prepared {
        meta,
        lease,
        next,
        patch,
        paths,
        already_applied: false,
    });
    Ok(())
}
pub(crate) fn capture(
    dir: &Path,
    id: &str,
    apply: bool,
    token: &CancellationToken,
) -> Result<Capture> {
    let mut prepared = Vec::new();
    collect(dir, id, token, &mut prepared)?;
    let mut captured = Capture {
        patches: prepared.iter().map(|p| p.patch.clone()).collect(),
        changes: Vec::new(),
    };
    if !apply {
        return Ok(captured);
    }
    static MERGE: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = lock(MERGE.get_or_init(|| Mutex::new(())));
    for item in &mut prepared {
        let git = Git {
            cwd: &item.lease.source,
            token,
        };
        if git
            .run(&["apply".as_ref(), "--check".as_ref(), item.patch.as_os_str()])
            .is_err()
        {
            git.run(&[
                "apply".as_ref(),
                "--reverse".as_ref(),
                "--check".as_ref(),
                item.patch.as_os_str(),
            ])
            .with_context(|| {
                format!(
                    "isolation change conflict; keeping patch {} and workspace {}",
                    item.patch.display(),
                    item.lease.target.display()
                )
            })?;
            item.already_applied = true;
        }
    }
    for item in &mut prepared {
        if !item.already_applied {
            let before = item
                .paths
                .iter()
                .map(|p| fs::read(item.lease.source.join(p)).ok())
                .collect::<Vec<_>>();
            Git {
                cwd: &item.lease.source,
                token,
            }
            .run(&["apply".as_ref(), item.patch.as_os_str()])?;
            for (path, before) in item.paths.iter().zip(before) {
                let path = item.lease.source.join(path);
                let after = fs::read(&path).ok();
                captured.changes.push(Change {
                    path,
                    before,
                    after,
                });
            }
        }
        item.lease.baseline = item.next.clone();
        atomic_json(&item.meta, &item.lease)?;
    }
    Ok(captured)
}
