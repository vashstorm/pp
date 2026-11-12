use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{self, Mode, OFlags},
    io::Errno,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

const MAINTENANCE: &str = "When code changes make project descriptions, commands, paths, or workflows in AGENTS.md outdated or inconsistent, update AGENTS.md as part of the same task. Preserve valid project constraints; never rewrite rules merely to conceal a violation. Project instructions apply only within the specified directory and its descendants in the specified project root. Parent rules remain effective; deeper rules take precedence on conflicts within their scope. Rules from sibling directories or other projects do not apply outside their scope. All project rules remain subject to higher-priority instructions and the user's current request. Read target files before modifying them to discover applicable rules. If a tool reports rules were not included in the model request, reconsider the change using the updated rules and submit new tool calls; do not bypass the check using bash or external tools.";

pub const INIT_PROMPT: &str = "Create or update AGENTS.md in the startup working directory. First inspect the existing AGENTS.md, if present, and relevant project files using the available tools. Based only on observed evidence, document the project purpose, architecture, development commands, testing procedures, and necessary project constraints. If AGENTS.md already exists, preserve valid rules and user guidance while correcting outdated descriptions, commands, paths, and workflows. Keep the document concise and useful to future coding agents; do not invent facts or expose secrets. Do not change unrelated code. Use write or edit to save AGENTS.md, then briefly describe the changes actually made.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Document {
    pub(crate) path: PathBuf,
    root: PathBuf,
    pub(crate) content: String,
}

/// 每个 Agent 独立保存已激活的目录和最近一次请求的完整规则快照。
pub(crate) struct State {
    cwd: PathBuf,
    directories: BTreeSet<PathBuf>,
    current: Vec<Document>,
    sent: Vec<Document>,
    roots: BTreeSet<PathBuf>,
    sent_roots: BTreeSet<PathBuf>,
    blocked: Option<String>,
}

impl State {
    pub(crate) fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_owned(),
            directories: BTreeSet::new(),
            current: Vec::new(),
            sent: Vec::new(),
            roots: BTreeSet::new(),
            sent_roots: BTreeSet::new(),
            blocked: None,
        }
    }

    fn root(&self, directory: &Path) -> Option<PathBuf> {
        directory
            .ancestors()
            .find(|parent| parent.join(".git").exists())
            .map(Path::to_owned)
            .or_else(|| directory.starts_with(&self.cwd).then(|| self.cwd.clone()))
    }

    fn chain(&self, directory: &Path) -> Result<Vec<Document>> {
        let Some(root) = self.root(directory) else {
            return Ok(Vec::new());
        };
        let mut directories = Vec::new();
        for parent in directory.ancestors() {
            directories.push(parent);
            if parent == root {
                break;
            }
        }
        let mut documents = Vec::new();
        for directory in directories.into_iter().rev() {
            let path = directory.join("AGENTS.md");
            if let Some(content) = read_file(&path)?
                && !content.trim().is_empty()
            {
                documents.push(Document {
                    path,
                    root: root.clone(),
                    content,
                });
            }
        }
        Ok(documents)
    }

    pub(crate) fn activate(&mut self, directory: PathBuf) -> Result<()> {
        // 先校验整条链，读取失败时不把半份规则当成已加载。
        self.chain(&directory)?;
        self.directories.insert(directory);
        Ok(())
    }

    pub(crate) fn remember(&mut self, directory: PathBuf) {
        if directory.is_absolute() {
            self.directories.insert(directory);
        }
    }

    pub(crate) fn refresh(&mut self) -> Result<Vec<Document>> {
        let mut documents = BTreeMap::new();
        let mut roots = BTreeSet::new();
        for directory in &self.directories {
            if let Some(root) = self.root(directory) {
                roots.insert(root);
            }
            for document in self.chain(directory)? {
                documents.insert(document.path.clone(), document);
            }
        }
        self.current = documents.into_values().collect();
        self.roots = roots;
        self.current.sort_by(|a, b| {
            a.path
                .components()
                .count()
                .cmp(&b.path.components().count())
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(self.current.clone())
    }

    pub(crate) fn sent(&mut self) {
        self.sent.clone_from(&self.current);
        self.sent_roots.clone_from(&self.roots);
        self.blocked = None;
    }

    pub(crate) fn paths(&self) -> Vec<PathBuf> {
        self.current
            .iter()
            .map(|document| document.path.clone())
            .collect()
    }

    pub(crate) fn scope_summary(&self) -> String {
        let mut summary = String::new();
        for root in &self.roots {
            if self
                .current
                .iter()
                .any(|document| root != &document.root && root.starts_with(&document.root))
            {
                summary.push_str(&format!(
                    "\n[Project root boundary: {}]\nAncestor project rules do not apply inside this project root.\n",
                    root.display(),
                ));
            }
        }
        summary
    }

    pub(crate) fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    pub(crate) fn block(&mut self, message: String) {
        self.blocked = Some(message);
    }

    pub(crate) fn check(&mut self, directory: PathBuf) -> Result<()> {
        self.directories.insert(directory.clone());
        let documents = self.chain(&directory)?;
        let sent_root = directory
            .ancestors()
            .find(|parent| self.sent_roots.contains(*parent));
        let sent: Vec<_> = self
            .sent
            .iter()
            .filter(|document| {
                Some(document.root.as_path()) == sent_root
                    && document
                        .path
                        .parent()
                        .is_some_and(|parent| directory.starts_with(parent))
            })
            .cloned()
            .collect();
        if documents != sent {
            self.blocked = Some("Not executed: applicable AGENTS.md rules changed or were not included in the model request. The next request will include the current rules; reconsider and submit new tool calls.".into());
        }
        if let Some(message) = &self.blocked {
            anyhow::bail!("{message}");
        }
        Ok(())
    }
}

pub(crate) fn render(documents: &[Document]) -> String {
    let mut text = String::new();
    for document in documents {
        text.push_str(&format!(
            "\n\n[Project instructions: {}]\nApplies to directory: {}\nProject root: {}\n\n{}\n[End project instructions]\n",
            document.path.display(), document.path.parent().unwrap_or(&document.root).display(),
            document.root.display(), document.content,
        ));
    }
    text
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

pub(crate) fn compose(base: &str, project: &str, skills: &str) -> String {
    let mut system = format!("{base}\n\n{MAINTENANCE}");
    if !project.is_empty() {
        system.push_str("\n\n");
    }
    system.push_str(project);
    system.push_str(skills);
    system
}

#[cfg(test)]
mod tests;
