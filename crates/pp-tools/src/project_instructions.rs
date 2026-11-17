use anyhow::{Context, Result, ensure};
use rustix::{
    fs::{self, Mode, OFlags},
    io::Errno,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

pub const MAINTENANCE: &str = "When code changes make project descriptions, commands, paths, or workflows in AGENTS.md outdated or inconsistent, update AGENTS.md as part of the same task. Preserve valid project constraints; never rewrite rules merely to conceal a violation. Project instructions apply only within the specified directory and its descendants in the specified project root. Parent rules remain effective; deeper rules take precedence on conflicts within their scope. Rules from sibling directories or other projects do not apply outside their scope. All project rules remain subject to higher-priority instructions and the user's current request. Read target files before modifying them to discover applicable rules. If a tool reports rules were not included in the model request, reconsider the change using the updated rules and submit new tool calls; do not bypass the check using bash or external tools.";

pub const INIT_PROMPT: &str = "Create or update AGENTS.md in the startup working directory. First inspect the existing AGENTS.md, if present, and relevant project files using the available tools. Based only on observed evidence, document the project purpose, architecture, development commands, testing procedures, and necessary project constraints. If AGENTS.md already exists, preserve valid rules and user guidance while correcting outdated descriptions, commands, paths, and workflows. Keep the document concise and useful to future coding agents; do not invent facts or expose secrets. Do not change unrelated code. Use write or edit to save AGENTS.md, then briefly describe the changes actually made.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Document {
    pub path: PathBuf,
    pub root: PathBuf,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedDocument {
    pub document: Document,
    pub sent: bool,
}

/// 单个规则文件的观测指纹：device/inode/长度/mtime（秒+纳秒）。
///
/// 与派生会话索引同一口径。只看 mtime 会漏掉“同长度、同一时间刻度内的覆盖写”，
/// 因此 inode 与长度一起参与；真需要字节级精确的 edit/write 前置门禁仍每次整读，
/// 不走该指纹（见 [`State::check`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    len: u64,
    seconds: i64,
    nanoseconds: i64,
}

impl Fingerprint {
    /// 只 stat，不跟随符号链接；文件缺失返回 `None`，其他错误上抛。
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {} metadata", path.display()));
            }
        };
        // 与 `read_file` 同一规则：符号链接、目录与其他特殊文件都不是规则来源。
        ensure!(
            metadata.is_file(),
            "{} must be a regular file",
            path.display()
        );
        Ok(Some(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            len: metadata.len(),
            seconds: metadata.mtime(),
            nanoseconds: metadata.mtime_nsec(),
        }))
    }
}

/// 目录链上的候选规则文件与它所属的项目根。
#[derive(Clone, Debug, PartialEq, Eq)]
struct RuleSource {
    path: PathBuf,
    root: PathBuf,
}

/// 最近一次成功读取的正文与读到它时的指纹。
struct Cached {
    fingerprint: Fingerprint,
    content: String,
}

/// 每个 Agent 独立保存已激活的目录和最近一次请求的完整规则快照。
pub struct State {
    cwd: PathBuf,
    directories: BTreeSet<PathBuf>,
    /// 当前规则快照与其共享副本：来源未变化时整份复用，不复制正文。
    current: Arc<[Document]>,
    sent: Arc<[Document]>,
    roots: BTreeSet<PathBuf>,
    sent_roots: BTreeSet<PathBuf>,
    /// 生成 `current` 时的候选来源与观测指纹，用来判断磁盘是否仍然一致。
    sources: Vec<RuleSource>,
    observed: Vec<Option<Fingerprint>>,
    /// 只保存成功读取的正文：读取失败不能当作规则缺失而被缓存。
    cache: BTreeMap<PathBuf, Cached>,
    blocked: Option<String>,
}

impl State {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_owned(),
            // 启动目录的祖先规则在首个请求前加载，后代目录仍按访问激活。
            directories: BTreeSet::from([cwd.to_owned()]),
            current: Arc::from(Vec::new()),
            sent: Arc::from(Vec::new()),
            roots: BTreeSet::new(),
            sent_roots: BTreeSet::new(),
            sources: Vec::new(),
            observed: Vec::new(),
            cache: BTreeMap::new(),
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

