use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{self, Mode, OFlags},
    io::Errno,
};
use std::{fs::File, io::Read, path::Path};

const MAINTENANCE: &str = "When code changes make project descriptions, commands, paths, or workflows in AGENTS.md outdated or inconsistent, update AGENTS.md as part of the same task. Preserve valid project constraints; never rewrite rules merely to conceal a violation. Project instructions apply within the specified directory and remain subject to higher-priority instructions and the user's current request.";

pub(crate) const INIT_PROMPT: &str = "Create or update AGENTS.md in the startup working directory. First inspect the existing AGENTS.md, if present, and relevant project files using the available tools. Based only on observed evidence, document the project purpose, architecture, development commands, testing procedures, and necessary project constraints. If AGENTS.md already exists, preserve valid rules and user guidance while correcting outdated descriptions, commands, paths, and workflows. Keep the document concise and useful to future coding agents; do not invent facts or expose secrets. Do not change unrelated code. Use write or edit to save AGENTS.md, then briefly describe the changes actually made.";

pub(crate) fn load(cwd: &Path) -> Result<String> {
    let path = cwd.join("AGENTS.md");
    let mut content = read_file(&path)?.unwrap_or_default();
    if content.trim().is_empty() {
        content.clear();
    }
    Ok(content)
}

/// 读取普通 UTF-8 指令文件；缺失与空正文分开表示，保留显式空覆盖。
pub(crate) fn read_file(path: &Path) -> Result<Option<String>> {
    // 禁止跟随符号链接；非阻塞打开使 FIFO 等特殊文件不会卡住 worker。
    let fd = match fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("loading {} (symlinks are forbidden)", path.display()));
        }
    };
    let mut file = File::from(fd);
    ensure!(
        file.metadata()
            .with_context(|| format!("reading {} metadata", path.display()))?
            .is_file(),
        "{} must be a regular file",
        path.display()
    );
    let mut content = String::new();
    file.read_to_string(&mut content)
        .with_context(|| format!("reading {} as UTF-8", path.display()))?;
    Ok(Some(content))
}

pub(crate) fn compose(base: &str, cwd: &Path, project: &str, skills: &str) -> String {
    let mut system = format!("{base}\n\n{MAINTENANCE}");
    if !project.is_empty() {
        system.push_str(&format!(
            "\n\n[Project instructions: {}]\nApplies to directory: {}\n\n{project}\n[End project instructions]\n",
            cwd.join("AGENTS.md").display(),
            cwd.display()
        ));
    }
    system.push_str(skills);
    system
}