    /// 一条目录链上的候选规则文件（项目根在前）；没有隐式根时为空。
    fn chain_sources(&self, directory: &Path) -> Vec<RuleSource> {
        let Some(root) = self.root(directory) else {
            return Vec::new();
        };
        let mut directories = Vec::new();
        for parent in directory.ancestors() {
            directories.push(parent);
            if parent == root {
                break;
            }
        }
        directories
            .into_iter()
            .rev()
            .map(|directory| RuleSource {
                path: directory.join("AGENTS.md"),
                root: root.clone(),
            })
            .collect()
    }

    /// 已激活范围的候选规则文件（去重后按深度、路径排序）与项目根集合。
    fn sources(&self) -> (Vec<RuleSource>, BTreeSet<PathBuf>) {
        let mut found = BTreeMap::new();
        let mut roots = BTreeSet::new();
        for directory in &self.directories {
            if let Some(root) = self.root(directory) {
                roots.insert(root);
            }
            for source in self.chain_sources(directory) {
                // 同一文件被多条已激活链覆盖时，与既有顺序一致由后者决定所属根。
                found.insert(source.path.clone(), source);
            }
        }
        let mut sources: Vec<RuleSource> = found.into_values().collect();
        sources.sort_by(|left, right| {
            left.path
                .components()
                .count()
                .cmp(&right.path.components().count())
                .then_with(|| left.path.cmp(&right.path))
        });
        (sources, roots)
    }

    /// 只 stat 候选文件；`None` 表示当前不存在。
    fn observe(sources: &[RuleSource]) -> Result<Vec<Option<Fingerprint>>> {
        sources
            .iter()
            .map(|source| Fingerprint::read(&source.path))
            .collect()
    }

    /// 命中指纹时取缓存正文，否则整读并记录当前指纹。
    fn cached(&mut self, source: &RuleSource, fingerprint: Fingerprint) -> Result<Option<&str>> {
        if self
            .cache
            .get(&source.path)
            .is_none_or(|cached| cached.fingerprint != fingerprint)
        {
            let Some(content) = read_file(&source.path)? else {
                return Ok(None);
            };
            self.cache.insert(
                source.path.clone(),
                Cached {
                    fingerprint,
                    content,
                },
            );
        }
        Ok(self
            .cache
            .get(&source.path)
            .map(|cached| cached.content.as_str()))
    }

    /// 按观测结果装配快照；指纹未变化的文件不再读盘。
    fn load(
        &mut self,
        sources: &[RuleSource],
        observed: &[Option<Fingerprint>],
    ) -> Result<Vec<Document>> {
        let mut documents = Vec::new();
        for (source, fingerprint) in sources.iter().zip(observed) {
            let Some(fingerprint) = *fingerprint else {
                continue;
            };
            let Some(content) = self.cached(source, fingerprint)? else {
                continue;
            };
            // 缺失与空白正文都不追加，也不清空父规则。
            if content.trim().is_empty() {
                continue;
            }
            documents.push(Document {
                path: source.path.clone(),
                root: source.root.clone(),
                content: content.to_owned(),
            });
        }
        Ok(documents)
    }

    /// edit/write 前置门禁的目录链：坚持整读，保证判定与磁盘字节一致。
    fn chain_fresh(&self, directory: &Path) -> Result<Vec<Document>> {
        let mut documents = Vec::new();
        for source in self.chain_sources(directory) {
            if let Some(content) = read_file(&source.path)?
                && !content.trim().is_empty()
            {
                documents.push(Document {
                    path: source.path,
                    root: source.root,
                    content,
                });
            }
        }
        Ok(documents)
    }

    /// 读取目录链的当前正文（激活校验用），指纹未变化的文件复用缓存。
    fn chain(&mut self, directory: &Path) -> Result<Vec<Document>> {
        let sources = self.chain_sources(directory);
        let observed = Self::observe(&sources)?;
        self.load(&sources, &observed)
    }

    pub fn activate(&mut self, directory: PathBuf) -> Result<()> {
        // 先校验整条链，读取失败时不把半份规则当成已加载。
        self.chain(&directory)?;
        self.directories.insert(directory);
        Ok(())
    }

    pub fn remember(&mut self, directory: PathBuf) {
        if directory.is_absolute() {
            self.directories.insert(directory);
        }
    }

    /// 每个普通／摘要请求的当前规则快照。
    ///
    /// 激活范围、候选来源与指纹都未变化时直接共享上一份快照（不读盘、不复制正文）；
    /// 只有指纹变化的文件重新读取，因此磁盘变更在下一轮请求一定可见。
    pub fn refresh_shared(&mut self) -> Result<Arc<[Document]>> {
        let (sources, roots) = self.sources();
        let observed = Self::observe(&sources)?;
        if roots == self.roots && sources == self.sources && observed == self.observed {
            return Ok(Arc::clone(&self.current));
        }
        let documents = self.load(&sources, &observed)?;
        self.current = Arc::from(documents);
        self.sources = sources;
        self.observed = observed;
        self.roots = roots;
        Ok(Arc::clone(&self.current))
    }

    /// 需要独占列表的调用方（独立预览、既有测试）使用；请求路径用 [`State::refresh_shared`]。
    pub fn refresh(&mut self) -> Result<Vec<Document>> {
        Ok(self.refresh_shared()?.to_vec())
    }

    pub fn inspect(&mut self) -> Result<(Vec<InspectedDocument>, String)> {
        let (sources, roots) = self.sources();
        let observed = Self::observe(&sources)?;
        let documents = self.load(&sources, &observed)?;
        let scope = scope_summary_for(&documents, &roots);
        let inspected = documents
            .into_iter()
            .map(|document| {
                let sent = self.sent.iter().any(|existing| existing == &document);
                InspectedDocument { document, sent }
            })
            .collect();
        Ok((inspected, scope))
    }

    pub fn sent(&mut self) {
        // 与 `current` 共享同一份快照：每轮请求不再深拷贝全部规则正文。
        self.sent = Arc::clone(&self.current);
        self.sent_roots.clone_from(&self.roots);
        self.blocked = None;
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.current
            .iter()
            .map(|document| document.path.clone())
            .collect()
    }

    pub fn scope_summary(&self) -> String {
        scope_summary_for(&self.current, &self.roots)
    }

    pub fn blocked(&self) -> Option<&str> {
        self.blocked.as_deref()
    }

    pub fn block(&mut self, message: String) {
        self.blocked = Some(message);
    }

    pub fn check(&mut self, directory: PathBuf) -> Result<()> {
        self.directories.insert(directory.clone());
        // 门禁按磁盘字节判定：同长度、同 mtime 的覆盖写不能绕过规则检查。
        let documents = self.chain_fresh(&directory)?;
        let sent_root = directory
            .ancestors()
            .find(|parent| self.sent_roots.contains(*parent));
        let sent = self.sent.iter().filter(|document| {
            Some(document.root.as_path()) == sent_root
                && document
                    .path
                    .parent()
                    .is_some_and(|parent| directory.starts_with(parent))
        });
        if documents.iter().ne(sent) {
            self.blocked = Some("Not executed: applicable AGENTS.md rules changed or were not included in the model request. The next request will include the current rules; reconsider and submit new tool calls.".into());
        }
        if let Some(message) = &self.blocked {
            anyhow::bail!("{message}");
        }
        Ok(())
    }
}

fn scope_summary_for(documents: &[Document], roots: &BTreeSet<PathBuf>) -> String {
    let mut summary = String::new();
    for root in roots {
        if documents
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

pub fn render(documents: &[Document]) -> String {
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
pub fn read_file(path: &Path) -> Result<Option<String>> {
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

pub fn compose(base: &str, project: &str, skills: &str) -> String {
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
